use super::{
    ignore_pattern_valid, middle_elide, palette, pattern_border, pattern_hint, wipe_mesh, Cow,
    WIPE_BAND_MIN,
};
use crate::test_ui::with_ui;

fn body_font(ui: &egui::Ui) -> egui::FontId {
    egui::TextStyle::Body.resolve(ui.style())
}

const DEEP: &str = "/media/shared/QuickSearch/crates/quicksearch-gui/src/search_tab.rs";

#[test]
fn text_that_fits_comes_back_untouched() {
    with_ui(|ui| {
        let font = body_font(ui);
        let out = middle_elide(ui, DEEP, 10_000.0, &font);
        assert!(matches!(out, Cow::Borrowed(_)), "borrowed when it fits");
        assert_eq!(out, DEEP);
        // Nothing to elide, however little room there is.
        assert!(matches!(middle_elide(ui, "", 0.0, &font), Cow::Borrowed(_)));
    });
}

#[test]
fn eliding_keeps_both_ends_and_cuts_once() {
    with_ui(|ui| {
        let out = middle_elide(ui, DEEP, 200.0, &body_font(ui));
        assert!(matches!(out, Cow::Owned(_)), "{out}");
        assert_eq!(out.matches('…').count(), 1, "{out}");
        let (head, tail) = out.split_once('…').expect("one ellipsis");
        assert!(
            !head.is_empty() && !tail.is_empty(),
            "both ends survive: {out}"
        );
        assert!(DEEP.starts_with(head), "{out}");
        assert!(DEEP.ends_with(tail), "{out}");
        assert!(
            tail.ends_with("search_tab.rs"),
            "the filename survives: {out}"
        );
    });
}

/// The load-bearing property. If the result laid out in the same font
/// were wider than the budget, egui would elide it a *second* time and
/// paint two ellipses.
#[test]
fn the_result_fits_the_budget_it_was_given() {
    with_ui(|ui| {
        let font = body_font(ui);
        for width in [40.0f32, 60.0, 121.5, 200.0, 337.5, 480.0] {
            let out = middle_elide(ui, DEEP, width, &font);
            let painted = ui.fonts(|f| {
                f.layout_no_wrap(out.to_string(), font.clone(), egui::Color32::WHITE)
                    .size()
                    .x
            });
            assert!(
                painted <= width,
                "at {width}: {out:?} lays out at {painted}"
            );
        }
    });
}

/// An ellipsis is the least that can stand for the text; a column too
/// narrow even for that gets it anyway rather than a panic.
#[test]
fn degenerate_widths_never_panic() {
    with_ui(|ui| {
        let font = body_font(ui);
        for width in [f32::NEG_INFINITY, -50.0, 0.0] {
            assert_eq!(middle_elide(ui, DEEP, width, &font), "…", "at {width}");
        }
    });
}

/// Indices walk by whole characters, so a path of multi-byte glyphs
/// slices cleanly instead of panicking mid-codepoint.
#[test]
fn multi_byte_paths_split_on_character_boundaries() {
    with_ui(|ui| {
        let font = body_font(ui);
        let path = "/srv/données/日本語/архив/файл-très-long.txt";
        for width in [30.0f32, 55.0, 90.0, 140.0, 210.0, 400.0] {
            let out = middle_elide(ui, path, width, &font);
            let (head, tail) = out.split_once('…').unwrap_or((out.as_ref(), ""));
            assert!(path.starts_with(head), "at {width}: {out}");
            assert!(path.ends_with(tail), "at {width}: {out}");
        }
    });
}

/// The trap behind "my ignore filters don't work" reports: ".jpg" is an
/// exact-name pattern, and the hint must say so and offer "*.jpg".
#[test]
fn extension_like_patterns_get_a_hint() {
    let hint = pattern_hint(".jpg").expect("hint for .jpg");
    assert!(hint.contains("*.jpg"), "{}", hint);
    assert!(hint.contains("exactly"), "{}", hint);

    // Fires for genuine exact-name patterns too — the statement it
    // makes is just as true for .git, so no allowlist.
    assert!(pattern_hint(".git").is_some());
    assert!(pattern_hint("  .venv  ").is_some(), "trimmed first");

    let targz = pattern_hint(".tar.gz").expect("hint for .tar.gz");
    assert!(targz.contains("*.tar.gz"), "{}", targz);
}

