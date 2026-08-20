pub trait StrUtils {
    fn split_control_word(&self) -> (&str, &str);

    fn is_only_whitespace(&self) -> bool;
}

impl StrUtils for str {
    /// LOCAL PATCH (QuickSearch): split a control word from what follows it,
    /// by the rule the RTF specification actually gives.
    ///
    /// A control word is `\`, then ASCII letters, then an optional numeric
    /// parameter (an optional `-` and digits). It ends at the first character
    /// that is not part of that. If that character is a space, the space is the
    /// delimiter and is consumed; if it is anything else, it is *not* consumed
    /// and begins the next token.
    ///
    /// This replaces `split_first_whitespace`, which ended the word at
    /// whitespace and nowhere else, so `\u233?after` came back as the single
    /// ident `\u233?after` — an unrecognised control word, taking the escaped
    /// character and the rest of the word with it. A `\uN` escape is followed
    /// by an ANSI fallback character that the spec lets be anything, and a
    /// literal `?` is the common choice.
    ///
    /// The function it replaces is deleted rather than kept: this was its only
    /// caller, and a vendored copy has no other consumer to keep it for.
    fn split_control_word(&self) -> (&str, &str) {
        // Byte indices are safe here without a char boundary check: every
        // character this scans past is ASCII, and it stops at the first that
        // is not.
        let bytes = self.as_bytes();
        // `\` itself, which the caller has already matched.
        let mut end = 1;
        while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
            end += 1;
        }
        // The numeric parameter, if there is one. A lone `-` with no digits
        // after it is not a parameter, so it is left to the next token.
        let digits_start = end + usize::from(end < bytes.len() && bytes[end] == b'-');
        let mut digits_end = digits_start;
        while digits_end < bytes.len() && bytes[digits_end].is_ascii_digit() {
            digits_end += 1;
        }
        if digits_end > digits_start {
            end = digits_end;
        }
        // A single space after the word is the delimiter and belongs to it.
        // Any other terminator, and any *further* space, is the next token's.
        let tail = end + usize::from(end < bytes.len() && bytes[end] == b' ');
        return (&self[..end], &self[tail..]);
    }

    fn is_only_whitespace(&self) -> bool {
        self.chars().all(|c| c.is_ascii_whitespace())
    }
}

// Macros
// Specify the path to the test files
#[macro_export]
macro_rules! include_test_file {
    ($filename:expr) => {
        include_str!(concat!("../resources/tests/", $filename))
    };
}

// Recursive call to the tokenize method of the lexer
#[macro_export]
macro_rules! recursive_tokenize {
    ($tail:expr) => {
        Lexer::tokenize($tail)
    };
    ($tail:expr, $ret:expr) => {
        if $tail.len() > 0 {
            if let Ok(tail_tokens) = Lexer::tokenize($tail) {
                // Push all the tokens in the result vector
                for token in tail_tokens {
                    $ret.push(token);
                }
            }
        }
    };
}

#[macro_export]
macro_rules! recursive_tokenize_with_init {
    ($init:expr, $tail:expr) => {{
        let mut ret = vec![$init];
        recursive_tokenize!($tail, ret);
        return Ok(ret);
    }};
}

#[cfg(test)]
mod test {
    use super::*;

    // LOCAL PATCH (QuickSearch): the two `split_first_whitespace` tests went
    // with the function they covered. `split_control_word` is exercised from
    // QuickSearch's own suite instead — `src/extract/rtf.rs` and
    // `tests/extraction_corpus.rs` — because this crate is excluded from the
    // workspace and `cargo test` never reaches these.
    #[test]
    fn test_split_control_word() {
        // A space delimiter belongs to the word; anything else does not.
        assert_eq!(r"\b I'm bold".split_control_word(), (r"\b", r"I'm bold"));
        assert_eq!(r"\u233?after".split_control_word(), (r"\u233", "?after"));
        assert_eq!(r"\u-233\'3f".split_control_word(), (r"\u-233", r"\'3f"));
        // A second space is text, not a second delimiter.
        assert_eq!(r"\b  bold".split_control_word(), (r"\b", " bold"));
        // A lone `-` is not a numeric parameter.
        assert_eq!(r"\b-x".split_control_word(), (r"\b", "-x"));
        // Nothing after the word at all.
        assert_eq!(r"\par".split_control_word(), (r"\par", ""));
    }
}
