//! The Search tab: query strip, streaming results table, snippet
//! preview, context menu, ignore-filter dialog, and syntax help.

use std::time::Instant;

use egui::text::{LayoutJob, TextFormat};
use egui_extras::{Column, TableBuilder};
use quicksearch_core::search::{SearchHit, SearchUpdate};
use quicksearch_core::snippet::Snippet;

use crate::format::{fmt_elapsed, fmt_mtime, human_size};
use crate::platform;
use crate::ui_util::middle_elide;

/// Width of the query strip's status slot, in points. Wide enough for the
/// longest query time `fmt_elapsed` produces, and held whether the slot is
/// showing the spinner, a time or nothing, so the query box stays put.
const STATUS_SLOT_WIDTH: f32 = 52.0;

/// How long the old results take to clear: a plain dip to nothing, with no
/// wipe. Very short on purpose — the swap waits for it, so it is latency in
/// front of every new result set, and there is nothing to look at on the
/// way out anyway.
const FADE_OUT_SECS: f32 = 0.15;
/// How long the new results take to wipe in. It can afford to be five times
/// as long, because the reveal starts at the top of the table — the first
/// hits are readable within a couple of frames either way.
const FADE_IN_SECS: f32 = 0.50;
/// The fraction of the reveal over which the section-wide opacity climbs to
/// full. The rest is carried by the wipe alone, so rows the edge has
/// already uncovered sit at full strength instead of dimming along with the
/// ones still to come.
const FADE_ALPHA_SPAN: f32 = 0.50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Rank,
    Name,
    Path,
    Size,
    Modified,
}

pub struct IgnoreDialog {
    pub source_path: String,
    /// `*.{ext}`, when the file has an extension.
    pub ext_pattern: Option<String>,
    pub name_pattern: String,
    pub dir_pattern: String,
    pub persist: bool,
}

/// Glob ignoring everything under `dir`, spelled with the platform
/// separator. `Path::join` inserts a separator only where one is needed, so
/// a drive root yields `C:\*` rather than the never-matching `C:\/*` a
/// `format!("{}/*")` would produce.
fn dir_ignore_pattern(dir: &std::path::Path) -> String {
    dir.join("*").to_string_lossy().into_owned()
}

/// What the tab asks the app to do after this frame.
#[derive(Default)]
pub struct SearchActions {
    /// Re-run the search (query/fuzzy/session filters changed *now*, not
    /// debounced — e.g. a chip was removed).
    pub rerun: bool,
    /// Persist an ignore pattern into the config.
    pub persist_ignore: Option<String>,
    /// The fuzzy toggle changed; remember it in the config.
    pub save_fuzzy_default: Option<bool>,
}

/// Add `incoming` to `set`, keeping at most `limit` of them — the best by
/// **rank**, whatever column the table is currently sorted by.
///
/// Retention and display are separate questions. What to keep is about
/// relevance; what order to show it in is the user's choice. That distinction
/// only started to matter once the cascade began streaming: hits now arrive in
/// table order, so a cap that simply stopped accepting at `limit` would fill
/// the table with whatever the scan happened to reach first and never show the
/// good ones. Dropping the worst-ranked instead means a rank-1 hit found late
/// in a scan still displaces a rank-10 one found early.
fn admit(set: &mut Vec<SearchHit>, incoming: Vec<SearchHit>, limit: usize, limited: &mut bool) {
    set.extend(incoming);
    if set.len() > limit {
        set.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
        });
        set.truncate(limit);
        *limited = true;
    }
}

pub struct SearchTab {
    pub query: String,
    pub fuzzy: bool,
    /// Set on every edit; the app fires the search after the debounce.
    pub pending_edit: Option<Instant>,
    pub generation: u64,
    pub results: Vec<SearchHit>,
    /// The next search's hits, held back while the old table fades out;
    /// swapped into `results` at zero opacity. Prevents the empty-refill
    /// strobe while typing.
    staging: Vec<SearchHit>,
    staging_has_snippets: bool,
    /// True from search start until the staged set has been swapped in.
    swap_pending: bool,
    /// How much of the results section the reveal still hides: 0 fully on
    /// screen, 1 fully covered. Set to 1 at the swap and travels back down
    /// to 0 as the new results wipe in — and *only* then. Clearing the old
    /// set holds it still, so an interrupted reveal does not flash the rows
    /// it had already covered back on screen on the way out.
    wipe: f32,
    /// The section's own opacity, which is all there is to clearing the old
    /// results. Follows `wipe` while the new ones arrive and decays on its
    /// own while the old ones go, so it is continuous across the turn.
    ///
    /// Both are stepped by frame time rather than handed to egui's animation
    /// manager, which reads the value of a transition it is already running
    /// against whatever duration the current call passes — and the two
    /// directions here have very different ones.
    fade: f32,
    /// Display permutation over `results`.
    order: Vec<u32>,
    sort: (SortKey, bool),
    sort_dirty: bool,
    pub selected: Option<u32>,
    pub running: bool,
    /// When the in-flight search was submitted.
    search_started: Option<Instant>,
    /// Wall time of the last completed search (all cascade passes).
    elapsed: Option<std::time::Duration>,
    pub limited: bool,
    pub error: Option<String>,
    pub session_ignores: Vec<String>,
    pub ignore_dialog: Option<IgnoreDialog>,
    pub help_open: bool,
    has_snippets: bool,
    /// Display-row index hovered last frame; drives the row hover fill.
    /// egui_extras' own tracking needs `row.response().hovered()`, which is
    /// false whenever a selectable label wins the hit-test, so we track it
    /// ourselves via `contains_pointer()`.
    hovered_row: Option<usize>,
    focus_query: bool,
    /// Query syntax-highlight segments, cached per text.
    highlight: crate::query_highlight::HighlightCache,
    /// Screen rects of the Match cells rendered last frame, in display
    /// order — the capture driver's coordinate-free hover targets.
    #[cfg(feature = "capture")]
    pub(crate) capture_match_rects: Vec<egui::Rect>,
}

impl SearchTab {
    pub fn new(fuzzy_default: bool) -> SearchTab {
        SearchTab {
            query: String::new(),
            fuzzy: fuzzy_default,
            pending_edit: None,
            generation: 0,
            results: Vec::new(),
            staging: Vec::new(),
            staging_has_snippets: false,
            swap_pending: false,
            wipe: 0.0,
            fade: 1.0,
            order: Vec::new(),
            sort: (SortKey::Rank, true),
            sort_dirty: false,
            selected: None,
            running: false,
            search_started: None,
            elapsed: None,
            limited: false,
            error: None,
            session_ignores: Vec::new(),
            ignore_dialog: None,
            help_open: false,
            has_snippets: false,
            hovered_row: None,
            focus_query: true,
            highlight: Default::default(),
            #[cfg(feature = "capture")]
            capture_match_rects: Vec::new(),
        }
    }

    /// Pre-fill the query and let the normal debounce path run it, so a
    /// command-line query lands the user on results rather than an empty box.
    pub fn seed(&mut self, query: String) {
        self.query = query;
        self.pending_edit = Some(Instant::now());
    }

    /// The query has executed, the stage swap has landed and the table has
    /// wiped all the way back in — what the capture driver's
    /// `wait_search_done` means by "done". The reveal is part of it because
    /// it outlasts the swap by half a second, and a screenshot taken during
    /// it catches a half-drawn table.
    #[cfg(feature = "capture")]
    pub(crate) fn capture_settled(&self) -> bool {
        !self.running && self.pending_edit.is_none() && self.fade_settled()
    }

    /// Re-arm the one-shot first-frame focus: tab switches drop egui focus,
    /// and injected text needs the caret back in the search box.
    #[cfg(feature = "capture")]
    pub(crate) fn capture_focus(&mut self) {
        self.focus_query = true;
    }

    /// Screen rect of the Nth visible Match cell from the last rendered
    /// frame, if that many are on screen.
    #[cfg(feature = "capture")]
    pub(crate) fn capture_match_cell(&self, n: usize) -> Option<egui::Rect> {
        self.capture_match_rects.get(n).copied()
    }

    /// A new search was submitted under `generation`. The previous
    /// results stay on screen (fading out); the new ones stage until the
    /// fade reaches zero.
    pub fn on_search_started(&mut self, generation: u64) {
        self.generation = generation;
        self.staging.clear();
        self.staging_has_snippets = false;
        self.swap_pending = true;
        self.running = true;
        self.search_started = Some(Instant::now());
        self.elapsed = None;
        self.limited = false;
        self.error = None;
    }

    /// Nothing left to animate: the section is fully on screen and no result
    /// swap is waiting on it.
    fn fade_settled(&self) -> bool {
        !self.swap_pending && self.wipe <= 0.0 && self.fade >= 1.0
    }

    /// Move the transition on by `dt` seconds.
    ///
    /// The two halves are not mirror images. Clearing the old results is a
    /// plain fade — the reveal holds where it stands, because a search fired
    /// part way through the previous one's wipe would otherwise un-cover the
    /// rows it had just covered, flashing them back on screen on the way
    /// out. The opacity carries straight on from whatever the reveal had it
    /// at, so the turn is continuous either way.
    ///
    /// Steps are clamped rather than merely added: a stalled frame must not
    /// overshoot into a value the swap test or the scrim would have to guard
    /// against.
    fn advance_fade(&mut self, dt: f32) {
        let dt = dt.max(0.0);
        if self.swap_pending {
            self.fade = (self.fade - dt / FADE_OUT_SECS).max(0.0);
        } else {
            self.wipe = (self.wipe - dt / FADE_IN_SECS).max(0.0);
            self.fade = ((1.0 - self.wipe) / FADE_ALPHA_SPAN).min(1.0);
        }
    }