#[test]
fn working_patterns_get_no_hint() {
    for p in [
        "*.jpg",        // the fixed spelling itself
        "node_modules", // plain name
        ".hidden*",     // wildcard: the author knows about globs
        ".[jJ]pg",      // character class counts as a wildcard
        ".git/",        // separator: a path pattern
        r".git\",       // …either flavor
        ".",            // too short to be an extension
        "",
        "   ",
        "D:/",  // working drive-root spelling
        r"D:\", // …either flavor
        "cache-??",
    ] {
        assert_eq!(pattern_hint(p), None, "hinted on {:?}", p);
    }
}

#[test]
fn bare_drive_letters_get_a_hint() {
    let hint = pattern_hint("D:").expect("hint for D:");
    assert!(hint.contains("D:\\"), "{}", hint);
    assert!(pattern_hint("d:").is_some(), "case does not matter");
    assert_eq!(pattern_hint("DD:"), None, "not a drive letter");
    assert_eq!(pattern_hint("4:"), None, "not a drive letter");
}

#[test]
fn blank_patterns_are_invalid() {
    // IgnoreSet::compile would silently skip all of these.
    assert!(!ignore_pattern_valid(""));
    assert!(!ignore_pattern_valid("   "));
    assert!(!ignore_pattern_valid("/"));
    assert!(!ignore_pattern_valid("\\"));
    assert!(!ignore_pattern_valid("  //  "));
}

#[test]
fn malformed_globs_are_invalid() {
    assert!(!ignore_pattern_valid("["));
    assert!(!ignore_pattern_valid("foo[")); // unclosed character class
}

#[test]
fn usual_patterns_are_valid() {
    assert!(ignore_pattern_valid("*.tmp")); // extension
    assert!(ignore_pattern_valid("node_modules")); // name
    assert!(ignore_pattern_valid("Thumbs.db"));
    assert!(ignore_pattern_valid("/home/x/docs/*")); // directory
    assert!(ignore_pattern_valid("C:\\Windows\\Temp\\*"));
    assert!(ignore_pattern_valid("cache-??")); // wildcards
}

#[test]
fn empty_editor_keeps_the_theme_border() {
    // Nothing typed yet is not an error to flag.
    for dark in [true, false] {
        assert_eq!(pattern_border("", dark), None);
        assert_eq!(pattern_border("   ", dark), None);
        assert_eq!(pattern_border("\t\n", dark), None);
    }
}

#[test]
fn typed_text_is_judged() {
    for dark in [true, false] {
        let p = palette(dark);
        assert_eq!(pattern_border("*.tmp", dark), Some(p.green));
        assert_eq!(pattern_border("  node_modules  ", dark), Some(p.green));
        assert_eq!(pattern_border("foo[", dark), Some(p.red));
        // Typed, but trims away to nothing under the pattern rules —
        // still worth flagging, unlike a box the user has not filled in.
        assert_eq!(pattern_border("/", dark), Some(p.red));
    }
}

// --- The results wipe ---------------------------------------------------

const SECTION: egui::Rect = egui::Rect {
    min: egui::pos2(10.0, 100.0),
    max: egui::pos2(410.0, 500.0),
};
const FILL: egui::Color32 = egui::Color32::from_rgb(27, 27, 27);

/// Every vertex of `mesh` as (y, alpha), in paint order.
fn ramp(mesh: &egui::Mesh) -> Vec<(f32, u8)> {
    mesh.vertices
        .iter()
        .map(|v| (v.pos.y, v.color.a()))
        .collect()
}

/// The y ranges the mesh's quads cover, merged where they touch.
fn covered_spans(mesh: &egui::Mesh) -> Vec<(f32, f32)> {
    let mut spans: Vec<(f32, f32)> = Vec::new();
    for quad in mesh.vertices.chunks(4) {
        let (top, bottom) = (quad[0].pos.y, quad[3].pos.y);
        match spans.last_mut() {
            Some(last) if (last.1 - top).abs() < 1e-3 => last.1 = bottom,
            _ => spans.push((top, bottom)),
        }
    }
    spans
}

