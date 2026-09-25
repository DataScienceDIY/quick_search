//! The Help tab: what QuickSearch is, how to start, how results are ranked
//! and what ends up in the index. A control's own help lives on its hover tip
//! (`tips.rs`), and the query language in the Search tab's syntax window;
//! README.md covers building, installing and configuring.

use crate::ui_util::hint;

/// Returns true when the "Show the introduction again" button was clicked.
pub fn ui(ui: &mut egui::Ui) -> bool {
    let mut replay = false;
    // A page-like column, so a maximized window does not stretch paragraphs
    // into single lines. Only ever a cap: `set_max_width` widens as readily
    // as it narrows, and text wider than the panel is clipped, not wrapped.
    // Measured here because a `ScrollArea`'s content may be wider than its
    // viewport; the scroll bar's width comes off the top.
    let column = (ui.available_width() - ui.spacing().scroll.allocated_width()).min(620.0);
    let scroll = egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            ui.set_max_width(column);

            section_heading(ui, "Welcome to QuickSearch");
            ui.label(
                "QuickSearch searches the files on this computer and the text \
                 inside them. It reads your folders once and answers from that \
                 index instead of the disk, so results appear as you type, even \
                 across hundreds of thousands of files.",
            );
            ui.add_space(4.0);
            ui.label(
                "It never connects to the internet. The index can be encrypted \
                 with a password from the Settings tab.",
            );

            ui.add_space(6.0);
            ui.label("The primary home of this software is:");
            let link = |ui: &mut egui::Ui, url: &str| ui.hyperlink_to(url, url);
            link(ui, "https://quicksearch.karsttech.com");
            link(ui, "https://code.karsttech.com/jeremy/quick_search");
            ui.add_space(6.0);
            ui.label("The code is also mirrored to GitHub for bug reports and issues:");
            link(ui, "https://github.com/DataScienceDIY/quick_search");

            ui.add_space(6.0);
            if ui.button("Show the introduction again").clicked() {
                replay = true;
            }

            ui.add_space(6.0);
            ui.label(hint(
                "Hover any control to see what it does and what to set it to. \
                 This tab covers the ideas behind them; the ? button beside the \
                 search box covers the query language.",
            ));

            ui.add_space(12.0);
            section_heading(ui, "Getting started");
            ui.label(
                "1.  On first run QuickSearch indexes your home folder. The status \
                 bar shows progress; you can search meanwhile, but files not yet \
                 reached won't appear.",
            );
            ui.label(
                "2.  To search other places, add them on the Manage Index tab. \
                 Indexed folders are watched, so the index keeps up as files \
                 change.",
            );
            ui.label(
                "3.  Type in the search box on the Search tab. Results appear as \
                 you type, best matches first.",
            );
            ui.label(
                "4.  To shrink the index, use the Content filters on the Manage \
                 Index tab (see What gets indexed, below).",
            );

            ui.add_space(12.0);
            section_heading(ui, "Searching");
            ui.label("Plain words match names, contents and paths:");
            ui.monospace("quarterly budget");
            ui.label("Filters narrow the results and combine with the words:");
            ui.monospace("type:Document modified:>=2024-01-01 report");
            ui.label("The ? button left of the search box shows the full query syntax.");
            ui.add_space(6.0);
            ui.label(
                "•  A match in a name or path is highlighted in that column; a \
                 match in the contents shows the surrounding text in the Content \
                 Match column, with more on hover.",
            );
            ui.label(
                "•  Right-click a result to open it or its folder, copy its path, \
                 or hide files like it from the results.",
            );
            ui.label(
                "•  Click a column header to sort by it; right-click any header to \
                 choose the columns shown.",
            );

            ui.add_space(12.0);
            ranking_section(ui);

            ui.add_space(12.0);
            what_gets_indexed_section(ui);

            ui.add_space(12.0);
            duplicates_section(ui);

            ui.add_space(12.0);
            section_heading(ui, "Updates");
            ui.label(
                "QuickSearch never sends anything off your computer, so it does \
                 not update itself.",
            );
            ui.label("To update, install the latest version yourself from:");
            link(ui, "https://quicksearch.karsttech.com");

            ui.add_space(12.0);
            section_heading(ui, "The other tabs");
            let prose = prose_width(ui.available_width());
            egui::Grid::new("help-tabs")
                .num_columns(2)
                .spacing([CELL_SPACING, 5.0])
                .show(ui, |ui| {
                    for (name, what) in OTHER_TABS {
                        ui.strong(name);
                        cell(ui, prose, what);
                        ui.end_row();
                    }
                });

            ui.add_space(12.0);
            section_heading(ui, "Terminal");
            ui.label("QuickSearch also searches from a terminal:");
            ui.monospace("quicksearch \"quarterly budget\"");
            ui.label("On Windows use quicksearch-cli. Either way, --help lists the flags.");

            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                // Several widgets make one sentence, so the spacing between
                // them comes from the text.
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(hint(
                    "Building from source, installing and configuring are covered in ",
                ));
                if let Some(path) = readme_path() {
                    let path = path.display().to_string();
                    if ui
                        .link(egui::RichText::new("README.md").small())
                        .on_hover_text(&path)
                        .clicked()
                    {
                        crate::platform::open_file(&path);
                    }
                } else {
                    ui.label(hint("README.md"));
                }
                ui.label(hint("."));
            });
        });
    crate::ui_util::more_below_hint(ui, &scroll);
    replay
}

