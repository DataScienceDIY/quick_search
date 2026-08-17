//! The Search tab: query strip, streaming results table, snippet
//! preview, context menu, ignore-filter dialog, and syntax help.

use std::time::Instant;

use egui::text::{LayoutJob, TextFormat};
use egui_extras::{Column, TableBuilder};
use quicksearch_core::config::ColumnsConfig;
use quicksearch_core::live::{LiveUpdate, Target, WindowUpdate};
use quicksearch_core::search::{MatchField, SearchHit, SearchUpdate};
use quicksearch_core::snippet::Snippet;

use crate::color::rank_tier_color;
use crate::format::{fmt_elapsed, fmt_mtime, human_size};
use crate::platform;

mod help_window;
mod ignore_dialog;
mod snippet_render;
#[cfg(test)]
mod tests;

use crate::ui_util::hint;
use ignore_dialog::dir_ignore_pattern;
pub use ignore_dialog::IgnoreDialog;
use snippet_render::{centered_match_job, marked_field_job, path_cell_job, snippet_job};

/// Fixed width (points) of the query strip's status slot, sized for the
/// longest `fmt_elapsed` output, so the query box never resizes.
const STATUS_SLOT_WIDTH: f32 = 52.0;

/// Points reserved inside the query box for the repeat-search button, held
/// whether or not the button is showing: a text field whose contents shift
/// sideways every time a search finishes is worse than 20 lost points.
const REPEAT_SLOT_W: i8 = 20;

/// Width (points) of the Fuzzy label-plus-box slot, sized to hold both with a
/// little slack so the strip's spacing does not depend on the font.
const FUZZY_SLOT_WIDTH: f32 = 66.0;

/// Shared by the Fuzzy checkbox and its label, which are separate widgets so
/// the label can sit on the left — `egui::Checkbox` pushes its icon leftmost
/// unconditionally, so no layout direction can flip them.
const FUZZY_HINT: &str = "Also run fuzzy filename and full-text passes (slower)";

/// What the Content Match column shows for a row that did not match on
/// content. An em dash, not a hyphen: at body size `-` reads as a typo and `–`
/// is indistinguishable from one.
const NO_CONTENT_MATCH: &str = "—";

/// How long the visible rows must hold still before they are watched. Scrolling
/// through a long result list would otherwise re-register on every frame.
const LIVE_ARM_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

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
    /// The column picker changed; remember it in the config.
    pub save_columns: Option<ColumnsConfig>,
    /// Replace the live-result watch set. `Some(vec![])` clears it; `None`
    /// leaves whatever is registered alone.
    pub live_targets: Option<Vec<Target>>,
}

/// Whether the live watchers should be pointed at the visible rows this frame.
///
/// Every clause earns its place. `settled` folds in three things — no search
/// running, no edit pending, and the reveal animation finished — because
/// watching rows that do not correspond to the text in the box would re-cut
/// their snippets against the wrong query. The delay is what stops a scroll
/// from re-registering on every frame.
fn should_arm(
    enabled: bool,
    armed_already: bool,
    changed_at: Option<Instant>,
    settled: bool,
    now: Instant,
) -> bool {
    enabled
        && settled
        && !armed_already
        && changed_at.is_some_and(|t| now.duration_since(t) >= LIVE_ARM_DELAY)
}

/// Whether two target lists ask for the same watches.
///
/// Compares what decides *what is watched* and nothing else. `Target` also
/// carries the size and modified time the row is displaying, but those are
/// only the baseline for the watcher's arm-time sweep — re-registering every
/// inotify watch because a file's size moved would tear down and rebuild the
/// whole set on every write.
fn same_watch_set(a: &[Target], b: &[Target]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.path == y.path && x.text == y.text)
}

/// Recolour a laid-out cell as "the file behind this row is gone".
///
/// The Name column says so with a `RichText`, but that column is optional —
/// with it hidden, a struck-through name is no indication at all. Every other
/// column carries its own share instead of relying on it. Match highlighting
/// goes with it: nothing about a file that is not there is still a hit.
fn mark_missing_job(ui: &egui::Ui, job: &mut egui::text::LayoutJob, strike: bool) {
    let color = ui.visuals().weak_text_color();
    for section in &mut job.sections {
        section.format.color = color;
        section.format.background = egui::Color32::TRANSPARENT;
        section.format.strikethrough = if strike {
            egui::Stroke::new(1.0, color)
        } else {
            egui::Stroke::NONE
        };
    }
}

/// The watch target a row asks for.
fn target_for(hit: &SearchHit) -> Target {
    Target {
        path: hit.path.clone(),
        // Only a row showing body text needs its snippet re-cut — and the
        // watcher has to know which matcher cut it; a filename match costs
        // one metadata call per change and never opens the file.
        text: hit.content_tier(),
        // What the row is *displaying*, which on a fresh result is whatever
        // the index said. Sweeping the disk against it at arm time is what
        // turns "watch these rows" into "and tell me if the index was already
        // out of date about them".
        size: hit.size,
        mtime: hit.mtime,
    }
}

/// Which of the two kinds a results column is, in the sense every table
/// library means it: `QHeaderView::Interactive` against `Stretch`, AG Grid's
/// plain `width` against `flex`, GTK's `expand`.
///
/// Columns holding variable-length text flex, so a wider window gives them the
/// room; the ones holding a number or a date do not, because 52 points is as
/// much Rank as there will ever be to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    /// Keeps whatever width it was last given, and can be dragged to another.
    Fixed,
    /// Shares the space the fixed columns leave, in proportion to its current
    /// width — which is also its weight, so a column dragged to a new width
    /// keeps that *share* through the next window resize rather than those
    /// pixels.
    Flex,
}

