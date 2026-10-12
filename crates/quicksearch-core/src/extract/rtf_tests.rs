use super::*;
use crate::extract::on_worker;

fn scratch() -> Scratch {
    Scratch::new(&crate::config::Config::default())
}

/// The default `maximum_text_file_size`, which bounds a read.
fn max_read() -> usize {
    scratch().limits().read
}

fn read_capped_to_vec(path: &Path, cap: usize) -> Result<Vec<u8>, ExtractError> {
    let mut buf = Vec::new();
    read_capped(path, cap, &mut buf).map(|()| buf)
}

fn extract(path: &Path) -> Result<String, ExtractError> {
    crate::extract::extract_text(&RtfExtractor, path)
}

fn tmp(tag: &str, body: &[u8]) -> std::path::PathBuf {
    let p = crate::testutil::scratch_dir(tag).join("sample.rtf");
    crate::testutil::touch(&p, body);
    p
}

/// `body` through both entry points, which must agree. The disk path runs
/// with no text limit, as the head path does, so long fixtures agree too.
fn both(tag: &str, body: &[u8]) -> Result<String, ExtractError> {
    let p = tmp(tag, body);
    let mut scratch = scratch();
    scratch.limits.text = usize::MAX;
    let mut disk = String::new();
    let disk = RtfExtractor
        .extract(&p, "application/rtf", &mut disk, &mut scratch)
        .map(|()| disk);
    let mut head = String::new();
    let head = RtfExtractor
        .extract_from_head(&p, body, &mut head)
        .expect("RTF takes the head path")
        .map(|()| head);
    std::fs::remove_file(&p).ok();
    assert_eq!(disk, head, "head and disk extraction disagree");
    disk
}

/// [`both`] on the indexer's worker stack, under a watchdog: hostile input
/// that hangs fails its test instead of wedging the run.
fn hostile(tag: &'static str, body: Vec<u8>) -> Result<String, ExtractError> {
    on_worker(move || both(tag, &body))
}

