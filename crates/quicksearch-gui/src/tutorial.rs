//! The first-start tour: a few pages explaining what QuickSearch indexes,
//! how results are ranked, and what the parts of the Search tab do.
//!
//! Shown once, to an installation that has never run before — `[ui]
//! tutorial_seen` is `Some(false)` only in a config file this version created,
//! so upgrading into this version does not summon it. The Help tab can bring
//! it back afterwards, which is also what keeps this from being write-only.

use crate::ui_util::{centered_modal, hint};

/// One page of the tour. Static text, so the pages are a table rather than a
/// match arm each.
struct Page {
    title: &'static str,
    /// Paragraphs. Rendered in order with a little space between them.
    body: &'static [&'static str],
    /// Rendered small and de-emphasised under the body — where to go, rather
    /// than what the thing is.
    pointer: Option<&'static str>,
}

const PAGES: &[Page] = &[
    Page {
        title: "Welcome to QuickSearch",
        body: &[
            "QuickSearch keeps an index of the folders you choose, and searches \
             as you type.",
            "By default only your user folder is indexed and searchable.",
            "Because the answers come from the index rather than from reading \
             your disk, results appear as fast as you can type, even across \
             hundreds of thousands of files.",
        ],
        pointer: Some("A quick tutorial for new users! Hit Skip to Exit."),
    },
    Page {
        title: "What is indexed",
        body: &[
            "Indexing is QuickSearch reading through your folders once and \
             remembering what it found, so that searching later is instant. It \
             runs on its own in the background and keeps up with changes as you \
             make them.",
            "QuickSearch never connects to the internet, and always respects your privacy. \
             QuickSearch can encrypt your index to make this remembered data more secure.",
            "These are the folders being indexed right now:",
        ],
        pointer: Some(
            "To index more locations, open the Manage Index tab and add a folder. \
             To set an index password, look near the bottom of the Settings tab.",
        ),
    },
    Page {
        title: "How results are ranked",
        body: &[
            "The best search matches come first (have the lowest rank). \
             Exact file-name matchs are best, then names that contain what you typed; then \
             files whose contents contain the search terms, the ones mentioning it most often \
             first; and last, files matched only by their full folder path.",
            "The coloured number in the Rank column is which of those tiers a \
             result came from: blue is a great match, red is a distant one. \
             Clicking a column heading sorts by something else instead.",
        ],
        pointer: Some("Right-click any column heading to choose which columns are shown."),
    },
    Page {
        title: "The status bar",
        body: &[
            "The line along the bottom of the window is what QuickSearch is \
             doing. While it is indexing it shows the phase, how far through it \
             is, and how fast; when it has nothing to do it shows how many files \
             are indexed.",
            "Searching works the whole time, including during that first indexing run, \
             but some files might not be shown in the results until the scan completes.",
        ],
        pointer: None,
    },
    Page {
        title: "Typos, and what a result can do",
        body: &[
            "Tick \"Fuzzy\"beside the search box to also match words with typos \
             in them — \"repot\" will find \"report\". It searches more \
             thoroughly, so it is a little slower; leave it off until you need \
             it.",
            "Right-click any result for more: open it, open the folder holding \
             it, copy its path, or build a filter that hides files like it from \
             future searches.",
        ],
        pointer: Some(
            "The ? button left of the search box lists the filters you can type \
             into a query, like type:Document or modified:>=2024-01-01.",
        ),
    },
    Page {
        title: "Duplicates",
        body: &[
            "The Duplicates tab looks for files across all indexed folders for identical copies. \
             They are shown grouped together, with the largest wasted space first.",
            "It is a quick way to find the same download sitting in three \
             places. QuickSearch only shows you the groups; deleting anything is \
             left to you.",
        ],
        pointer: Some(
            "For speed, files are compared by size and by how they begin (first 8KB). \
             This is not a guarantee of an exact match. You can right click a result to verify before you delete anything.",
        ),
    },
    Page {
        title: "Settings",
        body: &[
            "The Settings tab, at the right-hand end of the tab strip, is where \
             you can tweak and tune the software. Mouse over any of \
             the settings for a brief description of what they do.",
            "Most changes wait for the Apply & Save button at the bottom.",
            "QuickSearch is completely free for anyone to use. If you love it, please let your friends know about us!",
        ],
        pointer: None,
    },
];

/// The open tour.
pub struct Tutorial {
    page: usize,
}

impl Tutorial {
    pub fn new() -> Tutorial {
        Tutorial { page: 0 }
    }

