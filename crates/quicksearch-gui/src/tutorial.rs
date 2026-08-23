//! The first-start tour. Shown once: `[ui] tutorial_seen` is `Some(false)`
//! only in a config file this version created, so upgrading does not summon
//! it. The Help tab can bring it back afterwards.
//!
//! Every page points at what it is talking about. Words wrapped in `[square
//! brackets]` in a page's prose are keywords: the brackets are markup,
//! stripped before anything is painted, and what was inside is coloured and
//! pulsed. The n-th keyword on a page pairs with the n-th entry of its
//! `spots` and the two share a colour, so "[search bar]" pulsing blue in the
//! prose is unambiguously about the box pulsing blue on screen. The pairing
//! is only as good as the two lists agreeing, which
//! `every_keyword_has_a_spot_to_point_at` holds them to.
//!
//! The window is deliberately not modal and not anchored: the user can drag
//! it off whatever it is covering, and can use the app underneath while it
//! is up.

use std::borrow::Cow;

use egui::text::{LayoutJob, TextFormat};

use crate::app::Tab;
use crate::spotlight::{self, Spot};
use crate::ui_util::hint;

struct Page {
    title: &'static str,
    /// A first paragraph built from live state, ahead of `body`.
    lead: Option<fn(&[String]) -> String>,
    body: &'static [&'static str],
    /// Rendered small under the body — where to go, not what the thing is.
    pointer: Option<&'static str>,
    /// Switched to as the page is entered, so the page's subject is on screen.
    tab: Option<Tab>,
    /// Paired in order with the keywords in `lead` then `body`.
    spots: &'static [Spot],
    /// Typed into the search box a character at a time, on entry.
    type_query: Option<&'static str>,
}

