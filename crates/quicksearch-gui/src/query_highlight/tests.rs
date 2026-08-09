use super::*;

/// Readable projection: (slice, class, chip) per segment.
fn segs(text: &str) -> Vec<(String, Class, bool)> {
    assert_tiles(text);
    classify(text)
        .into_iter()
        .map(|s| (text[s.range.clone()].to_string(), s.class, s.chip))
        .collect()
}

/// Segments must tile 0..len exactly: contiguous, ascending, complete.
fn assert_tiles(text: &str) {
    let segs = classify(text);
    let mut cursor = 0usize;
    for s in &segs {
        assert_eq!(
            s.range.start, cursor,
            "gap or overlap in {:?}: {:?}",
            text, segs
        );
        assert!(s.range.end > s.range.start, "empty seg in {:?}", text);
        cursor = s.range.end;
    }
    assert_eq!(cursor, text.len(), "segments must cover {:?}", text);
}

use Class::*;

fn owned(v: Vec<(&str, Class, bool)>) -> Vec<(String, Class, bool)> {
    v.into_iter()
        .map(|(s, c, b)| (s.to_string(), c, b))
        .collect()
}

#[test]
fn empty_input_yields_no_segments() {
    assert!(classify("").is_empty());
}

#[test]
fn plain_words_stay_plain() {
    assert_eq!(
        segs("budget report"),
        owned(vec![
            ("budget", Plain, false),
            (" ", Plain, false),
            ("report", Plain, false),
        ])
    );
}

#[test]
fn every_recognized_filter_chips() {
    for input in [
        "type:Audio",
        "modified:>=2024-01-01",
        "mtime:<2023-12-01",
        "path:/home/me",
        "folder:/x",
        "includefolder:/x",
        "name:report",
        "filename:report",
        "mime:application/pdf",
        "regex:foo",
    ] {
        let all = segs(input);
        assert!(
            all.iter().all(|(_, _, chip)| *chip),
            "{:?}: whole filter must chip: {:?}",
            input,
            all
        );
        assert_eq!(all[0].1, Keyword, "{:?}", input);
        assert_eq!(all[1].1, Operator, "{:?}", input);
        assert!(
            all[2..]
                .iter()
                .all(|(_, c, _)| *c == Argument || *c == Operator),
            "{:?}: {:?}",
            input,
            all
        );
    }
}

#[test]
fn keys_are_case_insensitive() {
    assert_eq!(segs("TYPE:Audio")[0], ("TYPE".to_string(), Keyword, true));
    assert_eq!(
        segs("Modified:>=2024-01-01")[0],
        ("Modified".to_string(), Keyword, true)
    );
}

#[test]
fn colon_comparator_runs_are_one_green_stretch() {
    assert_eq!(
        segs("modified:>=2024-01-01"),
        owned(vec![
            ("modified", Keyword, true),
            (":", Operator, true),
            (">=", Operator, true),
            ("2024-01-01", Argument, true),
        ])
    );
}

#[test]
fn unrecognized_keys_stay_plain() {
    for input in ["foo:bar", "12:30", "foo:bar:baz"] {
        assert!(
            segs(input).iter().all(|(_, c, chip)| *c == Plain && !chip),
            "{:?}: {:?}",
            input,
            segs(input)
        );
    }
    // Stars in glue are literal to the engine — no green.
    assert!(segs("foo:ba*r").iter().all(|(_, c, _)| *c == Plain));
}

#[test]
fn drive_letters_do_not_split() {
    assert_eq!(
        segs(r"path:C:\Users\me"),
        owned(vec![
            ("path", Keyword, true),
            (":", Operator, true),
            (r"C:\Users\me", Argument, true),
        ])
    );
    assert_eq!(segs(r"C:\data"), owned(vec![(r"C:\data", Plain, false)]));
}

#[test]
fn quoted_phrases_get_green_delimiters() {
    assert_eq!(
        segs("\"exact phrase\""),
        owned(vec![
            ("\"", Operator, false),
            ("exact phrase", Plain, false),
            ("\"", Operator, false),
        ])
    );
    // Inner "" escapes are content bytes.
    assert_eq!(
        segs("\"a\"\"b\""),
        owned(vec![
            ("\"", Operator, false),
            ("a\"\"b", Plain, false),
            ("\"", Operator, false),
        ])
    );
    // Quoted stars are literal — content stays plain.
    assert!(segs("\"a*b\"")
        .iter()
        .all(|(s, c, _)| s == "\"" || *c == Plain));
}

#[test]
fn quoted_filter_values_are_blue_with_green_quotes() {
    assert_eq!(
        segs("path:\"/home/me/My Docs\""),
        owned(vec![
            ("path", Keyword, true),
            (":", Operator, true),
            ("\"", Operator, true),
            ("/home/me/My Docs", Argument, true),
            ("\"", Operator, true),
        ])
    );
    // Empty quoted value: two delimiters, no content seg, no panic.
    assert_eq!(
        segs("path:\"\""),
        owned(vec![
            ("path", Keyword, true),
            (":", Operator, true),
            ("\"", Operator, true),
            ("\"", Operator, true),
        ])
    );
}