/// A section's heading and the gap under it.
fn section_heading(ui: &mut egui::Ui, text: &str) {
    ui.heading(egui::RichText::new(text).strong());
    ui.add_space(4.0);
}

/// The other tabs, each with what it is for.
const OTHER_TABS: [(&str, &str); 4] = [
    (
        "Manage Index",
        "indexing status and controls, the indexed folders, and the filters \
         that decide what is skipped",
    ),
    (
        "Duplicates",
        "groups of files that are probably copies; verify before deleting",
    ),
    ("Logs", "warnings from indexing"),
    (
        "Settings",
        "every setting in one place; hover any control for what it does",
    ),
];

/// `(stage, tier, what)`. Each chip wears the Rank column's colour for the
/// *first* cascade stage its tier covers (see the rank table in
/// `search::cascade`): exact name 1–2, name contains 3–4, text inside 5–6,
/// fuzzy 7–8, path 9–11.
const RANKING: [(u8, &str, &str); 5] = [
    (1, "Exact name", "the file is called exactly what you typed"),
    (
        3,
        "Name contains",
        "what you typed is somewhere in the file's name",
    ),
    (
        5,
        "Text inside",
        "the words are in the file's contents, most mentions first",
    ),
    (
        7,
        "Close spelling",
        "a name or some text within a typo or two of what you typed, \
         only while Fuzzy is ticked",
    ),
    (
        9,
        "Path only",
        "no match in the name or text, but a folder along the way matched",
    ),
];

/// The tiers of `search::cascade`, collapsed to the ones a reader can act on:
/// its eleven ranks pair up (exact case, then any case) everywhere but the
/// fuzzy stages.
fn ranking_section(ui: &mut egui::Ui) {
    section_heading(ui, "How results are ranked");
    ui.label(
        "Each result falls in a tier by where it matched, and tiers come out in \
         this order. Within a tier, a match in the case you typed beats one \
         that ignores case, and a file that mentions your words more often \
         comes first.",
    );
    ui.add_space(6.0);
    let prose = prose_width(ui.available_width());
    egui::Grid::new("help-ranking")
        .num_columns(2)
        .spacing([CELL_SPACING, 5.0])
        .striped(true)
        .show(ui, |ui| {
            for (stage, tier, what) in RANKING {
                ui.label(
                    egui::RichText::new(format!(" {} ", tier))
                        .strong()
                        .background_color(crate::color::rank_tier_color(stage))
                        // The same near-black the Search tab's chips carry,
                        // which every ramp colour holds contrast against.
                        .color(egui::Color32::from_rgb(32, 32, 32)),
                );
                cell(ui, prose, what);
                ui.end_row();
            }
        });
    ui.add_space(6.0);
    ui.label(
        "The Rank column shows each result's tier, coloured from blue for a \
         close match to red for a distant one. Sorting by another column sets \
         this order aside until you sort by Rank again.",
    );
    ui.label(hint(
        "Results stream in while a search runs and are only added at the end, \
         so the list never reshuffles as you read.",
    ));
}

