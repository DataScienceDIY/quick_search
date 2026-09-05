//! The Help tab: what QuickSearch is, how to start, how results are ranked
//! and what ends up in the index. Anything belonging to one control lives on
//! that control's hover tip (`tips.rs`); the query language lives in the
//! Search tab's syntax window; the complete technical reference stays in
//! README.md.

use crate::ui_util::hint;

/// Returns true when the "Show the introduction again" button was clicked.
pub fn ui(ui: &mut egui::Ui) -> bool {
    let mut replay = false;
    // Cap the column like a document page: a maximized window would
    // otherwise stretch every paragraph into one long line. Only ever a cap,
    // never a floor: `set_max_width` widens a `Ui` as readily as it narrows
    // one, and text laid out wider than the panel is *clipped* by the scroll
    // area rather than wrapped, so the ends of the lines simply vanish.
    //
    // Measured out here, on the panel: inside a vertical `ScrollArea` the
    // content `Ui` is free to be wider than the viewport, so its own
    // `available_width` is no guide to what will be visible. The bar's
    // allocation comes off the top, or the last few characters of every line
    // would sit under it (zero while the bars float, as they do by default).
    let column = (ui.available_width() - ui.spacing().scroll.allocated_width()).min(620.0);
    let scroll = egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            ui.set_max_width(column);

            ui.heading(egui::RichText::new("Welcome to QuickSearch").strong());
            ui.add_space(4.0);
            ui.label(
                "QuickSearch is a search engine for the files on this computer \
                 and for the text inside them. It reads through your folders \
                 once, remembers what it found, and then answers from what it \
                 remembers rather than by going back to the disk. That is why \
                 results appear as fast as you can type, across hundreds of \
                 thousands of files.",
            );
            ui.add_space(4.0);
            ui.label(
                "It never connects to the internet and nothing ever leaves this \
                 computer. What it remembers can be encrypted with a password, \
                 from the Settings tab.",
            );

            ui.add_space(6.0);
            ui.label("The primary home of this software is:");
            let link = |ui: &mut egui::Ui, url: &str| ui.hyperlink_to(url, url);
            link(ui, "https://quicksearch.karsttech.com");
            link(ui, "https://code.karsttech.com/jeremy/quick_search");
            ui.add_space(6.0);
            ui.label(
                "The code is also mirrored to GitHub for easier bug reporting \
                 and issue tracking:",
            );
            link(ui, "https://github.com/DataScienceDIY/quick_search");

            ui.add_space(6.0);
            if ui.button("Show the introduction again").clicked() {
                replay = true;
            }

            ui.add_space(6.0);
            ui.label(hint(
                "Where to look for what: hover any control for what that one \
                 control does and what to set it to. This tab covers the ideas \
                 behind them. The ? button beside the search box covers the \
                 query language. README.md is the complete reference.",
            ));

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("Getting started").strong());
            ui.add_space(4.0);
            ui.label(
                "1.  The first time QuickSearch runs it starts indexing your home \
                 folder on its own. The status bar along the bottom shows the \
                 progress, and searching already works while it runs, though a \
                 file it has not reached yet cannot appear until it does.",
            );
            ui.label(
                "2.  To search other places, open the Manage Index tab and add them \
                 to the folder list. Indexed folders are watched, so the index \
                 follows your files as they change, and adding one leaves \
                 everything already indexed alone.",
            );
            ui.label(
                "3.  Type in the search box on the Search tab. Results appear as \
                 you type, best matches first.",
            );
            ui.label(
                "4.  If the index grows larger than you would like, narrow it with \
                 the two filters under Content filters on the Manage Index tab. \
                 They are explained under What gets indexed, below.",
            );

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("Searching").strong());
            ui.add_space(4.0);
            ui.label("Plain words match file names, file contents, and paths:");
            ui.monospace("quarterly budget");
            ui.label(
                "Filters narrow the results and combine freely with the search \
                 words:",
            );
            ui.monospace("type:Document modified:>=2024-01-01 report");
            ui.label("The ? button left of the search box shows the full query syntax.");
            ui.add_space(6.0);
            ui.label(
                "•  A match in a file's name or path is highlighted in that \
                 column; a match in its contents shows a snippet of the \
                 surrounding text in the Content Match column, with the rest on \
                 hover.",
            );
            ui.label(
                "•  Right-click a result to open it, open its containing folder, \
                 or hide files like it from the results.",
            );
            ui.label(
                "•  Click a column header to sort by it; right-click any header to \
                 choose which columns are shown.",
            );

            ui.add_space(12.0);
            ranking_section(ui);

            ui.add_space(12.0);
            what_gets_indexed_section(ui);

            ui.add_space(12.0);
            duplicates_section(ui);

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("Updates").strong());
            ui.add_space(4.0);
            ui.label(
                "QuickSearch never sends anything off of your computer, so it \
                 never updates itself automatically.",
            );
            ui.label("To update, install the latest version yourself from:");
            link(ui, "https://quicksearch.karsttech.com");

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("The other tabs").strong());
            ui.add_space(4.0);
            let prose = prose_width(ui.available_width());
            egui::Grid::new("help-tabs")
                .num_columns(2)
                .spacing([CELL_SPACING, 5.0])
                .show(ui, |ui| {
                    let row = |ui: &mut egui::Ui, name: &str, what: &str| {
                        ui.strong(name);
                        cell(ui, prose, what);
                        ui.end_row();
                    };
                    row(
                        ui,
                        "Manage Index",
                        "indexing status and controls, the indexed folder list, \
                         and the filters that decide what is skipped",
                    );
                    row(
                        ui,
                        "Duplicates",
                        "files that are probably identical copies of each other, \
                         grouped; verify before deleting anything",
                    );
                    row(ui, "Logs", "warnings from issues during indexing");
                    row(
                        ui,
                        "Settings",
                        "everything QuickSearch can be told to do, in one \
                         place; hover any control for what it means",
                    );
                });

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("Terminal").strong());
            ui.add_space(4.0);
            ui.label("QuickSearch also searches straight from a terminal:");
            ui.monospace("quicksearch \"quarterly budget\"");
            ui.label(
                "On Windows use quicksearch-cli. Either way, --help lists all \
                 the flags.",
            );

            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                // The sentence is assembled from several widgets, so the
                // spacing between them has to come from the text itself.
                ui.spacing_mut().item_spacing.x = 0.0;
                let quiet = |text: &str| crate::ui_util::hint(text);
                ui.label(quiet(
                    "Building from source, configuration, query structuring and more \
                     are covered in ",
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
                    ui.label(quiet("README.md"));
                }
                ui.label(quiet("."));
            });
        });
    crate::ui_util::more_below_hint(ui, &scroll);
    replay
}

