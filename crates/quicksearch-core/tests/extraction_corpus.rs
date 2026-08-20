//! End-to-end extraction coverage across every format QuickSearch claims.
//!
//! The corpus itself — what is written, by which library, and why that library
//! rather than the reader's own — is documented in [`corpus`]. This file is
//! only the assertions.
//!
//! Three layers, deliberately, because they fail for different reasons:
//!
//! 1. [`every_format_extracts_its_planted_text`] goes through
//!    `mime::guess_mime_from_head` and `extract::Registry`, so a file typed
//!    wrongly and a file parsed wrongly are both caught, and the failure
//!    message says which.
//! 2. [`head_extraction_agrees_with_reading_the_file`] pins the walk-time
//!    shortcut against the content-pass path, in both directions.
//! 3. [`the_whole_corpus_indexes_and_is_searchable`] is the product claim: the
//!    text reached FTS5 and a user typing a word from the document finds it.

mod common;
mod corpus;

use std::path::Path;
use std::sync::atomic::AtomicU64;

use quicksearch_core::config::Config;
use quicksearch_core::extract::Registry;
use quicksearch_core::mime;
use quicksearch_core::query::split_for_cascade;
use quicksearch_core::search::cascade;
use quicksearch_core::search::SearchOptions;

use corpus::Sample;

/// How much of a file the MIME sniff is shown. The same default the indexer
/// uses, so dispatch here matches dispatch in a real run.
const HEAD: usize = 8 * 1024;

/// Read the leading [`HEAD`] bytes, which is what the walk hands the sniff.
fn head_of(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    bytes[..bytes.len().min(HEAD)].to_vec()
}

/// Whether the file is small enough that the walk would extract it from the
/// buffer it already hashed, rather than reopening it.
fn fits_in_head(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() as usize)
        .unwrap_or(usize::MAX)
        <= HEAD
}

/// Prefix every failure with the seed, so a red CI job reproduces locally.
fn ctx(sample: &Sample) -> String {
    format!(
        "[{} | {} | QUICKSEARCH_CORPUS_SEED={}]",
        sample.label,
        sample.path.display(),
        corpus::seed()
    )
}

/// Every corpus file is typed, claimed by an extractor, and yields its planted
/// lipsum in order.
#[test]
fn every_format_extracts_its_planted_text() {
    let (_dir, samples) = corpus::build("corpus-extract");
    let registry = Registry::default_set();

    for sample in &samples {
        let head = head_of(&sample.path);
        let mime = mime::guess_mime_from_head(&sample.path, &head)
            .unwrap_or_else(|| panic!("{} no MIME resolved", ctx(sample)));
        assert!(
            registry.supports(&mime),
            "{} MIME {mime:?} is claimed by no extractor",
            ctx(sample)
        );

        let content = registry
            .extract(&sample.path, &mime)
            .unwrap_or_else(|e| panic!("{} extraction failed: {e}", ctx(sample)))
            .unwrap_or_else(|| panic!("{} MIME {mime:?} dispatched nowhere", ctx(sample)));

        if let Err(why) = corpus::match_in_order(&content.text, &sample.must_contain) {
            panic!("{} extracted text is wrong\n{why}", ctx(sample));
        }
    }

    // A corpus that silently stopped generating anything would pass every
    // assertion above.
    assert!(
        samples.len() >= 35,
        "corpus shrank to {} samples",
        samples.len()
    );
}