    pub fn apply_update(&mut self, update: SearchUpdate, display_limit: usize) {
        if update.generation() != self.generation {
            return;
        }
        match update {
            SearchUpdate::Started { .. } => {}
            SearchUpdate::Hits { hits, .. } => {
                if self.swap_pending {
                    // Old results are still fading out; hold the new ones.
                    // Nothing of this generation is displayed yet, so there is
                    // no selection to keep.
                    self.staging_has_snippets |= hits.iter().any(|h| h.snippet.is_some());
                    admit(&mut self.staging, hits, display_limit, &mut self.limited);
                } else {
                    self.has_snippets |= hits.iter().any(|h| h.snippet.is_some());
                    // A row can be selected while batches are still arriving,
                    // and admitting them may reorder or drop rows out from
                    // under its index, so carry the selection by file id.
                    let selected_id = self
                        .selected
                        .and_then(|i| self.results.get(i as usize))
                        .map(|h| h.file_id);
                    admit(&mut self.results, hits, display_limit, &mut self.limited);
                    self.selected = selected_id.and_then(|id| {
                        self.results
                            .iter()
                            .position(|h| h.file_id == id)
                            .map(|i| i as u32)
                    });
                    // Arrival order is *scan* order — the cascade streams each
                    // pass as it runs, so a better-ranked hit can turn up after
                    // a worse one. Re-establish the table's own order on every
                    // batch, under whichever column is keyed.
                    self.sort_dirty = true;
                }
            }
            SearchUpdate::Completed { limited, .. } => {
                self.running = false;
                self.elapsed = self.search_started.map(|t| t.elapsed());
                self.limited |= limited;
            }
            SearchUpdate::Error { message, .. } => {
                self.running = false;
                self.elapsed = self.search_started.map(|t| t.elapsed());
                self.error = Some(message);
            }
        }
    }

    pub fn result_count_label(&self) -> Option<String> {
        if self.query.trim().is_empty() && self.results.is_empty() {
            return None;
        }
        Some(if self.limited {
            format!("{}+ results (truncated)", self.results.len())
        } else {
            format!("{} results", self.results.len())
        })
    }

    fn resort(&mut self) {
        let (key, ascending) = self.sort;
        let selected_id = self
            .selected
            .and_then(|i| self.results.get(i as usize))
            .map(|h| h.file_id);
        self.order = (0..self.results.len() as u32).collect();
        let results = &self.results;
        self.order.sort_by(|&a, &b| {
            let (a, b) = (&results[a as usize], &results[b as usize]);
            let ord = match key {
                SortKey::Rank => a
                    .rank
                    .partial_cmp(&b.rank)
                    .unwrap_or(std::cmp::Ordering::Equal),
                SortKey::Name => a.name.cmp(&b.name),
                SortKey::Path => a.path.cmp(&b.path),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.mtime.cmp(&b.mtime),
            };
            let ord = if ascending { ord } else { ord.reverse() };
            // Break ties on the path, which is unique, so the order is total.
            // Without it a stable sort falls back to insertion order — and
            // that is now the order batches happened to stream in, so equal
            // keys would shuffle under the pointer on every arrival. The
            // tiebreak stays ascending regardless of the key's direction; it
            // is there for stability, not as a second sort the user asked for.
            ord.then_with(|| a.path.cmp(&b.path))
        });
        // Selection follows the file, not the visual slot.
        self.selected = selected_id.and_then(|id| {
            self.results
                .iter()
                .position(|h| h.file_id == id)
                .map(|i| i as u32)
        });
        self.sort_dirty = false;
    }