/// The tiers of `search::cascade`, collapsed to the ones a reader can act on:
/// its eleven ranks pair up (exact case, then any case) everywhere but the
/// fuzzy stages, and the pairing is an implementation detail here.
fn ranking_section(ui: &mut egui::Ui) {
    ui.heading(egui::RichText::new("How results are ranked").strong());
    ui.add_space(4.0);
    ui.label(
        "Every result is placed in a tier by where it matched, and the tiers \
         come out in this order. Within a tier, a match with the case you \
         typed comes before one that only matches ignoring case, and a file \
         mentioning your words more often comes before one mentioning them \
         once.",
    );
    ui.add_space(6.0);
    let prose = prose_width(ui.available_width());
    egui::Grid::new("help-ranking")
        .num_columns(2)
        .spacing([CELL_SPACING, 5.0])
        .striped(true)
        .show(ui, |ui| {
            // Each tier's chip wears the colour its results wear in the Rank
            // column, keyed by the *first* cascade stage the collapsed tier
            // covers (see the rank table in `search::cascade`): exact name
            // 1–2, name contains 3–4, text inside 5–6, fuzzy 7–8, path 9–11.
            let row = |ui: &mut egui::Ui, stage: u8, tier: &str, what: &str| {
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
            };
            row(
                ui,
                1,
                "Exact name",
                "the file is called exactly what you typed",
            );
            row(
                ui,
                3,
                "Name contains",
                "what you typed appears somewhere in the file's name",
            );
            row(
                ui,
                5,
                "Text inside",
                "the words are in the file's contents, most mentions first",
            );
            row(
                ui,
                7,
                "Close spelling",
                "a name or some text within a typo or two of what you typed, \
                 only while Fuzzy is ticked",
            );
            row(
                ui,
                9,
                "Path only",
                "nothing in the name or the text matched, but a folder along \
                 the way did",
            );
        });
    ui.add_space(6.0);
    ui.label(
        "The Rank column carries the tier a result came from, coloured from \
         blue for a close match to red for a distant one. Sorting by any other \
         column sets that ordering aside until you sort by Rank again.",
    );
    ui.label(hint(
        "Results stream in while a search runs and are only ever added to the \
         end, so the list never reshuffles under you as you read it.",
    ));
}