/// The walk-time buffer path and the content-pass path agree, and only the
/// formats that can support the shortcut take it.
#[test]
fn head_extraction_agrees_with_reading_the_file() {
    let (_dir, samples) = corpus::build("corpus-head");
    let registry = Registry::default_set();
    let mut opted_in = 0;

    for sample in &samples {
        let head = head_of(&sample.path);
        let mime = mime::guess_mime_from_head(&sample.path, &head).expect("MIME");
        let whole = std::fs::read(&sample.path).expect("read whole file");
        let from_head = registry.extract_complete_head(&sample.path, &mime, &whole);

        if !sample.head_path {
            // A format that seeks, or reads a trailer, must never be handed a
            // buffer — `None` here is what routes it back to the on-disk
            // extractor rather than to a wrong answer.
            assert!(
                from_head.is_none(),
                "{} took the head path but cannot support it",
                ctx(sample)
            );
            continue;
        }
        opted_in += 1;

        let from_head = from_head
            .unwrap_or_else(|| panic!("{} declined the head path", ctx(sample)))
            .unwrap_or_else(|e| panic!("{} head extraction failed: {e}", ctx(sample)));
        let from_disk = registry
            .extract(&sample.path, &mime)
            .expect("on-disk extraction")
            .expect("claimed");

        assert_eq!(
            from_head.text,
            from_disk.text,
            "{} head and disk extraction disagree",
            ctx(sample)
        );

        // The claim is only interesting where the walk would really take the
        // shortcut. `oversized.txt` is the deliberate exception.
        if fits_in_head(&sample.path) {
            assert!(
                corpus::match_in_order(&from_head.text, &sample.must_contain).is_ok(),
                "{} head extraction lost the planted text",
                ctx(sample)
            );
        }
    }

    assert!(
        opted_in >= 24,
        "only {opted_in} samples exercised the head path"
    );
}

/// The whole corpus indexes, and each file is findable by a word that appears
/// only in its body.
#[test]
fn the_whole_corpus_indexes_and_is_searchable() {
    let (dir, samples) = corpus::build("corpus-index");
    // The database goes in its own directory, not the one being walked. Left
    // inside, SQLite's `-wal` and `-shm` sidecars appear under the root and
    // survive only because a default ignore pattern happens to prune them —
    // which is a dependency this test has no reason to take on.
    let db = common::scratch_db("corpus-index-db");
    let config = Config::default();

    common::IndexOnce {
        db: &db,
        roots: vec![dir.to_string_lossy().into_owned()],
        config: &config,
        fresh_marker: false,
        encrypted: false,
    }
    .run();

    let conn = rusqlite::Connection::open(&db).expect("open index");
    for sample in &samples {
        let hits = search(&conn, &sample.needle);
        let paths: Vec<&str> = hits.iter().map(|p| p.as_str()).collect();
        assert_eq!(
            hits.len(),
            1,
            "{} searching {:?} returned {:?}",
            ctx(sample),
            sample.needle,
            paths
        );
        assert!(
            Path::new(&hits[0]) == sample.path,
            "{} searching {:?} found {:?}",
            ctx(sample),
            sample.needle,
            hits[0]
        );
    }
}

/// Every result path for `term`, run through the same cascade the GUI uses.
fn search(conn: &rusqlite::Connection, term: &str) -> Vec<String> {
    let split = split_for_cascade(term).expect("split");
    let latest = AtomicU64::new(1);
    let mut paths = Vec::new();
    cascade::run(
        conn,
        &split,
        &SearchOptions::default(),
        1,
        &latest,
        &mut |batch| {
            for hit in batch {
                paths.push(hit.path.clone());
            }
        },
    )
    .expect("cascade run")
    .expect("not cancelled");
    paths
}

