//! Approximate substring matching for the fuzzy cascade stages.
//!
//! Bitap (shift-and with errors, Wu–Manber): finds occurrences of a
//! pattern *within* a haystack with at most `k` Levenshtein edits
//! (insertion / deletion / substitution); the u64 bit-parallel update costs
//! O(k) word ops per haystack byte with zero allocations.
//!
//! Callers fold both sides to ASCII lowercase first (the pipeline-wide
//! convention). Patterns are limited to 64 bytes by the machine word; the
//! cascade skips fuzzy stages for longer terms.

/// Registers the automaton needs: one per error count `0..=k`.
///
/// `k` is bounded by [`edit_budget`]'s one-edit-per-three-characters ladder
/// against a pattern the machine word caps at 64 bytes, so it never exceeds 21
/// however hostile `fuzzy_max_edits` is (pinned by
/// `edit_budget_stays_within_the_bitap_word_size`). [`Bitap::new`] rejects
/// anything larger, so the array is always big enough.
const MAX_REGISTERS: usize = 22;

pub struct Bitap {
    /// `masks[c]` has bit `i` set iff `pattern[i] == c`.
    masks: [u64; 256],
    /// Pattern length in bytes (1..=64).
    len: usize,
    /// Maximum edit distance.
    k: usize,
}

impl Bitap {
    /// `None` when the pattern is empty, longer than 64 bytes, or the edit
    /// budget does not fit the registers (`reset` shifts by `k`, and the
    /// register array holds [`MAX_REGISTERS`]).
    pub fn new(pattern: &[u8], k: usize) -> Option<Bitap> {
        if pattern.is_empty() || pattern.len() > 64 || k >= MAX_REGISTERS {
            return None;
        }
        let mut masks = [0u64; 256];
        for (i, &b) in pattern.iter().enumerate() {
            masks[b as usize] |= 1u64 << i;
        }
        Some(Bitap {
            masks,
            len: pattern.len(),
            k,
        })
    }

    /// Reset the per-distance state registers. Bit `i` of `r[d]` set means "a
    /// match of pattern[..=i] with ≤ d errors ends at the current text
    /// position". With d errors the first d pattern bytes can be deleted
    /// before any text is read, hence the pre-set low bits.
    fn reset(&self, r: &mut [u64; MAX_REGISTERS]) {
        for (d, reg) in r.iter_mut().enumerate().take(self.k + 1) {
            *reg = if d == 0 { 0 } else { (1u64 << d) - 1 };
        }
    }

    /// Advance all registers by one haystack byte. Returns the smallest
    /// error count d for which the full pattern just matched, if any.
    #[inline]
    fn step(&self, r: &mut [u64], byte: u8) -> Option<usize> {
        let mask = self.masks[byte as usize];
        let done = 1u64 << (self.len - 1);
        let mut hit = None;
        let mut prev_old = r[0]; // R_old[d-1] for the d-th iteration
                                 // d = 0: exact prefix extension only.
        r[0] = ((r[0] << 1) | 1) & mask;
        if r[0] & done != 0 {
            hit = Some(0);
        }
        for d in 1..=self.k {
            let old = r[d];
            r[d] = (((old << 1) | 1) & mask)      // extend a ≤d-error state
                | prev_old                          // insertion in text
                | (prev_old << 1)                   // substitution
                | ((r[d - 1] << 1) | 1); // deletion (pattern byte skipped)
            prev_old = old;
            if hit.is_none() && r[d] & done != 0 {
                hit = Some(d);
            }
        }
        hit
    }

    /// Minimum edit distance (≤ k) of any occurrence of the pattern in
    /// `hay`, or `None` if nothing matches within k edits.
    pub fn best_distance(&self, hay: &[u8]) -> Option<usize> {
        self.best_distance_and_first(hay).map(|(d, _)| d)
    }

    /// [`best_distance`](Self::best_distance) plus the first match's
    /// approximate byte range, from one sweep. The range carries the same
    /// caveat as [`count_and_first`](Self::count_and_first): it assumes a
    /// pattern-length match, so edits can shift the true start by up to `k`.
    pub fn best_distance_and_first(&self, hay: &[u8]) -> Option<(usize, (usize, usize))> {
        let mut r = [0u64; MAX_REGISTERS];
        self.reset(&mut r);
        let mut best: Option<(usize, (usize, usize))> = None;
        for (i, &b) in hay.iter().enumerate() {
            if let Some(d) = self.step(&mut r, b) {
                let end = i + 1;
                let range = (end.saturating_sub(self.len), end);
                if d == 0 {
                    return Some((0, range));
                }
                if best.is_none_or(|(cur, _)| d < cur) {
                    best = Some((d, range));
                }
            }
        }
        best
    }