/// Field defaults for the pages that do not point anywhere.
const PLAIN: Page = Page {
    title: "",
    lead: None,
    body: &[],
    pointer: None,
    tab: None,
    spots: &[],
    type_query: None,
};

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
        pointer: Some(
            "A quick tutorial for new users! Drag this window by its title bar \
             if it covers something. Hit Skip to exit.",
        ),
        tab: Some(Tab::Search),
        ..PLAIN
    },
    Page {
        title: "Which folders are searched",
        lead: Some(folders_line),
        pointer: Some(
            "A folder you add is indexed in the background; everything already \
             in the index is left alone.",
        ),
        tab: Some(Tab::Manage),
        spots: &[Spot::IndexedFolderAdd],
        ..PLAIN
    },
    Page {
        title: "What is indexing?",
        body: &[
            "Indexing is QuickSearch reading through your folders once and \
             remembering what it found, so that searching later is instant. It \
             runs on its own in the background and keeps up with changes as you \
             make them.",
            "QuickSearch never connects to the internet, and always respects your privacy. \
             QuickSearch can encrypt your index to make this remembered data more secure.",
        ],
        pointer: Some("To set an index password, look near the bottom of the Settings tab."),
        tab: Some(Tab::Manage),
        ..PLAIN
    },
    Page {
        title: "Search: Input",
        body: &[
            "QuickSearch begins searching as soon as you start typing in the \
             [search bar]. It accepts words, filenames, file extensions, and \
             several other specifiers. Click the \"[?]\" block for full details.",
        ],
        pointer: Some(
            "Filters you can type into a query look like type:Document or \
             modified:>=2024-01-01.",
        ),
        tab: Some(Tab::Search),
        spots: &[Spot::SearchBar, Spot::QueryHelp],
        type_query: Some("QuickSearch"),
        ..PLAIN
    },
    Page {
        title: "How results are ranked",
        body: &[
            "The best search matches come first (have the lowest rank). \
             Exact file-name matches are best, then names that contain what you typed; then \
             files whose contents contain the search terms, the ones mentioning it most often \
             first; and last, files matched only by their full folder path.",
            "The coloured number in the [Rank] column is which of those tiers a \
             result came from: blue is a great match, red is a distant one. \
             Clicking a column heading sorts by something else instead.",
        ],
        pointer: Some("Right-click any column heading to choose which columns are shown."),
        tab: Some(Tab::Search),
        spots: &[Spot::RankColumn],
        ..PLAIN
    },
    Page {
        title: "The status bar",
        body: &[
            "The [status bar] along the bottom of the window is what QuickSearch \
             is doing. While it is indexing it shows the phase, how far through \
             it is, and how fast; when it has nothing to do it shows how many \
             files are indexed.",
            "Searching works the whole time, including during that first indexing run, \
             but some files might not be shown in the results until the scan completes.",
        ],
        tab: Some(Tab::Search),
        spots: &[Spot::StatusBar],
        ..PLAIN
    },
    Page {
        title: "Typos, and what a result can do",
        body: &[
            "Tick [Fuzzy] beside the search box to also match words with typos \
             in them — \"repot\" will find \"report\". It searches more \
             thoroughly, so it is a little slower; leave it off until you need \
             it.",
            "Right-click any result for more: open it, open the folder holding \
             it, copy its path, or build a filter that hides files like it from \
             future searches.",
        ],
        tab: Some(Tab::Search),
        spots: &[Spot::FuzzyToggle],
        ..PLAIN
    },
    Page {
        title: "Duplicates",
        body: &[
            "The [Duplicates] tab looks for files across all indexed folders for identical copies. \
             They are shown grouped together, with the largest wasted space first — or grouped \
             by file extension, if that is how you would rather work through them.",
            "It is a quick way to find the same download sitting in three \
             places. QuickSearch only shows you the groups; deleting anything is \
             left to you.",
        ],
        pointer: Some(
            "For speed, files are compared by size and by how they begin (first 8KB). \
             This is not a guarantee of an exact match. You can right click a result to verify before you delete anything.",
        ),
        tab: Some(Tab::Duplicates),
        spots: &[Spot::TabButton(Tab::Duplicates)],
        ..PLAIN
    },
    Page {
        title: "Settings",
        body: &[
            "The [Settings] tab, at the right-hand end of the tab strip, is where \
             you can tweak and tune the software. Mouse over any of \
             the settings for a brief description of what they do.",
            "Most changes wait for the Apply & Save button at the bottom.",
            "You can see this introduction again at any time from the [Help] tab \
             — the \"Show the introduction again\" button at the top.",
            "QuickSearch is completely free for anyone to use. If you love it, please let your friends know about us!",
        ],
        tab: Some(Tab::Settings),
        spots: &[
            Spot::TabButton(Tab::Settings),
            Spot::TabButton(Tab::Help),
        ],
        ..PLAIN
    },
];

/// One glow cycle, seconds.
const PULSE_PERIOD: f64 = 1.6;
/// How often the glow and the demonstration typing are stepped.
const ANIMATION_TICK: std::time::Duration = std::time::Duration::from_millis(50);
/// Seconds per character on a page that types into the search box.
const TYPE_INTERVAL: f64 = 0.07;
/// The tour's window keeps one id across pages: the title changes on every
/// Next, and an id derived from it would put the window back in the middle
/// each time, undoing wherever the user dragged it.
const WINDOW_ID: &str = "quicksearch-tutorial";

/// Whether the home folder is what is being indexed decides how the folders
/// page opens; a tour re-run from Help can find any set of roots at all.
fn folders_line(roots: &[String]) -> String {
    let trim = |p: &str| p.trim_end_matches(['/', '\\']).to_string();
    let home = quicksearch_core::platform::home_dir().map(|h| trim(&h.to_string_lossy()));
    match home {
        Some(home) if roots.iter().any(|root| trim(root) == home) => format!(
            "Your home folder ({home}) is indexed by default. To search other \
             locations, add them [here]."
        ),
        _ if roots.is_empty() => "No folders are indexed yet, so there is nothing to search. \
             Add the first one [here]."
            .to_string(),
        _ => format!(
            "These folders are indexed: {}. To search other locations, add them [here].",
            roots.join(", ")
        ),
    }
}

