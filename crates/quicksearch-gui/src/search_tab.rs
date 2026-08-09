//! The Search tab: query strip, streaming results table, snippet
//! preview, context menu, ignore-filter dialog, and syntax help.

use std::time::Instant;

use egui::text::{LayoutJob, TextFormat};
use egui_extras::{Column, TableBuilder};
use quicksearch_core::search::{SearchHit, SearchUpdate};
use quicksearch_core::snippet::Snippet;

use crate::color::rank_tier_color;
use crate::format::{fmt_elapsed, fmt_mtime, human_size};
use crate::platform;
use crate::ui_util::middle_elide;

mod help_window;
mod ignore_dialog;
mod snippet_render;
#[cfg(test)]
mod tests;

use crate::ui_util::hint;
use ignore_dialog::dir_ignore_pattern;
pub use ignore_dialog::IgnoreDialog;
use snippet_render::{centered_match_job, snippet_job};

/// Fixed width (points) of the query strip's status slot, sized for the
/// longest `fmt_elapsed` output, so the query box never resizes.
const STATUS_SLOT_WIDTH: f32 = 52.0;

/// Seconds for the old results to fade out; the swap waits on this.
const FADE_OUT_SECS: f32 = 0.15;
/// Seconds for the new results to wipe in from the top.
const FADE_IN_SECS: f32 = 0.50;
/// Fraction of the reveal over which section-wide opacity climbs to full.
const FADE_ALPHA_SPAN: f32 = 0.50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Rank,
    Name,
    Path,
    Size,
    Modified,
}

/// What the tab asks the app to do after this frame.
#[derive(Default)]
pub struct SearchActions {
    /// Re-run the search immediately (not debounced).
    pub rerun: bool,
    /// Persist an ignore pattern into the config.
    pub persist_ignore: Option<String>,
    /// The fuzzy toggle changed; remember it in the config.
    pub save_fuzzy_default: Option<bool>,
}

/// Add `incoming` to `set`, keeping at most `limit` of them — the best by
/// **rank**, whatever column the table is currently sorted by, so a good hit
/// found late in a scan still displaces a bad one found early.
fn admit(set: &mut Vec<SearchHit>, incoming: Vec<SearchHit>, limit: usize, limited: &mut bool) {
    set.extend(incoming);
    if set.len() > limit {
        set.sort_by(|a, b| a.rank.total_cmp(&b.rank).then_with(|| a.path.cmp(&b.path)));
        set.truncate(limit);
        *limited = true;
    }
}

/// egui_extras cell wrapper: one widget, centered and filling the cell.
fn centered_cell<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.with_layout(
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        contents,
    )
    .inner
}

pub struct SearchTab {
    pub query: String,
    pub fuzzy: bool,
    /// Set on every edit; the app fires the search after the debounce.
    pub pending_edit: Option<Instant>,
    pub generation: u64,
    pub results: Vec<SearchHit>,
    /// The next search's hits, swapped into `results` at zero opacity.
    staging: Vec<SearchHit>,
    staging_has_snippets: bool,
    /// True from search start until the staged set has been swapped in.
    swap_pending: bool,
    /// How much of the results section the reveal still hides: 1 at the swap, 0 fully shown.
    wipe: f32,
    /// The section's own opacity for the fade/wipe transition.
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
    /// Display-row index hovered last frame; tracked via `contains_pointer()`
    /// because `row.response().hovered()` is false whenever a selectable label
    /// wins the hit-test.
    hovered_row: Option<usize>,
    focus_query: bool,
    /// Query syntax-highlight segments, cached per text.
    highlight: crate::query_highlight::HighlightCache,
    /// Screen rects of last frame's Match cells, in display order — the
    /// capture driver's hover targets.
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

    /// Pre-fill the query and let the normal debounce path run it.
    pub fn seed(&mut self, query: String) {
        self.query = query;
        self.pending_edit = Some(Instant::now());
    }