/// The two Manage Index filters, side by side, because the confusion they
/// cause is about which one keeps a file out of the index (ignore patterns)
/// and which one only stops its text being read (the whitelist).
fn what_gets_indexed_section(ui: &mut egui::Ui) {
    section_heading(ui, "What gets indexed");
    ui.label(
        "Everything in your indexed folders, minus what the two filters on the \
         Manage Index tab take out. One decides which files are in the index at \
         all; the other decides which of those have their text read.",
    );

    ui.add_space(8.0);
    ui.strong("Ignore patterns: files and folders never indexed");
    ui.add_space(4.0);
    ui.label(
        "Patterns match file and folder names and whole paths, never file \
         contents. A file they match is gone from the index entirely, text \
         included.",
    );
    ui.add_space(4.0);
    ui.label(
        "•  No slash: a name. It must match a whole file or folder name, \
         anywhere in your indexed folders, so excluding an extension needs a \
         wildcard (*.jpg).",
    );
    ui.label(
        "•  With a slash: a path. It must match a whole path, and takes out \
         everything under it.",
    );
    ui.label("•  * matches any run of characters, including none; ? exactly one.");
    ui.add_space(6.0);
    ignore_examples(ui);
    ui.add_space(6.0);
    ui.label(hint(
        "Path patterns match from the start, so Vacation/Diary alone matches \
         nothing; */Vacation/Diary finds that folder anywhere. Case matters on \
         Linux but not on Windows or macOS, so *diary* catches Diary only there.",
    ));
    ui.label(hint(
        "Removing a pattern brings its files back at the next indexing run. \
         QuickSearch never deletes the files themselves.",
    ));

    ui.add_space(8.0);
    ui.strong("Full-text extensions whitelist: which files have their text read");
    ui.add_space(4.0);
    ui.label(
        "This limits contents only. Files it leaves out are still indexed and \
         found by name and path, just not by the words inside them.",
    );
    ui.add_space(4.0);
    ui.label(
        "•  Empty, the default, means QuickSearch reads the text of every file \
         type it understands.",
    );
    ui.label(
        "•  To narrow it, enter one extension per line, leading dot optional. \
         Listing only txt, md and pdf keeps the stored text small and focused \
         on documents; every other file stays findable by name.",
    );
    ui.label(
        "•  A non-empty list also leaves out files with no extension, such as \
         Makefile or README. Add the line (none) to include them.",
    );
    ui.label("•  Anything after a # is a comment, so a line can be switched off in place.");
}

/// What a duplicate group is and is not. The tab has room for the caution
/// but not the reasoning, and "they differ but look the same" is the
/// question the feature reliably provokes.
fn duplicates_section(ui: &mut egui::Ui) {
    section_heading(ui, "Duplicates");
    ui.label(
        "The Duplicates tab groups files that look like copies, biggest waste \
         first. It only lists them; it never deletes or moves anything.",
    );
    ui.add_space(4.0);
    ui.label(
        "A group is a strong suspicion, not a verdict: files are grouped by \
         their size and a hash of their first few kilobytes, and the rest is \
         never read. Before deleting anything, right-click the group and choose \
         Verify copies are identical to compare every byte.",
    );

    ui.add_space(8.0);
    ui.strong("Verified as different, but they look the same?");
    ui.add_space(4.0);
    ui.label(
        "They differ in bytes you never see. PDFs and Office documents store \
         their creation and modification dates, a document ID and a revision \
         number inside the file, so two invoices from one template, or one \
         document saved twice, are different files on disk.",
    );
    ui.label(hint(
        "The report names the byte that differs and the file's size. A \
         difference in the first few hundred bytes is almost always that \
         bookkeeping; one deep inside a large file is not.",
    ));

    ui.add_space(8.0);
    ui.strong("Groups you never want to see again");
    ui.add_space(4.0);
    ui.label(
        "•  Hide this group (right-click) drops one group for good. It is \
         remembered by content, so it stays hidden when the files are renamed \
         or moved and returns if they are edited. Show hidden lists hidden \
         groups; Clear forgets them all.",
    );
    ui.label(
        "•  Exclude drops whole sets of files, such as a backup folder meant to \
         hold copies. The box above the list takes ignore-filter patterns, and \
         the right-click menu offers the file's type or folder.",
    );
    ui.label(hint(
        "Neither affects search: excluded files stay indexed and findable by \
         name and content.",
    ));
}