/// The two Manage Index filters, side by side, because the confusion they
/// cause is about which one keeps a file out of the index (ignore patterns)
/// and which one only stops its text being read (the whitelist).
fn what_gets_indexed_section(ui: &mut egui::Ui) {
    ui.heading(egui::RichText::new("What gets indexed").strong());
    ui.add_space(4.0);
    ui.label(
        "Everything inside your indexed folders, minus whatever the two \
         filters on the Manage Index tab take out. The filters do different \
         jobs: one decides which files exist in the index at all, the other \
         decides which of them have their text read.",
    );

    ui.add_space(8.0);
    ui.strong("Ignore patterns: files and folders never indexed");
    ui.add_space(4.0);
    ui.label(
        "A pattern is compared against file and folder names, and against \
         whole paths. It is never compared against what is inside a file, so \
         nothing is excluded for the words it contains. A file kept out this \
         way is gone from the index completely, contents included.",
    );
    ui.add_space(4.0);
    ui.label(
        "•  A pattern with no slash in it is a name: it matches any file or \
         folder called that, anywhere under your indexed folders, and it has \
         to match the whole name. Excluding an extension therefore needs a \
         wildcard.",
    );
    ui.label(
        "•  A pattern with a slash in it is a path: it is matched against the \
         whole path of a file or folder, and takes out everything underneath \
         it.",
    );
    ui.label("•  * stands for any run of characters, including none; ? for exactly one.");
    ui.add_space(6.0);
    ignore_examples(ui);
    ui.add_space(6.0);
    ui.label(hint(
        "A path pattern has to match from the start, so Vacation/Diary on its \
         own matches nothing: the leading */ is what lets it find that folder \
         wherever it sits. Whether case matters follows the filesystem, so \
         *diary* also catches Diary on Windows and macOS, but not on Linux.",
    ));
    ui.label(hint(
        "Adding a pattern takes out the entries it matches. Deleting a pattern \
         from the list is what brings those files back, at the next indexing \
         run; deleting the files it matched is never something QuickSearch does.",
    ));

    ui.add_space(8.0);
    ui.strong("Full-text extensions whitelist: which files have their text read");
    ui.add_space(4.0);
    ui.label(
        "This one limits contents only. Files it leaves out are still indexed \
         and still turn up in results by their name and their path; what you \
         lose is finding them by the words inside them.",
    );
    ui.add_space(4.0);
    ui.label(
        "•  Empty, which is how it starts, means QuickSearch reads the text of \
         every file type it understands.",
    );
    ui.label(
        "•  To narrow it, enter one extension per line, the leading dot \
         optional. Listing only txt, md and pdf keeps the stored text small \
         and focused on documents, while every other file stays findable by \
         name.",
    );
    ui.label(
        "•  A list that has anything in it also leaves out files with no \
         extension, such as Makefile or README. Add the line (none) to include \
         them.",
    );
    ui.label("•  Anything after a # is a comment, so a line can be switched off in place.");
}

