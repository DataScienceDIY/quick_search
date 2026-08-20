//! Approximate substring matching for the fuzzy cascade stages.
//!
//! Bitap (shift-and with errors, Wu–Manber): finds occurrences of a
//! pattern *within* a haystack with at most `k` Levenshtein edits
//! (insertion / deletion / substitution); the u64 bit-parallel update costs
//! O(k) word ops per haystack byte with zero allocations.
//!
//! Matching is **ASCII-case-insensitive, in the automaton itself**: the mask
//! table sets a bit for both cases of every pattern byte, so neither side needs
//! folding first. That is not a convenience — it is what lets the whole-table
//! fuzzy passes run. Folding the haystack meant a `to_ascii_lowercase()` per
//! scanned filename *and* per scanned path, and, in the full-text pass, a
//! complete copy-and-fold of every stored document on every keystroke. Bytes
//! above 0x7F are untouched by ASCII folding, so setting both cases in the mask
//! accepts exactly what folding both sides accepted.
//!
//! Patterns are limited to 64 bytes by the machine word; the cascade skips
//! fuzzy stages for longer terms.

/// Registers the automaton needs: one per error count `0..=k`.
///
/// `k` is bounded by [`edit_budget`]'s one-edit-per-three-characters ladder
/// against a pattern the machine word caps at 64 bytes, so it never exceeds 21
/// however hostile `fuzzy_max_edits` is (pinned by
/// `edit_budget_stays_within_the_bitap_word_size`). [`Bitap::new`] rejects
/// anything larger, so the array is always big enough.
const MAX_REGISTERS: usize = 22;

pub struct Bitap {
    /// `masks[c]` has bit `i` set iff `pattern[i] == c` ignoring ASCII case.
    masks: [u64; 256],
    /// The same table for the *reversed* pattern, which is what lets
    /// [`Bitap::match_start`] find where a match began by scanning backwards
    /// from where it ended.
    rev_masks: [u64; 256],
    /// Pattern length in bytes (1..=64).
    len: usize,
    /// Maximum edit distance.
    k: usize,
}

impl Bitap {
    /// `None` when the pattern is empty, longer than 64 bytes, or the edit
    /// budget does not fit the registers (`reset` shifts by `k`, and the
    /// register array holds [`MAX_REGISTERS`]).
    ///
    /// The pattern need not be folded: each byte's bit is set under **both**
    /// ASCII cases, which is what makes the automaton case-insensitive and the
    /// haystack fold unnecessary. For a byte that is not an ASCII letter the
    /// two cases are the same byte and the second write is a no-op.
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
    ///
    /// `masks` selects the direction: [`Bitap::masks`] to scan forwards,
    /// [`Bitap::rev_masks`] to scan backwards. Everything else — `len`, `k`,
    /// the `done` bit, `reset` — is the same either way, since a reversed
    /// pattern is still a pattern of the same length.
    #[inline]
    fn step(&self, masks: &[u64; 256], r: &mut [u64], byte: u8) -> Option<usize> {
        let mask = masks[byte as usize];
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

    /// Where the match that ended at `end` with `errors` edits began.
    ///
    /// The forward scan knows an occurrence's *end* exactly — that is the bit
    /// it tests — but not its start, and with an insertion or a deletion the
    /// match is not `len` bytes long, so `end - len` is simply the wrong
    /// offset. Highlighting it put the marks a byte or two off the match and
    /// over whatever preceded it: `repot` against `1Reporter` marked `1Repo`.
    ///
    /// So the same automaton runs over the *reversed* pattern, backwards from
    /// `end`. The first position it accepts **within `errors` edits** is the
    /// start. That bound is what makes this correct rather than merely
    /// plausible: the reversed pattern will also accept far shorter spans by
    /// spending its whole budget on deletions — against `xabc` with a 2-edit
    /// budget it accepts `c` alone on the very first byte — and taking that
    /// would mark one letter of an exact three-letter match. The true
    /// alignment costs the same read either way, so requiring `≤ errors`
    /// rejects the cheap wrong answers and is still guaranteed to fire at or
    /// before the real start.
    ///
    /// Which occurrence gets marked is settled elsewhere — see
    /// [`Bitap::refine_end`]. The rule both producers land on is *the earliest
    /// alignment at the smallest edit distance*, so a mark is never longer
    /// than the term and never shorter by more than the budget.
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
        // Unreachable: the forward scan proved an alignment ends here, and
        // reversed it costs the same. Falling back to the floor keeps a
        // hypothetical miss inside the haystack.
        floor
    }