/// One ignore-pattern example: what it takes out, and the near miss it
/// leaves alone. The near miss is half the point, so it survives every
/// layout.
struct Example {
    pattern: &'static str,
    excluded: &'static str,
    kept: &'static str,
}

const IGNORE_EXAMPLES: &[Example] = &[
    Example {
        pattern: "node_modules",
        excluded: "a file or folder named exactly that, and all it holds",
        kept: "node_modules_old",
    },
    Example {
        pattern: "*.jpg",
        excluded: "holiday.jpg, 1.jpg",
        kept: "holiday.jpeg, holiday.jpg.exe",
    },
    Example {
        pattern: "?.jpg",
        excluded: "a.jpg, 1.jpg",
        kept: "12.jpg, holiday.jpg",
    },
    Example {
        pattern: "*diary*",
        excluded: "Mydiary.jpg, and a folder named Diary with all it holds",
        kept: "Pictures/Vacation",
    },
    Example {
        pattern: "*/Vacation/Diary",
        excluded: "that one folder, and everything under it",
        kept: "Pictures/Diary",
    },
];

/// Under this, three columns get so little each that the middle one wraps to
/// one or two words a line and the table is harder to read than a list.
const EXAMPLE_TABLE_MIN_WIDTH: f32 = 560.0;

/// The examples as a table where there is room for one, stacked where there
/// is not. The two layouts differ in widget count, hence
/// [`crate::ui_util::stable_section`].
fn ignore_examples(ui: &mut egui::Ui) {
    crate::ui_util::stable_section(ui, |ui| {
        let available = ui.available_width();
        if available < EXAMPLE_TABLE_MIN_WIDTH {
            for example in IGNORE_EXAMPLES {
                ui.monospace(example.pattern);
                cell(ui, available, format!("Excluded: {}", example.excluded));
                cell(ui, available, format!("Still indexed: {}", example.kept));
                ui.add_space(6.0);
            }
            return;
        }
        // The two prose columns share what the pattern column and the two
        // gaps leave over.
        let prose = (available - KEY_COL_WIDTH - 2.0 * CELL_SPACING) / 2.0;
        egui::Grid::new("help-ignore-examples")
            .num_columns(3)
            .spacing([CELL_SPACING, 5.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Pattern");
                cell(ui, prose, egui::RichText::new("Excluded").strong());
                cell(ui, prose, egui::RichText::new("Still indexed").strong());
                ui.end_row();
                for example in IGNORE_EXAMPLES {
                    ui.monospace(example.pattern);
                    cell(ui, prose, example.excluded);
                    cell(ui, prose, example.kept);
                    ui.end_row();
                }
            });
    });
}

/// Width allowed for a table's leading key column, generous enough for the
/// longest of them (`*/Vacation/Diary`, in monospace).
const KEY_COL_WIDTH: f32 = 140.0;
/// Both tables' horizontal cell spacing.
const CELL_SPACING: f32 = 14.0;

/// A prose cell in a child `Ui` of exactly `width` and exactly the wrapped
/// text's height. The fixed width keeps `Grid`'s default `Extend` wrap from
/// widening the whole tab, and a wrapped `Label` from shrinking its column to
/// a ribbon. The measured height keeps `Grid` from centring a zero-height box
/// on the row's midpoint, which would put the text half a row below its
/// stripe.
fn cell(ui: &mut egui::Ui, width: f32, text: impl Into<egui::WidgetText>) {
    let galley = text.into().into_galley(
        ui,
        Some(egui::TextWrapMode::Wrap),
        width,
        egui::TextStyle::Body,
    );
    ui.allocate_ui(egui::vec2(width, galley.size().y), |ui| {
        ui.add(egui::Label::new(galley));
    });
}