/// The page's prose, live parts and all, in the order it is painted.
fn paragraphs(page: &Page, roots: &[String]) -> Vec<Cow<'static, str>> {
    page.lead
        .map(|build| Cow::Owned(build(roots)))
        .into_iter()
        .chain(page.body.iter().copied().map(Cow::Borrowed))
        .collect()
}

/// Split prose into runs, `true` for the ones that were bracketed. An
/// unbalanced `[` is prose, not markup.
fn runs(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open + 1..].find(']').map(|i| open + 1 + i) else {
            break;
        };
        if open > 0 {
            out.push((&rest[..open], false));
        }
        out.push((&rest[open + 1..close], true));
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        out.push((rest, false));
    }
    out
}

/// 0 at the dimmest, 1 at the brightest.
fn pulse(time: f64) -> f32 {
    0.5 + 0.5 * (time * std::f64::consts::TAU / PULSE_PERIOD).sin() as f32
}

/// The n-th keyword's colour, shared with the n-th spot it points at.
fn accent(n: usize, dark_mode: bool) -> egui::Color32 {
    let p = crate::color::palette(dark_mode);
    [p.blue, p.green, p.orange][n % 3]
}

/// One paragraph, keywords coloured and tinted. `next_keyword` runs across
/// the whole page, not the paragraph, so it stays in step with `Page::spots`.
fn paragraph_job(ui: &egui::Ui, text: &str, next_keyword: &mut usize, pulse: f32) -> LayoutJob {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let dark_mode = ui.visuals().dark_mode;
    let mut job = LayoutJob::default();
    for (run, keyword) in runs(text) {
        let format = if keyword {
            let color = accent(*next_keyword, dark_mode);
            *next_keyword += 1;
            TextFormat {
                font_id: font_id.clone(),
                color,
                background: color.gamma_multiply(0.10 + 0.25 * pulse),
                ..Default::default()
            }
        } else {
            TextFormat {
                font_id: font_id.clone(),
                color: ui.visuals().text_color(),
                ..Default::default()
            }
        };
        job.append(run, 0.0, format);
    }
    job
}

/// Rings around a widget, painted into the *panel* layer: over the widget,
/// but under the tour's own window, which the user can drag aside rather
/// than have the glow drawn across.
fn glow(ctx: &egui::Context, rect: egui::Rect, color: egui::Color32, pulse: f32) {
    let painter = ctx.layer_painter(egui::LayerId::background());
    let strength = 0.45 + 0.55 * pulse;
    painter.rect_filled(rect.expand(2.0), 4.0, color.gamma_multiply(0.08 * strength));
    for ring in 0..3u8 {
        let grow = 2.0 + 3.0 * f32::from(ring);
        let alpha = (0.65 - 0.18 * f32::from(ring)) * strength;
        painter.rect_stroke(
            rect.expand(grow),
            4.0 + grow,
            egui::Stroke::new(2.0, color.gamma_multiply(alpha)),
            egui::StrokeKind::Outside,
        );
    }
}

/// The search box being filled in a character at a time.
struct Typing {
    text: &'static str,
    started: f64,
    emitted: usize,
}

impl Typing {
    /// The prefix typed by `now`, but only when it has grown. `SearchTab::seed`
    /// re-arms the debounce on every call, so re-emitting the same text every
    /// frame would hold the search off for as long as the page is up.
    fn advance(&mut self, now: f64) -> Option<String> {
        let total = self.text.chars().count();
        let due = (((now - self.started) / TYPE_INTERVAL).max(0.0) as usize).min(total);
        if due <= self.emitted {
            return None;
        }
        self.emitted = due;
        Some(self.text.chars().take(due).collect())
    }
}

/// What the tour asks the app to do after this frame.
#[derive(Default)]
pub struct TourActions {
    /// The tour is over — skipped or read to the end.
    pub dismissed: bool,
    pub goto_tab: Option<Tab>,
    pub set_query: Option<String>,
    pub focus_search: bool,
}