    /// What the capture driver's `wait_search_done` means by "done": query
    /// executed, swap landed, and the wipe finished — a screenshot during the
    /// reveal catches a half-drawn table.
    #[cfg(feature = "capture")]
    pub(crate) fn capture_settled(&self) -> bool {
        !self.running && self.pending_edit.is_none() && self.fade_settled()
    }

    /// Re-arm the one-shot first-frame focus (tab switches drop egui focus).
    pub(crate) fn request_focus(&mut self) {
        self.focus_query = true;
    }

    /// Screen rect of the Nth visible Match cell from the last rendered
    /// frame, if that many are on screen.
    #[cfg(feature = "capture")]
    pub(crate) fn capture_match_cell(&self, n: usize) -> Option<egui::Rect> {
        self.capture_match_rects.get(n).copied()
    }

    /// A new search was submitted under `generation`; its hits stage until
    /// the old results fade out.
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

    /// Move the transition on by `dt` seconds. Clearing the old results holds
    /// `wipe` still — a search fired mid-reveal must not flash already-covered
    /// rows back on screen on the way out.
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
                    self.staging_has_snippets |= hits.iter().any(|h| h.snippet.is_some());
                    admit(&mut self.staging, hits, display_limit, &mut self.limited);
                } else {
                    self.has_snippets |= hits.iter().any(|h| h.snippet.is_some());
                    // Admitting a batch may reorder or drop rows out from
                    // under the selection's index, so carry it by file id.
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
                    // Hits arrive in scan order, not rank order; re-sort on
                    // every batch.
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
                SortKey::Rank => a.rank.total_cmp(&b.rank),
                SortKey::Name => a.name.cmp(&b.name),
                SortKey::Path => a.path.cmp(&b.path),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.mtime.cmp(&b.mtime),
            };
            let ord = if ascending { ord } else { ord.reverse() };
            // Tie-break on the unique path so equal keys don't shuffle as
            // batches stream in.
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

    /// A sortable column header. The sort indicator is a painter-drawn
    /// triangle: the default egui fonts have no ▲/▼ glyphs — they render
    /// as boxes.
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

        // --- Query strip: laid out right to left, the query box taking
        // whatever is left of the row. -------------------------------------
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
                let show_elapsed =
                    !self.running && self.elapsed.is_some() && !self.query.trim().is_empty();
                ui.allocate_ui_with_layout(
                    egui::vec2(STATUS_SLOT_WIDTH, ui.spacing().interact_size.y),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        // Hold the width from the inside — the child otherwise
                        // shrinks to its content.
                        ui.set_min_width(STATUS_SLOT_WIDTH);
                        if self.running {
                            ui.add(egui::Spinner::new().size(16.0));
                        } else if show_elapsed {
                            if let Some(elapsed) = self.elapsed {
                                ui.label(hint(fmt_elapsed(elapsed)))
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
                    // Select the existing text, written straight to widget
                    // state so the selection is in place the frame focus lands.
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), response.id) {
                        let all = egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(self.query.chars().count()),
                        );
                        state.cursor.set_char_range(Some(all));
                        state.store(ui.ctx(), response.id);
                    }
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
                ui.label(hint("Ignoring:"));
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
            ui.label(hint("No results."));
        }

        // Repaints have to be asked for by hand, since the fade/wipe values
        // are ours rather than the animation manager's — and the swap frame
        // needs one too.
        self.advance_fade(ui.input(|i| i.stable_dt));
        if self.swap_pending && self.fade <= 0.0 {
            self.results = std::mem::take(&mut self.staging);
            self.has_snippets = self.staging_has_snippets;
            self.selected = None;
            self.swap_pending = false;
            self.wipe = 1.0;
            // Staged hits arrived in scan order; re-sort.
            self.sort_dirty = true;
        }
        if !self.fade_settled() {
            ui.ctx().request_repaint();
        }

        if self.sort_dirty {
            self.resort();
        }

        // Section-wide opacity; modal windows and the notices above render at
        // full opacity on their own layers.
        ui.set_opacity(self.fade);

        // --- Results table ------------------------------------------------
        // Reserve room for the preview strip; only content matches get one.
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
        let order = std::mem::take(&mut self.order);
        #[cfg(feature = "capture")]
        let mut capture_match_rects: Vec<egui::Rect> = Vec::new();

        // Top of the wiped section; its bottom is known only after the
        // preview strip is laid out.
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
                                centered_cell(ui, |ui| {
                                    ui.label(egui::RichText::new("Match").strong());
                                });
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

                            // Selectable labels win egui's hit-test over the
                            // row, so union their responses into the row's or
                            // clicks over glyphs would miss.
                            let mut cell_responses: Vec<egui::Response> = Vec::new();

                            row.col(|ui| {
                                cell_responses.push(ui.label(&hit.name));
                            });
                            row.col(|ui| {
                                // Center-elided: egui's own truncation would
                                // drop the deepest directories. Sizing-pass
                                // cell rects are not final, so don't measure
                                // against them.
                                let shown = if ui.is_sizing_pass() {
                                    std::borrow::Cow::Borrowed(hit.path.as_str())
                                } else {
                                    middle_elide(
                                        ui,
                                        &hit.path,
                                        // A point of slack against rounding
                                        // disagreements with egui's layout.
                                        ui.available_width() - 1.0,
                                        &body_font,
                                    )
                                };
                                let elided = matches!(shown, std::borrow::Cow::Owned(_));
                                // egui offers a full-text tooltip only when
                                // *it* elided the galley — and it is handed
                                // the already-shortened string here.
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
                                let snippet = hit.snippet.as_ref();
                                // Name and path matches show a whole field,
                                // rendered bracketed: [matched field].
                                let whole_field =
                                    hit.stage <= 4 || hit.stage == 7 || hit.stage >= 9;
                                row.col(|ui| {
                                    if let Some(snip) = snippet {
                                        let width = ui.available_width();
                                        let job = centered_match_job(ui, snip, width, whole_field);
                                        let mut response = centered_cell(ui, |ui| ui.label(job));
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
                                let response =
                                    centered_cell(ui, |ui| ui.label(human_size(hit.size)));
                                cell_responses.push(response);
                            });
                            row.col(|ui| {
                                let color = recency_color(ui, hit.mtime);
                                let response = centered_cell(ui, |ui| {
                                    ui.label(egui::RichText::new(fmt_mtime(hit.mtime)).color(color))
                                });
                                cell_responses.push(response);
                            });
                            row.col(|ui| {
                                let response = centered_cell(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(" {:.2} ", hit.rank))
                                            .background_color(rank_tier_color(hit.stage))
                                            .color(egui::Color32::from_rgb(32, 32, 32)),
                                    )
                                });
                                cell_responses.push(response);
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

        if let Some(snip) = &preview_snippet {
            ui.separator();
            let job = snippet_job(ui, snip, 2);
            ui.label(job);
        }

        // Painted last so the scrim covers the whole section, and outside the
        // opacity set above so it is not itself faded.
        let section = egui::Rect::from_x_y_ranges(
            ui.max_rect().x_range(),
            section_top..=ui.min_rect().bottom(),
        );
        crate::ui_util::wipe_scrim(ui, section, self.wipe);

        self.ignore_dialog_ui(ui.ctx(), &mut actions);
        self.help_window_ui(ui.ctx());
        actions
    }
}

/// Timestamp color: fresh files get a green tint that fades into the weak
/// text color over ~2 years on a log scale. The fade runs through OKLab —
/// blending sRGB bytes instead dips through a darker, muddier green.
fn recency_color(ui: &egui::Ui, mtime: i64) -> egui::Color32 {
    let now = quicksearch_core::log::now_unix() as i64;
    let age_hours = ((now - mtime).max(0) as f32 / 3600.0).max(1.0);
    const HORIZON_HOURS: f32 = 24.0 * 365.0 * 2.0;
    let t = (age_hours.ln() / HORIZON_HOURS.ln()).clamp(0.0, 1.0);
    let fresh = crate::color::palette(ui.visuals().dark_mode).green;
    crate::color::oklab_lerp(fresh, ui.visuals().weak_text_color(), t)
}
