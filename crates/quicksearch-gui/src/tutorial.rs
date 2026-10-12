//! The first-start tour. Shown once: `[ui] tutorial_seen` is `Some(false)`
//! only in a config file this version created, so upgrading does not summon
//! it. The Help tab can bring it back afterwards.
//!
//! `[bracketed]` words in a page's prose are keywords: the brackets are
//! stripped, and the n-th keyword is painted in the same colour as the glow
//! around the n-th entry of `spots`. The window is not modal or anchored, so
//! the user can drag it aside and keep using the app.
//!
//! Nothing here animates once a page has settled: the GUI can be
//! software-rendered, where every repaint of an idle window costs real CPU.
//! A page asks for one frame as it is entered and, on the page that types,
//! one per character until the query is typed.

use std::borrow::Cow;
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};

use crate::app::Tab;
use crate::spotlight::{self, Spot};
use crate::ui_util::hint;

struct Page {
    title: &'static str,
    /// A first paragraph built from live state, ahead of `body`.
    lead: Option<fn(&[String]) -> String>,
    body: &'static [&'static str],
    /// Rendered small under the body: where to go, not what the thing is.
    pointer: Option<&'static str>,
    /// Switched to as the page is entered, so the page's subject is on screen.
    tab: Tab,
    /// Paired in order with the keywords in `lead` then `body`.
    spots: &'static [Spot],
    /// Typed into the search box a character at a time, on entry.
    type_query: Option<&'static str>,
    /// A control of the page's own, under its prose.
    extra: Option<Extra>,
}

/// What a page offers in the tour's own window. Most pages point at a widget
/// in the app instead; these two are setup the user can do from here.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Extra {
    /// The zoom slider, on the welcome page.
    Scale,
    /// The command to bind to a key, with Copy and a way to the desktop's
    /// keyboard settings.
    Shortcut,
}

/// Field defaults: a Search tab page that points at nothing.
const PLAIN: Page = Page {
    title: "",
    lead: None,
    body: &[],
    pointer: None,
    tab: Tab::Search,
    spots: &[],
    type_query: None,
    extra: None,
};

