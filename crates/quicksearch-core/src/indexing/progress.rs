//! Progress reporting types for a run: per-root and overall counters,
//! and the reconcile prologue's own meter.

/// Where one root's pipeline is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootPhase {
    /// The parallel walk is discovering and writing file metadata.
    Walking,
    /// The walk finished; content extraction is draining this root's
    /// pending rows.
    Extracting,
    Done,
}

/// Progress for one indexing root. Each root runs its own walker and its
/// own extraction cursor; the GUI shows one row per root.
#[derive(Debug, Clone)]
pub struct RootProgress {
    pub root: String,
    pub phase: RootPhase,
    /// Files the walk has seen so far. Final and exact once the root leaves
    /// [`RootPhase::Walking`] — see [`RootProgress::walk_denominator`].
    pub walked: usize,
    /// What to divide `walked` by; `None` until one lands. Exact on a root
    /// walked before; a first-time root falls back to the `find` scan, which
    /// counts tree *entries* and so reads high. Read it through
    /// [`RootProgress::walk_denominator`].
    pub walk_total: Option<usize>,
    /// Rows with searchable text: this run's, plus earlier runs' once
    /// `extract_total` is known.
    pub extracted: usize,
    /// The root's whole searchable set: pending + already-extracted rows when
    /// the walk finished — the count of files that have or will have text,
    /// not of files under the root. `None` until the root's content pass has
    /// counted its range: a scan that takes seconds on a large root, and one
    /// that used to run on the writer thread with every other root waiting.
    pub extract_total: Option<usize>,
    pub current_file: Option<String>,
    /// Threads busy right now / pool size, for the pool this root's current
    /// phase is running. Both zero once the root is done: its threads are
    /// gone, and a dead pool's size would read "0/44 workers".
    pub active_workers: usize,
    pub total_workers: usize,
}

impl RootProgress {
    /// This root's walk-phase contribution to a progress denominator.
    ///
    /// Once the walk ends `walked` is final and exact, so `walk_total` is
    /// dropped: keeping it (it can read 1.6x high) is what stopped the bar
    /// reaching 100%.
    pub fn walk_denominator(&self) -> Option<usize> {
        match self.phase {
            // Never below what has already been walked: an overtaken
            // denominator pins the bar at 100% mid-walk.
            RootPhase::Walking => self.walk_total.map(|t| t.max(self.walked)),
            RootPhase::Extracting | RootPhase::Done => Some(self.walked),
        }
    }
}

/// Files processed and the run's total, across every root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverallProgress {
    /// Both halves of the work: every file the walks have seen, plus every
    /// row with searchable text. A file is counted once for each, so this is
    /// a work-units figure rather than a file count.
    pub processed: usize,
    /// `None` while a still-walking root has no count yet; no root past its
    /// walk can withhold one, so a run always gains a total in the end.
    pub total: Option<usize>,
}

impl OverallProgress {
    /// Completed share, clamped to 1. `None` when there is nothing to
    /// divide by — an unknown total, or a run with no work in it at all.
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.processed as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }
}

/// Aggregate every root's progress into the one pair the status bar shows.
///
/// A root contributes its extraction half only once `extract_total` is known
/// — both to `processed` and to `total`, so the two stay in step. Until then
/// (during the walk, and for the moments after it while the pass counts) it
/// contributes its walk alone.
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

/// How far a configuration reconciliation has got. Reported by both places
/// one can run: a full run's prologue
/// ([`IndexingService::reconcile_stored_config`]) and the coordinator's
/// between-runs pass ([`crate::scope::advance`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileProgress {
    /// Stored rows re-tested against the current configuration so far.
    pub examined: usize,
    /// Rows in the index, counted once when the scan starts. `None` while
    /// the plan is doing whole-range work that reads no rows at all.
    pub total: Option<usize>,
    pub deleted: usize,
    /// Rows whose content state or stored text was re-decided.
    pub recontented: usize,
}

impl ReconcileProgress {
    /// Completed share, clamped to 1. `None` when there is nothing to divide
    /// by, matching [`OverallProgress::fraction`].
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.examined as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }
}