#[test]
fn unterminated_quote_is_a_quote_in_progress() {
    assert_eq!(
        segs("\"unclosed phrase"),
        owned(vec![
            ("\"", Operator, false),
            ("unclosed phrase", Plain, false),
        ])
    );
    // Filters before the open quote keep their colors.
    let all = segs("type:Audio \"x");
    assert_eq!(all[0], ("type".to_string(), Keyword, true));
    assert_eq!(all[4], ("\"".to_string(), Operator, false));
    assert_eq!(all[5], ("x".to_string(), Plain, false));
}

#[test]
fn trailing_bare_keys_color_optimistically_without_chip() {
    assert_eq!(
        segs("type:"),
        owned(vec![("type", Keyword, false), (":", Operator, false)])
    );
    assert_eq!(
        segs("modified:>="),
        owned(vec![
            ("modified", Keyword, false),
            (":", Operator, false),
            (">=", Operator, false),
        ])
    );
    assert_eq!(
        segs("foo:"),
        owned(vec![("foo", Plain, false), (":", Plain, false)])
    );
}

#[test]
fn stars_in_words_and_name_values_go_green() {
    assert_eq!(
        segs("rep*ort"),
        owned(vec![
            ("rep", Plain, false),
            ("*", Operator, false),
            ("ort", Plain, false),
        ])
    );
    assert_eq!(
        segs("name:re*.txt"),
        owned(vec![
            ("name", Keyword, true),
            (":", Operator, true),
            ("re", Argument, true),
            ("*", Operator, true),
            (".txt", Argument, true),
        ])
    );
    // Edge and doubled stars keep tiling intact.
    assert_tiles("*foo");
    assert_tiles("foo*");
    assert_tiles("**");
    assert_tiles("*");
    // In non-glob filter values the star is a literal character.
    assert_eq!(
        segs("path:/da*ta")[2],
        ("/da*ta".to_string(), Argument, true)
    );
}

#[test]
fn invalid_arguments_go_error_uniformly() {
    // (`regex:(` is not here: `(` lexes as a paren, so that input is an
    // *incomplete* filter — bare-key optimism applies, not an error.)
    for input in [
        "type:NotAThing",
        "modified:>=tomorrow",
        "regex:[",
        "type:Doc*",
    ] {
        let all = segs(input);
        assert_eq!(all[0].1, Keyword, "{:?}", input);
        let last = all.last().unwrap();
        assert_eq!(last.1, InvalidArg, "{:?}: {:?}", input, all);
        assert!(last.2, "invalid values keep the chip: {:?}", input);
    }
    // name:= is an unsupported op → its value is invalid too.
    let all = segs("name=x");
    assert_eq!(all.last().unwrap().1, InvalidArg);
}

#[test]
fn valid_regex_argument_is_blue_with_green_stars() {
    assert_eq!(
        segs("regex:foo.*bar"),
        owned(vec![
            ("regex", Keyword, true),
            (":", Operator, true),
            ("foo.", Argument, true),
            ("*", Operator, true),
            ("bar", Argument, true),
        ])
    );
}

#[test]
fn a_second_regex_filter_is_invalid() {
    let all = segs("regex:foo regex:bar");
    assert_eq!(all[2], ("foo".to_string(), Argument, true));
    assert_eq!(all.last().unwrap(), &("bar".to_string(), InvalidArg, true));
}

#[test]
fn multi_filter_queries_chip_separately() {
    let all = segs("type:Document budget modified:>=2024-01-01");
    // The word and the whitespace around it stay un-chipped.
    assert_eq!(
        all.iter()
            .filter(|(_, _, chip)| !chip)
            .map(|(s, _, _)| s.as_str())
            .collect::<Vec<_>>(),
        vec![" ", "budget", " "]
    );
}

#[test]
fn spaced_filters_chip_their_interior_gaps() {
    // `type : Audio` is still a filter to the lexer/splitter.
    assert_eq!(
        segs("type : Audio"),
        owned(vec![
            ("type", Keyword, true),
            (" ", Plain, true),
            (":", Operator, true),
            (" ", Plain, true),
            ("Audio", Argument, true),
        ])
    );
}

#[test]
fn demoted_operators_stay_plain() {
    assert!(segs("(alpha AND beta) OR gamma")
        .iter()
        .all(|(_, c, chip)| *c == Plain && !chip));
    // Dangling comparators are literal text.
    assert!(segs("a > b").iter().all(|(_, c, _)| *c == Plain));
    // Leading operator, nothing else.
    assert!(segs(">foo").iter().all(|(_, c, _)| *c == Plain));
}

#[test]
fn adjacency_between_filter_and_quote() {
    // `Audio"q"`: the word ends at the quote; the filter is complete
    // and the quoted phrase stands alone.
    let all = segs("type:Audio\"q\"");
    assert_eq!(all[2], ("Audio".to_string(), Argument, true));
    assert_eq!(all[3], ("\"".to_string(), Operator, false));
}

#[test]
fn unicode_offsets_hold_up() {
    assert_tiles("\"José\" type:Audio naïve*file");
    let all = segs("naïve*café");
    assert_eq!(
        all,
        owned(vec![
            ("naïve", Plain, false),
            ("*", Operator, false),
            ("café", Plain, false),
        ])
    );
}