/// One results column's fixed characteristics: which kind it is, how narrow it
/// may ever get, and how wide it starts before anyone has dragged anything.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnPlan {
    kind: ColumnKind,
    floor: f32,
    initial: f32,
}

/// Lay the columns out across `budget`: the standard fixed/flex allocation.
///
/// The fixed columns take their current width off the top and the flex ones
/// share what is left, in proportion to `current` — which doubles as their
/// weight. A flex column that would land under its floor is pinned there and
/// drops out of the split, and the rest re-share, repeatedly, since pinning
/// one can push another under. AG Grid states the same rule: "if a column with
/// flex is being constrained by its minWidth/maxWidth rules, other flex
/// columns should take up the remaining available space".
///
/// Once the flex columns are all at their floors there is nothing left to give
/// and the fixed ones have to shrink too, so they join the split rather than
/// letting the table overflow. Past that — the floors alone over budget — the
/// window is narrower than the table can be, and the floors are returned:
/// overflowing honestly beats collapsing a column to nothing.
///
/// Doing this here rather than with `Column::remainder()` is forced.
/// `egui_extras` reloads a **resizable** column as `Size::exact(stored_width)`
/// and drops its `width_range`, so a remainder stops being one the moment the
/// table is resizable; and only the *last* column gets the fill-the-remainder
/// special case, so a second flex column could never absorb anything. Marking
/// a flex column `resizable(false)` is worse still — that path floors it at
/// `max_used`, which for a clipped column is its own laid-out width, so it
/// grows and never shrinks (emilk/egui#8048, fixed upstream in 0.35).
fn fit_widths(current: &[f32], plans: &[ColumnPlan], budget: f32) -> Vec<f32> {
    debug_assert_eq!(current.len(), plans.len());
    let floor_total: f32 = plans.iter().map(|p| p.floor).sum();
    if plans.is_empty() || floor_total >= budget {
        return plans.iter().map(|p| p.floor).collect();
    }

    let mut out: Vec<f32> = plans
        .iter()
        .zip(current)
        .map(|(p, &w)| w.max(p.floor))
        .collect();
    // The columns still sharing what is left. Fixed ones are not in the split
    // at all until the flex ones have nothing left to give; the rest have been
    // pinned to a floor.
    let mut free: Vec<usize> = (0..plans.len())
        .filter(|&i| plans[i].kind == ColumnKind::Flex)
        .collect();
    let mut fixed_joined = false;
    loop {
        if free.is_empty() {
            // Every flex column bottomed out and the total still does not fit:
            // the fixed columns give up the difference in proportion, once.
            if fixed_joined || out.iter().sum::<f32>() <= budget {
                return out;
            }
            fixed_joined = true;
            free = (0..plans.len())
                .filter(|&i| plans[i].kind == ColumnKind::Fixed)
                .collect();
            continue;
        }
        let taken: f32 = (0..plans.len())
            .filter(|i| !free.contains(i))
            .map(|i| out[i])
            .sum();
        let share_budget = budget - taken;
        // The weight is the width as it stands, not as it will be clamped:
        // floors decide what a column *gets*, never what it is owed.
        let share: f32 = free.iter().map(|&i| current[i]).sum();
        // Nothing to take proportions from (a first frame, or every free
        // column measured zero): split what is left evenly.
        let widths: Vec<f32> = if share > 0.0 {
            free.iter()
                .map(|&i| current[i] / share * share_budget)
                .collect()
        } else {
            vec![share_budget / free.len() as f32; free.len()]
        };
        let Some(under) = free
            .iter()
            .zip(&widths)
            .position(|(&i, &w)| w < plans[i].floor)
        else {
            for (&i, &w) in free.iter().zip(&widths) {
                out[i] = w;
            }
            return out;
        };
        let pinned = free.remove(under);
        out[pinned] = plans[pinned].floor;
    }
}

/// [`fit_widths`], holding one column at the width the pointer just gave it.
///
/// A drag is the user stating a width, so the layout takes it as given and the
/// others absorb the difference; refitting the dragged column too would fight
/// the pointer. It is still bounded — held no wider than leaves every other
/// column its floor — so a drag can never make the table overflow.
fn fit_around(current: &[f32], plans: &[ColumnPlan], budget: f32, held: Option<usize>) -> Vec<f32> {
    let Some(held) = held.filter(|&i| i < plans.len()) else {
        return fit_widths(current, plans, budget);
    };
    // Everything up to and including the dragged column keeps the width it
    // has; only what lies to its right gives way.
    //
    // This is the whole of what makes a drag controllable. `egui_extras` sets
    // the dragged column to `column_width + pointer.x - x`, and `x` is the
    // running right edge — which already contains `column_width`, so the
    // expression is really "put this column's right edge on the pointer,
    // measured from its left one". Move anything to its left and that left
    // edge shifts, so the divider resizes on its own and slides out from under
    // the cursor. Refitting every column but the held one, which is what this
    // used to do, moved them on every frame of every drag.
    let mut out: Vec<f32> = current.to_vec();
    let held_width = current[held].clamp(
        plans[held].floor,
        grow_ceiling(current, plans, budget, held),
    );
    out[held] = held_width;

    let left: f32 = out[..=held].iter().sum();
    let tail = fit_widths(&current[held + 1..], &plans[held + 1..], budget - left);
    out[held + 1..].copy_from_slice(&tail);
    out
}