const PAGES: &[Page] = &[
    Page {
        title: "Welcome to QuickSearch",
        body: &[
            "QuickSearch is a search engine for your local files and their contents.",
            "By default, only your user folder is indexed and searchable.",
            "Answers come from the index rather than the disk, so results \
             appear as you type, even across hundreds of thousands of files.",
        ],
        pointer: Some(
            "A short tour for new users. Drag this window by its title bar if it \
             is in the way, or click Skip to leave.",
        ),
        extra: Some(Extra::Scale),
        ..PLAIN
    },
    Page {
        title: "Open QuickSearch hotkey",
        body: &[
            "Want a key that brings QuickSearch up from anywhere? Click the \
             button below and press the keys; it works at once.",
            "QuickSearch doesn't run in the background, so once closed the key \
             can't reach it. To have the key start it too, bind the command \
             below in your desktop's keyboard settings.",
        ],
        pointer: Some("Both of these are on the Settings tab too, under Interface."),
        extra: Some(Extra::Shortcut),
        ..PLAIN
    },
    Page {
        title: "Which folders are searched",
        lead: Some(folders_line),
        pointer: Some(
            "A folder you add is indexed in the background; everything already \
             in the index is left alone.",
        ),
        tab: Tab::Manage,
        spots: &[Spot::IndexedFolderAdd],
        ..PLAIN
    },
    Page {
        title: "What is indexing?",
        body: &[
            "Indexing is QuickSearch reading your folders once and remembering \
             what it found, so later searches are instant. It runs in the \
             background and keeps up as your files change.",
            "You decide what gets read: [ignore patterns] keep files and folders \
             out of the index entirely, and the [extensions whitelist] limits \
             which file types have their text read. Both are under Content \
             filters on this tab.",
            "QuickSearch never connects to the internet, and it can encrypt the \
             index with a password.",
        ],
        pointer: Some("The password setting is near the bottom of the Settings tab."),
        tab: Tab::Manage,
        spots: &[Spot::IgnorePatterns, Spot::ExtWhitelist],
        ..PLAIN
    },
    Page {
        title: "Search: Input",
        body: &[
            "QuickSearch searches as you type in the [search bar]. It takes \
             words, file names, extensions and filters; click the \"[?]\" \
             button for the full syntax.",
        ],
        pointer: Some("Filters look like type:Document or modified:>=2024-01-01."),
        spots: &[Spot::SearchBar, Spot::QueryHelp],
        type_query: Some("QuickSearch"),
        ..PLAIN
    },
    Page {
        title: "How results are ranked",
        body: &[
            "The best matches come first, with the lowest rank: exact file \
             names, then names containing what you typed, then files whose \
             contents contain it (most mentions first), and last, files matched \
             only by a folder in their path.",
            "The coloured number in the [Rank] column shows a result's tier: \
             blue is a close match, red a distant one. Click a column heading \
             to sort by something else.",
        ],
        pointer: Some("Right-click any column heading to choose the columns shown."),
        spots: &[Spot::RankColumn],
        ..PLAIN
    },
    Page {
        title: "The status bar",
        body: &[
            "The [status bar] along the bottom shows what QuickSearch is \
             doing: while indexing, the phase, progress and speed; when idle, \
             how many files are indexed.",
            "Searching works throughout, even during the first indexing run, \
             though files not yet reached won't appear until the scan gets to \
             them.",
        ],
        spots: &[Spot::StatusBar],
        ..PLAIN
    },
    Page {
        title: "Fuzzy finding",
        body: &[
            "Tick [Fuzzy] beside the search box to also match words with typos, \
             so \"repot\" finds \"report\". The extra searching takes a little \
             longer, but exact matches arrive just as fast and come first.",
            "Right-click any result to open it or its folder, copy its path, or \
             build a filter that hides files like it from future searches.",
        ],
        spots: &[Spot::FuzzyToggle],
        ..PLAIN
    },
    Page {
        title: "Duplicates",
        body: &[
            "The [Duplicates] tab finds copies of the same file across your \
             indexed folders, grouped with the most wasted space first, or by \
             extension if you prefer.",
            "It is a quick way to find the same download sitting in three \
             places. It only shows the groups; deleting is up to you.",
            "For speed, files are matched on size and their first and last \
             few kilobytes, so a group is a strong suspicion, not a certainty. \
             Right-click a group and choose \"Verify copies are identical\" to \
             compare every byte before you delete anything.",
        ],
        pointer: Some(
            "From the same menu you can hide a group for good, or exclude a \
             folder that is meant to hold copies.",
        ),
        tab: Tab::Duplicates,
        spots: &[Spot::TabButton(Tab::Duplicates)],
        ..PLAIN
    },
    Page {
        title: "Settings",
        body: &[
            "The [Settings] tab, at the right end of the tab strip, is where you \
             tune QuickSearch. Hover any setting for a short description.",
            "Most changes wait for the Apply & Save button at the bottom.",
            "To see this introduction again, use the \"Show the introduction \
             again\" button at the top of the [Help] tab.",
            "QuickSearch is free for anyone to use. If you like it, please tell \
             your friends!",
        ],
        tab: Tab::Settings,
        spots: &[Spot::TabButton(Tab::Settings), Spot::TabButton(Tab::Help)],
        ..PLAIN
    },
];

/// Seconds per character on a page that types into the search box, and how
/// often that page repaints until the last one is typed.
const TYPE_INTERVAL: f64 = 0.07;
/// The tour's window keeps one id across pages: the title changes on every
/// Next, and an id derived from it would put the window back in the middle
/// each time, undoing wherever the user dragged it.
const WINDOW_ID: &str = "quicksearch-tutorial";

