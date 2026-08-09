//! Indexing-rate estimation for the status displays.
//!
//! The rate shown is a rolling [`WINDOW`] average, not a run average. The
//! tracker records a point only when the counter *changes*, prunes points
//! older than the window but never below two (so a rate slower than one
//! file per window stays computable), and measures against `now` so the
//! estimate decays during stalls instead of freezing at the last burst.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The averaging window; public so the display can name it.
pub const WINDOW: Duration = Duration::from_secs(30);

pub struct SpeedTracker {
    /// (when, counter value) — appended only on counter change.
    points: VecDeque<(Instant, usize)>,
}

impl SpeedTracker {
    pub fn new() -> SpeedTracker {
        SpeedTracker {
            points: VecDeque::new(),
        }
    }

    /// Reset between phases (each phase restarts its counter).
    pub fn reset(&mut self) {
        self.points.clear();
    }

    pub fn record(&mut self, files_processed: usize) {
        self.record_at(Instant::now(), files_processed);
    }

    fn record_at(&mut self, now: Instant, files_processed: usize) {
        match self.points.back() {
            Some(&(_, last)) if last == files_processed => return,
            // Counter went backwards — a new phase started without an
            // explicit reset.
            Some(&(_, last)) if files_processed < last => self.points.clear(),
            _ => {}
        }
        self.points.push_back((now, files_processed));
        // Prune points that have fallen out of the window, but always keep
        // at least two so a slow but steady rate never becomes unmeasurable.
        while self.points.len() > 2 && now.duration_since(self.points[0].0) > WINDOW {
            self.points.pop_front();
        }
    }

    /// Estimated files/sec over the last [`WINDOW`], measured from the
    /// oldest retained progress point to *now*; `None` until two data
    /// points exist. During a stall nothing is recorded, so the growing
    /// span decays the estimate toward zero.
    pub fn files_per_sec(&self) -> Option<f64> {
        self.files_per_sec_at(Instant::now())
    }

    fn files_per_sec_at(&self, now: Instant) -> Option<f64> {
        let (t0, c0) = *self.points.front()?;
        let (_, c1) = *self.points.back()?;
        if self.points.len() < 2 {
            return None;
        }
        let span = now.duration_since(t0).as_secs_f64();
        if span <= 0.0 {
            return None;
        }
        Some((c1 - c0) as f64 / span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_two_points() {
        let mut t = SpeedTracker::new();
        let base = Instant::now();
        assert!(t.files_per_sec_at(base).is_none());
        t.record_at(base, 10);
        assert!(t.files_per_sec_at(base).is_none());
    }

    #[test]
    fn slow_rate_is_measurable_not_zero() {
        // One file every 2.5 s.
        let mut t = SpeedTracker::new();
        let base = Instant::now();
        for i in 0..4 {
            t.record_at(base + Duration::from_millis(2500 * i), 10 + i as usize);
        }
        let rate = t
            .files_per_sec_at(base + Duration::from_millis(7500))
            .unwrap();
        assert!((rate - 0.4).abs() < 0.01, "expected ~0.4/s, got {}", rate);
    }

    #[test]
    fn unchanged_counter_adds_no_points() {
        let mut t = SpeedTracker::new();
        let base = Instant::now();
        for i in 0..100 {
            t.record_at(base + Duration::from_millis(50 * i), 42);
        }
        assert_eq!(t.points.len(), 1, "only the first observation recorded");
    }

    #[test]
    fn stall_decays_toward_zero() {
        let mut t = SpeedTracker::new();
        let base = Instant::now();
        t.record_at(base, 0);
        t.record_at(base + Duration::from_secs(1), 100); // 100/s burst
        let just_after = t.files_per_sec_at(base + Duration::from_secs(1)).unwrap();
        let stalled = t.files_per_sec_at(base + Duration::from_secs(20)).unwrap();
        assert!(just_after > 90.0);
        assert!(
            stalled < 6.0,
            "estimate must decay during a stall: {}",
            stalled
        );
    }

    #[test]
    fn pruning_keeps_at_least_two_points() {
        let mut t = SpeedTracker::new();
        let base = Instant::now();
        t.record_at(base, 1);
        t.record_at(base + Duration::from_secs(30), 2);
        // Far beyond the history window; both points are older than 60 s
        // relative to this record.
        t.record_at(base + Duration::from_secs(300), 3);
        assert!(t.points.len() >= 2);
        assert!(t
            .files_per_sec_at(base + Duration::from_secs(300))
            .is_some());
    }

    #[test]
    fn window_forgets_an_older_burst() {
        // 10,000 files in the first second, then a steady 10/s. A run
        // average would still read in the hundreds; the window must report
        // what the run is doing now.
        let mut t = SpeedTracker::new();
        let base = Instant::now();
        t.record_at(base, 0);
        t.record_at(base + Duration::from_secs(1), 10_000);
        for i in 2..=90 {
            t.record_at(base + Duration::from_secs(i), 10_000 + 10 * i as usize);
        }
        let now = base + Duration::from_secs(90);
        let rate = t.files_per_sec_at(now).unwrap();
        assert!((rate - 10.0).abs() < 0.5, "expected ~10/s, got {}", rate);
        assert!(
            t.points
                .iter()
                .all(|&(at, _)| now.duration_since(at) <= WINDOW),
            "no point older than the window survives while others remain"
        );
    }

    #[test]
    fn counter_regression_resets() {
        let mut t = SpeedTracker::new();
        let base = Instant::now();
        t.record_at(base, 500);
        t.record_at(base + Duration::from_secs(1), 600);
        // New phase restarts from a small number.
        t.record_at(base + Duration::from_secs(2), 3);
        assert_eq!(t.points.len(), 1);
    }
}