pub struct Tutorial {
    page: usize,
    /// The page whose arrival has already been acted on. Entering a page
    /// switches tabs once, not every frame: a user who clicks another tab
    /// mid-page is not dragged back.
    shown: Option<usize>,
    typing: Option<Typing>,
    /// The user has dragged the window, so where it sits is their business.
    moved: bool,
}

impl Tutorial {
    pub fn new() -> Tutorial {
        Tutorial {
            page: 0,
            shown: None,
            typing: None,
            moved: false,
        }
    }

    /// `roots` is the live indexed-folder list, which the folders page reads.
    pub fn ui(&mut self, ctx: &egui::Context, roots: &[String]) -> TourActions {
        let page = &PAGES[self.page.min(PAGES.len() - 1)];
        let (first, last) = (self.page == 0, self.page + 1 == PAGES.len());
        let mut actions = TourActions::default();
        let mut step: i64 = 0;
        let now = ctx.input(|i| i.time);

        if self.shown != Some(self.page) {
            self.shown = Some(self.page);
            actions.goto_tab = page.tab;
            self.typing = page.type_query.map(|text| Typing {
                text,
                started: now,
                emitted: 0,
            });
            if self.typing.is_some() {
                // From empty, with the caret in the box: what follows should
                // read as someone typing, not as a value appearing.
                actions.set_query = Some(String::new());
                actions.focus_search = true;
            }
        }
        if let Some(typing) = &mut self.typing {
            if let Some(typed) = typing.advance(now) {
                actions.set_query = Some(typed);
            }
        }

        let lit = pulse(now);
        let mut dismissed = false;
        let mut window = egui::Window::new(page.title)
            .id(egui::Id::new(WINDOW_ID))
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER);
        if !self.moved {
            // Re-centred every frame rather than positioned once: the window
            // is measured on its first frame and the viewport is not
            // necessarily its final size on ours, so a position chosen then
            // can be anywhere. Dropped the moment the user drags — the drag
            // delta is applied after this, so the first drag frame already
            // moves and the next one stops overriding.
            window = window.current_pos(ctx.screen_rect().center());
        }
        let shown = window.show(ctx, |ui| {
            ui.set_max_width(520.0);
            let mut keyword = 0usize;
            for paragraph in paragraphs(page, roots) {
                let job = paragraph_job(ui, &paragraph, &mut keyword, lit);
                ui.label(job);
                ui.add_space(6.0);
            }
            if let Some(pointer) = page.pointer {
                ui.label(hint(pointer));
            }

            ui.add_space(10.0);
            ui.separator();
            // Three equal thirds: the only layout that centres Skip without
            // measuring the buttons either side, whose widths change per page.
            ui.columns(3, |cols| {
                // A column lays out *justified*: a button put straight into
                // one is stretched to the full third.
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
                // `Align::Min`: a column is as tall as the window, so
                // centring drops the button far below the two beside it.
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
        if shown.is_some_and(|window| window.response.dragged()) {
            self.moved = true;
        }

        // The widgets this page names, in the colours its keywords were given.
        let dark_mode = ctx.style().visuals.dark_mode;
        for (n, spot) in page.spots.iter().enumerate() {
            if let Some(rect) = spotlight::rect(ctx, *spot) {
                glow(ctx, rect, accent(n, dark_mode), lit);
            }
        }
        if !page.spots.is_empty() || self.typing.is_some() {
            ctx.request_repaint_after(ANIMATION_TICK);
        }

        // After the closure, so the rendered page stays the one its buttons
        // were laid out for.
        if step != 0 {
            let next = self.page as i64 + step;
            self.page = next.clamp(0, PAGES.len() as i64 - 1) as usize;
        }
        if dismissed {
            // Leave the user on the tab they will actually work in, with a
            // box holding only what they typed themselves.
            actions.dismissed = true;
            actions.goto_tab = Some(Tab::Search);
            actions.set_query = Some(String::new());
        }
        actions
    }
}

#[cfg(test)]
mod tests;