    /// A sortable column header: the whole cell is the click target, the
    /// label is centered, and the sort indicator is a painter-drawn
    /// triangle (the default egui fonts have no ▲/▼ glyphs — they render
    /// as boxes).
    fn sort_header(&mut self, ui: &mut egui::Ui, key: SortKey, label: &str) {
        let (cur, asc) = self.sort;
        let selected = cur == key;
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            if response.hovered() {
                ui.painter()
                    .rect_filled(rect, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
            }
            let font_id = egui::TextStyle::Body.resolve(ui.style());
            let color = ui.visuals().strong_text_color();
            let galley = ui
                .painter()
                .layout_no_wrap(label.to_string(), font_id, color);
            let text_size = galley.size();
            let arrow_space = if selected { 11.0 } else { 0.0 };
            let text_pos = egui::pos2(
                rect.center().x - (text_size.x + arrow_space) / 2.0,
                rect.center().y - text_size.y / 2.0,
            );
            ui.painter().galley(text_pos, galley, color);
            if selected {
                let cx = text_pos.x + text_size.x + 7.0;
                let cy = rect.center().y;
                let (w, h) = (3.5, 3.0);
                let points = if asc {
                    vec![
                        egui::pos2(cx, cy - h),
                        egui::pos2(cx - w, cy + h),
                        egui::pos2(cx + w, cy + h),
                    ]
                } else {
                    vec![
                        egui::pos2(cx, cy + h),
                        egui::pos2(cx - w, cy - h),
                        egui::pos2(cx + w, cy - h),
                    ]
                };
                ui.painter().add(egui::Shape::convex_polygon(
                    points,
                    color,
                    egui::Stroke::NONE,
                ));
            }
        }
        if response.clicked() {
            self.sort = if selected { (key, !asc) } else { (key, true) };
            self.sort_dirty = true;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> SearchActions {
        let mut actions = SearchActions::default();

        // --- Query strip -------------------------------------------------
        // Laid out right to left: help, fuzzy and the status slot pin to the
        // right edge, and the query box takes whatever is left. The status
        // slot keeps a fixed width whether it holds the spinner, the query
        // time or nothing at all, so the box never resizes as you type.
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("?").on_hover_text("Query syntax help").clicked() {
                    self.help_open = !self.help_open;
                }
                if ui
                    .checkbox(&mut self.fuzzy, "Fuzzy")
                    .on_hover_text("Also run fuzzy filename and full-text passes (slower)")
                    .changed()
                {
                    actions.save_fuzzy_default = Some(self.fuzzy);
                    actions.rerun = true;
                }
                // Spinner while searching, then the total wall time of all
                // cascade passes once it lands.
                let show_elapsed =
                    !self.running && self.elapsed.is_some() && !self.query.trim().is_empty();
                ui.allocate_ui_with_layout(
                    egui::vec2(STATUS_SLOT_WIDTH, ui.spacing().interact_size.y),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        // The child shrinks to its content when it finishes,
                        // so hold the width from the inside — otherwise an
                        // empty slot would give its space back to the box.
                        ui.set_min_width(STATUS_SLOT_WIDTH);
                        if self.running {
                            ui.add(egui::Spinner::new().size(16.0));
                        } else if show_elapsed {
                            if let Some(elapsed) = self.elapsed {
                                ui.label(egui::RichText::new(fmt_elapsed(elapsed)).small().weak())
                                    .on_hover_text("Time to run all search passes");
                            }
                        }
                    },
                );
                let width = ui.available_width();
                let highlight = &mut self.highlight;
                let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                    crate::query_highlight::galley(ui, highlight, buf.as_str())
                };
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .desired_width(width.max(120.0))
                        .hint_text("Search names and contents…  (type:Document regex:… budget*)")
                        .layouter(&mut layouter),
                );
                if self.focus_query {
                    response.request_focus();
                    self.focus_query = false;
                }
                if response.changed() {
                    self.pending_edit = Some(Instant::now());
                }
            });
        });

        // Session ignore chips.
        if !self.session_ignores.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Ignoring:").small().weak());
                let mut remove: Option<usize> = None;
                for (i, pattern) in self.session_ignores.iter().enumerate() {
                    if ui
                        .small_button(format!("{} 🗙", pattern))
                        .on_hover_text("Remove this session filter")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                }
                if let Some(i) = remove {
                    self.session_ignores.remove(i);
                    actions.rerun = true;
                }
            });
        }

        // Notices.
        if let Some(err) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, err);
        } else if self.limited {
            ui.label(
                egui::RichText::new(format!(
                    "Showing first {} matches; refine the query (limit configurable in Options).",
                    self.results.len()
                ))
                .small()
                .weak(),
            );
        } else if !self.running
            && !self.swap_pending
            && self.results.is_empty()
            && !self.query.trim().is_empty()
            && self.error.is_none()
        {
            ui.label(egui::RichText::new("No results.").small().weak());
        }

        // Result-set transitions arrive instead of strobing: the old table
        // dips out over `FADE_OUT_SECS` while the new hits stage, the sets
        // swap once nothing is left on screen, and the new table is then
        // uncovered from the top down over `FADE_IN_SECS`. Repaints have to
        // be asked for by hand, since the values are ours rather than the
        // animation manager's — and the swap frame needs one too, having
        // just finished the fade-out without yet starting the reveal.
        self.advance_fade(ui.input(|i| i.stable_dt));
        if self.swap_pending && self.fade <= 0.0 {
            self.results = std::mem::take(&mut self.staging);
            self.has_snippets = self.staging_has_snippets;
            self.selected = None;
            self.swap_pending = false;
            // The new set starts fully covered, and the reveal walks it back
            // down from there.
            self.wipe = 1.0;
            // Staged hits arrived in scan order too, so the table has to be
            // ordered here as well — including under the default key.
            self.sort_dirty = true;
        }
        if !self.fade_settled() {
            ui.ctx().request_repaint();
        }

        if self.sort_dirty {
            self.resort();
        }

        // The section-wide half of the effect: the whole of the fade-out,
        // and a short climb at the start of the reveal so its leading edge
        // does not have to carry that alone. Rows the edge has passed stay
        // at full strength. The modal windows and the notices above render
        // at full opacity on their own layers either way.
        ui.set_opacity(self.fade);

        // --- Results table ------------------------------------------------
        // Reserve room for the selected-row snippet preview strip. Only
        // content matches get one — a filename match's "snippet" is the
        // name, already on screen.
        let preview_snippet: Option<Snippet> = self
            .selected
            .and_then(|i| self.results.get(i as usize))
            .filter(|h| matches!(h.stage, 5 | 6 | 8))
            .and_then(|h| h.snippet.clone());
        let preview_height = if preview_snippet.is_some() { 44.0 } else { 0.0 };
        let table_height = (ui.available_height() - preview_height).max(60.0);

        let body_font = egui::TextStyle::Body.resolve(ui.style());
        let text_height = body_font.size + 4.0;
        let mut open_ignore_dialog: Option<usize> = None;
        let mut hovered_now: Option<usize> = None;
        // Moved out of `self` rather than cloned. The row closure needs `&mut
        // self` for selection and hover, so a field read would conflict — but
        // a plain local does not, and this runs every frame.
        let order = std::mem::take(&mut self.order);
        // Same local-then-assign dance for the capture driver's hover
        // targets: rebuilt every frame from what actually rendered.
        #[cfg(feature = "capture")]
        let mut capture_match_rects: Vec<egui::Rect> = Vec::new();

        // Where the wiped section begins. Its end is only known once the
        // preview strip below has been laid out.
        let section_top = ui.cursor().top();

        let table_scroll = ui
            .push_id("results", |ui| {
                let mut table = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .max_scroll_height(table_height)
                    .min_scrolled_height(60.0)
                    .column(Column::initial(220.0).at_least(80.0).clip(true)) // name
                    .column(Column::remainder().at_least(120.0).clip(true)); // path
                if self.has_snippets {
                    table = table.column(Column::remainder().at_least(120.0).clip(true));
                }
                table = table
                    .column(Column::exact(72.0)) // size
                    .column(Column::exact(110.0)) // modified
                    .column(Column::exact(52.0)); // rank

                table
                    .header(text_height + 4.0, |mut header| {
                        header.col(|ui| self.sort_header(ui, SortKey::Name, "Name"));
                        header.col(|ui| self.sort_header(ui, SortKey::Path, "Path"));
                        if self.has_snippets {
                            header.col(|ui| {
                                ui.with_layout(
                                    egui::Layout::centered_and_justified(
                                        egui::Direction::LeftToRight,
                                    ),
                                    |ui| {
                                        ui.label(egui::RichText::new("Match").strong());
                                    },
                                );
                            });
                        }
                        header.col(|ui| self.sort_header(ui, SortKey::Size, "Size"));
                        header.col(|ui| self.sort_header(ui, SortKey::Modified, "Modified"));
                        header.col(|ui| self.sort_header(ui, SortKey::Rank, "Rank"));
                    })
                    .body(|body| {
                        body.rows(text_height, order.len(), |mut row| {
                            let display_ix = row.index();
                            let result_ix = order[display_ix] as usize;
                            let hit = &self.results[result_ix];
                            row.set_selected(self.selected == Some(result_ix as u32));
                            row.set_hovered(self.hovered_row == Some(display_ix));

                            // Labels stay selectable for copy-paste, which makes
                            // them win egui's hit-test over the row. Collect their
                            // responses and union them into the row's below so
                            // clicks land even when the pointer is over glyphs.
                            let mut cell_responses: Vec<egui::Response> = Vec::new();

                            row.col(|ui| {
                                cell_responses.push(ui.label(&hit.name));
                            });
                            row.col(|ui| {
                                // Center-elided: egui's own truncation keeps
                                // the head and drops the deepest directories
                                // — the half that actually says where the
                                // file lives. A sizing pass hands out cell
                                // rects that are not final yet, so nothing is
                                // measured against one.
                                let shown = if ui.is_sizing_pass() {
                                    std::borrow::Cow::Borrowed(hit.path.as_str())
                                } else {
                                    middle_elide(
                                        ui,
                                        &hit.path,
                                        // A point of slack, so a rounding
                                        // disagreement with egui's layout
                                        // cannot cost a second ellipsis.
                                        ui.available_width() - 1.0,
                                        &body_font,
                                    )
                                };
                                let elided = matches!(shown, std::borrow::Cow::Owned(_));
                                // egui offers a full-text tooltip only when
                                // *it* elided the galley, and it would be
                                // handed the string already shortened here.
                                let mut response = ui.add(
                                    egui::Label::new(egui::RichText::new(shown.as_ref()).weak())
                                        .show_tooltip_when_elided(false),
                                );
                                if elided {
                                    response = response.on_hover_text(&hit.path);
                                }
                                cell_responses.push(response);
                            });
                            if self.has_snippets {
                                // Borrowed, not cloned: a snippet window runs
                                // to 600 characters and this is per visible row
                                // per frame. The hover job is built inside the
                                // closure, so an un-hovered row builds nothing.
                                let snippet = hit.snippet.as_ref();
                                // Name and path matches show a whole field, so
                                // they render bracketed: [matched field].
                                let whole_field =
                                    hit.stage <= 4 || hit.stage == 7 || hit.stage >= 9;
                                row.col(|ui| {
                                    if let Some(snip) = snippet {
                                        let width = ui.available_width();
                                        let job = centered_match_job(ui, snip, width, whole_field);
                                        let mut response = ui
                                            .with_layout(
                                                egui::Layout::centered_and_justified(
                                                    egui::Direction::LeftToRight,
                                                ),
                                                |ui| ui.label(job),
                                            )
                                            .inner;
                                        if !snip.ranges.is_empty() {
                                            response = response.on_hover_ui(|ui| {
                                                ui.set_max_width(520.0);
                                                let job = snippet_job(ui, snip, 10);
                                                ui.label(job);
                                            });
                                        }
                                        #[cfg(feature = "capture")]
                                        capture_match_rects.push(response.rect);
                                        cell_responses.push(response);
                                    }
                                });
                            }
                            row.col(|ui| {
                                let response = ui.with_layout(
                                    egui::Layout::centered_and_justified(
                                        egui::Direction::LeftToRight,
                                    ),
                                    |ui| ui.label(human_size(hit.size)),
                                );
                                cell_responses.push(response.inner);
                            });
                            row.col(|ui| {
                                let color = recency_color(ui, hit.mtime);
                                let response = ui.with_layout(
                                    egui::Layout::centered_and_justified(
                                        egui::Direction::LeftToRight,
                                    ),
                                    |ui| {
                                        ui.label(
                                            egui::RichText::new(fmt_mtime(hit.mtime)).color(color),
                                        )
                                    },
                                );
                                cell_responses.push(response.inner);
                            });
                            row.col(|ui| {
                                let response = ui.with_layout(
                                    egui::Layout::centered_and_justified(
                                        egui::Direction::LeftToRight,
                                    ),
                                    |ui| {
                                        ui.label(
                                            egui::RichText::new(format!(" {:.2} ", hit.rank))
                                                .background_color(rank_tier_color(hit.stage))
                                                .color(egui::Color32::from_rgb(32, 32, 32)),
                                        )
                                    },
                                );
                                cell_responses.push(response.inner);
                            });

                            let mut response = row.response();
                            for r in cell_responses {
                                response |= r;
                            }
                            if response.contains_pointer() {
                                hovered_now = Some(display_ix);
                            }
                            if response.clicked() || response.secondary_clicked() {
                                self.selected = Some(result_ix as u32);
                            }
                            if response.double_clicked() {
                                platform::open_file(&self.results[result_ix].path);
                            }
                            response.context_menu(|ui| {
                                let path = self.results[result_ix].path.clone();
                                if ui.button("Open containing folder").clicked() {
                                    platform::reveal_in_folder(&path);
                                    ui.close();
                                }
                                if ui.button("Open").clicked() {
                                    platform::open_file(&path);
                                    ui.close();
                                }
                                if ui.button("Copy path").clicked() {
                                    ui.ctx().copy_text(path.clone());
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Build ignore filter…").clicked() {
                                    open_ignore_dialog = Some(result_ix);
                                    ui.close();
                                }
                            });
                        });
                    })
            })
            .inner;
        // Put the permutation back for the next frame.
        self.order = order;
        crate::ui_util::more_below_hint(ui, &table_scroll);
        self.hovered_row = hovered_now;
        #[cfg(feature = "capture")]
        {
            self.capture_match_rects = capture_match_rects;
        }

        if let Some(ix) = open_ignore_dialog {
            let hit = &self.results[ix];
            self.ignore_dialog = Some(IgnoreDialog {
                source_path: hit.path.clone(),
                ext_pattern: std::path::Path::new(&hit.name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| format!("*.{}", e)),
                name_pattern: hit.name.clone(),
                dir_pattern: std::path::Path::new(&hit.path)
                    .parent()
                    .map(dir_ignore_pattern)
                    .unwrap_or_default(),
                persist: false,
            });
        }

        // Selected-row preview strip: the full snippet, wrapped.
        if let Some(snip) = &preview_snippet {
            ui.separator();
            let job = snippet_job(ui, snip, 2);
            ui.label(job);
        }

        // The reveal uncovers everything from the column headers to the
        // bottom of the preview strip together — table, scroll bar, "more
        // below" hint and all. Painted last so it covers them, and outside
        // the opacity set above so the scrim itself is not faded by it.
        let section = egui::Rect::from_x_y_ranges(
            ui.max_rect().x_range(),
            section_top..=ui.min_rect().bottom(),
        );
        crate::ui_util::wipe_scrim(ui, section, self.wipe);

        self.ignore_dialog_ui(ui.ctx(), &mut actions);
        self.help_window_ui(ui.ctx());
        actions
    }

    fn ignore_dialog_ui(&mut self, ctx: &egui::Context, actions: &mut SearchActions) {
        use crate::ui_util::{bordered_button, pattern_edit, BLUE, ORANGE};
        let Some(dialog) = &mut self.ignore_dialog else {
            return;
        };
        // The chosen pattern; each "Ignore this …" button applies exactly
        // that filter and closes the dialog.
        let mut chosen: Option<String> = None;
        let mut cancel = false;
        egui::Window::new("Ignore filter")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(430.0);
                ui.label(format!("From: {}", dialog.source_path));
                ui.separator();

                // --- Extension ---------------------------------------------
                ui.horizontal(|ui| match &dialog.ext_pattern {
                    Some(ext) => {
                        ui.monospace(ext);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(bordered_button("Ignore this extension", ORANGE))
                                .clicked()
                            {
                                chosen = Some(ext.clone());
                            }
                        });
                    }
                    None => {
                        ui.label(egui::RichText::new("(no file extension)").weak());
                    }
                });
                ui.separator();

                // --- Filename ----------------------------------------------
                ui.horizontal(|ui| {
                    let (_, valid) =
                        pattern_edit(ui, &mut dialog.name_pattern, 240.0, "filename or glob");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(valid, bordered_button("Ignore this filename", ORANGE))
                            .clicked()
                        {
                            chosen = Some(dialog.name_pattern.trim().to_string());
                        }
                    });
                });
                // Inside a stable section, or the hint's appearance would
                // rename the directory editor below and drop its focus.
                crate::ui_util::pattern_hint_label(ui, &dialog.name_pattern);
                ui.separator();

                // --- Directory ---------------------------------------------
                ui.horizontal(|ui| {
                    let (_, valid) =
                        pattern_edit(ui, &mut dialog.dir_pattern, 240.0, "directory glob");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(valid, bordered_button("Ignore this directory", ORANGE))
                            .clicked()
                        {
                            chosen = Some(dialog.dir_pattern.trim().to_string());
                        }
                    });
                });
                ui.separator();

                // --- Persist + close ---------------------------------------
                egui::Frame::new()
                    .stroke(egui::Stroke::new(1.0, BLUE))
                    .corner_radius(4)
                    .inner_margin(egui::Margin::symmetric(6, 3))
                    .show(ui, |ui| {
                        ui.checkbox(&mut dialog.persist, "Persist to config");
                    });
                ui.label(
                    egui::RichText::new(
                        "Session filters hide results immediately. Persisted filters also \
                         exclude files from the index at the next reindex.",
                    )
                    .small()
                    .weak(),
                );
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        if let Some(pattern) = chosen {
            let dialog = self.ignore_dialog.take().unwrap();
            if !self.session_ignores.contains(&pattern) {
                self.session_ignores.push(pattern.clone());
            }
            if dialog.persist {
                actions.persist_ignore = Some(pattern);
            }
            actions.rerun = true;
        } else if cancel {
            self.ignore_dialog = None;
        }
    }

    fn help_window_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.help_open;
        egui::Window::new("Query syntax")
            .open(&mut open)
            .resizable(false)
            .default_width(540.0)
            .show(ctx, |ui| {
                ui.label(
                    "Everything that is not a filter is matched as one phrase, in order. \
                     Filters combine freely with the search text.",
                );
                ui.add_space(6.0);
                egui::Grid::new("query-syntax-table")
                    .num_columns(2)
                    .spacing([18.0, 5.0])
                    .striped(true)
                    .show(ui, |ui| {
                        let row = |ui: &mut egui::Ui, syntax: &str, meaning: &str| {
                            ui.monospace(syntax);
                            ui.label(meaning);
                            ui.end_row();
                        };
                        row(
                            ui,
                            "budget report",
                            "names, contents, and paths containing \"budget report\"",
                        );
                        row(
                            ui,
                            "\"exact phrase\"",
                            "quotes keep spaces, stars, and filter-like words literal",
                        );
                        row(
                            ui,
                            "bud*report",
                            "* matches any run of characters (within a line); \
                             also works in name: values",
                        );
                        row(
                            ui,
                            "regex:\"(foo|bar)\\d+\"",
                            "regular expression, matched against names, contents, \
                             and paths",
                        );
                        row(
                            ui,
                            "type:Document",
                            "file class: Audio, Image, Video, Document, Text, \
                             Archive, Spreadsheet, Presentation, Folder",
                        );
                        row(
                            ui,
                            "modified:>=2024-01-01",
                            "modification date (yyyy-mm-dd); also <, <=, > and =",
                        );
                        row(
                            ui,
                            "path:/home/me/docs",
                            "only results in that folder and its subfolders; \
                             quote paths containing spaces",
                        );
                        row(ui, "mime:application/pdf", "exact MIME type");
                        row(
                            ui,
                            "name:report",
                            "filename contains, applied as an unranked filter",
                        );
                    });
                ui.add_space(6.0);
                ui.label("Example:");
                ui.monospace("type:Document modified:>=2024-01-01 quarterly budget");
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Ranking: exact filename matches, then filename substrings, then \
                         full-text matches (ordered by occurrences), then fuzzy matches \
                         when enabled, and finally matches on the rest of the file path.",
                    )
                    .small()
                    .weak(),
                );
                ui.label(
                    egui::RichText::new(
                        "The complete reference, including ranking details and the \
                         fuzzy edit budget, is the \"Query syntax\" section of \
                         README.md in the QuickSearch folder.",
                    )
                    .small()
                    .weak(),
                );
            });
        self.help_open = open;
    }
}