#[test]
fn a_revealed_section_paints_no_scrim() {
    // The steady state is the common one: no shape, no vertices, no cost.
    assert!(wipe_mesh(SECTION, 0.0, FILL).is_none());
    assert!(wipe_mesh(SECTION, -0.5, FILL).is_none());
}

#[test]
fn a_degenerate_section_paints_no_scrim() {
    let flat = egui::Rect::from_min_max(egui::pos2(10.0, 100.0), egui::pos2(410.0, 100.0));
    assert!(wipe_mesh(flat, 0.5, FILL).is_none());
    let sliver = egui::Rect::from_min_max(egui::pos2(10.0, 100.0), egui::pos2(10.0, 500.0));
    assert!(wipe_mesh(sliver, 0.5, FILL).is_none());
}

#[test]
fn an_unstarted_wipe_covers_the_whole_section() {
    let mesh = wipe_mesh(SECTION, 1.0, FILL).expect("fully hidden");
    assert!(
        ramp(&mesh).iter().all(|&(_, a)| a == 255),
        "nothing may show through before the reveal starts: {:?}",
        ramp(&mesh)
    );
    assert_eq!(
        covered_spans(&mesh),
        vec![(SECTION.top(), SECTION.bottom())],
        "the quads must tile the section with no gap"
    );
}

#[test]
fn the_scrim_ramps_clear_at_the_top_to_solid_at_the_bottom() {
    let mesh = wipe_mesh(SECTION, 0.5, FILL).expect("mid-travel");
    let ramp = ramp(&mesh);
    assert_eq!(ramp.first().expect("vertices").1, 0, "the top is untouched");
    assert_eq!(ramp.last().expect("vertices").1, 255, "the bottom is gone");
    // Alpha only ever increases downward, and the quads stay contiguous:
    // a gradient, not a stack of steps.
    for pair in ramp.windows(2) {
        assert!(
            pair[1].0 >= pair[0].0 && pair[1].1 >= pair[0].1,
            "vertices run down the section, clear to opaque: {ramp:?}"
        );
    }
    assert_eq!(covered_spans(&mesh).len(), 1, "one contiguous scrim");
}

#[test]
fn less_of_the_section_shows_the_further_the_wipe_is_from_done() {
    // How much of the section's height the scrim swallows: the integral
    // of alpha down it, so a widening gradient counts as well as a
    // growing solid block.
    let hidden_height = |wipe: f32| {
        let mesh = wipe_mesh(SECTION, wipe, FILL).expect("travelling");
        mesh.vertices
            .chunks(4)
            .map(|quad| {
                let alpha = |v: &egui::epaint::Vertex| v.color.a() as f32 / 255.0;
                (quad[3].pos.y - quad[0].pos.y) * (alpha(&quad[0]) + alpha(&quad[3])) / 2.0
            })
            .sum::<f32>()
    };
    let mut previous = 0.0;
    for step in 1..=10 {
        let hidden = hidden_height(step as f32 / 10.0);
        assert!(
            hidden > previous,
            "each step hides more than the last: {hidden} after {previous}"
        );
        previous = hidden;
    }
    assert!(
        (previous - SECTION.height()).abs() < 0.5,
        "and the section is entirely gone by the end: {previous} of {}",
        SECTION.height()
    );
}

#[test]
fn a_short_section_still_gets_a_gradient() {
    // Two rows tall: the proportional band would be a few points, small
    // enough to read as a hard cut, so the floor takes over.
    let short = egui::Rect::from_min_max(egui::pos2(10.0, 100.0), egui::pos2(410.0, 130.0));
    let mesh = wipe_mesh(short, 0.5, FILL).expect("mid-travel");
    let gradient = mesh
        .vertices
        .chunks(4)
        .any(|quad| quad[0].color.a() < quad[3].color.a());
    assert!(
        gradient,
        "the edge ramps rather than cutting: {:?}",
        ramp(&mesh)
    );
    let (clear, covered) = super::wipe_edges(short, 0.5);
    assert!(
        (covered - clear - WIPE_BAND_MIN).abs() < 1e-3,
        "the band is held at its floor: {}",
        covered - clear
    );
}
