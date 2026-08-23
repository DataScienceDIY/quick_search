//! Approximate substring matching (bitap / Wu–Manber shift-and) for the fuzzy
//! cascade stages: at most `k` Levenshtein edits, O(k) word ops per haystack
//! byte, zero allocations. Patterns are capped at 64 bytes by the machine word.
//!
//! Case-insensitivity lives in the mask table (both cases of every pattern
//! byte are set), so neither side is folded — don't reintroduce haystack
//! folding; measured, it cost a copy per scanned field and per stored document.

/// Registers: one per error count `0..=k`. [`edit_budget`]'s ladder against a
/// 64-byte pattern caps `k` at 21 (pinned by
/// `edit_budget_stays_within_the_bitap_word_size`); [`Bitap::new`] rejects more.
const MAX_REGISTERS: usize = 22;

pub struct Bitap {
    /// `masks[c]` has bit `i` set iff `pattern[i] == c` ignoring ASCII case.
    masks: [u64; 256],
    /// The same table for the *reversed* pattern, for [`Bitap::match_start`].
    rev_masks: [u64; 256],
    len: usize,
    k: usize,
}

impl Bitap {
    /// `None` when the pattern is empty, longer than 64 bytes, or `k` does not
    /// fit the registers. The pattern need not be folded: each byte's bit is
    /// set under both ASCII cases.
    pub fn new(pattern: &[u8], k: usize) -> Option<Bitap> {
        if pattern.is_empty() || pattern.len() > 64 || k >= MAX_REGISTERS {
            return None;
        }
        let mut masks = [0u64; 256];
        let mut rev_masks = [0u64; 256];
        for (i, &b) in pattern.iter().enumerate() {
            let (lower, upper) = (b.to_ascii_lowercase(), b.to_ascii_uppercase());
            let (fwd, rev) = (1u64 << i, 1u64 << (pattern.len() - 1 - i));
            masks[lower as usize] |= fwd;
            masks[upper as usize] |= fwd;
            rev_masks[lower as usize] |= rev;
            rev_masks[upper as usize] |= rev;
        }
        Some(Bitap {
            masks,
            rev_masks,
            len: pattern.len(),
            k,
        })
    }

    /// Bit `i` of `r[d]` set means "a match of pattern[..=i] with ≤ d errors
    /// ends at the current text position". With d errors the first d pattern
    /// bytes can be deleted before any text is read, hence the pre-set low bits.
    fn reset(&self, r: &mut [u64; MAX_REGISTERS]) {
        for (d, reg) in r.iter_mut().enumerate().take(self.k + 1) {
            *reg = if d == 0 { 0 } else { (1u64 << d) - 1 };
        }
    }