    /// Render. `roots` is the live indexed-folder list, named on the page
    /// about indexing so the tour describes this installation rather than a
    /// generic one.
    /// Returns true once the tour is finished with — skipped or read to the
    /// end — which is the caller's cue to remember that and drop it.
    pub fn ui(&mut self, ctx: &egui::Context, roots: &[String]) -> bool {
        let page = &PAGES[self.page.min(PAGES.len() - 1)];
        let (first, last) = (self.page == 0, self.page + 1 == PAGES.len());
        let mut dismissed = false;
        let mut step: i64 = 0;

        centered_modal(ctx, page.title, |ui| {
            ui.set_max_width(520.0);
            for paragraph in page.body {
                ui.label(*paragraph);
                ui.add_space(6.0);
            }
            // The one page that shows live state rather than static text.
            if self.page == 1 {
                if roots.is_empty() {
                    ui.label(hint("No folders are indexed yet."));
                } else {
                    for root in roots {
                        ui.monospace(root);
                    }
                }
                ui.add_space(6.0);
            }
            if let Some(pointer) = page.pointer {
                ui.label(hint(pointer));
            }

            ui.add_space(10.0);
            ui.separator();
            // Three equal thirds rather than one row: it is the only layout
            // that puts Skip in the middle without measuring the buttons
            // either side of it, whose widths change with the page ("Next"
            // becoming "Finish") and with the counter's digits.
            ui.columns(3, |cols| {
                // A column lays its contents out *justified*, so a button put
                // straight into one is stretched to the full third. The other
                // two escape that by nesting their own layout; this one has to
                // say so.
                cols[0].with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                    if ui.add_enabled(!first, egui::Button::new("Back")).clicked() {
                        step = -1;
                    }
                });
                cols[1].vertical_centered(|ui| {
                    if ui.button("Skip").clicked() {
                        dismissed = true;
                    }
                });
                // `Align::Min`, not `Center`: a column is as tall as the rest
                // of the window, so centring in it drops the button a hundred
                // points below the two beside it.
                cols[2].with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    let p = crate::color::palette(ui.visuals().dark_mode);
                    let next = if last { "Finish" } else { "Next" };
                    if ui
                        .add(crate::ui_util::bordered_button(next, p.blue))
                        .clicked()
                    {
                        if last {
                            dismissed = true;
                        } else {
                            step = 1;
                        }
                    }
                    ui.label(hint(format!("{} of {}", self.page + 1, PAGES.len())));
                });
            });
        });

        // Applied after the closure so the page a frame rendered stays the page
        // its buttons were laid out for.
        if step != 0 {
            let next = self.page as i64 + step;
            self.page = next.clamp(0, PAGES.len() as i64 - 1) as usize;
        }
        dismissed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::{click_at, painted_text, raw_input};

    const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

    /// Two passes: an `egui::Window` is measured on its first frame and only
    /// placed on the next, so a single pass paints nothing to read back and
    /// has nothing at a known position to click.
    fn frame(
        ctx: &egui::Context,
        tour: &mut Tutorial,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, bool) {
        let roots = ["/home/me".to_string()];
        let _ = ctx.run(raw_input(SCREEN, Vec::new()), |ctx| {
            tour.ui(ctx, &roots);
        });
        let mut dismissed = false;
        let out = ctx.run(raw_input(SCREEN, events), |ctx| {
            dismissed = tour.ui(ctx, &roots);
        });
        (out, dismissed)
    }

    /// Every page has something to say, and says it.
    #[test]
    fn every_page_paints_its_own_title_and_body() {
        for (page, spec) in PAGES.iter().enumerate() {
            let ctx = crate::test_ui::ctx();
            let mut tour = Tutorial { page };
            let (out, _) = frame(&ctx, &mut tour, Vec::new());
            let painted = painted_text(&out);
            assert!(
                painted.iter().any(|t| t == spec.title),
                "page {page} painted no title: {painted:?}"
            );
            assert!(!spec.body.is_empty(), "page {page} has an empty body");
            assert!(
                painted
                    .iter()
                    .any(|t| t == &format!("{} of {}", page + 1, PAGES.len())),
                "page {page} did not say where it is: {painted:?}"
            );
        }
    }

    /// The page about indexing names *this* installation's folders, not a
    /// generic example.
    #[test]
    fn the_indexing_page_lists_the_configured_folders() {
        let ctx = crate::test_ui::ctx();
        let mut tour = Tutorial { page: 1 };
        let roots = ["/srv/projects".to_string()];
        let _ = ctx.run(raw_input(SCREEN, Vec::new()), |ctx| {
            tour.ui(ctx, &roots);
        });
        let out = ctx.run(raw_input(SCREEN, Vec::new()), |ctx| {
            tour.ui(ctx, &roots);
        });
        let painted = painted_text(&out);
        assert!(painted.iter().any(|t| t == "/srv/projects"), "{painted:?}");
    }

    /// The y of the footer row on `page`, found by walking down the middle
    /// column, which only Skip occupies. Wrapped text height moves the row
    /// from page to page, so it is probed rather than guessed.
    fn footer_y(ctx: &egui::Context, page: usize) -> f32 {
        for y in (150..600).step_by(2) {
            let mut t = Tutorial { page };
            let (_, dismissed) = frame(ctx, &mut t, click_at(egui::pos2(500.0, y as f32)));
            if dismissed {
                return y as f32;
            }
        }
        panic!("no Skip button down the middle of page {page}");
    }

    /// The stretch of x along the footer row that one button answers a click
    /// on — where it is, and how wide.
    #[derive(Debug, Clone, Copy)]
    struct Span {
        lo: f32,
        hi: f32,
    }

    impl Span {
        fn width(&self) -> f32 {
            self.hi - self.lo
        }
    }

    /// The three footer buttons, found by what clicking each one does: Back
    /// steps a page back, Next steps one forward, Skip dismisses.
    ///
    /// Must be run on a middle page — on the last page Finish and Skip both
    /// dismiss without moving, and on the first Back is disabled.
    fn footer_spans(ctx: &egui::Context, page: usize) -> [Option<Span>; 3] {
        assert!(page > 0 && page + 1 < PAGES.len(), "probe a middle page");
        let y = footer_y(ctx, page);
        let mut spans: [Option<Span>; 3] = [None; 3];
        for x in 150..850 {
            let mut t = Tutorial { page };
            let (_, dismissed) = frame(ctx, &mut t, click_at(egui::pos2(x as f32, y)));
            let x = x as f32;
            let which = if dismissed {
                1 // Skip
            } else if t.page + 1 == page {
                0 // Back
            } else if t.page == page + 1 {
                2 // Next
            } else {
                continue;
            };
            match &mut spans[which] {
                Some(span) => span.hi = x,
                slot => *slot = Some(Span { lo: x, hi: x }),
            }
        }
        spans
    }

    /// The footer reads Back, then Skip, then Next — and each does what its
    /// label says. Positions are probed rather than asserted against numbers:
    /// the window auto-sizes to the page's text, so the thirds move.
    ///
    /// The widths are the other half of it. `Ui::columns` lays a column out
    /// justified, so a button dropped straight into one comes out as wide as
    /// the whole third — which is what Back was until it was given a layout of
    /// its own. Two buttons with four-letter labels either side of the row
    /// have to come out the same size.
    #[test]
    fn the_footer_runs_back_then_skip_then_next_at_the_same_size() {
        let ctx = crate::test_ui::ctx();
        let [back, skip, next] = footer_spans(&ctx, 1);
        let back = back.expect("no Back button in the footer");
        let skip = skip.expect("no Skip button in the footer");
        let next = next.expect("no Next button in the footer");
        assert!(
            back.lo < skip.lo,
            "Back ({back:?}) is not left of Skip ({skip:?})"
        );
        assert!(
            skip.lo < next.lo,
            "Skip ({skip:?}) is not left of Next ({next:?})"
        );

        assert!(
            (back.width() - next.width()).abs() <= 2.0,
            "Back is {} wide against Next's {}",
            back.width(),
            next.width()
        );
        // Belt and braces on the shape of the bug: a stretched button fills
        // its third of a 520-point modal, which no four-letter label does.
        assert!(
            back.width() < 80.0,
            "Back is stretched to {} points",
            back.width()
        );
    }

    /// Back is disabled on the first page, so nothing in the footer can walk
    /// the tour off the front.
    #[test]
    fn the_first_page_cannot_go_back() {
        let ctx = crate::test_ui::ctx();
        let y = footer_y(&ctx, 0);
        for x in (150..850).step_by(4) {
            let mut t = Tutorial { page: 0 };
            let _ = frame(&ctx, &mut t, click_at(egui::pos2(x as f32, y)));
            assert!(
                t.page == 0 || t.page == 1,
                "clicking x={x} left page {}",
                t.page
            );
        }
    }

    /// Clicking anywhere in the button row, on any page; collects what fired.
    fn sweep(ctx: &egui::Context, tour: &mut Tutorial) -> Vec<usize> {
        let mut seen = Vec::new();
        for y in (200..500).step_by(4) {
            for x in (240..760).step_by(8) {
                let mut t = Tutorial { page: tour.page };
                let (_, dismissed) = frame(ctx, &mut t, click_at(egui::pos2(x as f32, y as f32)));
                if dismissed {
                    seen.push(t.page);
                }
            }
        }
        seen
    }

    /// Both ways out of the tour report the dismissal, so the flag gets set
    /// whichever the user takes. Positions depend on wrapped text height, so
    /// the button row is swept rather than guessed at — the same approach the
    /// confirmation modals' tests take.
    #[test]
    fn skip_and_finish_both_dismiss() {
        let ctx = crate::test_ui::ctx();

        // Skip is on every page.
        let mut tour = Tutorial { page: 0 };
        assert!(
            !sweep(&ctx, &mut tour).is_empty(),
            "Skip never fired on the first page"
        );

        // Finish only on the last, where it replaces Next.
        let mut tour = Tutorial {
            page: PAGES.len() - 1,
        };
        assert!(
            !sweep(&ctx, &mut tour).is_empty(),
            "Finish never fired on the last page"
        );
        let ctx = crate::test_ui::ctx();
        let mut tour = Tutorial {
            page: PAGES.len() - 1,
        };
        let (out, _) = frame(&ctx, &mut tour, Vec::new());
        assert!(
            painted_text(&out).contains(&"Finish".to_string()),
            "the last page still offers Next"
        );
    }
}