struct SnippetFormats {
    normal: TextFormat,
    highlight: TextFormat,
    weak: TextFormat,
}

fn snippet_formats(ui: &egui::Ui) -> SnippetFormats {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    SnippetFormats {
        normal: TextFormat {
            font_id: font_id.clone(),
            color: ui.visuals().text_color(),
            ..Default::default()
        },
        highlight: TextFormat {
            font_id: font_id.clone(),
            color: ui.visuals().strong_text_color(),
            background: ui.visuals().selection.bg_fill.gamma_multiply(0.4),
            ..Default::default()
        },
        weak: TextFormat {
            font_id,
            color: ui.visuals().weak_text_color(),
            ..Default::default()
        },
    }
}

/// The mark on a snippet that starts partway into its window. Named because
/// `first_visible_byte` has to pay for its width in advance.
const SNIPPET_LEAD: &str = "… ";

/// Append `window[range]` to `job`, highlighting whatever parts of `ranges`
/// (byte offsets into `window`) fall inside it.
///
/// Shared by the one-line Match cell and the multi-row hover and preview
/// jobs: all three paint a sub-slice of one window with the same highlights,
/// and clipping a range against a slice is exactly the sort of off-by-one
/// that gets fixed in one of two copies.
fn append_marked(
    job: &mut LayoutJob,
    fmt: &SnippetFormats,
    window: &str,
    ranges: &[(usize, usize)],
    range: std::ops::Range<usize>,
) {
    let mut cursor = range.start;
    for &(a, b) in ranges {
        let (a, b) = (a.max(range.start), b.min(range.end));
        if a >= b {
            continue; // wholly before or after the slice
        }
        if a > cursor {
            job.append(&window[cursor..a], 0.0, fmt.normal.clone());
        }
        job.append(&window[a..b], 0.0, fmt.highlight.clone());
        cursor = b;
    }
    if cursor < range.end {
        job.append(&window[cursor..range.end], 0.0, fmt.normal.clone());
    }
}

/// The byte offset in `snip.window` that rendering has to start at for the
/// first match to land on a row that survives `max_rows`. `0` — render the
/// whole window — whenever it already does, which is the usual case.
///
/// epaint lays a job out row by row and simply stops at `wrap.max_rows`, and
/// *every* `\n` costs a row, blank line or not. A snippet window opens a
/// third of its byte budget ahead of the hit, so a couple of hundred bytes of
/// ragged lead-in — indented code, a run of blank lines — spends the whole
/// row budget before layout reaches the match, and the mouseover ends up
/// showing context with nothing in it to be context *for*.
///
/// The row is measured rather than estimated from a character budget: with a
/// proportional font and word wrapping, characters-per-row is wrong in both
/// directions (`ui_util::middle_elide` documents the same lesson), and an
/// estimate would have to re-derive what epaint already knows about tabs
/// (four spaces wide), `\r` (invisible) and empty paragraphs. The probe is
/// one more galley, memoized by job hash, for the one hovered row per frame.
fn first_visible_byte(
    ui: &egui::Ui,
    snip: &Snippet,
    fmt: &SnippetFormats,
    max_rows: usize,
    wrap_width: f32,
) -> usize {
    let Some(&(match_start, _)) = snip.ranges.first() else {
        return 0; // a head-of-file window, with nothing to keep on screen
    };

    // The rendered job pays for a leading mark this probe does not, so the
    // probe wraps to a narrower width — a point narrower still, since epaint
    // rounds `wrap.max_width` before laying out. Every rendered row then
    // holds at least what the probe row starting at the same character held,
    // so the match cannot drift *down* a row when the job is rebuilt.
    let lead_width = ui.fonts(|f| {
        SNIPPET_LEAD
            .chars()
            .map(|c| f.glyph_width(&fmt.normal.font_id, c))
            .sum::<f32>()
    });
    let mut probe = LayoutJob::default();
    probe.wrap.max_width = (wrap_width - lead_width - 1.0).max(1.0);
    probe.append(&snip.window, 0.0, fmt.normal.clone());
    let galley = ui.fonts(|f| f.layout_job(probe));

    // Cursors index characters; snippet ranges are byte offsets. epaint
    // counts the `\n` that ends a row, so the two spaces line up 1:1.
    let cursor = egui::text::CCursor {
        index: snip.window[..match_start].chars().count(),
        // At a wrap, the character belongs to the row it is drawn on, not
        // the one it was pushed off.
        prefer_next_row: true,
    };
    let match_row = galley.layout_from_cursor(cursor).row;

    // epaint trades a glyph or two off the end of the last visible row for
    // its own overflow ellipsis, so a match sitting there only counts as
    // visible when there was nothing below it to elide in the first place.
    let visible_rows = if galley.rows.len() > max_rows {
        max_rows.saturating_sub(1)
    } else {
        max_rows
    };
    if match_row < visible_rows {
        return 0;
    }

    // Keep a third of the budget as lead-in so the hit is not pinned to the
    // top edge — the same shape as the window `snippet::extract` picks. The
    // two-row preview strip keeps none, and starts on the match's own row.
    let mut cursor = cursor;
    for _ in 0..max_rows / 3 {
        // `Some(0.0)` asks for the row above, not the character above.
        cursor = galley.cursor_up_one_row(&cursor, Some(0.0)).0;
    }
    let start_char = galley.cursor_begin_of_row(&cursor).index;
    snip.window
        .char_indices()
        .nth(start_char)
        .map_or(snip.window.len(), |(i, _)| i)
}