/// The text of `body`, which must parse, through [`hostile`].
fn text(body: &[u8]) -> String {
    hostile("rtf-text", body.to_vec()).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn extracts_text_without_control_words() {
    let body = br"{\rtf1\ansi Hello {\b World}!}";
    let p = tmp("basic", body);
    let c = extract(&p).unwrap();
    assert_eq!(c, "Hello World!");
    std::fs::remove_file(&p).ok();
}

#[test]
fn head_extraction_matches_reading_the_file() {
    // `\'e9` is the RTF hex escape for an e-acute: the literal stays 7-bit ASCII.
    let body = br"{\rtf1\ansi caf\'e9 at noon}";
    let p = tmp("agree", body);
    let from_disk = extract(&p).unwrap();
    let mut from_head = String::new();
    RtfExtractor
        .extract_from_head(&p, body, &mut from_head)
        .unwrap()
        .unwrap();
    assert_eq!(from_disk, from_head);
    assert!(from_disk.contains("café"), "{:?}", from_disk);
    std::fs::remove_file(&p).ok();
}

/// A `\\uN` escape naming a lone UTF-16 surrogate costs one character, not
/// the document: it decodes to one `U+FFFD`. Both entry points stay
/// exercised, through the registry, whose panic containment must keep
/// working for every other extractor.
#[test]
fn a_lone_surrogate_escape_costs_one_character() {
    // `\u55296` is a lone high surrogate; the `?` is its ANSI fallback,
    // delimited the way a real producer writes one.
    let body = "{\\rtf1\\ansi before \\u55296?after}".as_bytes();
    let p = tmp("surrogate", body);

    // The on-disk path, as the content pass reaches it.
    let outcome = crate::file_handling::decide_content(
        p.to_str().unwrap(),
        Some("application/rtf"),
        &crate::extract::Registry::default_set(),
        &crate::config::Config::default(),
        &mut scratch(),
    );
    let disk = match &outcome {
        crate::file_handling::ContentOutcome::Done { text } => text.clone(),
        other => panic!("a malformed escape must not fail the document: {other:?}"),
    };
    assert_eq!(disk, "before \u{FFFD}after");

    // The head path, through the registry.
    let head =
        crate::extract::Registry::default_set().extract_head_to_string(&p, "application/rtf", body);
    assert_eq!(
        head.expect("claimed").expect("parsed"),
        disk,
        "head and disk extraction must agree"
    );

    // A low half alone, and a high half whose partner never comes.
    assert_eq!(text(br"{\rtf1 a\u-9216?b}"), "a\u{FFFD}b");
    assert_eq!(
        text(br"{\rtf1 a\u-10179?b\u-8704?c}"),
        "a\u{FFFD}b\u{FFFD}c"
    );
    assert_eq!(text(br"{\rtf1 a\u-10179?}"), "a\u{FFFD}");

    std::fs::remove_file(&p).ok();
}

/// `\\par` ends a paragraph and must reach the text as a line break, as
/// `\\line` does.
#[test]
fn paragraph_breaks_reach_the_text() {
    let body = br"{\rtf1\ansi First paragraph.\par Second paragraph.\par}";
    let p = tmp("par", body);
    let text = extract(&p).unwrap();
    assert!(
        text.contains("First paragraph.\nSecond paragraph."),
        "paragraphs ran together: {text:?}"
    );
    std::fs::remove_file(&p).ok();
}

#[test]
fn malformed_input_errors_and_names_the_file() {
    let p = tmp("broken", b"not rtf");
    let err = extract(&p).unwrap_err();
    assert!(
        err.contains(&p.display().to_string()),
        "must name the file: {}",
        err
    );
    std::fs::remove_file(&p).ok();
}

#[test]
fn supports_rtf_mimes_only() {
    let e = RtfExtractor;
    assert!(e.supports("application/rtf"));
    assert!(e.supports("text/rtf"));
    assert!(!e.supports("text/plain"));
    assert!(!e.supports("application/pdf"));
}

#[test]
fn a_file_over_the_cap_is_refused() {
    let body = vec![b'x'; 4096];
    let p = tmp("cap", &body);
    let err = read_capped_to_vec(&p, 100).unwrap_err();
    assert!(err.contains("over the 100-byte read limit"), "{err}");
    assert_eq!(
        read_capped_to_vec(&p, max_read()).unwrap().len(),
        4096,
        "a file under the cap must be read whole"
    );
    std::fs::remove_file(&p).ok();
}

/// procfs reports a size of zero for files with content, so the open passes
/// the size check and only the capped read stops it.
#[cfg(target_os = "linux")]
#[test]
fn a_read_whose_fstat_says_zero_stops_at_the_cap() {
    let maps = Path::new("/proc/self/maps");
    assert_eq!(std::fs::metadata(maps).unwrap().len(), 0);
    assert_eq!(read_capped_to_vec(maps, 100).unwrap().len(), 100);
}

#[test]
fn a_missing_file_is_an_error_naming_it() {
    let p = crate::testutil::scratch_dir("rtf-missing").join("nope.rtf");
    let err = read_capped_to_vec(&p, max_read()).unwrap_err();
    assert!(err.contains(&p.display().to_string()), "{err}");
}

// -- Codepages ---------------------------------------------------------

#[test]
fn cp1252_escapes_decode_as_cp1252() {
    // Latin-1 would make 0x80-0x9F C1 controls.
    assert_eq!(
        text(br"{\rtf1\ansi\ansicpg1252 don\'92t \'93q\'94 \'80 \'85}"),
        "don\u{2019}t \u{201C}q\u{201D} \u{20AC} \u{2026}"
    );
}

#[test]
fn ansicpg_names_the_document_codepage() {
    assert_eq!(
        text(br"{\rtf1\ansi\ansicpg1251 \'cf\'f0\'e8\'e2\'e5\'f2}"),
        "Привет"
    );
    // Mac Roman 0x8E is e-acute.
    assert_eq!(text(br"{\rtf1\mac caf\'8e}"), "café");
    // A codepage with no ASCII-compatible encoding reads as windows-1252.
    for cpg in ["99999", "1200", "1201", "50220"] {
        let body = format!(r"{{\rtf1\ansi\ansicpg{cpg} \'93q\'94}}");
        assert_eq!(text(body.as_bytes()), "\u{201C}q\u{201D}", "{cpg}");
    }
}

#[test]
fn a_font_charset_switches_the_codepage_and_back() {
    let table = r"{\fonttbl{\f0\fnil\fcharset0 Arial;}{\f1\fnil\fcharset204 Arial Cyr;}}";
    let body = format!(r"{{\rtf1\ansi\deff0{table}\f0 caf\'e9 {{\f1 \'cf\'f0\'e8}} \'e9}}");
    assert_eq!(text(body.as_bytes()), "café При é");
    // `\plain` returns to the `\deff` font.
    let body = format!(r"{{\rtf1\ansi\deff1{table}\f0 \'e9\plain \'e9}}");
    assert_eq!(text(body.as_bytes()), "éй");
}

#[test]
fn text_keeps_its_order_across_a_codepage_flush() {
    let table = r"{\fonttbl{\f0\fcharset0 A;}{\f1\fcharset204 B;}}";
    let body = format!(r"{{\rtf1\ansi{table}\f0 a\'e9\f1 b\'e9\u1078\'e6c}}");
    assert_eq!(text(body.as_bytes()), "aébйжc");
}

#[test]
fn fcharset_0_and_1_mean_the_document_codepage() {
    for charset in [0, 1] {
        let body = format!(
            r"{{\rtf1\ansi\ansicpg1251{{\fonttbl{{\f0\fcharset{charset} Arial;}}}}\f0 \'cf}}"
        );
        assert_eq!(text(body.as_bytes()), "П", "fcharset{charset}");
    }
}

#[test]
fn cpg_wins_over_fcharset() {
    let body = br"{\rtf1\ansi{\fonttbl{\f0\fcharset204\cpg1253 X;}}\f0 \'e1}";
    assert_eq!(text(body), "α");
}

#[test]
fn shift_jis_pairs_with_ascii_trail_bytes_stay_whole() {
    // テスト: each trail byte (`e`, `X`, `g`) is a literal ASCII letter.
    let unit = r"\'83e\'83X\'83g";
    let body = format!(r"{{\rtf1\ansi\ansicpg932 {unit}}}");
    assert_eq!(text(body.as_bytes()), "テスト");
    // 日本, whose second trail byte is `{`, escaped.
    assert_eq!(text(br"{\rtf1\ansi\ansicpg932 \'93\'fa\'96\'7b}"), "日本");
    // A long run is never cut between a lead and its trail.
    let body = format!(r"{{\rtf1\ansi\ansicpg932 {}}}", unit.repeat(5_000));
    let out = text(body.as_bytes());
    assert!(!out.contains('\u{FFFD}'), "a pair was split");
    assert_eq!(out.chars().count(), 15_000);
    // The same through a font charset.
    let body = br"{\rtf1\ansi{\fonttbl{\f0\fcharset128 MS Gothic;}}\f0 \'83e\'83X\'83g}";
    assert_eq!(text(body), "テスト");
}

#[test]
fn a_dangling_lead_byte_costs_one_replacement() {
    assert_eq!(text(br"{\rtf1\ansicpg932 {\'83}x}"), "\u{FFFD}x");
    let body = br"{\rtf1\ansicpg932{\fonttbl{\f1\fcharset0 A;}}\'83\f1 x}";
    assert_eq!(text(body), "\u{FFFD}x");
}

#[test]
fn an_escaped_byte_order_mark_is_text() {
    assert_eq!(
        text(br"{\rtf1\ansi\ansicpg1252 \'ef\'bb\'bf}"),
        "\u{EF}\u{BB}\u{BF}"
    );
}

#[test]
fn a_unicode_escape_skips_its_fallback() {
    // `\'cf` is `П` in cp1251, and the fallback here: one character, not two.
    assert_eq!(text(br"{\rtf1\ansi\ansicpg1251 \u1077\'cf}"), "е");
    // `\ucN` counts units; a brace ends the count.
    assert_eq!(text(br"{\rtf1\uc2 a\u233?!b}"), "aéb");
    assert_eq!(text(br"{\rtf1\uc2 a{\u233?}!b}"), "aé!b");
    assert_eq!(text(br"{\rtf1\uc0 a\u233?b}"), "aé?b");
    assert_eq!(text(br"{\rtf1 a\u233\'e9\par b}"), "aé\nb");
    // A control word is a unit too.
    assert_eq!(text(br"{\rtf1 a\u233\emdash b}"), "aéb");
    // `\uc` is the group's.
    assert_eq!(text(br"{\rtf1 {\uc0 \u233}\u233?}"), "éé");
    // A `\uN` is never another's fallback.
    assert_eq!(text(br"{\rtf1 \u1087\u1088?}"), "пр");
}

#[test]
fn a_surrogate_pair_is_one_character() {
    // U+1F600 as Word writes it, a fallback after each half.
    assert_eq!(text(br"{\rtf1\uc1 a\u-10179?\u-8704?b}"), "a\u{1F600}b");
    assert_eq!(
        text(br"{\rtf1\uc1 a\u-10179\'3f\u-8704\'3fb}"),
        "a\u{1F600}b"
    );
    assert_eq!(text(br"{\rtf1\uc0 \u55357\u56832}"), "\u{1F600}");
    // LibreOffice's: one fallback for the pair, with `\uc` changed between.
    assert_eq!(
        text(br"{\rtf1 a\uc0 \u-10179\uc1 \u-8704\'3fb}"),
        "a\u{1F600}b"
    );
}

// -- UTF-8 mode --------------------------------------------------------

#[test]
fn a_utf8_file_keeps_its_literal_utf8() {
    assert_eq!(text("{\\rtf1\\ansi café 日本}".as_bytes()), "café 日本");
    // `\'hh` stays a codepage byte in the same file.
    assert_eq!(text("{\\rtf1\\ansi café caf\\'e9}".as_bytes()), "café café");
}

/// A `\uN`'s fallback written as a literal character is one unit, however
/// many bytes it takes in UTF-8; skipping its lead byte alone left the rest
/// to decode as U+FFFD.
#[test]
fn a_utf8_fallback_character_is_skipped_whole() {
    assert_eq!(text("{\\rtf1 caf\\u233 é}".as_bytes()), "café");
    assert_eq!(text("{\\rtf1 \\u26085 日本}".as_bytes()), "日本");
    assert_eq!(
        text("{\\rtf1 a\\uc0 \\u-10179\\uc1 \\u-8704 😀b}".as_bytes()),
        "a\u{1F600}b"
    );
    assert_eq!(text("{\\rtf1\\uc2 \\u233 éé!}".as_bytes()), "é!");
    // An ASCII unit is still one byte.
    assert_eq!(text("{\\rtf1\\uc2 \\u233 é?!}".as_bytes()), "é!");
}

#[test]
fn one_stray_byte_decodes_the_literals_with_the_codepage() {
    let mut body = "{\\rtf1\\ansi café ".as_bytes().to_vec();
    body.extend_from_slice(b"\xe9}");
    assert_eq!(text(&body), "cafÃ© é");
}

#[test]
fn a_bin_cut_through_a_utf8_character_does_not_panic() {
    // Valid UTF-8 as a whole; `\bin1` takes the first byte of the `é`.
    let body = "{\\rtf1 a\\bin1 éb}".as_bytes().to_vec();
    assert_eq!(hostile("rtf-bin-utf8", body).unwrap(), "a\u{FFFD}b");
}

// -- Text words --------------------------------------------------------

#[test]
fn control_words_stand_for_their_characters() {
    let cases: [(&[u8], &str); 8] = [
        (br"{\rtf1\ansi hello\emdash world}", "hello\u{2014}world"),
        (br"{\rtf1\ansi 2020\endash 2025}", "2020\u{2013}2025"),
        (br"{\rtf1\ansi \bullet Item one}", "\u{2022}Item one"),
        (
            br"{\rtf1\ansi \ldblquote Hello\rdblquote  and \lquote hi\rquote}",
            "\u{201C}Hello\u{201D} and \u{2018}hi\u{2019}",
        ),
        (br"{\rtf1\ansi col1\tab col2\line next}", "col1\tcol2\nnext"),
        (
            br"{\rtf1 a\emspace b\enspace c\qmspace d\zwj e\zwnj f}",
            "a\u{2003}b\u{2002}c\u{2005}d\u{200D}e\u{200C}f",
        ),
        (br"{\rtf1 one\sect two\page three}", "one\ntwo\nthree"),
        // WordPad's apostrophe.
        (
            b"{\\rtf1\\ansi\\ansicpg1252\\deff0\\nouicompat{\\fonttbl{\\f0\\fnil\\fcharset0 Calibri;}}\r\n\
              {\\*\\generator Riched20 10.0.19041}\\viewkind4\\uc1 \r\n\
              \\pard\\sa200\\sl276\\slmult1\\f0\\fs22\\lang9 don\\rquote t\\par\r\n}\r\n\0",
            "don\u{2019}t\n",
        ),
    ];
    for (body, want) in cases {
        assert_eq!(text(body), want, "{}", String::from_utf8_lossy(body));
    }
    // Scrivener's output.
    let body = b"{\\rtf1\\ansi\\ansicpg1252\\deff0\n\
        {\\fonttbl{\\f0\\fnil\\fcharset0 TimesNewRomanPSMT;}}\n\
        \\f0\\fs24 The transformation in reverse\\emdash confident expert.\\par\n\
        He said, \\ldblquote Hello.\\rdblquote\\par}";
    assert_eq!(
        text(body),
        "The transformation in reverse\u{2014}confident expert.\n\
         He said, \u{201C}Hello.\u{201D}\n"
    );
}

#[test]
fn table_cells_and_rows_are_separated() {
    assert_eq!(text(br"{\rtf1 A\cell B\cell\row}"), "A\tB\t\n");
    assert_eq!(text(br"{\rtf1 A\nestcell B}"), "A\tB");
}

// -- Symbols -----------------------------------------------------------

#[test]
fn control_symbols_keep_the_text_after_them() {
    assert_eq!(
        text(br"{\rtf1\ansi Mr.\~Smith went home.}"),
        "Mr.\u{A0}Smith went home."
    );
    assert_eq!(text(br"{\rtf1 co\-op}"), "coop");
    assert_eq!(text(br"{\rtf1 non\_breaking}"), "non\u{2011}breaking");
    assert_eq!(text(br"{\rtf1 a\|b\:c}"), "abc");
    assert_eq!(text(br"{\rtf1 \#rest}"), "rest");
    assert_eq!(text(br"{\rtf1 \Foo bar}"), "bar");
    // `\'` without two hex digits is nothing, and what follows stays.
    assert_eq!(text(br"{\rtf1 a\'zzb}"), "azzb");
}

#[test]
fn an_escaped_line_break_is_a_line_break() {
    assert_eq!(text(b"{\\rtf1 a\\\r\nb}"), "a\nb");
    assert_eq!(text(b"{\\rtf1 a\\\nb}"), "a\nb");
}

// -- Destinations ------------------------------------------------------

#[test]
fn document_information_is_not_text() {
    assert_eq!(text(br"{\rtf1{\info{\title T}{\author A}}Body}"), "Body");
}

#[test]
fn pictures_are_not_text() {
    let body =
        br"{\rtf1 a{\*\shppict{\pict\pngblip 89504e47}}{\nonshppict{\pict\wmetafile8 2024face}}b}";
    assert_eq!(text(body), "ab");
}

#[test]
fn text_after_a_long_picture_survives() {
    let mut body = br"{\rtf1 {\pict\pngblip ".to_vec();
    for _ in 0..300 * 1024 / 32 {
        body.extend_from_slice(b"0123456789abcdef0123456789abcdef");
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"}needle}");
    assert_eq!(hostile("rtf-pict", body).unwrap(), "needle");
}

#[test]
fn bin_data_is_skipped_whole() {
    // The six bytes hold braces that would otherwise close groups.
    assert_eq!(
        hostile("rtf-bin", br"{\rtf1{\pict\bin6 {}}{{x}After}".to_vec()).unwrap(),
        "After"
    );
    // Outside a picture, too.
    assert_eq!(text(br"{\rtf1 a\bin3 }}}b}"), "ab");
}

#[test]
fn a_bin_length_past_the_end_is_clamped() {
    assert_eq!(
        hostile("rtf-bin-long", br"{\rtf1 a\bin999999999 xyz".to_vec()).unwrap(),
        "a"
    );
    assert_eq!(text(br"{\rtf1 a\bin-5 b}"), "ab");
}

#[test]
fn a_field_keeps_its_result_not_its_instruction() {
    let body = br#"{\rtf1 see {\field{\*\fldinst HYPERLINK "http://x.example/2024"}{\fldrslt link}} here}"#;
    assert_eq!(text(body), "see link here");
    let body = br#"{\rtf1 {\field{\fldinst PAGE}{\fldrslt 3}}}"#;
    assert_eq!(text(body), "3");
}

#[test]
fn headers_and_footers_are_text() {
    assert_eq!(text(br"{\rtf1{\header H}{\footer F}Body}"), "HFBody");
}

#[test]
fn tables_of_colours_and_styles_leave_nothing_behind() {
    let body =
        br"{\rtf1{\colortbl;\red0\green0\blue0;}{\stylesheet{\s0 Normal;}{\s1 Heading 1;}}x}";
    assert_eq!(text(body), "x");
    // Nor does the font table, or a list label.
    let body = br"{\rtf1{\fonttbl{\f0\fswiss Helvetica;}}{\listtext 1.\tab}{\pntext 2.}x}";
    assert_eq!(text(body), "x");
}

#[test]
fn the_scan_stops_at_the_text_limit() {
    let p = tmp("rtf-limit", b"");
    let run = |body: String| {
        crate::testutil::touch(&p, body.as_bytes());
        let mut scratch = scratch();
        scratch.limits.text = 16;
        let mut out = String::new();
        RtfExtractor
            .extract(&p, "application/rtf", &mut out, &mut scratch)
            .unwrap();
        out
    };
    let out = run(format!(r"{{\rtf1 {}}}", "abcdefgh".repeat(1_000)));
    assert_eq!(out, "abcdefgh".repeat(2));
    // Pending codepage bytes are decoded once more, which can grow them.
    let out = run(format!(r"{{\rtf1\ansicpg932 {}}}", r"\'83e".repeat(1_000)));
    assert!(out.len() <= 16 * 3, "{} bytes", out.len());
    assert!(out.starts_with("テテテ"), "{out:?}");
    std::fs::remove_file(&p).ok();
}

// -- Cost --------------------------------------------------------------

/// A million `\` line breaks: each is a control symbol, which a scan that
/// rescanned line runs would make quadratic.
fn escaped_breaks(pair: &[u8]) -> Vec<u8> {
    let mut body = br"{\rtf1 ".to_vec();
    for _ in 0..1_000_000 {
        body.extend_from_slice(pair);
    }
    body
}

#[test]
fn a_million_escaped_line_breaks_return_promptly() {
    for pair in [&b"\\\n"[..], b"\\\r\n"] {
        let mut body = escaped_breaks(pair);
        body.extend_from_slice(b"x}");
        let out = hostile("rtf-breaks", body).unwrap();
        assert_eq!(out.len(), 1_000_001);
        assert!(out.ends_with("\nx"));

        let mut body = br"{\rtf1 {\*\qsfill ".to_vec();
        body.extend_from_slice(&escaped_breaks(pair)[7..]);
        body.extend_from_slice(b"}after}");
        assert_eq!(hostile("rtf-breaks-skipped", body).unwrap(), "after");
    }
}

// -- Nesting -----------------------------------------------------------

#[test]
fn nesting_past_the_cap_is_an_error_naming_the_file() {
    let mut body = br"{\rtf1".to_vec();
    body.extend(std::iter::repeat_n(b'{', MAX_DEPTH));
    let err = hostile("rtf-deep", body).unwrap_err();
    assert!(err.contains("sample.rtf"), "must name the file: {err}");

    let mut body = br"{\rtf1".to_vec();
    body.extend(std::iter::repeat_n(b'{', 1_000_000));
    body.push(b'}');
    assert!(hostile("rtf-deeper", body).is_err());
}

#[test]
fn nesting_up_to_the_cap_is_read() {
    let mut body = br"{\rtf1 ".to_vec();
    body.extend(std::iter::repeat_n(b'{', MAX_DEPTH - 1));
    body.extend_from_slice(b"deep");
    body.extend(std::iter::repeat_n(b'}', MAX_DEPTH));
    assert_eq!(hostile("rtf-cap", body).unwrap(), "deep");
}

#[test]
fn stray_braces_after_the_document_are_ignored() {
    let mut body = br"{\rtf1 hi}".to_vec();
    body.extend(std::iter::repeat_n(b'}', 1_000_000));
    assert_eq!(hostile("rtf-stray", body).unwrap(), "hi");
}

#[test]
fn a_font_table_past_its_cap_is_ok() {
    let mut body = br"{\rtf1\ansi{\fonttbl".to_vec();
    for i in 0..300 {
        body.extend_from_slice(format!(r"{{\f{i}\fcharset204 F{i};}}").as_bytes());
    }
    // Font 255 is the last one kept; 299 decodes as the document's cp1252.
    body.extend_from_slice(br"}\f255 \'cf\f299 \'cf}");
    assert_eq!(hostile("rtf-fonts", body).unwrap(), "ПÏ");
}

// -- Framing -----------------------------------------------------------

#[test]
fn escaped_backslashes_and_braces_are_literal() {
    assert_eq!(text(br"{\rtf1 C:\\temp\\}after"), r"C:\temp\");
    assert_eq!(text(br"{\rtf1 a\\\par b}"), "a\\\nb");
    assert_eq!(text(br"{\rtf1 \{x\}}"), "{x}");
}

#[test]
fn bytes_around_the_document_are_ignored() {
    for tail in [
        &b"\0"[..],
        b"\x1a",
        b"\xef\xbb\xbf",
        b" junk {\\rtf1 more} }}",
    ] {
        let mut body = br"{\rtf1 hi}".to_vec();
        body.extend_from_slice(tail);
        assert_eq!(text(&body), "hi", "{tail:?}");
    }
    assert_eq!(text(b"\xef\xbb\xbf{\\rtf1 hi}"), "hi");
    assert_eq!(text(b"junk\r\n{ \\rtf1 hi}"), "hi");
}

#[test]
fn a_truncated_document_keeps_its_text() {
    assert_eq!(text(br"{\rtf1 hi"), "hi");
    assert_eq!(text(br"{\rtf1 {\b hi\'"), "hi");
    assert_eq!(text(br"{\rtf1 hi\"), "hi");
}

#[test]
fn only_a_missing_rtf_header_is_an_error() {
    for body in [&b"{x}"[..], b"", b"not rtf", b"{\\rt}"] {
        let err = hostile("rtf-header", body.to_vec()).unwrap_err();
        assert!(err.contains("sample.rtf"), "must name the file: {err}");
    }
}

// -- Line breaks in the source -----------------------------------------

#[test]
fn source_line_breaks_are_not_text() {
    assert_eq!(text(b"{\\rtf1 a\\par\r\nb}"), "a\nb");
    assert_eq!(text(b"{\\rtf1 Hel\r\nlo}"), "Hello");
    // After a control word, a line break is neither its delimiter nor text.
    assert_eq!(text(b"{\\rtf1 a\\b\r\nb}"), "ab");
}
