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

    /// The query has executed and the fade/stage swap has landed — what the
    /// capture driver's `wait_search_done` means by "done".
    #[cfg(feature = "capture")]
    pub(crate) fn capture_settled(&self) -> bool {
        !self.running && self.pending_edit.is_none() && !self.swap_pending
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

        // Result-set transitions pulse instead of strobing: the old table
        // fades out over 0.15 s while the new hits stage, the sets swap at
        // zero opacity, and the new table fades back in over 0.15 s.
        // `animate_value_with_time` keeps requesting repaints until the
        // value settles.
        let fade_target = if self.swap_pending { 0.0 } else { 1.0 };
        let fade =
            ui.ctx()
                .animate_value_with_time(egui::Id::new("qs-results-fade"), fade_target, 0.15);
        if self.swap_pending && fade <= 0.01 {
            self.results = std::mem::take(&mut self.staging);
            self.has_snippets = self.staging_has_snippets;
            self.selected = None;
            self.swap_pending = false;
            // Staged hits arrived in scan order too, so the table has to be
            // ordered here as well — including under the default key.
            self.sort_dirty = true;
        }

        if self.sort_dirty {
            self.resort();
        }

        // Fade covers the table and the preview strip below it; the modal
        // windows and notices render at full opacity on their own layers.
        ui.set_opacity(fade);

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

/// Build a highlighted snippet LayoutJob from byte ranges, wrapped to at
/// most `max_rows`. Cheap enough to run per visible row per frame.
fn snippet_job(ui: &egui::Ui, snip: &Snippet, max_rows: usize) -> LayoutJob {
    let fmt = snippet_formats(ui);
    let mut job = LayoutJob::default();
    job.wrap.max_rows = max_rows;
    if max_rows == 1 {
        job.wrap.break_anywhere = true;
    }
    if snip.truncated_start {
        job.append("… ", 0.0, fmt.weak.clone());
    }
    let mut cursor = 0;
    for &(start, end) in &snip.ranges {
        if start > cursor {
            job.append(&snip.window[cursor..start], 0.0, fmt.normal.clone());
        }
        job.append(&snip.window[start..end], 0.0, fmt.highlight.clone());
        cursor = end;
    }
    if cursor < snip.window.len() {
        job.append(&snip.window[cursor..], 0.0, fmt.normal.clone());
    }
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
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let char_width = ui.fonts(|f| f.glyph_width(&font_id, '0')).max(1.0);
    let mut budget = ((width_px / char_width) as usize).saturating_sub(2).max(8);
    if whole_field {
        budget = budget.saturating_sub(2); // room for the brackets
    }

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
    let (start, end) = match snip.ranges.first().copied() {
        Some((a, b)) => {
            let match_chars = window[a..b].chars().count();
            let side = budget.saturating_sub(match_chars) / 2;
            let before = &window[..a];
            let after = &window[b..];
            let before_count = before.chars().count();
            let after_count = after.chars().count();
            // Equal context on both sides; leftover budget from a short
            // side flows to the other.
            let take_before = (side + side.saturating_sub(after_count)).min(before_count);
            let take_after = (side + side.saturating_sub(before_count)).min(after_count);
            let start = if take_before == 0 {
                a
            } else {
                before
                    .char_indices()
                    .nth_back(take_before - 1)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            };
            let end = b + after
                .char_indices()
                .nth(take_after)
                .map(|(i, _)| i)
                .unwrap_or(after.len());
            (start, end)
        }
        None => {
            // No ranges (shouldn't happen for match cells) — head trim.
            let end = window
                .char_indices()
                .nth(budget)
                .map(|(i, _)| i)
                .unwrap_or(window.len());
            (0, end)
        }
    };

    let mut job = LayoutJob::default();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    if whole_field {
        job.append("[", 0.0, fmt.weak.clone());
    }
    if start > 0 || snip.truncated_start {
        job.append("…", 0.0, fmt.weak.clone());
    }
    let mut cursor = start;
    for &(a, b) in &snip.ranges {
        let (a, b) = (a.max(start), b.min(end));
        if a >= b || a >= end {
            continue;
        }
        if a > cursor {
            job.append(&window[cursor..a], 0.0, fmt.normal.clone());
        }
        job.append(&window[a..b], 0.0, fmt.highlight.clone());
        cursor = b;
    }
    if cursor < end {
        job.append(&window[cursor..end], 0.0, fmt.normal.clone());
    }
    if end < window.len() || snip.truncated_end {
        job.append("…", 0.0, fmt.weak.clone());
    }
    if whole_field {
        job.append("]", 0.0, fmt.weak);
    }
    job
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

    /// Batches arriving during the fade get the same treatment; the ordering
    /// problem must not simply move inside the 250 ms window.
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
}