/// Build a highlighted snippet LayoutJob from byte ranges, wrapped to at
/// most `max_rows` and started far enough into the window that the first
/// match survives the cap. Cheap enough to run per visible row per frame.
fn snippet_job(ui: &egui::Ui, snip: &Snippet, max_rows: usize) -> LayoutJob {
    let fmt = snippet_formats(ui);
    // The width `ui.label` is about to wrap this job to: in a top-down `Ui`
    // it overwrites `wrap.max_width` with exactly `ui.available_width()`.
    // Setting it here anyway is what lets `first_visible_byte` — and a test —
    // lay the job out and see the rows the user will see.
    let wrap_width = ui.available_width();
    let start = first_visible_byte(ui, snip, &fmt, max_rows, wrap_width);

    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    job.wrap.max_rows = max_rows;
    if start > 0 || snip.truncated_start {
        job.append(SNIPPET_LEAD, 0.0, fmt.weak.clone());
    }
    append_marked(
        &mut job,
        &fmt,
        &snip.window,
        &snip.ranges,
        start..snip.window.len(),
    );
    if snip.truncated_end {
        job.append(" …", 0.0, fmt.weak);
    }
    job
}

/// The Match column cell: one line with the (first) matched span centered
/// and an equal amount of context on both sides, trimmed to what fits the
/// column width. Matches on a whole field — a filename or a path — are
/// wrapped in brackets: `[name]`.
fn centered_match_job(
    ui: &egui::Ui,
    snip: &Snippet,
    width_px: f32,
    whole_field: bool,
) -> LayoutJob {
    let fmt = snippet_formats(ui);

    // Newlines force line breaks even in a one-row LayoutJob, wrecking the
    // centered single-line cell. Flatten them to spaces — a byte-for-byte
    // ASCII replacement, so the match ranges stay valid. The mouseover
    // renders the original window untouched.
    //
    // Only copied when there is something to flatten: name and path snippets
    // never contain these, and this runs per visible row per frame.
    let flattened: Option<String> = snip
        .window
        .contains(['\n', '\r', '\t'])
        .then(|| snip.window.replace(['\n', '\r', '\t'], " "));
    let window = flattened.as_deref().unwrap_or(&snip.window);

    // The budget is in pixels, summed from the font's own glyph advances,
    // because nothing else is holding this cell inside its column: the
    // centered-and-justified layout puts egui in Extend mode, which lays the
    // job out at infinite width, and `Column::clip` then trims a *centered*
    // overflow from both ends at once — silently, and taking the highlighted
    // match with it. A character count scaled by one sample glyph overshoots
    // by a quarter of the column on ordinary text.
    let (start, end, decorate) = ui.fonts(|f| {
        let font_id = &fmt.normal.font_id;
        let width_of = |c: char| f.glyph_width(font_id, c);
        let ellipsis = width_of('…');
        let brackets = if whole_field {
            width_of('[') + width_of(']')
        } else {
            0.0
        };
        let mut marks = 0.0;
        if snip.truncated_start {
            marks += ellipsis;
        }
        if snip.truncated_end {
            marks += ellipsis;
        }
        if fits_within(window, width_px - brackets - marks, width_of) {
            return (0, window.len(), true);
        }

        // Something has to go, so either end may gain a mark. Reserving for
        // a cut that does not happen costs a few points of context; missing
        // one overflows the column, which is the failure with no mark to
        // show for it.
        let budget = width_px - brackets - 2.0 * ellipsis;
        let Some(&(a, b)) = snip.ranges.first() else {
            // No ranges (shouldn't happen for match cells) — head trim.
            return (0, take_forward(window, 0, budget.max(0.0), width_of), true);
        };
        if budget <= 0.0 {
            // A column narrower than its own punctuation. Spend every point
            // on the hit and drop the decoration: the table's 120pt floor
            // keeps this out of reach, but a cell that overflows is clipped
            // from both ends without a mark to say so, which is the whole
            // failure this budget exists to prevent.
            return (a, take_forward(window, a, width_px, width_of), false);
        }
        if !fits_within(&window[a..b], budget, width_of) {
            // A hit wider than the whole column — a greedy regex or wildcard
            // match. Its beginning is the part that has to survive.
            return (a, take_forward(window, a, budget, width_of), true);
        }
        let match_w: f32 = window[a..b].chars().map(width_of).sum();

        // Equal context on both sides, grown outward one character at a
        // time; whichever side is currently narrower is fed first, so the
        // leftover from a short side flows to the other.
        let (mut start, mut end) = (a, b);
        let (mut before_w, mut after_w) = (0.0f32, 0.0f32);
        loop {
            let prev = window[..start].chars().next_back();
            let next = window[end..].chars().next();
            let used = before_w + match_w + after_w;
            let prev_fits = prev.is_some_and(|c| used + width_of(c) <= budget);
            let next_fits = next.is_some_and(|c| used + width_of(c) <= budget);
            if !prev_fits && !next_fits {
                break;
            }
            // The preferred side wins when it fits; otherwise the other one
            // does, since at least one of them just did.
            let take_prev = if before_w <= after_w {
                prev_fits
            } else {
                !next_fits
            };
            if take_prev {
                let c = prev.expect("prev_fits");
                start -= c.len_utf8();
                before_w += width_of(c);
            } else {
                let c = next.expect("next_fits");
                end += c.len_utf8();
                after_w += width_of(c);
            }
        }
        (start, end, true)
    });

    let mut job = LayoutJob::default();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    if whole_field && decorate {
        job.append("[", 0.0, fmt.weak.clone());
    }
    if decorate && (start > 0 || snip.truncated_start) {
        job.append("…", 0.0, fmt.weak.clone());
    }
    append_marked(&mut job, &fmt, window, &snip.ranges, start..end);
    if decorate && (end < window.len() || snip.truncated_end) {
        job.append("…", 0.0, fmt.weak.clone());
    }
    if whole_field && decorate {
        job.append("]", 0.0, fmt.weak);
    }
    job
}

/// Whether the whole of `text` fits in `budget` pixels. Stops at the first
/// character that does not, so measuring a 600-character snippet window
/// against a 200-point column costs no more than the column can hold — and
/// this runs for every visible row, every frame.
fn fits_within(text: &str, budget: f32, width_of: impl Fn(char) -> f32) -> bool {
    let mut used = 0.0;
    for c in text.chars() {
        used += width_of(c);
        if used > budget {
            return false;
        }
    }
    true
}

/// The byte offset one past the last character of `text[from..]` that still
/// fits in `budget` pixels.
fn take_forward(text: &str, from: usize, budget: f32, width_of: impl Fn(char) -> f32) -> usize {
    let mut end = from;
    let mut used = 0.0;
    for c in text[from..].chars() {
        let w = width_of(c);
        if used + w > budget {
            break;
        }
        used += w;
        end += c.len_utf8();
    }
    end
}

/// Jet-colormap chip color per cascade stage — the rank reads as a
/// colorbar: cool blue for the strongest matches, warming through cyan,
/// green and yellow to red for the weakest path tiers. Pastel rather than
/// true jet, since every channel stays at or above 127 so the chip's dark
/// text keeps its contrast in both themes.
fn rank_tier_color(stage: u8) -> egui::Color32 {
    match stage {
        1 => egui::Color32::from_rgb(127, 127, 255), // name exact, exact case
        2 => egui::Color32::from_rgb(127, 178, 255), // name exact, any case
        3 => egui::Color32::from_rgb(127, 229, 255), // name substring, exact case
        4 => egui::Color32::from_rgb(127, 255, 229), // name substring, any case
        5 => egui::Color32::from_rgb(127, 255, 178), // full text, exact case
        6 => egui::Color32::from_rgb(127, 255, 127), // full text, any case
        7 => egui::Color32::from_rgb(178, 255, 127), // fuzzy name
        8 => egui::Color32::from_rgb(229, 255, 127), // fuzzy full text
        9 => egui::Color32::from_rgb(255, 229, 127), // path substring, exact case
        10 => egui::Color32::from_rgb(255, 178, 127), // path substring, any case
        _ => egui::Color32::from_rgb(255, 127, 127), // fuzzy path
    }
}