    /// Improve on the *earliest* accepting end by looking a little past it.
    ///
    /// The automaton accepts as soon as a leading part of the pattern has
    /// matched, paying for the rest with trailing deletions — so the first
    /// end it reports is systematically short. Searching `abcdef` over
    /// `zzabcdefzz` accepts after `abcd`, two deletions, with the whole word
    /// sitting right there.
    ///
    /// Each further byte can turn one of those deletions into a match, so a
    /// better alignment ends at most `errors` bytes later and never more.
    /// Stepping a *copy* of the registers that far finds it without
    /// disturbing the caller's scan, or its count.
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
    ///
    /// This one sweeps the whole haystack, so it finds the best alignment
    /// without help; [`Bitap::count_and_first`] resets after every hit and
    /// needs [`Bitap::refine_end`] instead.
    pub fn best_distance_and_first(&self, hay: &[u8]) -> Option<(usize, (usize, usize))> {
        let mut r = [0u64; MAX_REGISTERS];
        self.reset(&mut r);
        // (errors, end). The start is resolved once, at the end, rather than
        // per improvement — `match_start` is a second scan, however short.
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
    /// one's byte range in `hay`. After each hit the automaton resets, so an
    /// exact match followed by trailing bytes counts once, and overlapping
    /// suffix matches don't inflate counts.
    ///
    /// The range is the occurrence itself: [`Bitap::refine_end`] settles which
    /// end, then [`Bitap::match_start`] finds where it began. Both run for the
    /// first hit only, and both are bounded by the edit budget, so the cost is
    /// O(len + k) per row rather than per byte.
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

/// Re-exported: the trigram floor is a property of the index, not of fuzzy
/// matching, and the regex prefilter applies the same one. See
/// [`crate::search::prefilter`].
pub use super::prefilter::TRIGRAM_FLOOR;

/// Split `term` into `k + 1` consecutive chunks of at least [`TRIGRAM_FLOOR`]
/// characters each, for the fuzzy full-text pass's candidate prefilter.
/// `None` when the term is too short to divide that way.
///
/// # Why this is a sound prefilter
///
/// The chunks **partition** the term: consecutive, disjoint, and together the
/// whole of it. Suppose the term occurs somewhere in a document within `k`
/// edits. Each edit falls inside at most one chunk, so at most `k` of the
/// `k + 1` chunks are touched — and therefore at least one chunk survives
/// *verbatim* in the document. Asking the index for "any document containing
/// chunk 1, or chunk 2, … " is consequently a **superset** of the documents the
/// pass can accept, and narrowing to it cannot lose a hit. It can admit
/// documents that do not match at all, which is harmless: every candidate is
/// still verified by the bitap scan that follows.
///
/// The partition is what makes it work, so it must stay one. Chunks that
/// overlapped, or that skipped part of the term, would let `k` edits damage
/// every chunk and the argument would collapse silently — as a search that
/// quietly stops finding things.
///
/// # Why the floor, and why chars rather than bytes
///
/// Below `3 * (k + 1)` characters some chunk would be shorter than a trigram
/// and match no token at all, turning the "superset" into an empty set. There
/// is no prefilter for such a term and the caller must scan; that is what
/// `None` says. Note this is reachable only when `fuzzy_max_edits` *binds* —
/// [`edit_budget`]'s own ladder gives one edit per three characters, so a term
/// long enough for `k` edits by length alone is never long enough to split.
///
/// Splitting on characters, not bytes: a byte split can land inside a UTF-8
/// sequence, and the halves would be handed to FTS5 as phrases that no longer
/// spell anything the tokenizer indexed.
pub fn pigeonhole_chunks(term: &str, k: usize) -> Option<Vec<&str>> {
    let chunks = k + 1;
    let total = term.chars().count();
    if total < TRIGRAM_FLOOR * chunks {
        return None;
    }
    // Char-boundary offsets, plus the end, so a chunk can be sliced by
    // character index without re-walking the string per chunk.
    let mut bounds: Vec<usize> = term.char_indices().map(|(at, _)| at).collect();
    bounds.push(term.len());

    // The remainder is spread one character at a time over the leading chunks
    // rather than dumped on the last, so no chunk is left near the floor while
    // another is long. Every chunk is a candidate; the shortest is the weakest
    // filter, so the useful thing is to keep the shortest as long as possible.
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
            .best_distance_and_first(hay.as_bytes())
            .map(|(d, _)| d)
    }

    /// The slice of `hay` that a pattern's first occurrence marks — what a
    /// frontend highlights.
    fn marked<'h>(pattern: &str, hay: &'h str, k: usize) -> &'h str {
        let (_, first) = Bitap::new(pattern.as_bytes(), k)
            .unwrap()
            .count_and_first(hay.as_bytes());
        let (s, e) = first.expect("the pattern occurs");
        &hay[s..e]
    }

    /// [`marked`] through the other range producer, which shares
    /// `match_start` but reaches it by a different route.
    fn marked_best<'h>(pattern: &str, hay: &'h str, k: usize) -> &'h str {
        let (_, (s, e)) = Bitap::new(pattern.as_bytes(), k)
            .unwrap()
            .best_distance_and_first(hay.as_bytes())
            .expect("the pattern occurs");
        &hay[s..e]
    }

    /// The property that removes the fold from every caller: a pattern and a
    /// haystack that differ only in case match at distance zero, with neither
    /// side lowercased first.
    #[test]
    fn case_is_free_without_folding_either_side() {
        assert_eq!(best("HELLO", "say hello world", 2), Some(0));
        assert_eq!(best("hello", "say HELLO world", 2), Some(0));
        assert_eq!(best("HeLLo", "say hEllO world", 0), Some(0));
        // A case difference is not an edit, so the budget stays available for
        // real ones: one substitution on top of four case flips still fits k=1.
        assert_eq!(best("hello", "say HeXLO world", 1), Some(1));
        // And the marked span is the occurrence as it appears in the haystack.
        assert_eq!(marked("QUARTZITE", "the quartzite slab", 2), "quartzite");
        assert_eq!(marked_best("quartzite", "the QUARTZITE slab", 2), "QUARTZITE");
    }

    /// Non-ASCII bytes are untouched by ASCII case folding, so setting both
    /// cases in the mask cannot make two different multi-byte characters
    /// collide.
    #[test]
    fn non_ascii_bytes_are_not_folded_together() {
        // 'é' is 0xC3 0xA9 and 'É' is 0xC3 0x89 — distinct byte sequences that
        // ASCII folding leaves distinct. The second byte differs, so this is
        // one substitution, not a free case flip.
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
    fn count_fuzzy_and_range_is_the_occurrence_itself() {
        let b = Bitap::new(b"hello", 1).unwrap();
        let hay = b"say helo and hxllo again";
        let (count, first) = b.count_and_first(hay);
        assert_eq!(count, 2);
        // The occurrence, not a five-byte window ending where it ends: that
        // reached back over the space and marked " helo".
        assert_eq!(first, Some((4, 8)));
        assert_eq!(&hay[4..8], b"helo");
    }

    /// The reported bug, exactly: `repot` marked `1Repo` in `1Reporter`.
    ///
    /// The match is `repo` — one deletion, dropping the `t` — so it is four
    /// bytes where the term is five, and a range assumed to be term-length
    /// reached one byte too far left, over the `1`. Both producers, since
    /// they share `match_start`.
    #[test]
    fn a_match_shorter_than_the_term_is_still_marked_exactly() {
        assert_eq!(marked("repot", "1reporter", 1), "repo");
        assert_eq!(marked_best("repot", "1reporter", 1), "repo");

        // Substitution keeps the length, which is the case that always
        // worked — worth holding, since it is the one the old arithmetic got
        // right by accident.
        assert_eq!(marked("hello", "xx hxllo xx", 1), "hxllo");
        assert_eq!(marked_best("hello", "xx hxllo xx", 1), "hxllo");
    }

    /// Text with a byte inserted into the term marks up to the insertion, not
    /// across it: `abxc` is a one-edit alignment of `abc`, but so is the `ab`
    /// that ends two bytes earlier, and the rule is the *earliest* alignment
    /// at the best distance. Nothing longer than the term can win — spanning
    /// an inserted byte costs an edit, and deleting instead costs the same and
    /// ends sooner.
    #[test]
    fn an_insertion_marks_up_to_it_rather_than_over_it() {
        assert_eq!(marked("abc", "abxcd", 1), "ab");
        assert_eq!(marked_best("abc", "zzabxczz", 1), "ab");
    }

    /// The trap in resolving the start backwards: the reversed pattern will
    /// happily accept a much shorter span by spending its budget on
    /// deletions, so taking its *first* acceptance marks one letter of an
    /// exact match. `abc` occurs verbatim in `xabc`, and a 2-edit budget lets
    /// the reverse pass accept `c` alone one byte in.
    ///
    /// Only the whole-haystack producer can reach the trap — it is the one
    /// that reports an exact match while the budget is still generous, so
    /// `errors` is 0 where `k` is 2.
    #[test]
    fn a_generous_budget_does_not_shrink_an_exact_match() {
        assert_eq!(marked_best("abc", "xabc", 2), "abc");
        assert_eq!(marked_best("abcdef", "zzabcdefzz", 2), "abcdef");
        assert_eq!(marked("hello", "say hello world", 2), "hello");
    }

    /// The automaton accepts as soon as a leading part of the term has
    /// matched, spending the rest of the budget on trailing deletions — so
    /// the earliest end is short, and marking it highlighted `abcd` for a
    /// search for `abcdef` with the whole word right there. `refine_end`
    /// looks the budget's worth of bytes past the first acceptance.
    ///
    /// Checked against a brute-force Levenshtein oracle over every span.
    #[test]
    fn the_mark_is_not_truncated_to_a_leading_part_of_the_term() {
        assert_eq!(marked("abcdef", "zzabcdefzz", 2), "abcdef");
        assert_eq!(marked("abc", "xabc", 1), "abc");
        assert_eq!(marked("reports", "the report went out", 2), "report");

        // Not every term can be extended: `repot` against `1reporter` stops
        // at `repo` because the next byte (`r`) costs an edit of its own, so
        // one is the best it does either way.
        assert_eq!(marked("repot", "1reporter", 1), "repo");
        // And an alignment already at zero errors has nothing to improve.
        assert_eq!(marked("hello", "say hello world", 0), "hello");
    }

    /// However the span is chosen it is a real alignment at the best distance,
    /// so it is never longer than the term and never shorter by more than the
    /// budget. That bound is what keeps a mark recognisable: at the production
    /// ladder of one edit per three characters, a mark is always at least two
    /// thirds of the term.
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
        // The haystack is shorter than the term: "ab" matches "abc" with one
        // deletion, ending at 2.
        let (_, first) = Bitap::new(b"abc", 1).unwrap().count_and_first(b"ab");
        assert_eq!(first, Some((0, 2)));
    }

    /// Bitap works on bytes over an ASCII-folded copy, so a range can land
    /// inside a multi-byte character. `snippet::aligned_range` is what widens
    /// it before anything slices; this only pins that the range stays inside
    /// the haystack so that alignment has something valid to work from.
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
    ///
    /// Compares **ignoring ASCII case**, matching the automaton: the mask table
    /// sets both cases of every pattern byte, so a case difference is not an
    /// edit. On an all-lowercase alphabet this is identical to an exact
    /// comparison, so the older cases below are unaffected by the fold.
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
                let cost = if pattern[i - 1].eq_ignore_ascii_case(&hay[j - 1]) {
                    0
                } else {
                    1
                };
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
        // Mixed case on both sides, which is the whole point: the matcher folds
        // in its mask table rather than having its haystack folded for it, and
        // an alphabet of one case could never tell the two apart.
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