    /// Count non-overlapping occurrences (at ≤ k edits) and report the
    /// first match's approximate byte range in `hay`. After each hit the
    /// automaton resets, so an exact match followed by trailing bytes
    /// counts once, and overlapping suffix matches don't inflate counts.
    /// The reported range assumes pattern-length matches — edits can shift
    /// the true start by up to k bytes, which is fine for snippet windows.
    pub fn count_and_first(&self, hay: &[u8]) -> (usize, Option<(usize, usize)>) {
        let mut r = [0u64; MAX_REGISTERS];
        self.reset(&mut r);
        let mut count = 0usize;
        let mut first: Option<(usize, usize)> = None;
        for (i, &b) in hay.iter().enumerate() {
            if self.step(&mut r, b).is_some() {
                count += 1;
                if first.is_none() {
                    let end = i + 1;
                    first = Some((end.saturating_sub(self.len), end));
                }
                self.reset(&mut r);
            }
        }
        (count, first)
    }
}

/// The cascade's edit-distance budget for a folded term: one edit per
/// three characters, capped by `[search].fuzzy_max_edits`. Terms outside
/// 3..=64 bytes skip the fuzzy stages entirely (< 3 is noise, > 64 exceeds
/// the word size), and a cap of 0 disables them everywhere.
///
/// At the default cap of 2 this is the historic ladder: 3–5 bytes get one
/// edit, 6–64 get two.
pub fn edit_budget(term_len: usize, max_edits: usize) -> Option<usize> {
    if !(3..=64).contains(&term_len) || max_edits == 0 {
        return None;
    }
    Some((term_len / 3).min(max_edits))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn best(pattern: &str, hay: &str, k: usize) -> Option<usize> {
        Bitap::new(pattern.as_bytes(), k)
            .unwrap()
            .best_distance(hay.as_bytes())
    }

    #[test]
    fn exact_substring_is_distance_zero() {
        assert_eq!(best("hello", "say hello world", 2), Some(0));
        assert_eq!(best("hello", "hello", 0), Some(0));
    }

    #[test]
    fn single_edits_are_distance_one() {
        assert_eq!(best("hello", "xx hxllo xx", 2), Some(1), "substitution");
        assert_eq!(best("hello", "xx helo xx", 2), Some(1), "deletion");
        assert_eq!(best("hello", "xx heXllo xx", 2), Some(1), "insertion");
    }

    #[test]
    fn two_edits() {
        assert_eq!(best("hello", "xx hxlo xx", 2), Some(2));
        assert_eq!(best("hello", "xx ho xx", 2), None, "3 edits > k");
    }

    #[test]
    fn no_match_within_budget() {
        assert_eq!(best("hello", "completely different", 1), None);
        assert_eq!(best("abc", "", 1), None);
    }

    #[test]
    fn k_zero_is_exact_search() {
        assert_eq!(best("abc", "xxabcxx", 0), Some(0));
        assert_eq!(best("abc", "xxabxcx", 0), None);
    }

    #[test]
    fn pattern_length_limits() {
        assert!(Bitap::new(b"", 1).is_none());
        assert!(Bitap::new(&[b'a'; 65], 1).is_none());
        assert!(Bitap::new(&[b'a'; 64], 1).is_some());
    }

    #[test]
    fn oversized_k_is_rejected_not_shifted() {
        // `reset` shifts by k and writes k+1 registers, so both the u64 shift
        // and the fixed array have to be respected.
        assert!(Bitap::new(b"abc", MAX_REGISTERS).is_none());
        assert!(Bitap::new(b"abc", 64).is_none());
        assert!(Bitap::new(b"abc", usize::MAX).is_none());
        assert!(Bitap::new(b"abc", MAX_REGISTERS - 1).is_some());
    }

    #[test]
    fn count_non_overlapping() {
        let b = Bitap::new(b"ab", 0).unwrap();
        let (count, first) = b.count_and_first(b"ab ab ab");
        assert_eq!(count, 3);
        assert_eq!(first, Some((0, 2)));

        // "aaaa" contains "aaa" once non-overlapping.
        let b = Bitap::new(b"aaa", 0).unwrap();
        let (count, _) = b.count_and_first(b"aaaa");
        assert_eq!(count, 1);
    }

    #[test]
    fn count_fuzzy_and_range_sane() {
        let b = Bitap::new(b"hello", 1).unwrap();
        let hay = b"say helo and hxllo again";
        let (count, first) = b.count_and_first(hay);
        assert_eq!(count, 2);
        let (s, e) = first.unwrap();
        assert!(s < e && e <= hay.len());
        let window = &hay[s..e];
        assert!(
            std::str::from_utf8(window).unwrap().contains("hel"),
            "first range should cover the first hit, got {:?}",
            std::str::from_utf8(window)
        );
    }

    #[test]
    fn edit_budget_default_cap_is_the_historic_ladder() {
        assert_eq!(edit_budget(0, 2), None);
        assert_eq!(edit_budget(2, 2), None);
        assert_eq!(edit_budget(3, 2), Some(1));
        assert_eq!(edit_budget(5, 2), Some(1));
        assert_eq!(edit_budget(6, 2), Some(2));
        assert_eq!(edit_budget(64, 2), Some(2));
        assert_eq!(edit_budget(65, 2), None);
        assert_eq!(edit_budget(usize::MAX, 2), None);
    }

    #[test]
    fn edit_budget_scales_with_length_up_to_the_cap() {
        assert_eq!(edit_budget(3, 4), Some(1));
        assert_eq!(edit_budget(6, 4), Some(2));
        assert_eq!(edit_budget(9, 4), Some(3));
        assert_eq!(edit_budget(12, 4), Some(4));
        assert_eq!(edit_budget(64, 4), Some(4), "cap wins over length");
    }

    #[test]
    fn edit_budget_cap_of_one_stays_strict() {
        for len in 3..=64 {
            assert_eq!(edit_budget(len, 1), Some(1));
        }
    }

    #[test]
    fn edit_budget_zero_disables_fuzzy() {
        for len in 0..=70 {
            assert_eq!(edit_budget(len, 0), None);
        }
    }

    /// Even a hostile config value can't produce a k the bitap rejects:
    /// the length ladder caps it at 21 for the longest legal term.
    #[test]
    fn edit_budget_stays_within_the_bitap_word_size() {
        for len in 3..=64 {
            let k = edit_budget(len, usize::MAX).unwrap();
            assert!(k <= 21, "len={} gave k={}", len, k);
            assert!(Bitap::new(&vec![b'a'; len], k).is_some());
        }
    }

    /// Brute-force oracle: minimum Levenshtein distance between `pattern`
    /// and any substring of `hay`, capped at k.
    fn oracle(pattern: &[u8], hay: &[u8], k: usize) -> Option<usize> {
        // An occurrence must end at some text position; empty text has none.
        // Without this, k >= pattern-length "matches" empty text by deleting
        // every pattern byte — a degenerate non-occurrence the automaton
        // rightly never reports.
        if hay.is_empty() {
            return None;
        }
        // Standard DP where row 0 is all zeros (match can start anywhere).
        let m = pattern.len();
        let mut prev: Vec<usize> = vec![0; hay.len() + 1];
        let mut cur = vec![0; hay.len() + 1];
        let mut best = usize::MAX;
        // dp[i][j] = min edits to match pattern[..i] ending at hay[..j]
        for i in 1..=m {
            cur[0] = i;
            for j in 1..=hay.len() {
                let cost = if pattern[i - 1] == hay[j - 1] { 0 } else { 1 };
                cur[j] = (prev[j - 1] + cost).min(prev[j] + 1).min(cur[j - 1] + 1);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        // The best occurrence may end at any text position, so the answer is
        // the smallest value in the final row.
        best = prev.iter().copied().min().unwrap_or(best);
        if best <= k {
            Some(best)
        } else {
            None
        }
    }

    #[test]
    fn matches_brute_force_oracle() {
        // Deterministic LCG so the test is reproducible.
        let mut seed: u64 = 0x2545F4914F6CDD1D;
        let mut rng = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        let alphabet = b"abcx";
        for _ in 0..500 {
            let plen = 3 + rng() % 6;
            let hlen = rng() % 20;
            let pattern: Vec<u8> = (0..plen).map(|_| alphabet[rng() % 4]).collect();
            let hay: Vec<u8> = (0..hlen).map(|_| alphabet[rng() % 4]).collect();
            for k in 0..=4 {
                let got = Bitap::new(&pattern, k).unwrap().best_distance(&hay);
                let want = oracle(&pattern, &hay, k);
                assert_eq!(
                    got,
                    want,
                    "pattern={:?} hay={:?} k={}",
                    std::str::from_utf8(&pattern),
                    std::str::from_utf8(&hay),
                    k
                );
            }
        }
    }
}