/// What a duplicate group is and is not. Written out here because the tab
/// itself has room for the caution and not for the reasoning, and because the
/// question it answers — "it says these differ, but they look the same" — is
/// the one the feature reliably provokes.
fn duplicates_section(ui: &mut egui::Ui) {
    ui.heading(egui::RichText::new("Duplicates").strong());
    ui.add_space(4.0);
    ui.label(
        "The Duplicates tab groups files that look like copies of each other, \
         biggest waste first, so the space worth reclaiming is at the top. \
         Nothing there is ever deleted or moved for you; the tab only shows \
         you the groups.",
    );
    ui.add_space(4.0);
    ui.label(
        "A group is a strong suspicion, not a verdict. Indexing reads each \
         file's size and its first few kilobytes and hashes those, and that \
         hash is what puts two files in a group — the rest of the file was \
         never read. Right-click a group and choose Verify copies are \
         identical to read all of it and compare every byte, which is the \
         answer to have before deleting anything.",
    );

    ui.add_space(8.0);
    ui.strong("When the verification says two files differ and they look the same");
    ui.add_space(4.0);
    ui.label(
        "They do differ, in bytes you never see. PDFs and Office documents \
         carry the date they were created and last modified, a document ID, \
         and a revision number, all stored inside the file itself. Two \
         invoices printed from one template, or one document saved twice, \
         are different files on disk however identical they look on screen.",
    );
    ui.label(hint(
        "The report says which byte disagreed and how big the file was. A \
         difference in the first few hundred bytes of a document is almost \
         always that bookkeeping; one in the middle of a large file is not.",
    ));

    ui.add_space(8.0);
    ui.strong("Groups you do not want to be shown again");
    ui.add_space(4.0);
    ui.label(
        "•  Hide this group, from the right-click menu, drops one group for \
         good — the group that is not really a duplicate at all. It is \
         remembered by content, so it stays hidden when those files are \
         renamed or moved, and comes back if they are edited. Show hidden \
         lists them again and Clear forgets the lot.",
    );
    ui.label(
        "•  Exclude leaves out whole sets of files: the box above the list \
         takes the same patterns as the ignore filters, and the right-click \
         menu offers this file's type and this file's folder. That is the one \
         for a backup folder that is meant to hold copies.",
    );
    ui.label(hint(
        "Neither hides anything from search. An excluded file is still \
         indexed, still found by name and still found by its contents; it is \
         only left out of this one listing.",
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
/// is not. Inside a [`crate::ui_util::stable_section`] because the two
/// layouts allocate different numbers of widgets, and a resize across the
/// threshold would otherwise rename everything below them.
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

/// A prose cell in one of this tab's tables, laid out in a child `Ui` of
/// exactly `width`.
///
/// Two things force the explicit width. Grid cells default to
/// `TextWrapMode::Extend`, which lays a long cell out past the panel and, far
/// worse, *widens the `Ui`* it was drawn in, so every paragraph below the
/// table wraps to that width and is then clipped by the scroll area. Wrapping
/// the cell instead fixes that but breaks the other way: a wrapped `Label`
/// reports its narrowest possible width as what it wants, and `Grid` sizes
/// the column to that, squeezing prose into a two-word ribbon. A child `Ui`
/// of a width we chose settles both. Pinned by
/// `tests::a_window_narrower_than_the_column_reflows_rather_than_clipping`.
fn cell(ui: &mut egui::Ui, width: f32, text: impl Into<egui::WidgetText>) {
    ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
        ui.add(egui::Label::new(text).wrap());
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
    /// The Help tab is the largest block of prose in the app and the only tab
    /// with no other test, which makes it the widest net for the one thing
    /// dropping egui's emoji faces could break: a character with no glyph,
    /// painted as `◻`.
    #[test]
    fn the_help_tab_paints_no_missing_glyphs() {
        let ctx = crate::test_ui::ctx();
        let input = crate::test_ui::raw_input(egui::vec2(1000.0, 900.0), vec![]);
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                super::ui(ui);
            });
        });
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
        let ctx = crate::test_ui::ctx();
        let input = crate::test_ui::raw_input(egui::vec2(1000.0, 4000.0), vec![]);
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                super::ui(ui);
            });
        });
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
    /// pre-clip, so this measures the layout rather than the paint.
    #[test]
    fn a_window_narrower_than_the_column_reflows_rather_than_clipping() {
        // 640 is the smallest window the app allows, and the UI scale
        // divides it: 400 is roughly that window at 1.6x, and 250 is it at
        // the 2.5x ceiling — the narrowest layout the app can produce.
        for width in [250.0_f32, 320.0, 400.0, 480.0, 560.0, 620.0] {
            let ctx = crate::test_ui::ctx();
            let input = crate::test_ui::raw_input(egui::vec2(width, 6000.0), vec![]);
            let out = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    super::ui(ui);
                });
            });
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
    /// cascade stage — the chips exist to demonstrate the blue→red ramp the
    /// paragraph under the table describes.
    #[test]
    fn the_ranking_tiers_wear_the_rank_colors() {
        let ctx = crate::test_ui::ctx();
        let input = crate::test_ui::raw_input(egui::vec2(1000.0, 4000.0), vec![]);
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                super::ui(ui);
            });
        });
        // A RichText background is a section format in the galley, not a
        // separate rect shape.
        let mut backgrounds = Vec::new();
        for clipped in &out.shapes {
            if let egui::epaint::Shape::Text(text) = &clipped.shape {
                for section in &text.galley.job.sections {
                    backgrounds.push(section.format.background);
                }
            }
        }
        for stage in [1u8, 3, 5, 7, 9] {
            let color = crate::color::rank_tier_color(stage);
            assert!(
                backgrounds.contains(&color),
                "no chip painted in stage {}'s colour {:?}",
                stage,
                color
            );
        }
    }

    /// The near-miss column is half of what the examples teach, so the
    /// narrow layout has to keep it rather than dropping to pattern-only.
    #[test]
    fn the_stacked_examples_keep_both_halves_of_each_row() {
        let ctx = crate::test_ui::ctx();
        let input = crate::test_ui::raw_input(egui::vec2(400.0, 6000.0), vec![]);
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                super::ui(ui);
            });
        });
        let painted = crate::test_ui::painted_text(&out).join("\n");
        for example in super::IGNORE_EXAMPLES {
            for expected in [example.pattern, example.excluded, example.kept] {
                assert!(painted.contains(expected), "no {:?}", expected);
            }
        }
    }
}