    /// Advance all registers by one haystack byte; returns the smallest error
    /// count d at which the full pattern just matched. `masks` selects the
    /// scan direction: [`Bitap::masks`] forwards, [`Bitap::rev_masks`] back.
    #[inline]
    fn step(&self, masks: &[u64; 256], r: &mut [u64], byte: u8) -> Option<usize> {
        let mask = masks[byte as usize];
        let done = 1u64 << (self.len - 1);
        let mut hit = None;
        let mut prev_old = r[0]; // R_old[d-1] for the d-th iteration
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

    /// Where the match that ended at `end` with `errors` edits began: the same
    /// automaton runs over the *reversed* pattern, backwards from `end`; the
    /// first position accepted **within `errors` edits** is the start. The
    /// bound is load-bearing — unbounded, the reverse scan spends its budget
    /// on deletions and marks one letter of an exact match.
    fn match_start(&self, hay: &[u8], end: usize, errors: usize) -> usize {
        // A ≤k-edit alignment of a len-byte pattern is at most len+k long,
        // so nothing before this can be the start.
        let floor = end.saturating_sub(self.len + self.k);
        let mut r = [0u64; MAX_REGISTERS];
        self.reset(&mut r);
        for (back, &b) in hay[floor..end].iter().rev().enumerate() {
            if self
                .step(&self.rev_masks, &mut r, b)
                .is_some_and(|d| d <= errors)
            {
                return end - (back + 1);
            }
        }
        // Unreachable: the forward scan proved an alignment ends here. The
        // floor keeps a hypothetical miss inside the haystack.
        floor
    }

    /// Improve on the *earliest* accepting end, which is systematically short
    /// (the automaton pays for the pattern's tail with trailing deletions). A
    /// better alignment ends at most `errors` bytes later, never more; a copy
    /// of the registers is stepped so the caller's scan is undisturbed.
    fn refine_end(
        &self,
        hay: &[u8],
        r: &[u64; MAX_REGISTERS],
        end: usize,
        errors: usize,
    ) -> (usize, usize) {
        let mut best = (errors, end);
        let mut probe = *r;
        for (ahead, &b) in hay[end..].iter().take(errors).enumerate() {
            if let Some(d) = self.step(&self.masks, &mut probe, b) {
                if d < best.0 {
                    best = (d, end + ahead + 1);
                }
            }
        }
        best
    }

    /// The smallest edit distance (≤ k) at which the pattern occurs in `hay`,
    /// and that occurrence's byte range — the span a frontend marks.
    pub fn best_distance_and_first(&self, hay: &[u8]) -> Option<(usize, (usize, usize))> {
        let mut r = [0u64; MAX_REGISTERS];
        self.reset(&mut r);
        let mut best: Option<(usize, usize)> = None;
        for (i, &b) in hay.iter().enumerate() {
            if let Some(d) = self.step(&self.masks, &mut r, b) {
                let end = i + 1;
                if d == 0 {
                    best = Some((0, end));
                    break;
                }
                if best.is_none_or(|(cur, _)| d < cur) {
                    best = Some((d, end));
                }
            }
        }
        best.map(|(d, end)| (d, (self.match_start(hay, end, d), end)))
    }

    /// Count non-overlapping occurrences (at ≤ k edits) and report the first
    /// one's byte range in `hay`. After each hit the automaton resets, so
    /// overlapping suffix matches don't inflate counts.
    pub fn count_and_first(&self, hay: &[u8]) -> (usize, Option<(usize, usize)>) {
        let mut r = [0u64; MAX_REGISTERS];
        self.reset(&mut r);
        let mut count = 0usize;
        let mut first: Option<(usize, usize)> = None;
        for (i, &b) in hay.iter().enumerate() {
            if let Some(d) = self.step(&self.masks, &mut r, b) {
                count += 1;
                if first.is_none() {
                    let (errors, end) = self.refine_end(hay, &r, i + 1, d);
                    first = Some((self.match_start(hay, end, errors), end));
                }
                self.reset(&mut r);
            }
        }
        (count, first)
    }
}

/// Re-exported: the trigram floor is a property of the index; see
/// [`crate::search::prefilter`].
pub use super::prefilter::TRIGRAM_FLOOR;

/// Split `term` into `k + 1` consecutive chunks of at least [`TRIGRAM_FLOOR`]
/// characters each, for the fuzzy full-text pass's candidate prefilter.
/// `None` when the term is too short to divide that way.
///
/// Sound because the chunks **partition** the term: `k` edits touch at most
/// `k` of the `k + 1` chunks, so one survives verbatim and "any chunk present"
/// is a superset of what the pass accepts. It must stay a partition — overlap
/// or gaps collapse the argument silently. The floor exists because a
/// sub-trigram chunk matches no token; the split is by characters, not bytes,
/// so no chunk breaks a UTF-8 sequence.
pub fn pigeonhole_chunks(term: &str, k: usize) -> Option<Vec<&str>> {
    let chunks = k + 1;
    let total = term.chars().count();
    if total < TRIGRAM_FLOOR * chunks {
        return None;
    }
    let mut bounds: Vec<usize> = term.char_indices().map(|(at, _)| at).collect();
    bounds.push(term.len());

    // Remainder goes to the leading chunks: the shortest chunk is the weakest
    // filter, so keep it as long as possible.
    let (base, extra) = (total / chunks, total % chunks);
    let mut out = Vec::with_capacity(chunks);
    let mut start = 0usize;
    for i in 0..chunks {
        let end = start + base + usize::from(i < extra);
        out.push(&term[bounds[start]..bounds[end]]);
        start = end;
    }
    Some(out)
}

/// The cascade's edit-distance budget for a folded term: one edit per three
/// characters, capped by `[search].fuzzy_max_edits`. Terms outside 3..=64
/// bytes skip the fuzzy stages entirely; a cap of 0 disables them.
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
            .best_distance_and_first(hay.as_bytes())
            .map(|(d, _)| d)
    }

    fn marked<'h>(pattern: &str, hay: &'h str, k: usize) -> &'h str {
        let (_, first) = Bitap::new(pattern.as_bytes(), k)
            .unwrap()
            .count_and_first(hay.as_bytes());
        let (s, e) = first.expect("the pattern occurs");
        &hay[s..e]
    }

    fn marked_best<'h>(pattern: &str, hay: &'h str, k: usize) -> &'h str {
        let (_, (s, e)) = Bitap::new(pattern.as_bytes(), k)
            .unwrap()
            .best_distance_and_first(hay.as_bytes())
            .expect("the pattern occurs");
        &hay[s..e]
    }

    #[test]
    fn case_is_free_without_folding_either_side() {
        assert_eq!(best("HELLO", "say hello world", 2), Some(0));
        assert_eq!(best("hello", "say HELLO world", 2), Some(0));
        assert_eq!(best("HeLLo", "say hEllO world", 0), Some(0));
        // One substitution on top of four case flips still fits k=1.
        assert_eq!(best("hello", "say HeXLO world", 1), Some(1));
        assert_eq!(marked("QUARTZITE", "the quartzite slab", 2), "quartzite");
        assert_eq!(
            marked_best("quartzite", "the QUARTZITE slab", 2),
            "QUARTZITE"
        );
    }

    #[test]
    fn non_ascii_bytes_are_not_folded_together() {
        // 'é' is C3 A9 and 'É' is C3 89: one substitution, not a free case flip.
        assert_eq!(best("café", "le café", 0), Some(0));
        assert_eq!(best("café", "le CAFÉ", 0), None, "É is not a fold of é");
        assert_eq!(best("café", "le CAFÉ", 1), Some(1), "and costs one edit");
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
        // `reset` shifts by k and writes k+1 registers.
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

        let b = Bitap::new(b"aaa", 0).unwrap();
        let (count, _) = b.count_and_first(b"aaaa");
        assert_eq!(count, 1);
    }

    #[test]
    fn count_fuzzy_and_range_is_the_occurrence_itself() {
        let b = Bitap::new(b"hello", 1).unwrap();
        let hay = b"say helo and hxllo again";
        let (count, first) = b.count_and_first(hay);
        assert_eq!(count, 2);
        assert_eq!(first, Some((4, 8)));
        assert_eq!(&hay[4..8], b"helo");
    }

    /// The reported bug, exactly: `repot` marked `1Repo` in `1Reporter` — a
    /// range assumed to be term-length reached one byte too far left.
    #[test]
    fn a_match_shorter_than_the_term_is_still_marked_exactly() {
        assert_eq!(marked("repot", "1reporter", 1), "repo");
        assert_eq!(marked_best("repot", "1reporter", 1), "repo");

        assert_eq!(marked("hello", "xx hxllo xx", 1), "hxllo");
        assert_eq!(marked_best("hello", "xx hxllo xx", 1), "hxllo");
    }

    #[test]
    fn an_insertion_marks_up_to_it_rather_than_over_it() {
        assert_eq!(marked("abc", "abxcd", 1), "ab");
        assert_eq!(marked_best("abc", "zzabxczz", 1), "ab");
    }

    /// The trap in resolving the start backwards: unbounded, the reverse pass
    /// spends a generous budget on deletions and accepts one letter of an
    /// exact match (`c` alone in `xabc` at k=2).
    #[test]
    fn a_generous_budget_does_not_shrink_an_exact_match() {
        assert_eq!(marked_best("abc", "xabc", 2), "abc");
        assert_eq!(marked_best("abcdef", "zzabcdefzz", 2), "abcdef");
        assert_eq!(marked("hello", "say hello world", 2), "hello");
    }

    /// The earliest accepting end is short (trailing deletions), and marking
    /// it highlighted `abcd` for a search for `abcdef` with the whole word
    /// right there.
    #[test]
    fn the_mark_is_not_truncated_to_a_leading_part_of_the_term() {
        assert_eq!(marked("abcdef", "zzabcdefzz", 2), "abcdef");
        assert_eq!(marked("abc", "xabc", 1), "abc");
        assert_eq!(marked("reports", "the report went out", 2), "report");

        assert_eq!(marked("repot", "1reporter", 1), "repo");
        assert_eq!(marked("hello", "say hello world", 0), "hello");
    }

    #[test]
    fn the_marked_span_is_within_the_budget_of_the_terms_length() {
        for (term, hay) in [
            ("repot", "1reporter"),
            ("abcdef", "zzabcdefzz"),
            ("quarterly", "the quartrly budget"),
            ("hello", "say helo and hxllo again"),
            ("reports", "the report went out"),
        ] {
            let k = edit_budget(term.len(), 2).expect("a real budget");
            let span = marked(term, hay, k).len();
            assert!(
                span <= term.len() && term.len() - span <= k,
                "{term:?} in {hay:?} (k={k}) marked {span} bytes"
            );
        }
    }

    #[test]
    fn a_zero_budget_marks_exactly_the_term() {
        assert_eq!(marked("hello", "say hello world", 0), "hello");
        assert_eq!(marked("ab", "ab ab", 0), "ab");
    }

    /// A match at the very start, and one whose end is inside the term's own
    /// length, are where the offset arithmetic can underflow.
    #[test]
    fn a_match_at_the_start_of_the_haystack_stays_in_bounds() {
        assert_eq!(marked("repot", "reporter", 1), "repo");
        // Haystack shorter than the term: one deletion, ending at 2.
        let (_, first) = Bitap::new(b"abc", 1).unwrap().count_and_first(b"ab");
        assert_eq!(first, Some((0, 2)));
    }

    /// A byte range can land inside a multi-byte character;
    /// `snippet::aligned_range` widens it before anything slices. This only
    /// pins that the range stays inside the haystack.
    #[test]
    fn a_range_over_multibyte_text_stays_within_the_haystack() {
        let hay = "café notes — le rapport";
        let (_, first) = Bitap::new(b"raport", 1)
            .unwrap()
            .count_and_first(hay.as_bytes());
        let (s, e) = first.expect("one deletion from 'rapport'");
        assert!(s < e && e <= hay.len(), "({s}, {e}) outside {}", hay.len());
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

    #[test]
    fn edit_budget_stays_within_the_bitap_word_size() {
        for len in 3..=64 {
            let k = edit_budget(len, usize::MAX).unwrap();
            assert!(k <= 21, "len={} gave k={}", len, k);
            assert!(Bitap::new(&vec![b'a'; len], k).is_some());
        }
    }

    /// Brute-force oracle: minimum Levenshtein distance between `pattern` and
    /// any substring of `hay`, capped at k, ignoring ASCII case to match the
    /// automaton.
    fn oracle(pattern: &[u8], hay: &[u8], k: usize) -> Option<usize> {
        // Without this, k >= pattern-length "matches" empty text by deleting
        // every pattern byte — a non-occurrence the automaton never reports.
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
                let cost = if pattern[i - 1].eq_ignore_ascii_case(&hay[j - 1]) {
                    0
                } else {
                    1
                };
                cur[j] = (prev[j - 1] + cost).min(prev[j] + 1).min(cur[j - 1] + 1);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        best = prev.iter().copied().min().unwrap_or(best);
        if best <= k {
            Some(best)
        } else {
            None
        }
    }

    #[test]
    fn matches_brute_force_oracle() {
        let mut seed: u64 = 0x2545F4914F6CDD1D;
        let mut rng = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        // Mixed case on both sides: a one-case alphabet could never exercise
        // the mask-table fold.
        let alphabet = b"abcxABCX";
        for _ in 0..500 {
            let plen = 3 + rng() % 6;
            let hlen = rng() % 20;
            let pattern: Vec<u8> = (0..plen).map(|_| alphabet[rng() % 4]).collect();
            let hay: Vec<u8> = (0..hlen).map(|_| alphabet[rng() % 4]).collect();
            for k in 0..=4 {
                let got = Bitap::new(&pattern, k)
                    .unwrap()
                    .best_distance_and_first(&hay)
                    .map(|(d, _)| d);
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
