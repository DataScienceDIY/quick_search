//! Progress reporting types for a run: per-root and overall counters,
//! and the reconcile prologue's own meter.

/// Where one root's pipeline is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootPhase {
    /// The parallel walk is discovering and writing file metadata.
    Walking,
    /// The walk finished; content extraction is draining this root's pending rows.
    Extracting,
    Done,
}

/// Index upkeep the writer stops file work to do. Every one of these blocks
/// the writer, so the per-root counters are frozen for its whole life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceStep {
    /// Folding the write-ahead log back into the index file.
    Checkpoint,
    /// Deleting the rows of files that are no longer on disk.
    RemovingStale,
    /// Merging the full-text index's segments.
    MergingText,
    /// Re-reading each root's stored totals.
    RootCounts,
    /// Marking files above the size limit as having no text.
    SizeLimit,
}

/// Progress for one indexing root; the GUI shows one row per root.
#[derive(Debug, Clone)]
pub struct RootProgress {
    pub root: String,
    pub phase: RootPhase,
    /// Files the walk has seen so far; final and exact once the walk ends.
    pub walked: usize,
    /// What to divide `walked` by; `None` until one lands. A first-time root's
    /// count reads high — read it through [`RootProgress::walk_denominator`].
    pub walk_total: Option<usize>,
    /// Rows with searchable text, including earlier runs' once `extract_total` is known.
    pub extracted: usize,
    /// The count of files that have or will have text — not of files under the
    /// root. `None` until the content pass has counted its range.
    pub extract_total: Option<usize>,
    /// A file the root has recently reached, as a whole path in every phase —
    /// sampled, not every file, and during extraction the last one *written*.
    pub current_file: Option<String>,
    /// Threads busy / pool size for the current phase's pool; both zero once done.
    pub active_workers: usize,
    pub total_workers: usize,
}

impl RootProgress {
    /// This root's walk-phase contribution to a progress denominator. After the
    /// walk `walk_total` is dropped: it can read 1.6x high and pin the bar short of 100%.
    pub fn walk_denominator(&self) -> Option<usize> {
        match self.phase {
            // An overtaken denominator pins the bar at 100% mid-walk.
            RootPhase::Walking => self.walk_total.map(|t| t.max(self.walked)),
            RootPhase::Extracting | RootPhase::Done => Some(self.walked),
        }
    }
}

/// Files processed and the run's total, across every root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverallProgress {
    /// Work units, not files: each file counts once walked and once extracted.
    pub processed: usize,
    /// `None` while a still-walking root has no count yet; always lands eventually.
    pub total: Option<usize>,
}

impl OverallProgress {
    /// Completed share, clamped to 1; `None` when there is nothing to divide by.
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.processed as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }
}

/// Aggregate every root's progress into the one pair the status bar shows. A root's
/// extraction half joins `processed` and `total` together, only once `extract_total` is known.
pub fn overall_progress(roots: &[RootProgress]) -> OverallProgress {
    let processed = roots
        .iter()
        .map(|r| r.walked + r.extract_total.map_or(0, |_| r.extracted))
        .sum();
    let mut total = Some(0usize);
    for r in roots {
        match (total, r.walk_denominator()) {
            (Some(acc), Some(walk)) => {
                total = Some(acc + walk + r.extract_total.unwrap_or(0));
            }
            _ => {
                total = None;
                break;
            }
        }
    }
    OverallProgress { processed, total }
}

/// How far a configuration reconciliation has got.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileProgress {
    /// Stored rows re-tested against the current configuration so far.
    pub examined: usize,
    /// `None` while the plan is doing whole-range work that reads no rows.
    pub total: Option<usize>,
    pub deleted: usize,
    /// Rows whose content state or stored text was re-decided.
    pub recontented: usize,
}

impl ReconcileProgress {
    /// Completed share, clamped to 1, matching [`OverallProgress::fraction`].
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.examined as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }
}