/// The widest a column may be dragged: everything the columns to its *right*
/// could give up, and nothing more.
///
/// Only the right-hand side is on offer, for the reason in [`fit_around`] —
/// taking from the left would move the divider away from the pointer. So the
/// last column's ceiling is its own width, and its divider is inert: its right
/// edge is the window's edge, and there is nothing beyond it to trade with.
fn grow_ceiling(current: &[f32], plans: &[ColumnPlan], budget: f32, i: usize) -> f32 {
    let left: f32 = current[..i].iter().sum();
    let right_floor: f32 = plans[i + 1..].iter().map(|p| p.floor).sum();
    (budget - left - right_floor).max(plans[i].floor)
}

/// The sort to actually apply: the requested one, or Rank when the column it
/// keys on is not on screen.
///
/// Hiding the column you are sorted by would otherwise strand you in a sort
/// you can neither see nor click your way out of. Rank is the fallback because
/// it is what a fresh search uses and it needs no column of its own to mean
/// something.
fn effective_sort(sort: (SortKey, bool), cols: &ColumnsConfig) -> (SortKey, bool) {
    let shown = match sort.0 {
        // The path column is mandatory, and rank ordering is meaningful with
        // or without its column.
        SortKey::Path | SortKey::Rank => true,
        SortKey::Name => cols.name,
        SortKey::Size => cols.size,
        SortKey::Modified => cols.modified,
    };
    if shown {
        sort
    } else {
        (SortKey::Rank, true)
    }
}

/// Byte ranges into `field` to highlight, or `None` when the hit's snippet is
/// not that field verbatim.
///
/// Core promises that name- and path-tier snippets are the whole field, which
/// is what lets a column paint its own text and mark the match inside it. This
/// re-checks rather than trusting: ranges cut for a *window* would index the
/// wrong glyphs here, and painting a confidently wrong highlight is worse than
/// painting none. Cheap per visible row — the table is virtualized and a name
/// or a path is short.
fn whole_field_ranges<'a>(snip: Option<&'a Snippet>, field: &str) -> Option<&'a [(usize, usize)]> {
    let snip = snip?;
    if snip.truncated_start || snip.truncated_end || snip.window != field {
        return None;
    }
    snip.ranges
        .iter()
        .all(|&(a, b)| {
            a <= b && b <= field.len() && field.is_char_boundary(a) && field.is_char_boundary(b)
        })
        .then_some(snip.ranges.as_slice())
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
    /// Which columns to paint, mirrored from `[search.columns]`.
    pub columns: ColumnsConfig,
    /// Mirrored from `[search] live_results`.
    pub live_enabled: bool,
    /// The rows rendered last frame — the "visually shown" set — as the
    /// targets they would be watched as.
    ///
    /// Targets rather than row indices because a row's path is what the
    /// watcher keys on, and a rename changes the path without moving the row:
    /// keyed on indices, a renamed row would never be re-armed and would stop
    /// tracking after its first move.
    live_wanted: Vec<Target>,
    /// The targets the watcher is currently registered for.
    live_armed: Vec<Target>,
    /// When `live_wanted` last changed; the arm delay runs from here.
    live_changed_at: Option<Instant>,
    /// Files that have vanished from under a row on screen, by `file_id`.
    /// The row stays put and is struck through rather than being removed —
    /// dropping it would shift every index below it while someone is reading.
    gone: std::collections::HashSet<i64>,
    /// Set on every edit; the app fires the search after the debounce.
    pub pending_edit: Option<Instant>,
    pub generation: u64,
    pub results: Vec<SearchHit>,
    /// The next search's hits, swapped into `results` at zero opacity.
    staging: Vec<SearchHit>,
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
    /// Each results column's width as it was actually laid out last frame,
    /// and the width the table had to lay them out in.
    ///
    /// Measured rather than remembered: once the table is resizable
    /// `egui_extras` owns the widths and offers no way to read them back, and
    /// this is what [`fit_widths`] needs to keep the user's proportions
    /// across a window resize.
    col_widths: Vec<f32>,
    /// The widths asked for last frame. A column that came back a different
    /// width is the one under the pointer — `egui_extras` offers no way to ask.
    col_wanted: Vec<f32>,
    /// The column being dragged, held for the length of the drag.
    ///
    /// Re-deciding it every frame does not work: the table lays out from the
    /// widths it stored a frame earlier, so on the frames where the lag has
    /// caught up there is nothing to tell a drag from a settled layout, and
    /// the ceiling that keeps the drag inside the window would come and go.
    col_drag: Option<usize>,
    /// Display-row index hovered last frame; tracked via `contains_pointer()`
    /// because `row.response().hovered()` is false whenever a selectable label
    /// wins the hit-test.
    hovered_row: Option<usize>,
    focus_query: bool,
    /// Query syntax-highlight segments, cached per text.
    highlight: crate::query_highlight::HighlightCache,
    /// Screen rects of last frame's Content Match cells, in display order — the
    /// capture driver's hover targets.
    #[cfg(feature = "capture")]
    pub(crate) capture_match_rects: Vec<egui::Rect>,
}