/// Two RTF lexer bugs this corpus found, now fixed in `vendor/rtf-parser`.
///
/// Both came from a lexer that decided where a control word ends by looking
/// for whitespace, and then trimmed leading spaces off whatever followed. The
/// second reproduced on a file LibreOffice wrote.
///
/// **A `\uN` escape with a literal fallback took the rest of the word with
/// it.** The escape is followed by an ANSI fallback character for readers that
/// predate Unicode, and the spec allows any character there. Terminating a
/// control word at whitespace only meant `before \u233?after end` lexed
/// `\u233?after` as one unrecognised control word and yielded `"before end"` —
/// the accented character gone, and `after` with it. The loss ran to the next
/// space. The spellings that put a backslash where the lexer needed a boundary
/// — LibreOffice's `\uN\'3f` and Word's `\uN\'hh` — were unaffected.
///
/// **A space between two escaped words was dropped.** After a `\'hh` escape
/// the lexer re-tokenised the remainder, trimming its leading spaces before
/// deciding what it was. A plain-text remainder kept the untrimmed slice and
/// survived; one beginning with another control word did not. So
/// `Καλημέρα κόσμε` — every character escaped on both sides of the space —
/// came back as `Καλημέρακόσμε`, two words collapsed into one FTS term. That
/// hit every script outside cp1252: Greek, Cyrillic, Hebrew, CJK. Latin-1 text
/// was fine, because there the escapes sit *inside* words (`caf\'e9`) and the
/// space that follows is plain text.
///
/// The fixes are `StrUtils::split_control_word` and the `\'hh` arm of
/// `Lexer::tokenize`, both marked LOCAL PATCH; the fallback characters are now
/// counted off against `\ucN` in the parser rather than guessed at. See the
/// `[patch.crates-io]` note in the workspace manifest.
#[test]
fn rtf_unicode_escapes_survive_extraction() {
    let dir = quicksearch_core::testutil::scratch_dir("rtf-escapes");
    let extract = |name: &str, body: &str| {
        use quicksearch_core::extract::Extractor;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        quicksearch_core::extract::rtf::RtfExtractor
            .extract(&path)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .text
    };

    // The literal fallback. Both halves matter: the escape survives, and so
    // does the word it used to swallow.
    let text = extract("literal.rtf", r"{\rtf1\ansi before \u233?after end}");
    assert!(text.contains("before éafter end"), "{text:?}");
    // And the fallback itself is *not* text. It repeats the character for
    // readers that cannot do Unicode; indexing it would put a `?` inside every
    // word containing a non-cp1252 character.
    assert!(!text.contains('?'), "fallback character indexed: {text:?}");

    // The same document in the two spellings that always worked, to pin that
    // counting fallbacks did not break the ones the old mask handled.
    for (label, body) in [
        ("libreoffice", r"{\rtf1\ansi before \u233\'3fafter end}"),
        ("word", r"{\rtf1\ansi before \u233\'e9after end}"),
    ] {
        let text = extract(&format!("{label}.rtf"), body);
        assert!(text.contains("before éafter end"), "{label}: {text:?}");
    }

    // `\ucN` is the fallback count, and it is not always one.
    let text = extract("uc2.rtf", r"{\rtf1\ansi\uc2 a\u233?!b}");
    assert!(text.contains("aéb"), "two fallbacks not skipped: {text:?}");
    let text = extract("uc0.rtf", r"{\rtf1\ansi\uc0 a\u233?b}");
    assert!(
        text.contains("aé?b"),
        "\\uc0 means no fallback, so the `?` is real text: {text:?}"
    );

    // Exactly the bytes LibreOffice writes for `Καλημέρα κόσμε`.
    let text = extract(
        "escaped-words.rtf",
        concat!(
            r"{\rtf1\ansi ",
            r"\u922\'3f\u945\'3f\u955\'3f\u951\'3f",
            r"\u956\'3f\u941\'3f\u961\'3f\u945\'3f",
            " ",
            r"\u954\'3f\u972\'3f\u963\'3f\u956\'3f\u949\'3f",
            "}"
        ),
    );
    assert!(
        text.contains("Καλημέρα κόσμε"),
        "the space between two fully escaped words was dropped: {text:?}"
    );

    // A `\'hh` that is nobody's fallback is still text, and a space after it
    // is still a space — the case the second fix must not overshoot.
    let text = extract("standalone-hex.rtf", r"{\rtf1\ansi caf\'e9 \'e0 noon}");
    assert!(text.contains("café à noon"), "{text:?}");

    // A surrogate pair is one character, and `\uc1` puts a fallback between
    // its halves. U+1F600, written the way Word writes it.
    let text = extract("astral.rtf", r"{\rtf1\ansi a\u-10179?\u-8704?b}");
    assert!(
        text.contains("a\u{1F600}b"),
        "surrogate pair lost: {text:?}"
    );
}

/// The corpus's needles and the shared harness's body term are the same word,
/// deliberately.
///
/// `common::BODY_TERM` exists because a term that reaches the index through a
/// file *name* as well as a body is answered mostly by the filename pass — so
/// it plants one that only ever appears in a body. The corpus needs exactly
/// that property, per file, for its end-to-end search to be attributable to
/// extraction rather than to the name.
///
/// `corpus` does not name `common` (it has no other reason to depend on the
/// harness), so this is what stops the two drifting apart silently.
#[test]
fn corpus_needles_use_the_shared_body_term() {
    assert_eq!(corpus::NEEDLE_PREFIX, common::BODY_TERM);
}