/// The prose width for a two-column table: everything the key column and the
/// spacing leave over.
fn prose_width(available: f32) -> f32 {
    (available - KEY_COL_WIDTH - CELL_SPACING).max(120.0)
}

/// Where this build left the README: under the install prefix's `share/doc`
/// (the .deb puts it in `/usr/share/doc/quicksearch/`), beside the executable
/// (the Windows installer and portable copies), or at the top of a build tree
/// a few levels above `target/`.
fn readme_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let installed = dir
        .parent()
        .map(|prefix| prefix.join("share/doc/quicksearch/README.md"));
    installed
        .into_iter()
        .chain(dir.ancestors().take(4).map(|d| d.join("README.md")))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    /// One pass of the whole tab in a `w` by `h` viewport.
    fn render(w: f32, h: f32) -> (egui::Context, egui::FullOutput) {
        let ctx = crate::test_ui::ctx();
        let out = crate::test_ui::run_panel(&ctx, egui::vec2(w, h), vec![], |ui| {
            super::ui(ui);
        });
        (ctx, out)
    }

    /// The Help tab is the largest block of prose in the app and the only tab
    /// with no other test, which makes it the widest net for the one thing
    /// dropping egui's emoji faces could break: a character with no glyph,
    /// painted as `◻`.
    #[test]
    fn the_help_tab_paints_no_missing_glyphs() {
        let (ctx, out) = render(1000.0, 900.0);
        assert!(
            !crate::test_ui::painted_text(&out).is_empty(),
            "the tab painted nothing, so the glyph check proves nothing"
        );
        crate::test_ui::assert_no_tofu(&ctx, &out);
    }

    /// Ranking and the filter rules live on this tab and nowhere else a user
    /// can read without hovering, so a section quietly dropped from `ui`
    /// would take the only copy with it. Tall enough a viewport that the
    /// scroll area paints the whole document.
    #[test]
    fn the_tab_carries_the_sections_that_live_nowhere_else() {
        let (_, out) = render(1000.0, 4000.0);
        let painted = crate::test_ui::painted_text(&out).join("\n");
        for expected in [
            "How results are ranked",
            "What gets indexed",
            // One row of each table: the grids are the part most easily lost
            // to a refactor, and the tooltips point here for them.
            "Path only",
            "*/Vacation/Diary",
            // The duplicates section exists to answer one question in full,
            // and the tab has room for the caution but not the reasoning.
            "Verify copies are identical",
            "revision number",
        ] {
            assert!(painted.contains(expected), "no {:?}: {}", expected, painted);
        }
    }

    /// The column is capped at 620 points so a maximized window does not
    /// stretch a paragraph into one long line. A cap that is also a floor
    /// lays the text out past the scroll area, which clips it rather than
    /// wrapping it: the ends of the lines simply vanish. Galley rects are
    /// pre-clip, so this measures the layout rather than the paint. Also
    /// what `cell`'s fixed width is for.
    #[test]
    fn a_window_narrower_than_the_column_reflows_rather_than_clipping() {
        // 640 is the smallest window the app allows, and the UI scale
        // divides it: 400 is roughly that window at 1.6x, and 250 is it at
        // the 2.5x ceiling, the narrowest layout the app can produce.
        for width in [250.0_f32, 320.0, 400.0, 480.0, 560.0, 620.0] {
            let (_, out) = render(width, 6000.0);
            let overflowing: Vec<_> = crate::test_ui::painted(&out)
                .into_iter()
                .filter(|(_, rect)| rect.max.x > width)
                .map(|(text, rect)| format!("{:?} reaches {}", text, rect.max.x))
                .collect();
            assert!(
                overflowing.is_empty(),
                "text laid out past the {}pt panel: {:#?}",
                width,
                overflowing
            );
        }
    }

    /// Every tier chip wears the Rank column's own colour for its first
    /// cascade stage: the chips exist to demonstrate the blue-to-red ramp the
    /// paragraph under the table describes.
    #[test]
    fn the_ranking_tiers_wear_the_rank_colors() {
        let (_, out) = render(1000.0, 4000.0);
        // A RichText background is a section format in the galley, not a
        // separate rect shape.
        let backgrounds: Vec<egui::Color32> = crate::test_ui::painted_backgrounds(&out)
            .into_iter()
            .map(|(_, color)| color)
            .collect();
        for (stage, _, _) in super::RANKING {
            let color = crate::color::rank_tier_color(stage);
            assert!(
                backgrounds.contains(&color),
                "no chip painted in stage {}'s colour {:?}",
                stage,
                color
            );
        }
    }

    /// A table cell allocated with no height of its own is centred on its
    /// row's midpoint by `Grid` and then laid out downward from there, which
    /// leaves every prose column half a row below its key. Also what `cell`'s
    /// measured height is for.
    #[test]
    fn both_halves_of_a_table_row_share_a_centre_line() {
        let (_, out) = render(1000.0, 4000.0);
        // One single-line and one wrapping row from each striped table: the
        // offset is half the row's height, so a wrapping row misses by twice
        // as much as a single-line one.
        let [exact, _, _, fuzzy, _] = super::RANKING;
        let [node_modules, jpg, ..] = super::IGNORE_EXAMPLES else {
            panic!("fewer ignore examples than the test reads");
        };
        for (key, prose) in [
            (format!(" {} ", exact.1), exact.2),
            (format!(" {} ", fuzzy.1), fuzzy.2),
            (jpg.pattern.to_string(), jpg.excluded),
            (node_modules.pattern.to_string(), node_modules.excluded),
        ] {
            let key_rect = crate::test_ui::painted_rect(&out, &key);
            let prose_rect = crate::test_ui::painted_rect(&out, prose);
            assert!(
                (key_rect.center().y - prose_rect.center().y).abs() < 1.0,
                "{:?} is centred at {} but {:?} at {}",
                key,
                key_rect.center().y,
                prose,
                prose_rect.center().y
            );
        }
    }

    /// The stripes themselves, so the rows are checked against what the user
    /// sees behind them rather than only against each other.
    #[test]
    fn table_text_sits_inside_its_stripe() {
        let (ctx, out) = render(1000.0, 4000.0);
        let faint = ctx.style().visuals.faint_bg_color;
        let stripes: Vec<egui::Rect> = crate::test_ui::painted_rects(&out)
            .into_iter()
            .filter(|r| r.fill == faint)
            .map(|r| r.rect)
            .collect();
        assert!(!stripes.is_empty(), "no striped rows painted");
        // Only every other row carries a stripe, so both of these are rows
        // egui paints one behind: the ranking table's fourth, the examples'
        // second.
        for prose in [super::RANKING[3].2, super::IGNORE_EXAMPLES[0].excluded] {
            let rect = crate::test_ui::painted_rect(&out, prose);
            let stripe = stripes
                .iter()
                .find(|s| s.contains(rect.center()))
                .unwrap_or_else(|| panic!("{:?} sits on no stripe: {:#?}", prose, stripes));
            assert!(
                stripe.y_range().contains(rect.top()) && stripe.y_range().contains(rect.bottom()),
                "{:?} spans {:?} but its stripe only {:?}",
                prose,
                rect.y_range(),
                stripe.y_range()
            );
        }
    }

    /// The near-miss column is half of what the examples teach, so the
    /// narrow layout has to keep it rather than dropping to pattern-only.
    #[test]
    fn the_stacked_examples_keep_both_halves_of_each_row() {
        let (_, out) = render(400.0, 6000.0);
        let painted = crate::test_ui::painted_text(&out).join("\n");
        for example in super::IGNORE_EXAMPLES {
            for expected in [example.pattern, example.excluded, example.kept] {
                assert!(painted.contains(expected), "no {:?}", expected);
            }
        }
    }
}