/// Timestamp color: fresh files get a green tint that fades into the weak
/// text color over ~2 years on a log scale.
fn recency_color(ui: &egui::Ui, mtime: i64) -> egui::Color32 {
    let now = quicksearch_core::log::now_unix() as i64;
    let age_hours = ((now - mtime).max(0) as f32 / 3600.0).max(1.0);
    const HORIZON_HOURS: f32 = 24.0 * 365.0 * 2.0;
    let t = (age_hours.ln() / HORIZON_HOURS.ln()).clamp(0.0, 1.0);
    let fresh = egui::Color32::from_rgb(87, 187, 122);
    let old = ui.visuals().weak_text_color();
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    egui::Color32::from_rgb(
        lerp(fresh.r(), old.r()),
        lerp(fresh.g(), old.g()),
        lerp(fresh.b(), old.b()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab_with_results(n: usize) -> SearchTab {
        let mut tab = SearchTab::new(false);
        tab.query = "alpha".into();
        tab.focus_query = false;
        tab.results = (0..n)
            .map(|i| SearchHit {
                file_id: i as i64,
                name: format!("alpha_widget_{i}.txt"),
                path: format!("/qs-test/alpha_widget_{i}.txt"),
                size: 116,
                mtime: 1_700_000_000,
                rank: 3.0,
                stage: 1,
                snippet: None,
            })
            .collect();
        tab.order = (0..n as u32).collect();
        tab
    }

    fn run_frame(
        ctx: &egui::Context,
        tab: &mut SearchTab,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let input = crate::test_ui::raw_input(egui::vec2(1000.0, 700.0), events);
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                tab.ui(ui);
            });
        })
    }

    /// Walk the pointer down the name column until it sits on `row`'s label
    /// glyphs. The text cursor proves a selectable label won the hit-test —
    /// exactly the case where row hover, clicks, and the context menu used
    /// to go dead — while `hovered_row` proves the row still tracks hover.
    fn hover_row_text(ctx: &egui::Context, tab: &mut SearchTab, row: usize) -> egui::Pos2 {
        for y in 40..250 {
            let pos = egui::pos2(60.0, y as f32);
            let out = run_frame(ctx, tab, vec![egui::Event::PointerMoved(pos)]);
            let over_text = out.platform_output.cursor_icon == egui::CursorIcon::Text;
            if over_text && tab.hovered_row == Some(row) {
                return pos;
            }
        }
        panic!("never landed on row {row}'s label text");
    }

    use crate::test_ui::painted_text;

    /// Far too long for the Path column at the test's 1000pt screen width.
    fn deep_path() -> String {
        concat!(
            "/media/shared/QuickSearch/crates/quicksearch-gui/src/",
            "deeply/nested/under/several/more/directories/alpha_widget_0.txt"
        )
        .to_string()
    }

    /// The Path column drops out of the *middle*, so the volume the file
    /// sits on and the directories right above it both stay on screen.
    /// egui's own truncation would keep the head and throw the tail away —
    /// and the tail is the half that says where the file lives.
    #[test]
    fn long_paths_elide_from_the_middle_of_the_path_column() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_results(1);
        let path = deep_path();
        tab.results[0].path = path.clone();

        let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
        let cell = painted
            .iter()
            .find(|t| t.starts_with("/media") && t.contains('…'))
            .unwrap_or_else(|| panic!("no elided path cell among {painted:?}"));

        assert!(!painted.contains(&path), "painted in full");
        assert_eq!(cell.matches('…').count(), 1, "elided twice: {cell}");
        let (head, tail) = cell.split_once('…').expect("one ellipsis");
        assert!(path.starts_with(head), "{cell}");
        assert!(path.ends_with(tail), "{cell}");
        assert!(
            tail.ends_with("alpha_widget_0.txt"),
            "the deep end survives: {cell}"
        );
    }

    /// The whole path stays reachable on hover. egui hands out that tooltip
    /// for free only while *it* did the eliding, so text shortened ahead of
    /// time has to bring its own — and it is easy to lose silently.
    #[test]
    fn an_elided_path_still_shows_the_whole_thing_on_hover() {
        let ctx = egui::Context::default();
        // Testing that the tooltip is wired up, not egui's hover timing.
        ctx.style_mut(|s| {
            s.interaction.tooltip_delay = 0.0;
            s.interaction.show_tooltips_only_when_still = false;
        });
        let mut tab = tab_with_results(1);
        let path = deep_path();
        tab.results[0].path = path.clone();

        run_frame(&ctx, &mut tab, vec![]); // settle the table's layout
        for y in 40..250 {
            // x lands in the Path column, past the 220pt Name column.
            let pos = egui::pos2(300.0, y as f32);
            let mut out = run_frame(&ctx, &mut tab, vec![egui::Event::PointerMoved(pos)]);
            if tab.hovered_row != Some(0) {
                continue;
            }
            // The tooltip is its own area, so it may land a frame behind.
            for _ in 0..3 {
                if painted_text(&out).contains(&path) {
                    return;
                }
                out = run_frame(&ctx, &mut tab, vec![]);
            }
        }
        panic!("the full path never appeared on hover");
    }

    fn click(pos: egui::Pos2, button: egui::PointerButton) -> Vec<egui::Event> {
        [true, false]
            .into_iter()
            .map(|pressed| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::default(),
            })
            .collect()
    }

    fn hit(id: i64, name: &str, rank: f64, size: u64) -> SearchHit {
        SearchHit {
            file_id: id,
            name: name.to_string(),
            path: format!("/d/{name}"),
            size,
            mtime: 1_700_000_000,
            rank,
            stage: rank as u8,
            snippet: None,
        }
    }

    fn batch(tab: &mut SearchTab, hits: Vec<SearchHit>) {
        tab.apply_update(
            SearchUpdate::Hits {
                generation: tab.generation,
                hits,
            },
            1000,
        );
        if tab.sort_dirty {
            tab.resort();
        }
    }

    /// A tab already past the fade, so batches land straight in `results`.
    fn streaming_tab() -> SearchTab {
        let mut tab = SearchTab::new(false);
        tab.focus_query = false;
        tab.query = "zebra".into();
        tab.swap_pending = false;
        tab
    }

    fn displayed(tab: &SearchTab) -> Vec<&str> {
        tab.order
            .iter()
            .map(|&i| tab.results[i as usize].name.as_str())
            .collect()
    }

    /// The cascade streams each pass as it scans, so a better-ranked hit can
    /// arrive after a worse one. The table has to stay ordered by its keyed
    /// column as batches land, not merely append them.
    #[test]
    fn later_batches_land_in_the_tables_sort_order() {
        let mut tab = streaming_tab();
        batch(&mut tab, vec![hit(1, "middling.txt", 4.0, 50)]);
        assert_eq!(displayed(&tab), vec!["middling.txt"]);

        // A rank-1 hit found later in the scan belongs on top.
        batch(&mut tab, vec![hit(2, "best.txt", 1.0, 10)]);
        assert_eq!(displayed(&tab), vec!["best.txt", "middling.txt"]);

        // And a rank-10 one belongs at the bottom, not wherever it arrived.
        batch(&mut tab, vec![hit(3, "worst.txt", 10.0, 90)]);
        assert_eq!(
            displayed(&tab),
            vec!["best.txt", "middling.txt", "worst.txt"]
        );
    }

    /// Rank is only the default. Under any other key, arrivals slot into that
    /// key's order.
    #[test]
    fn batches_respect_a_non_rank_sort_key() {
        let mut tab = streaming_tab();
        tab.sort = (SortKey::Name, true);
        batch(&mut tab, vec![hit(1, "mango.txt", 1.0, 50)]);
        batch(&mut tab, vec![hit(2, "apple.txt", 9.0, 10)]);
        batch(&mut tab, vec![hit(3, "zucchini.txt", 2.0, 90)]);
        assert_eq!(
            displayed(&tab),
            vec!["apple.txt", "mango.txt", "zucchini.txt"],
            "name order, not arrival or rank order"
        );

        tab.sort = (SortKey::Size, false);
        tab.sort_dirty = true;
        tab.resort();
        assert_eq!(
            displayed(&tab),
            vec!["zucchini.txt", "mango.txt", "apple.txt"]
        );
    }

    /// The user may re-key the sort at any time, including while results are
    /// still arriving: rows already shown must re-order, and later batches
    /// must land under the new key.
    #[test]
    fn re_keying_the_sort_mid_stream_reorders_everything() {
        let mut tab = streaming_tab();
        batch(&mut tab, vec![hit(1, "delta.txt", 1.0, 30)]);
        batch(&mut tab, vec![hit(2, "alpha.txt", 5.0, 10)]);
        assert_eq!(
            displayed(&tab),
            vec!["delta.txt", "alpha.txt"],
            "rank order"
        );

        // Header click, mid-search.
        tab.sort = (SortKey::Name, true);
        tab.sort_dirty = true;
        tab.resort();
        assert_eq!(
            displayed(&tab),
            vec!["alpha.txt", "delta.txt"],
            "rows that already arrived re-order under the new key"
        );

        batch(&mut tab, vec![hit(3, "bravo.txt", 2.0, 20)]);
        assert_eq!(
            displayed(&tab),
            vec!["alpha.txt", "bravo.txt", "delta.txt"],
            "and the next batch lands under it too"
        );
    }

    /// At the display cap, retention stays keyed on rank even when the table
    /// is shown in another order — otherwise a broad query fills up with
    /// whatever the scan reached first and never shows the good hits.
    #[test]
    fn a_late_better_hit_displaces_the_worst_at_the_cap() {
        let mut tab = streaming_tab();
        tab.sort = (SortKey::Name, true);
        let limit = 3;

        let send = |tab: &mut SearchTab, hits: Vec<SearchHit>| {
            tab.apply_update(
                SearchUpdate::Hits {
                    generation: tab.generation,
                    hits,
                },
                limit,
            );
            if tab.sort_dirty {
                tab.resort();
            }
        };

        send(
            &mut tab,
            vec![
                hit(1, "aaa.txt", 9.0, 10),
                hit(2, "bbb.txt", 8.0, 20),
                hit(3, "ccc.txt", 7.0, 30),
            ],
        );
        assert_eq!(displayed(&tab), vec!["aaa.txt", "bbb.txt", "ccc.txt"]);
        assert!(!tab.limited);

        // Full. A rank-1 arrival must still get in, evicting rank 9.
        send(&mut tab, vec![hit(4, "zzz.txt", 1.0, 40)]);
        assert!(tab.limited, "the cap was hit");
        assert_eq!(tab.results.len(), limit);
        assert_eq!(
            displayed(&tab),
            vec!["bbb.txt", "ccc.txt", "zzz.txt"],
            "worst rank dropped, display still in name order"
        );
    }

    /// Batches arriving while the old table wipes away get the same
    /// treatment; the ordering problem must not simply move inside
    /// `FADE_OUT_SECS`.
    #[test]
    fn staged_batches_are_ordered_once_the_fade_swaps() {
        let mut tab = SearchTab::new(false);
        tab.focus_query = false;
        tab.query = "zebra".into();
        tab.on_search_started(1);
        assert!(tab.swap_pending);

        for h in [hit(1, "worst.txt", 9.0, 10), hit(2, "best.txt", 1.0, 20)] {
            tab.apply_update(
                SearchUpdate::Hits {
                    generation: 1,
                    hits: vec![h],
                },
                1000,
            );
        }
        assert!(tab.results.is_empty(), "still staged behind the fade");

        // What the fade does when it reaches zero.
        tab.results = std::mem::take(&mut tab.staging);
        tab.swap_pending = false;
        tab.sort_dirty = true;
        tab.resort();
        assert_eq!(displayed(&tab), vec!["best.txt", "worst.txt"]);
    }

    /// Step the transition at a steady 60 fps until `done`, and report how
    /// long it took.
    fn run_fade(tab: &mut SearchTab, done: impl Fn(&SearchTab) -> bool) -> f32 {
        let dt = 1.0 / 60.0;
        for frame in 0..1000 {
            if done(tab) {
                return frame as f32 * dt;
            }
            tab.advance_fade(dt);
        }
        panic!("the transition never finished");
    }

    #[test]
    fn each_half_of_the_transition_takes_its_own_duration() {
        let mut tab = SearchTab::new(false);
        tab.swap_pending = true;
        let out = run_fade(&mut tab, |t| t.fade <= 0.0);
        assert_eq!(tab.fade, 0.0, "settles exactly on invisible");
        assert!(
            (out - FADE_OUT_SECS).abs() <= 1.0 / 60.0,
            "clearing runs for FADE_OUT_SECS, took {out}"
        );

        // What `ui` does at the swap.
        tab.swap_pending = false;
        tab.wipe = 1.0;
        let into = run_fade(&mut tab, |t| t.wipe <= 0.0);
        assert_eq!(tab.fade, 1.0, "and the reveal settles on fully opaque");
        assert!(
            (into - FADE_IN_SECS).abs() <= 1.0 / 60.0,
            "the reveal runs for FADE_IN_SECS, took {into}"
        );
    }

    #[test]
    fn a_stalled_frame_does_not_overshoot() {
        let mut tab = SearchTab::new(false);
        tab.swap_pending = true;
        tab.advance_fade(10.0);
        assert_eq!(tab.fade, 0.0, "a whole ten seconds lands, not passes");

        tab.swap_pending = false;
        tab.wipe = 1.0;
        tab.advance_fade(10.0);
        assert_eq!(tab.wipe, 0.0);
        assert_eq!(tab.fade, 1.0);
    }

    /// Clearing the old results is a plain fade. Nothing about the reveal
    /// runs backwards — a search fired part way through the previous one's
    /// wipe would otherwise uncover the rows it had just covered, flashing
    /// them back on screen on the very frame they were told to leave.
    #[test]
    fn clearing_results_holds_the_reveal_where_it_stands() {
        let mut tab = SearchTab::new(false);
        tab.wipe = 1.0;
        tab.advance_fade(FADE_IN_SECS / 5.0);
        let standing = tab.wipe;
        assert!((standing - 0.8).abs() < 1e-4, "one fifth in: {standing}");
        let carried = tab.fade;

        tab.swap_pending = true;
        tab.advance_fade(0.0);
        assert_eq!(tab.wipe, standing, "the reveal is frozen, not rewound");
        assert_eq!(tab.fade, carried, "and the opacity carries on from here");

        // The scrim holds its position for the whole of the fade-out, and
        // the opacity takes the full FADE_OUT_SECS to get from here to zero.
        let out = run_fade(&mut tab, |t| t.fade <= 0.0);
        assert_eq!(tab.wipe, standing, "still frozen at the end of it");
        let expected = carried * FADE_OUT_SECS;
        assert!(
            (out - expected).abs() <= 1.0 / 60.0,
            "a partly faded section clears proportionally: {out} vs {expected}"
        );
    }

    #[test]
    fn a_settled_section_stops_asking_for_frames() {
        let mut tab = SearchTab::new(false);
        // Nothing pending, nothing covered, nothing dimmed: the steady state
        // must not repaint forever.
        assert!(tab.fade_settled());
        tab.advance_fade(1.0 / 60.0);
        assert!(tab.fade_settled());

        // Whereas each stage of a transition keeps the frames coming, the
        // swap frame — cleared out but not yet revealing — included.
        tab.swap_pending = true;
        assert!(!tab.fade_settled());
        tab.advance_fade(FADE_OUT_SECS);
        tab.swap_pending = false;
        tab.wipe = 1.0;
        assert!(!tab.fade_settled());
    }

    /// Drive frames until the reveal has uncovered all but `to` of the
    /// section, and hand back the frame it got there on. `run_frame` leaves
    /// `RawInput::time` unset, so egui advances its own clock a predicted
    /// frame at a time and the tab sees a steady `stable_dt`.
    fn reveal_to(ctx: &egui::Context, tab: &mut SearchTab, to: f32) -> egui::FullOutput {
        for _ in 0..200 {
            let out = run_frame(ctx, tab, vec![]);
            if tab.wipe <= to {
                return out;
            }
        }
        panic!("the reveal never got down to {to}");
    }

    /// Drive frames until the staged results swap in, and hand back the
    /// frame it happened on — the one where the new set is fully covered
    /// and the reveal is about to start.
    fn swap_in(ctx: &egui::Context, tab: &mut SearchTab) -> egui::FullOutput {
        for _ in 0..200 {
            let out = run_frame(ctx, tab, vec![]);
            if !tab.swap_pending {
                return out;
            }
        }
        panic!("the staged results never swapped in");
    }

    /// Twenty staged hits under whatever generation is in flight.
    fn stage_results(tab: &mut SearchTab) {
        batch(
            tab,
            (0..20)
                .map(|i| hit(i, &format!("alpha_widget_{i}.txt"), 3.0, 116))
                .collect(),
        );
    }

    /// Where the first and last result rows were painted.
    fn row_bounds(out: &egui::FullOutput) -> (egui::Rect, egui::Rect) {
        let rows: Vec<egui::Rect> = crate::test_ui::painted(out)
            .into_iter()
            .filter(|(text, _)| text.starts_with("alpha_widget_"))
            .map(|(_, rect)| rect)
            .collect();
        (
            *rows.first().expect("rows painted"),
            *rows.last().expect("rows painted"),
        )
    }

    /// Vertices down the scrim as (y, alpha), in paint order.
    fn scrim(out: &egui::FullOutput) -> Vec<(f32, u8)> {
        let meshes = crate::test_ui::painted_meshes(out);
        assert_eq!(meshes.len(), 1, "one scrim over the section, no more");
        meshes[0]
            .vertices
            .iter()
            .map(|v| (v.pos.y, v.color.a()))
            .collect()
    }

    /// The y where the scrim first turns fully solid — the edge of what is
    /// still hidden.
    fn solid_from(ramp: &[(f32, u8)]) -> f32 {
        ramp.iter()
            .find(|&&(_, a)| a == 255)
            .expect("a solid stretch")
            .0
    }

    /// Clearing the old results paints no scrim at all: it is a plain dip
    /// to nothing, so there is no edge travelling anywhere on the way out.
    #[test]
    fn clearing_results_paints_no_scrim() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_results(20);

        let settled = run_frame(&ctx, &mut tab, vec![]);
        assert!(
            crate::test_ui::painted_meshes(&settled).is_empty(),
            "a settled table pays nothing for the effect"
        );

        tab.on_search_started(1);
        stage_results(&mut tab);
        let mut dimmest: f32 = 1.0;
        for frame in 0..200 {
            let out = run_frame(&ctx, &mut tab, vec![]);
            if !tab.swap_pending {
                // The swap frame belongs to the new set, which starts
                // covered — checked separately below.
                assert!(frame > 0, "the fade-out was over before it began");
                break;
            }
            assert!(
                crate::test_ui::painted_meshes(&out).is_empty(),
                "no scrim while the old results clear, at fade {}",
                tab.fade
            );
            dimmest = dimmest.min(tab.fade);
        }
        assert!(
            dimmest < 0.35,
            "the section really does dim on the way out: got no lower than {dimmest}"
        );
    }

    /// The new set arrives fully covered, headers included. The scrim
    /// carries its own alpha — painting it through the `Ui` would have
    /// scaled it by the section-wide opacity, which is exactly zero on this
    /// frame, leaving the unsorted new table on screen at full strength.
    #[test]
    fn new_results_start_completely_covered() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_results(20);
        run_frame(&ctx, &mut tab, vec![]);

        tab.on_search_started(1);
        stage_results(&mut tab);
        let swapped = swap_in(&ctx, &mut tab);
        assert_eq!(tab.wipe, 1.0, "the reveal starts from the top");

        let ramp = scrim(&swapped);
        assert!(
            ramp.iter().all(|&(_, a)| a == 255),
            "nothing shows through: {ramp:?}"
        );
        assert!(
            !crate::test_ui::painted_text(&swapped)
                .iter()
                .any(|t| t.starts_with("alpha_widget_")),
            "at zero opacity egui drops the section's shapes outright, so \
             the scrim is belt to that braces"
        );

        // Which is also why the rows have to be measured from a frame the
        // reveal has let some light through.
        let (first, last) = row_bounds(&reveal_to(&ctx, &mut tab, 0.8));
        let (top, bottom) = (
            ramp.first().expect("vertices").0,
            ramp.last().expect("vertices").0,
        );
        assert!(
            top < first.top(),
            "the scrim starts above the first row, so the column headers \
             are covered too: {top} vs {}",
            first.top()
        );
        assert!(
            bottom >= last.bottom(),
            "and runs past the last one: {bottom} vs {}",
            last.bottom()
        );
    }

    /// The reveal uncovers the table from the top down, and gets to the
    /// first rows early — which is the whole reason it can afford to run
    /// for half a second.
    #[test]
    fn new_results_are_uncovered_from_the_top_down() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_results(20);
        run_frame(&ctx, &mut tab, vec![]);

        tab.on_search_started(1);
        stage_results(&mut tab);
        swap_in(&ctx, &mut tab);

        // A fifth of the way in: the head of the table is out from behind
        // the scrim while the foot is still under it.
        let early_frame = reveal_to(&ctx, &mut tab, 0.8);
        let (first, last) = row_bounds(&early_frame);
        let early = solid_from(&scrim(&early_frame));
        assert!(
            early > first.bottom(),
            "the first row is readable a fifth of the way in: {early} vs {}",
            first.bottom()
        );
        assert!(
            early < last.top(),
            "while the last is still covered: {early} vs {}",
            last.top()
        );

        // …and the edge keeps going down, not back up.
        let later = solid_from(&scrim(&reveal_to(&ctx, &mut tab, 0.4)));
        assert!(
            later > early,
            "the edge travels downward: {early} then {later}"
        );
        assert!(
            later > last.top(),
            "and has uncovered the last row by then: {later} vs {}",
            last.top()
        );

        // It ends with the scrim gone entirely rather than lingering.
        let done = reveal_to(&ctx, &mut tab, 0.0);
        assert!(
            crate::test_ui::painted_meshes(&done).is_empty(),
            "the scrim clears away at the end of the reveal"
        );
        assert!(tab.fade_settled(), "and the section stops animating");
    }

    /// A selected row is identified by file id, so it survives both the
    /// re-ordering and the eviction that a new batch can cause.
    #[test]
    fn the_selection_follows_its_file_across_batches() {
        let mut tab = streaming_tab();
        batch(&mut tab, vec![hit(1, "chosen.txt", 5.0, 10)]);
        tab.selected = Some(0);

        batch(&mut tab, vec![hit(2, "better.txt", 1.0, 20)]);
        let sel = tab.selected.expect("still selected");
        assert_eq!(
            tab.results[sel as usize].file_id, 1,
            "selection follows the file, not the slot"
        );
    }

    #[test]
    fn rows_respond_over_selectable_label_text() {
        let ctx = egui::Context::default();
        let mut tab = tab_with_results(3);
        run_frame(&ctx, &mut tab, Vec::new());

        // Hovering glyphs still marks the row hovered (drives the hover fill).
        let pos = hover_row_text(&ctx, &mut tab, 0);

        // Left click on glyphs selects the row.
        run_frame(&ctx, &mut tab, click(pos, egui::PointerButton::Primary));
        assert_eq!(tab.selected, Some(0));

        // Right click on glyphs selects the row and opens the context menu.
        let pos = hover_row_text(&ctx, &mut tab, 1);
        run_frame(&ctx, &mut tab, click(pos, egui::PointerButton::Secondary));
        assert_eq!(tab.selected, Some(1));
        assert!(egui::Popup::is_any_open(&ctx));
    }

    /// `Path::join` adds a separator only where one is needed, so the
    /// pattern is spelled natively and a drive root does not become the
    /// never-matching `C:\/*`.
    #[test]
    fn dir_ignore_patterns_use_the_platform_separator() {
        use std::path::Path;
        #[cfg(unix)]
        {
            assert_eq!(dir_ignore_pattern(Path::new("/home/x")), "/home/x/*");
            assert_eq!(dir_ignore_pattern(Path::new("/")), "/*");
        }
        #[cfg(windows)]
        {
            assert_eq!(
                dir_ignore_pattern(Path::new(r"C:\Users\x")),
                r"C:\Users\x\*"
            );
            assert_eq!(dir_ignore_pattern(Path::new(r"C:\")), r"C:\*");
        }
    }

    /// The rank chips read as a jet colorbar: blue at the best ranks
    /// warming monotonically to red at the worst, and never so dark that
    /// the chip's fixed dark text loses its contrast. Stage 12 stands in
    /// for the catch-all arm.
    #[test]
    fn the_rank_ramp_runs_blue_to_red_and_stays_light() {
        let ramp: Vec<egui::Color32> = (1..=11).map(rank_tier_color).collect();
        let (first, last) = (ramp[0], ramp[10]);
        assert!(
            first.b() > first.r(),
            "the best rank should be blue: {first:?}"
        );
        assert!(
            last.r() > last.b(),
            "the worst rank should be red: {last:?}"
        );

        for pair in ramp.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(a.r() <= b.r(), "red must not cool off: {a:?} then {b:?}");
            assert!(a.b() >= b.b(), "blue must not warm up: {a:?} then {b:?}");
        }

        for stage in 1..=12u8 {
            let c = rank_tier_color(stage);
            assert!(
                c.r() >= 127 && c.g() >= 127 && c.b() >= 127,
                "stage {stage} is too dark for the chip's dark text: {c:?}"
            );
        }
        assert_eq!(
            rank_tier_color(12),
            last,
            "out-of-range stages share the fuzzy-path chip"
        );
    }

    // --- snippet rendering ----------------------------------------------

    use crate::test_ui::{painted_rows, with_ui};

    /// The rows `job` actually lays out: what the user sees, as opposed to
    /// the text the job was built from. `Galley::text` is the latter — it
    /// hands back the whole job, including every row epaint dropped at
    /// `wrap.max_rows` — so it cannot see a truncation at all.
    fn laid_out_rows(ui: &egui::Ui, job: LayoutJob) -> Vec<String> {
        ui.fonts(|f| f.layout_job(job))
            .rows
            .iter()
            .map(|r| r.text())
            .collect()
    }

    /// A content snippet whose lead-in is `lines` short lines — the shape
    /// that used to eat the whole row budget before the hit was reached.
    /// The lead-in is multi-byte on purpose: snippet ranges are byte
    /// offsets while row arithmetic counts characters.
    fn ragged_snippet(lines: usize) -> Snippet {
        let lead = "café\n".repeat(lines);
        Snippet {
            ranges: vec![(lead.len(), lead.len() + 6)],
            window: format!("{lead}NEEDLE and trailing context"),
            truncated_start: true,
            truncated_end: true,
        }
    }

    /// The mouseover exists to show the hit in context. A window whose
    /// lead-in is dozens of short lines spends the whole row budget before
    /// layout reaches the hit, and the tooltip ends up showing context with
    /// nothing in it to be context *for*.
    #[test]
    fn the_hover_snippet_keeps_the_match_when_the_lead_in_is_all_newlines() {
        with_ui(|ui| {
            ui.set_max_width(520.0); // what the Match cell's tooltip sets
            let snip = ragged_snippet(40);
            let rows = laid_out_rows(ui, snippet_job(ui, &snip, 10));
            assert!(rows.len() <= 10, "over the row budget: {rows:#?}");
            assert!(
                rows.iter().any(|r| r.contains("NEEDLE")),
                "the match never made it on screen: {rows:#?}"
            );
            // Trimmed at a line boundary and said so. A start landing mid
            // character would read "…afé" — or panic on a byte offset that
            // is not a char boundary.
            assert_eq!(rows[0], "… café", "{rows:#?}");
        });
    }

    /// Same bug, worse: the preview strip under the table has two rows to
    /// spend, so a single stray newline in the lead-in is enough.
    #[test]
    fn the_preview_strip_keeps_the_match_too() {
        with_ui(|ui| {
            let snip = ragged_snippet(40);
            let rows = laid_out_rows(ui, snippet_job(ui, &snip, 2));
            assert!(rows.len() <= 2, "over the row budget: {rows:#?}");
            assert!(
                rows.iter().any(|r| r.contains("NEEDLE")),
                "the match never made it on screen: {rows:#?}"
            );
        });
    }

    /// A window that already fits is rendered exactly as it arrived: no
    /// trimming, and no ellipsis for a trim that did not happen.
    #[test]
    fn a_snippet_that_fits_is_left_alone() {
        with_ui(|ui| {
            let snip = Snippet {
                window: "alpha beta NEEDLE gamma".into(),
                ranges: vec![(11, 17)],
                truncated_start: false,
                truncated_end: false,
            };
            assert_eq!(
                laid_out_rows(ui, snippet_job(ui, &snip, 10)),
                vec!["alpha beta NEEDLE gamma".to_string()]
            );
        });
    }

    /// The Match cell is laid out in Extend mode — egui hands it an infinite
    /// wrap width — so nothing but the cell's own budget keeps it inside the
    /// column, and an overshoot is clipped on *both* sides with no ellipsis,
    /// which in a narrow column eats the highlighted match itself.
    #[test]
    fn the_match_cell_stays_inside_its_column() {
        with_ui(|ui| {
            let snip = Snippet {
                window: "a long stretch of leading context NEEDLE and a long tail after it".into(),
                ranges: vec![(34, 40)],
                truncated_start: true,
                truncated_end: true,
            };
            // Down to widths the column itself cannot reach, so the budget
            // degrades rather than overflowing.
            for width in [20.0, 60.0, 90.0, 120.0, 150.0, 240.0, 400.0, 4000.0] {
                for whole_field in [false, true] {
                    let job = centered_match_job(ui, &snip, width, whole_field);
                    let galley = ui.fonts(|f| f.layout_job(job));
                    assert!(
                        galley.size().x <= width,
                        "{}pt of text in a {width}pt column (whole_field={whole_field}): {:?}",
                        galley.size().x,
                        galley.text()
                    );
                    // The Match column is `Column::remainder().at_least(120.0)`,
                    // so anything that wide has to keep the whole hit; below
                    // that, only its head can be shown, but it is still the
                    // hit that gets the room rather than the context.
                    let kept = if width >= 120.0 { "NEEDLE" } else { "N" };
                    assert!(
                        galley.text().contains(kept),
                        "the match was budgeted away at {width}pt: {:?}",
                        galley.text()
                    );
                }
            }
        });
    }

    /// End to end: hovering the Match cell puts the hit on screen. The cell
    /// paints the match once by itself, so the tooltip is the *second*
    /// appearance — asserting on one would pass with the bug present.
    #[test]
    fn hovering_the_match_cell_shows_the_match_in_the_tooltip() {
        let ctx = egui::Context::default();
        // Testing that the tooltip carries the match, not egui's hover timing.
        ctx.style_mut(|s| {
            s.interaction.tooltip_delay = 0.0;
            s.interaction.show_tooltips_only_when_still = false;
        });
        let mut tab = tab_with_results(1);
        tab.has_snippets = true;
        tab.results[0].stage = 6; // a full-text stage: no [brackets]
        tab.results[0].snippet = Some(ragged_snippet(40));

        run_frame(&ctx, &mut tab, vec![]); // settle the table's layout
        for y in 40..250 {
            // x lands in the Match column, past Name and Path.
            let pos = egui::pos2(600.0, y as f32);
            let mut out = run_frame(&ctx, &mut tab, vec![egui::Event::PointerMoved(pos)]);
            if tab.hovered_row != Some(0) {
                continue;
            }
            // The tooltip is its own area, so it may land a frame behind.
            for _ in 0..3 {
                let showing = painted_rows(&out)
                    .iter()
                    .filter(|r| r.contains("NEEDLE"))
                    .count();
                if showing >= 2 {
                    return;
                }
                out = run_frame(&ctx, &mut tab, vec![]);
            }
        }
        panic!("the match never appeared in the hover tooltip");
    }
}