impl SearchTab {
    pub fn new(fuzzy_default: bool, columns: ColumnsConfig, live_enabled: bool) -> SearchTab {
        SearchTab {
            query: String::new(),
            fuzzy: fuzzy_default,
            columns,
            live_enabled,
            live_wanted: Vec::new(),
            live_armed: Vec::new(),
            live_changed_at: None,
            gone: std::collections::HashSet::new(),
            pending_edit: None,
            generation: 0,
            results: Vec::new(),
            staging: Vec::new(),
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
            col_widths: Vec::new(),
            col_wanted: Vec::new(),
            col_drag: None,
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

    /// Whether what the table shows corresponds to the text in the query box:
    /// the query executed, the swap landed, and the reveal finished.
    ///
    /// Two things need exactly this. Arming the live watchers does, because
    /// watching rows that do not match the box would re-cut their snippets
    /// against the wrong query; and the capture driver's `wait_search_done`
    /// does, because a screenshot mid-reveal catches a half-drawn table. They
    /// were the same expression written twice, one of them behind the capture
    /// feature and so absent from every test build.
    pub(crate) fn settled(&self) -> bool {
        !self.running && self.pending_edit.is_none() && self.fade_settled()
    }

    /// Re-arm the one-shot first-frame focus (tab switches drop egui focus).
    pub(crate) fn request_focus(&mut self) {
        self.focus_query = true;
    }

    /// Re-sort before the next paint. Needed when the columns change from
    /// outside the tab: hiding the sorted column demotes the sort to Rank
    /// (see [`effective_sort`]), and the order has to be rebuilt for it.
    pub(crate) fn mark_sort_dirty(&mut self) {
        self.sort_dirty = true;
    }

    /// Screen rect of the Nth visible Content Match cell from the last rendered
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
        // The watches belong to the results being replaced. The app drops the
        // registration itself; this is the tab-side half.
        self.live_armed.clear();
        self.live_wanted.clear();
        self.live_changed_at = None;
        self.gone.clear();
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
                    admit(&mut self.staging, hits, display_limit, &mut self.limited);
                } else {
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

    /// Apply one live filesystem update to the row it names.
    ///
    /// Rows are found by path — the same key the watcher was armed with. The
    /// display order, the selection and `file_id` are all left alone: a row
    /// that jumps or vanishes under the pointer while someone is reading it is
    /// worse than a row that is briefly out of position.
    pub fn apply_live(&mut self, update: LiveUpdate) {
        match update {
            LiveUpdate::Renamed { path, to, name } => {
                let Some(hit) = self.results.iter_mut().find(|h| h.path == path) else {
                    return;
                };
                // A name- or path-tier snippet *is* the old field, so it has
                // to be rewritten. The marks go with it: nothing here says the
                // new name still matches the query, and an unhighlighted new
                // name is the honest rendering of that.
                let field = hit.match_field();
                if let Some(snip) = hit.snippet.as_mut() {
                    match field {
                        MatchField::Name => {
                            snip.window = name.clone();
                            snip.ranges.clear();
                        }
                        MatchField::Path => {
                            snip.window = to.clone();
                            snip.ranges.clear();
                        }
                        // A move does not touch the body.
                        MatchField::Contents => {}
                    }
                }
                self.gone.remove(&hit.file_id);
                hit.path = to;
                hit.name = name;
            }
            LiveUpdate::Changed {
                path,
                size,
                mtime,
                window,
            } => {
                let Some(hit) = self.results.iter_mut().find(|h| h.path == path) else {
                    return;
                };
                hit.size = size;
                hit.mtime = mtime;
                match window {
                    // Either not a body-text row, or one whose body could not
                    // be re-read. Both mean the cell is better left as it is
                    // than blanked on no evidence.
                    WindowUpdate::Unchanged => {}
                    WindowUpdate::Cut(snippet) => hit.snippet = Some(snippet),
                    WindowUpdate::NoMatch => hit.snippet = None,
                }
                self.gone.remove(&hit.file_id);
            }
            LiveUpdate::Gone { path } => {
                if let Some(hit) = self.results.iter().find(|h| h.path == path) {
                    self.gone.insert(hit.file_id);
                }
            }
        }
    }

    /// Whether `live_wanted` already describes exactly these rows.
    ///
    /// Compared field by field rather than by building the targets and
    /// testing equality, because the common frame is "nothing moved" and
    /// that frame must allocate nothing at all.
    fn live_wanted_current(&self, visible: &[u32]) -> bool {
        visible.len() == self.live_wanted.len()
            && visible.iter().zip(&self.live_wanted).all(|(&ix, want)| {
                self.results.get(ix as usize).is_some_and(|hit| {
                    hit.path == want.path
                        && hit.size == want.size
                        && hit.mtime == want.mtime
                        && hit.content_tier() == want.text
                })
            })
    }

    /// Drop the tab-side live state. The app drops the registration itself.
    pub(crate) fn reset_live(&mut self) {
        self.live_armed.clear();
        self.live_wanted.clear();
        self.live_changed_at = None;
    }

    pub fn result_count_label(&self) -> Option<String> {
        if self.query.trim().is_empty() && self.results.is_empty() {
            return None;
        }
        // The `+` is the whole warning here: it says the count is a floor.
        // The reason and the remedy live in the tab body's own notice, which
        // has room for a sentence; the status bar does not.
        Some(format!(
            "{}{} results",
            self.results.len(),
            if self.limited { "+" } else { "" }
        ))
    }

    fn resort(&mut self) {
        let (key, ascending) = effective_sort(self.sort, &self.columns);
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

    /// One column header: its label, the sort indicator when it is the active
    /// key, the click that re-keys the sort, and the right-click menu that
    /// picks columns. `key` is `None` for a header that does not sort.
    ///
    /// The sort indicator is a painter-drawn triangle: the default egui fonts
    /// have no ▲/▼ glyphs — they render as boxes.
    fn header_cell(
        &mut self,
        ui: &mut egui::Ui,
        key: Option<SortKey>,
        label: &str,
        picked: &mut Option<ColumnsConfig>,
    ) {
        let (cur, asc) = effective_sort(self.sort, &self.columns);
        let selected = key == Some(cur);
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
        if let Some(key) = key {
            if response.clicked() {
                self.sort = if selected { (key, !asc) } else { (key, true) };
                self.sort_dirty = true;
            }
        }
        // Every header carries the same picker, so a right-click lands wherever
        // the pointer happens to be along the row.
        response.context_menu(|ui| {
            let mut next = self.columns.clone();
            ui.label(hint("Columns"));
            let row = |ui: &mut egui::Ui, on: &mut bool, label: &str| {
                ui.checkbox(on, label);
            };
            row(ui, &mut next.name, "Name");
            // Shown checked and greyed rather than omitted: an absent entry
            // reads as an oversight, a disabled one answers the question.
            ui.add_enabled(false, egui::Checkbox::new(&mut true, "Path"))
                .on_disabled_hover_text(
                    "The path is always shown — it is the only column that \
                     identifies a result on its own.",
                );
            row(ui, &mut next.content_match, "Content Match");
            row(ui, &mut next.size, "Size");
            row(ui, &mut next.modified, "Modified");
            row(ui, &mut next.rank, "Rank");
            if next != self.columns {
                self.columns = next.clone();
                self.sort_dirty = true;
                *picked = Some(next);
            }
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> SearchActions {
        let mut actions = SearchActions::default();

        // --- Query strip: the syntax-help button anchors the left edge, then
        // everything else is laid out right to left with the query box taking
        // whatever is left of the row. --------------------------------------
        ui.horizontal(|ui| {
            if ui.button("?").on_hover_text("Query syntax help").clicked() {
                self.help_open = !self.help_open;
            }
            // Sized to what the `?` left behind, not `with_layout`: a
            // right-to-left child takes the row's *full* width, so after the
            // button has advanced the cursor its right edge lands a button's
            // width past the panel and the rightmost widget falls off screen.
            let rest = egui::vec2(
                (ui.max_rect().right() - ui.next_widget_position().x).max(0.0),
                ui.available_height(),
            );
            ui.allocate_ui_with_layout(
                rest,
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    // The label is a separate widget from the box so it can sit on
                    // the left: in a right-to-left layout the first widget added is
                    // the rightmost, and `egui::Checkbox` pushes its own icon
                    // leftmost whatever the direction, so the two have to be
                    // separate widgets in this order. Sensing clicks on the label
                    // keeps the target the combined widget used to have.
                    // The pair gets its own fixed-width, left-to-right slot,
                    // the way the status slot below does. A bare `ui.horizontal`
                    // here would be laid out by the surrounding right-to-left
                    // strip and land its contents past the panel's edge.
                    //
                    // Two widgets rather than one because `egui::Checkbox`
                    // pushes its own icon leftmost whatever the direction — and
                    // sensing clicks on the label keeps the target the combined
                    // widget gave it for free.
                    let toggled = ui
                        .allocate_ui_with_layout(
                            egui::vec2(FUZZY_SLOT_WIDTH, ui.spacing().interact_size.y),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let mut toggled = false;
                                if ui
                                    .add(egui::Label::new("Fuzzy").sense(egui::Sense::click()))
                                    .on_hover_text(FUZZY_HINT)
                                    .clicked()
                                {
                                    self.fuzzy = !self.fuzzy;
                                    toggled = true;
                                }
                                toggled
                                    | ui.add(egui::Checkbox::without_text(&mut self.fuzzy))
                                        .on_hover_text(FUZZY_HINT)
                                        .changed()
                            },
                        )
                        .inner;
                    if toggled {
                        actions.save_fuzzy_default = Some(self.fuzzy);
                        actions.rerun = true;
                    }
                    ui.separator();
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
                    let mut layouter =
                        move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                            crate::query_highlight::galley(ui, highlight, buf.as_str())
                        };
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .desired_width(width.max(120.0))
                            // The gutter is held whether or not the button is
                            // showing. `TextEdit` sizes its frame as
                            // `wrap_width + margin`, with `wrap_width` capped by
                            // what is available, so widening the right margin
                            // leaves the outer box exactly where it was and only
                            // insets the text — which is what keeps the query from
                            // shifting sideways every time a search finishes.
                            .margin(egui::Margin {
                                right: 4 + REPEAT_SLOT_W,
                                ..egui::Margin::symmetric(4, 2)
                            })
                            .hint_text(
                                "Search names and contents…  (type:Document regex:… budget*)",
                            )
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
                        // The watches belong to the results the old query
                        // produced. Dropping them here rather than at the next
                        // search means they go the instant the query stops
                        // describing what is on screen.
                        self.reset_live();
                        actions.live_targets = Some(Vec::new());
                    }

                    // Read *after* the edit above, so a keystroke hides the button
                    // on its own frame. `pending_edit.is_none()` is also what makes
                    // "repeat the last search" and "run what is in the box" the
                    // same thing: the flag is set on every edit and cleared in the
                    // same statement that fires the search, so whenever this is
                    // true the box holds exactly what last executed.
                    let show_repeat = !self.running
                        && self.elapsed.is_some()
                        && self.pending_edit.is_none()
                        && !self.query.trim().is_empty();
                    if show_repeat {
                        let slot = egui::Rect::from_min_max(
                            egui::pos2(
                                response.rect.right() - REPEAT_SLOT_W as f32,
                                response.rect.top(),
                            ),
                            response.rect.right_bottom(),
                        )
                        .shrink(2.0);
                        // `place`, not `put`: `put` advances the cursor, which in
                        // this right-to-left row would shove the query box sideways.
                        //
                        // This must stay *after* the TextEdit. egui derives widget
                        // ids from how many widgets precede them, so a button that
                        // comes and goes ahead of the box would rename it every
                        // time a search finished — and a TextEdit whose id changes
                        // loses focus and its in-progress edit. Nothing follows the
                        // button here, so the ordering alone is the fix.
                        if ui
                            .place(slot, egui::Button::new("⟳").frame_when_inactive(false))
                            .on_hover_text("Run this search again")
                            .clicked()
                        {
                            actions.rerun = true;
                        }
                    }
                },
            );
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
                    "Showing first {} matches; refine the query (limit configurable on the Settings tab).",
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
        // Driven by the selection rather than by the column, so the snippet
        // stays reachable even with the Content Match column switched off.
        let preview_snippet: Option<Snippet> = self
            .selected
            .and_then(|i| self.results.get(i as usize))
            .filter(|h| h.match_field() == MatchField::Contents)
            .and_then(|h| h.snippet.clone());
        let preview_height = if preview_snippet.is_some() { 44.0 } else { 0.0 };
        let table_height = (ui.available_height() - preview_height).max(60.0);

        // Shown whenever it is picked, whatever the results turned out to be.
        // A result set that matched only on names gets a column of em dashes,
        // which is the honest reading of "no content match here" — hiding the
        // column instead would make a checked box mean nothing.
        let show_match = self.columns.content_match;
        let cols = self.columns.clone();

        let body_font = egui::TextStyle::Body.resolve(ui.style());
        let text_height = body_font.size + 4.0;
        let mut open_ignore_dialog: Option<usize> = None;
        let mut hovered_now: Option<usize> = None;
        let mut picked: Option<ColumnsConfig> = None;
        // egui_extras invokes the body closure only for the rows it actually
        // renders, so collecting here *is* the "visually shown, not all
        // returned" set, for free.
        let mut visible_now: Vec<u32> = Vec::new();
        let order = std::mem::take(&mut self.order);
        #[cfg(feature = "capture")]
        let mut capture_match_rects: Vec<egui::Rect> = Vec::new();

        // Top of the wiped section; its bottom is known only after the
        // preview strip is laid out.
        let section_top = ui.cursor().top();

        // What each enabled column may never go under, and what it starts at.
        // Order must match the `header.col` and `row.col` calls below.
        let mut plans: Vec<ColumnPlan> = Vec::with_capacity(6);
        let flex = |floor: f32, initial: f32| ColumnPlan {
            kind: ColumnKind::Flex,
            floor,
            initial,
        };
        if cols.name {
            plans.push(flex(80.0, 220.0));
        }
        // The path column is not optional.
        plans.push(flex(120.0, 320.0));
        if show_match {
            plans.push(flex(120.0, 320.0));
        }
        // Natural widths, and no reason to grow: a date is as long as a date.
        // The floor is under the natural width all the same, so a window too
        // narrow for the table squeezes these before anything runs off the
        // edge — the flex columns give first, and only then these.
        for (on, floor, w) in [
            (cols.size, 52.0, 72.0),
            (cols.modified, 78.0, 110.0),
            (cols.rank, 40.0, 52.0),
        ] {
            if on {
                plans.push(ColumnPlan {
                    kind: ColumnKind::Fixed,
                    floor,
                    initial: w,
                });
            }
        }

        // What the columns themselves have to divide up: the width the table
        // is given, less what the table spends around them. Both parts are
        // read from the style rather than inferred from where the columns
        // ended up last frame — inferring it is a feedback loop, since a
        // frame in which the columns do not fill the width reads as a frame
        // with more overhead, which shrinks the budget, which keeps them from
        // filling it. `egui_extras` charges the same two: the scrollbar comes
        // off `available_rect_before_wrap`, and `Sizing::to_lengths` bills
        // one spacing between each pair of columns.
        let table_avail = ui.available_width();
        let stale = self.col_widths.len() != plans.len();
        let gaps = plans.len().saturating_sub(1) as f32 * ui.spacing().item_spacing.x;
        let budget = (table_avail - ui.spacing().scroll.allocated_width() - gaps).max(0.0);
        // A column toggled on or off means egui_extras has dropped the stored
        // widths anyway, so start from the plan.
        let current: Vec<f32> = if stale {
            plans.iter().map(|p| p.initial).collect()
        } else {
            self.col_widths.clone()
        };

        // Which column the pointer is on, if any: the one egui_extras sized
        // differently from what was asked for. There is no way to ask it
        // directly, and it has to be left alone — refitting a column while it
        // is being dragged fights the pointer.
        //
        // Gated on a held button because a width can differ from the request
        // for a duller reason: egui_extras lays a frame out from the widths it
        // stored at the end of the *previous* one, so every refit shows up a
        // frame late and would otherwise read as a drag. Nothing can be
        // dragged with nothing pressed, which settles it.
        if !ui.input(|i| i.pointer.any_down()) {
            self.col_drag = None;
        } else if self.col_drag.is_none() && !stale && self.col_wanted.len() == plans.len() {
            self.col_drag = current
                .iter()
                .zip(&self.col_wanted)
                .position(|(a, b)| (a - b).abs() > 0.5);
        }
        let dragged = self.col_drag.filter(|&i| i < plans.len());
        let targets = fit_around(&current, &plans, budget, dragged);

        // `width_range` is the only handle on a resizable table's widths, and
        // the clamp it drives is what actually moves them — so a column that
        // has to move is pinned, and one already where it belongs is left free
        // to be dragged. Pinning only what must move is what keeps a drag able
        // to start: were every column pinned to its target, no drag could ever
        // produce the first pixel of movement that identifies it.
        let ranges: Vec<(f32, f32)> = targets
            .iter()
            .zip(&current)
            .zip(&plans)
            .enumerate()
            .map(|(i, ((&target, &width), plan))| {
                match dragged {
                    // Left of the divider, and so not the drag's to touch: held
                    // exactly where it is, because the divider's position is
                    // measured from this column's edge. See `fit_around`.
                    Some(held) if i < held => (width, width),
                    // The dragged column itself follows the pointer, as far as
                    // the columns to its right can pay for.
                    Some(held) if i == held => {
                        (plan.floor, grow_ceiling(&current, &plans, budget, i))
                    }
                    // Right of the divider: absorbing, so pinned to its share.
                    Some(_) => (target, target),
                    None if (target - width).abs() > 0.5 => (target, target),
                    // Settled, so left free for a drag to start on — with the
                    // ceiling it will be held to once it does, rather than a
                    // looser one that would let the first frame jump.
                    None => (plan.floor, grow_ceiling(&current, &plans, budget, i)),
                }
            })
            .collect();
        self.col_wanted = targets;

        let mut measured: Vec<f32> = Vec::with_capacity(plans.len());
        let table_scroll = ui
            .push_id("results", |ui| {
                // Changing the column count makes egui_extras drop any widths
                // the user had dragged. That is the cost of the picker, and a
                // deliberate action on their part, so it is not worked around.
                let mut table = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .max_scroll_height(table_height)
                    .min_scrolled_height(60.0);
                for (plan, &(lo, hi)) in plans.iter().zip(&ranges) {
                    table = table.column(
                        Column::initial(plan.initial)
                            .at_least(lo)
                            .at_most(hi)
                            .clip(true),
                    );
                }

                table
                    .header(text_height + 4.0, |mut header| {
                        // Each header cell reports its column's laid-out
                        // width — the only way to read back what a resizable
                        // table decided, and what the next frame refits from.
                        let mut head = |sort, label, measured: &mut Vec<f32>| {
                            header.col(|ui| {
                                measured.push(ui.max_rect().width());
                                self.header_cell(ui, sort, label, &mut picked)
                            });
                        };
                        if cols.name {
                            head(Some(SortKey::Name), "Name", &mut measured);
                        }
                        head(Some(SortKey::Path), "Path", &mut measured);
                        if show_match {
                            head(None, "Content Match", &mut measured);
                        }
                        if cols.size {
                            head(Some(SortKey::Size), "Size", &mut measured);
                        }
                        if cols.modified {
                            head(Some(SortKey::Modified), "Modified", &mut measured);
                        }
                        if cols.rank {
                            head(Some(SortKey::Rank), "Rank", &mut measured);
                        }
                    })
                    .body(|body| {
                        body.rows(text_height, order.len(), |mut row| {
                            let display_ix = row.index();
                            let result_ix = order[display_ix] as usize;
                            let hit = &self.results[result_ix];
                            row.set_selected(self.selected == Some(result_ix as u32));
                            row.set_hovered(self.hovered_row == Some(display_ix));
                            visible_now.push(result_ix as u32);
                            let missing = self.gone.contains(&hit.file_id);

                            // Selectable labels win egui's hit-test over the
                            // row, so union their responses into the row's or
                            // clicks over glyphs would miss.
                            let mut cell_responses: Vec<egui::Response> = Vec::new();

                            let field = hit.match_field();
                            if cols.name {
                                row.col(|ui| {
                                    // A filename match is highlighted here
                                    // rather than in the Content Match column,
                                    // which shows a dash for it instead.
                                    let marks = (field == MatchField::Name)
                                        .then(|| {
                                            whole_field_ranges(hit.snippet.as_ref(), &hit.name)
                                        })
                                        .flatten()
                                        .unwrap_or(&[]);
                                    let mut job = marked_field_job(ui, &hit.name, marks);
                                    // A file that has gone from under the row
                                    // reads as struck through rather than
                                    // disappearing, so nothing below it moves
                                    // while it is being read. Same helper the
                                    // other columns use, so one concept has
                                    // one rendering.
                                    if missing {
                                        mark_missing_job(ui, &mut job, true);
                                    }
                                    cell_responses.push(ui.label(job));
                                });
                            }
                            row.col(|ui| {
                                // Center-elided: egui's own truncation would
                                // drop the deepest directories. Sizing-pass
                                // cell rects are not final, so don't measure
                                // against them.
                                let marks = (field == MatchField::Path)
                                    .then(|| whole_field_ranges(hit.snippet.as_ref(), &hit.path))
                                    .flatten()
                                    .unwrap_or(&[]);
                                let (mut job, elided) = if ui.is_sizing_pass() {
                                    // Nothing to elide against, so this is
                                    // the plain marked field.
                                    (marked_field_job(ui, &hit.path, marks), false)
                                } else {
                                    path_cell_job(
                                        ui,
                                        &hit.path,
                                        marks,
                                        // A point of slack against rounding
                                        // disagreements with egui's layout.
                                        ui.available_width() - 1.0,
                                        &body_font,
                                    )
                                };
                                // The path *is* the thing that no longer
                                // exists, so it is struck through like the
                                // name rather than merely dimmed.
                                if missing {
                                    mark_missing_job(ui, &mut job, true);
                                }
                                // egui offers a full-text tooltip only when
                                // *it* elided the galley — and it is handed
                                // the already-shortened string here.
                                let mut response =
                                    ui.add(egui::Label::new(job).show_tooltip_when_elided(false));
                                if elided {
                                    response = response.on_hover_text(&hit.path);
                                }
                                cell_responses.push(response);
                            });
                            if show_match {
                                let snippet = hit
                                    .snippet
                                    .as_ref()
                                    .filter(|_| field == MatchField::Contents);
                                row.col(|ui| {
                                    let response = match snippet {
                                        Some(snip) => {
                                            let width = ui.available_width();
                                            let mut job = centered_match_job(ui, snip, width);
                                            // Dimmed, not struck through: the
                                            // text is what the file *held*,
                                            // not a name that has gone stale.
                                            if missing {
                                                mark_missing_job(ui, &mut job, false);
                                            }
                                            let mut response =
                                                centered_cell(ui, |ui| ui.label(job));
                                            if !snip.ranges.is_empty() {
                                                response = response.on_hover_ui(|ui| {
                                                    ui.set_max_width(520.0);
                                                    let job = snippet_job(ui, snip, 10);
                                                    ui.label(job);
                                                });
                                            }
                                            response
                                        }
                                        // No tooltip: for a name hit it would
                                        // restate the filename already on
                                        // screen, highlighted, two columns left.
                                        None => centered_cell(ui, |ui| {
                                            ui.label(egui::RichText::new(NO_CONTENT_MATCH).weak())
                                        }),
                                    };
                                    #[cfg(feature = "capture")]
                                    capture_match_rects.push(response.rect);
                                    cell_responses.push(response);
                                });
                            }
                            if cols.size {
                                row.col(|ui| {
                                    let text = egui::RichText::new(human_size(hit.size));
                                    let text = if missing { text.weak() } else { text };
                                    let response = centered_cell(ui, |ui| ui.label(text));
                                    cell_responses.push(response);
                                });
                            }
                            if cols.modified {
                                row.col(|ui| {
                                    // Recency colouring says "this file was
                                    // touched recently", which is a claim
                                    // about a file that still exists.
                                    let color = if missing {
                                        ui.visuals().weak_text_color()
                                    } else {
                                        recency_color(ui, hit.mtime)
                                    };
                                    let response = centered_cell(ui, |ui| {
                                        ui.label(
                                            egui::RichText::new(fmt_mtime(hit.mtime)).color(color),
                                        )
                                    });
                                    cell_responses.push(response);
                                });
                            }
                            if cols.rank {
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
                            }

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
                                if ui.button("Open File").clicked() {
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
        // Sizing passes lay out a throwaway sample; taking their widths would
        // feed the next refit a measurement of nothing.
        if measured.len() == plans.len() {
            self.col_widths = measured;
        }
        self.order = order;
        actions.save_columns = picked;
        crate::ui_util::more_below_hint(ui, &table_scroll);
        self.hovered_row = hovered_now;

        // --- Live results: watch what is on screen once it holds still -----
        {
            let now = Instant::now();
            // Rebuilt only when it actually differs: this runs every frame,
            // and cloning a screenful of paths each time would be a steady
            // drip of allocation for nothing.
            if !self.live_wanted_current(&visible_now) {
                let rebuilt: Vec<Target> = visible_now
                    .iter()
                    .filter_map(|&ix| self.results.get(ix as usize))
                    .map(target_for)
                    .collect();
                // Only a different watch *set* restarts the arm delay. A row
                // whose size or modified time moved under it needs a fresh
                // baseline for the next sweep, not a fresh registration.
                if !same_watch_set(&rebuilt, &self.live_wanted) {
                    self.live_changed_at = Some(now);
                }
                self.live_wanted = rebuilt;
            }
            let settled = self.settled();
            let armed_already = same_watch_set(&self.live_wanted, &self.live_armed);
            if should_arm(
                self.live_enabled,
                armed_already,
                self.live_changed_at,
                settled,
                now,
            ) {
                self.live_armed = self.live_wanted.clone();
                actions.live_targets = Some(self.live_wanted.clone());
            } else if settled && !armed_already {
                // Once the reveal settles nothing else asks for frames, so a
                // bare `Instant` deadline would never come due. Same shape as
                // the search debounce in `app::tick_debounce`.
                if let Some(changed) = self.live_changed_at {
                    let waited = now.duration_since(changed);
                    ui.ctx()
                        .request_repaint_after(LIVE_ARM_DELAY.saturating_sub(waited));
                }
            }
        }
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