/// Whether the home folder is what is being indexed decides how the folders
/// page opens; a tour re-run from Help can find any set of roots at all.
fn folders_line(roots: &[String]) -> String {
    let trim = |p: &str| quicksearch_core::file_handling::trim_trailing_separators(p).to_string();
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

/// The n-th keyword's colour, shared with the n-th spot it points at.
fn accent(n: usize, dark_mode: bool) -> egui::Color32 {
    let p = crate::color::palette(dark_mode);
    [p.blue, p.green, p.orange][n % 3]
}

/// One paragraph, keywords coloured and tinted. `next_keyword` runs across
/// the whole page, not the paragraph, so it stays in step with `Page::spots`.
fn paragraph_job(ui: &egui::Ui, text: &str, next_keyword: &mut usize) -> LayoutJob {
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
                background: color.gamma_multiply(0.35),
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

/// The tour's note size: 2 points up on `hint`'s `Small`, which is a touch
/// fine for a first-time user's first window, and still below Body so a note
/// reads as an aside.
fn note(ui: &egui::Ui, text: impl Into<egui::RichText>) -> egui::RichText {
    hint(text).size(egui::TextStyle::Small.resolve(ui.style()).size + 2.0)
}

/// The live state the tour's own controls read, and the flag the shortcut
/// capture keeps between frames.
struct Live<'a> {
    /// `ctx.zoom_factor()`, read before the window is laid out.
    zoom: f32,
    /// The slider's position while it differs from `zoom`: chosen, not yet
    /// applied. `None` once Apply is clicked or nothing is pending.
    staged_scale: &'a mut Option<f32>,
    /// The shortcut in force, as the config spells it.
    hotkey: &'a str,
    capturing_hotkey: &'a mut bool,
}

/// The page's own control, under its prose. What they were used for goes
/// back through `actions`: the config is the app's to write, not the tour's.
fn extra_ui(ui: &mut egui::Ui, extra: Extra, live: &mut Live, actions: &mut TourActions) {
    ui.add_space(6.0);
    ui.separator();
    match extra {
        Extra::Scale => {
            // Staged until Apply, like the Settings tab's slider: applying
            // mid-drag rescales the slider under the pointer, so the handle
            // chases its own tail and the value cannot be chosen.
            let mut scale = live.staged_scale.unwrap_or(live.zoom);
            ui.horizontal(|ui| {
                ui.label("UI scale");
                // Scoped to this row, which is the whole of its use.
                ui.spacing_mut().slider_width = 220.0;
                ui.add(
                    egui::Slider::new(&mut scale, crate::app::SCALE_RANGE)
                        .step_by(0.05)
                        .fixed_decimals(2),
                );
                let pending = (scale - live.zoom).abs() > f32::EPSILON;
                *live.staged_scale = pending.then_some(scale);
                if ui.add_enabled(pending, egui::Button::new("Apply")).clicked() {
                    actions.set_scale = Some(scale);
                    *live.staged_scale = None;
                }
            });
            ui.label(note(
                ui,
                "Makes everything larger or smaller. You can change it later on \
                 the Settings tab.",
            ));
        }
        // Both of the Settings tab's shortcut controls, and the same two
        // widgets rather than a second pair that could drift from them: the
        // button that claims a key while QuickSearch is running, and under
        // it the command a desktop shortcut binds to start it.
        Extra::Shortcut => {
            let mut setting = live.hotkey.to_string();
            ui.horizontal(|ui| {
                ui.label("Search shortcut");
                crate::settings_tab::hotkey_edit(ui, &mut setting, live.capturing_hotkey);
            });
            if setting != live.hotkey {
                actions.set_hotkey = Some(setting);
            }
            crate::settings_tab::shortcut_note(ui, live.hotkey);
        }
    }
}

/// Rings around a widget, painted into the *panel* layer: over the widget,
/// but under the tour's own window, which the user can drag aside rather
/// than have the glow drawn across.
fn glow(ctx: &egui::Context, rect: egui::Rect, color: egui::Color32) {
    let painter = ctx.layer_painter(egui::LayerId::background());
    painter.rect_filled(rect.expand(2.0), 4.0, color.gamma_multiply(0.08));
    for ring in 0..3u8 {
        let grow = 2.0 + 3.0 * f32::from(ring);
        let alpha = 0.65 - 0.18 * f32::from(ring);
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

/// Whether the search box holds only what a page typed into it, or the start
/// of it (a Skip mid-typing). Read from the box itself, so a query the user
/// typed over the demonstration is theirs, whichever page they leave from.
fn holds_demo(query: &str) -> bool {
    !query.is_empty()
        && PAGES
            .iter()
            .filter_map(|p| p.type_query)
            .any(|demo| demo.starts_with(query))
}

/// What the tour asks the app to do after this frame.
#[derive(Default)]
pub struct TourActions {
    /// The tour is over: skipped or read to the end.
    pub dismissed: bool,
    pub goto_tab: Option<Tab>,
    pub set_query: Option<String>,
    pub focus_search: bool,
    /// A new UI scale from the welcome page's slider, sent only by its Apply
    /// button (see `extra_ui`), to apply and save.
    pub set_scale: Option<f32>,
    /// A shortcut captured on the shortcut page, to register and save.
    pub set_hotkey: Option<String>,
}

/// A footer click. Applied after the window, so the page rendered stays the
/// one its buttons were laid out for.
enum Nav {
    Back,
    Next,
    /// Skip, or Finish on the last page.
    Dismiss,
}

#[derive(Default)]
pub struct Tutorial {
    page: usize,
    /// The page whose arrival has already been acted on. Entering a page
    /// switches tabs once, not every frame: a user who clicks another tab
    /// mid-page is not dragged back.
    entered_page: Option<usize>,
    typing: Option<Typing>,
    /// The user has dragged the window, so where it sits is their business.
    user_dragged: bool,
    /// The shortcut button is armed and the next key combination is the
    /// answer: the Settings tab's own capture, with its own flag.
    capturing_hotkey: bool,
    /// The welcome page's scale slider, between moving and Apply.
    staged_scale: Option<f32>,
}

impl Tutorial {
    /// Arrival on the current page: its tab, and its typing from empty. The
    /// tab switches after this frame's panels are drawn, and its widgets mark
    /// the spots as they lay out, so one more frame paints both without
    /// waiting for input.
    fn enter_page(
        &mut self,
        ctx: &egui::Context,
        page: &Page,
        now: f64,
        actions: &mut TourActions,
    ) {
        self.entered_page = Some(self.page);
        actions.goto_tab = Some(page.tab);
        ctx.request_repaint();
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

    /// `roots` is the live indexed-folder list, which the folders page reads;
    /// `hotkey` is the shortcut in force, which the shortcut page both shows
    /// and rebinds; `query` is what the search box holds, which a dismissal
    /// clears only if the tour typed it.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        roots: &[String],
        hotkey: &str,
        query: &str,
    ) -> TourActions {
        let page = &PAGES[self.page.min(PAGES.len() - 1)];
        let (first, last) = (self.page == 0, self.page + 1 == PAGES.len());
        let mut actions = TourActions::default();
        let mut nav = None;
        let now = ctx.input(|i| i.time);

        if self.entered_page != Some(self.page) {
            self.enter_page(ctx, page, now, &mut actions);
        }
        if let Some(typing) = &mut self.typing {
            if let Some(typed) = typing.advance(now) {
                actions.set_query = Some(typed);
            }
        }

        // Read before the window: `set_zoom_factor` stores the new value at
        // once, so the slider reads back what it asked for on the next frame
        // rather than snapping to the old value mid-drag. It also follows an
        // ad-hoc Ctrl +/- zoom, which is the size the user is looking at.
        let zoom = ctx.zoom_factor();
        let mut window = egui::Window::new(page.title)
            .id(egui::Id::new(WINDOW_ID))
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER);
        if !self.user_dragged {
            // Re-centre every frame until the user drags: the viewport may
            // not have its final size on the first frame, so a one-off
            // position can land anywhere.
            window = window.current_pos(ctx.screen_rect().center());
        }
        let shown = window.show(ctx, |ui| {
            ui.set_max_width(520.0);
            let mut keyword = 0usize;
            for paragraph in paragraphs(page, roots) {
                let job = paragraph_job(ui, &paragraph, &mut keyword);
                ui.label(job);
                ui.add_space(6.0);
            }
            // The page's control, then its note, which may mention it.
            if let Some(extra) = page.extra {
                let mut live = Live {
                    zoom,
                    staged_scale: &mut self.staged_scale,
                    hotkey,
                    capturing_hotkey: &mut self.capturing_hotkey,
                };
                extra_ui(ui, extra, &mut live, &mut actions);
            }
            if let Some(pointer) = page.pointer {
                ui.label(note(ui, pointer));
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
                        nav = Some(Nav::Back);
                    }
                });
                cols[1].vertical_centered(|ui| {
                    if ui.button("Skip").clicked() {
                        nav = Some(Nav::Dismiss);
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
                        nav = Some(if last { Nav::Dismiss } else { Nav::Next });
                    }
                    ui.label(hint(format!("{} of {}", self.page + 1, PAGES.len())));
                });
            });
        });
        if shown.is_some_and(|window| window.response.dragged()) {
            self.user_dragged = true;
        }

        // The widgets this page names, in the colours its keywords were given.
        let dark_mode = ctx.style().visuals.dark_mode;
        for (n, spot) in page.spots.iter().enumerate() {
            if let Some(rect) = spotlight::rect(ctx, *spot) {
                glow(ctx, rect, accent(n, dark_mode));
            }
        }
        if self
            .typing
            .as_ref()
            .is_some_and(|t| t.emitted < t.text.chars().count())
        {
            ctx.request_repaint_after(Duration::from_secs_f64(TYPE_INTERVAL));
        }

        let last_page = PAGES.len() - 1;
        match nav {
            Some(Nav::Back) => self.page = self.page.saturating_sub(1).min(last_page),
            Some(Nav::Next) => self.page = (self.page + 1).min(last_page),
            Some(Nav::Dismiss) => {
                // Leave the user on the tab they will actually work in, with a
                // box holding only what they typed themselves. Decided here,
                // over anything typed this frame: a box the user has taken
                // over must not get the demonstration back.
                actions.dismissed = true;
                actions.goto_tab = Some(Tab::Search);
                actions.set_query = holds_demo(query).then(String::new);
            }
            None => {}
        }
        actions
    }
}

#[cfg(test)]
mod tests;
