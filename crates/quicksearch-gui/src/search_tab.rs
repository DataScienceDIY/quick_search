//! The Search tab: query strip, streaming results table, snippet
//! preview, context menu, ignore-filter dialog, and syntax help.

use std::time::Instant;

use egui::text::{LayoutJob, TextFormat};
use egui_extras::{Column, TableBuilder};
use quicksearch_core::search::{SearchHit, SearchUpdate};
use quicksearch_core::snippet::Snippet;

use crate::format::{fmt_elapsed, fmt_mtime, human_size};
use crate::platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Rank,
    Name,
    Path,
    Size,
    Modified,
}

pub struct IgnoreDialog {
    pub source_name: String,
    pub source_path: String,
    pub pattern: String,
    pub persist: bool,
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
    focus_query: bool,
    /// Query syntax-highlight segments, cached per text.
    highlight: crate::query_highlight::HighlightCache,
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
            focus_query: true,
            highlight: Default::default(),
        }
    }

    /// Pre-fill the query and let the normal debounce path run it, so a
    /// command-line query lands the user on results rather than an empty box.
    pub fn seed(&mut self, query: String) {
        self.query = query;
        self.pending_edit = Some(Instant::now());
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
                    for hit in hits {
                        if self.staging.len() >= display_limit {
                            self.limited = true;
                            break;
                        }
                        self.staging_has_snippets |= hit.snippet.is_some();
                        self.staging.push(hit);
                    }
                } else {
                    // Post-swap stream: later cascade passes append live.
                    for hit in hits {
                        if self.results.len() >= display_limit {
                            self.limited = true;
                            break;
                        }
                        self.has_snippets |= hit.snippet.is_some();
                        self.results.push(hit);
                    }
                    // Arrival order *is* rank order, so the default sort
                    // needs no work; anything else re-sorts on the set.
                    if self.sort != (SortKey::Rank, true) {
                        self.sort_dirty = true;
                    } else {
                        self.order = (0..self.results.len() as u32).collect();
                    }
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
                SortKey::Rank => a.rank.partial_cmp(&b.rank).unwrap_or(std::cmp::Ordering::Equal),
                SortKey::Name => a.name.cmp(&b.name),
                SortKey::Path => a.path.cmp(&b.path),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.mtime.cmp(&b.mtime),
            };
            if ascending {
                ord
            } else {
                ord.reverse()
            }
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
        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click());
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
        ui.horizontal(|ui| {
            let show_elapsed =
                !self.running && self.elapsed.is_some() && !self.query.trim().is_empty();
            let slot_room = if self.running {
                24.0
            } else if show_elapsed {
                60.0
            } else {
                0.0
            };
            let width = ui.available_width() - 170.0 - slot_room;
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
            // One slot right of the box: spinner while searching, then the
            // total wall time of all cascade passes once it lands.
            if self.running {
                ui.add(egui::Spinner::new().size(16.0));
            } else if show_elapsed {
                if let Some(elapsed) = self.elapsed {
                    ui.label(egui::RichText::new(fmt_elapsed(elapsed)).small().weak())
                        .on_hover_text("Time to run all search passes");
                }
            }
            if ui
                .checkbox(&mut self.fuzzy, "Fuzzy")
                .on_hover_text("Also run fuzzy filename and full-text passes (slower)")
                .changed()
            {
                actions.save_fuzzy_default = Some(self.fuzzy);
                actions.rerun = true;
            }
            if ui.button("?").on_hover_text("Query syntax help").clicked() {
                self.help_open = !self.help_open;
            }
        });

        // Session ignore chips.
        if !self.session_ignores.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Ignoring:").small().weak());
                let mut remove: Option<usize> = None;
                for (i, pattern) in self.session_ignores.iter().enumerate() {
                    if ui
                        .small_button(format!("{} ✕", pattern))
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
        // fades out over 0.25 s while the new hits stage, the sets swap at
        // zero opacity, and the new table fades back in over 0.25 s.
        // `animate_value_with_time` keeps requesting repaints until the
        // value settles.
        let fade_target = if self.swap_pending { 0.0 } else { 1.0 };
        let fade = ui.ctx().animate_value_with_time(
            egui::Id::new("qs-results-fade"),
            fade_target,
            0.25,
        );
        if self.swap_pending && fade <= 0.01 {
            self.results = std::mem::take(&mut self.staging);
            self.has_snippets = self.staging_has_snippets;
            self.selected = None;
            self.swap_pending = false;
            if self.sort == (SortKey::Rank, true) {
                self.order = (0..self.results.len() as u32).collect();
            } else {
                self.sort_dirty = true;
            }
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

        let text_height = egui::TextStyle::Body.resolve(ui.style()).size + 4.0;
        let mut open_ignore_dialog: Option<usize> = None;

        ui.push_id("results", |ui| {
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
                    let order = self.order.clone();
                    body.rows(text_height, order.len(), |mut row| {
                        let result_ix = order[row.index()] as usize;
                        let hit = &self.results[result_ix];
                        row.set_selected(self.selected == Some(result_ix as u32));

                        row.col(|ui| {
                            ui.label(&hit.name);
                        });
                        row.col(|ui| {
                            ui.label(egui::RichText::new(&hit.path).weak());
                        });
                        if self.has_snippets {
                            let snippet = hit.snippet.clone();
                            // Name and path matches show a whole field, so
                            // they render bracketed: [matched field].
                            let whole_field =
                                hit.stage <= 4 || hit.stage == 7 || hit.stage >= 9;
                            row.col(|ui| {
                                if let Some(snip) = &snippet {
                                    let width = ui.available_width();
                                    let job = centered_match_job(ui, snip, width, whole_field);
                                    let response = ui
                                        .with_layout(
                                            egui::Layout::centered_and_justified(
                                                egui::Direction::LeftToRight,
                                            ),
                                            |ui| ui.label(job),
                                        )
                                        .inner;
                                    if !snip.ranges.is_empty() {
                                        let hover = snip.clone();
                                        response.on_hover_ui(|ui| {
                                            ui.set_max_width(520.0);
                                            let job = snippet_job(ui, &hover, 10);
                                            ui.label(job);
                                        });
                                    }
                                }
                            });
                        }
                        row.col(|ui| {
                            ui.with_layout(
                                egui::Layout::centered_and_justified(
                                    egui::Direction::LeftToRight,
                                ),
                                |ui| {
                                    ui.label(human_size(hit.size));
                                },
                            );
                        });
                        row.col(|ui| {
                            let color = recency_color(ui, hit.mtime);
                            ui.with_layout(
                                egui::Layout::centered_and_justified(
                                    egui::Direction::LeftToRight,
                                ),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(fmt_mtime(hit.mtime)).color(color),
                                    );
                                },
                            );
                        });
                        row.col(|ui| {
                            ui.with_layout(
                                egui::Layout::centered_and_justified(
                                    egui::Direction::LeftToRight,
                                ),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(" {:.2} ", hit.rank))
                                            .background_color(rank_tier_color(hit.stage))
                                            .color(egui::Color32::from_rgb(32, 32, 32)),
                                    );
                                },
                            );
                        });

                        let response = row.response();
                        if response.clicked() {
                            self.selected = Some(result_ix as u32);
                        }
                        if response.double_clicked() {
                            platform::open_file(&self.results[result_ix].path);
                        }
                        response.context_menu(|ui| {
                            let path = self.results[result_ix].path.clone();
                            if ui.button("Open").clicked() {
                                platform::open_file(&path);
                                ui.close();
                            }
                            if ui.button("Open containing folder").clicked() {
                                platform::reveal_in_folder(&path);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Build ignore filter…").clicked() {
                                open_ignore_dialog = Some(result_ix);
                                ui.close();
                            }
                        });
                    });
                });
        });

        if let Some(ix) = open_ignore_dialog {
            let hit = &self.results[ix];
            self.ignore_dialog = Some(IgnoreDialog {
                source_name: hit.name.clone(),
                source_path: hit.path.clone(),
                pattern: hit.name.clone(),
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
        let Some(dialog) = &mut self.ignore_dialog else {
            return;
        };
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Ignore filter")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("From: {}", dialog.source_path));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("This name").clicked() {
                        dialog.pattern = dialog.source_name.clone();
                    }
                    if let Some(ext) = std::path::Path::new(&dialog.source_name)
                        .extension()
                        .and_then(|e| e.to_str())
                    {
                        if ui.button(format!("*.{}", ext)).clicked() {
                            dialog.pattern = format!("*.{}", ext);
                        }
                    }
                    if let Some(parent) = std::path::Path::new(&dialog.source_path)
                        .parent()
                        .and_then(|p| p.to_str())
                    {
                        if ui.button("This directory").clicked() {
                            dialog.pattern = format!("{}/*", parent);
                        }
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut dialog.pattern)
                        .desired_width(360.0)
                        .hint_text("glob pattern"),
                );
                ui.checkbox(&mut dialog.persist, "Persist to config");
                ui.label(
                    egui::RichText::new(
                        "Session filters hide results immediately. Persisted filters also \
                         exclude files from the index at the next reindex.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if apply {
            let dialog = self.ignore_dialog.take().unwrap();
            let pattern = dialog.pattern.trim().to_string();
            if !pattern.is_empty() {
                if !self.session_ignores.contains(&pattern) {
                    self.session_ignores.push(pattern.clone());
                }
                if dialog.persist {
                    actions.persist_ignore = Some(pattern);
                }
                actions.rerun = true;
            }
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
                             and paths; case-insensitive — use (?-i:…) to override; \
                             quote patterns containing spaces",
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
    let flattened = snip.window.replace(['\n', '\r', '\t'], " ");
    let window = flattened.as_str();
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

/// Tier-list chip color per cascade stage — lower rank, higher tier:
/// S-red for exact case-sensitive filename matches down through the
/// pastel ramp to purple for fuzzy full-text and on to the grey path
/// tiers. Dark text on these pastels stays readable in both themes.
fn rank_tier_color(stage: u8) -> egui::Color32 {
    match stage {
        1 => egui::Color32::from_rgb(255, 127, 127), // S
        2 => egui::Color32::from_rgb(255, 191, 127), // A
        3 => egui::Color32::from_rgb(255, 223, 127), // B
        4 => egui::Color32::from_rgb(255, 255, 127), // C
        5 => egui::Color32::from_rgb(191, 255, 127), // D
        6 => egui::Color32::from_rgb(127, 255, 127), // E
        7 => egui::Color32::from_rgb(127, 191, 255), // F
        8 => egui::Color32::from_rgb(191, 127, 255), // G
        9 => egui::Color32::from_rgb(223, 159, 255), // H — path, exact case
        10 => egui::Color32::from_rgb(239, 191, 239), // I — path, any case
        _ => egui::Color32::from_rgb(199, 199, 199), // J — fuzzy path
    }
}

/// Timestamp color: fresh files get a green tint that fades into the weak
/// text color over ~2 years on a log scale.
fn recency_color(ui: &egui::Ui, mtime: i64) -> egui::Color32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
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
