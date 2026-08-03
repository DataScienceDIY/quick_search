//! Indexing-rate estimation for the status displays.
//!
//! The old tracker sampled the counter every poll tick but pruned to a
//! 1-second window, so anything slower than ~1 file/sec measured a
//! genuine zero and displayed "0.0 files/sec" despite progress. This one
//! records a point only when the counter *changes*, keeps up to 60 s of
//! history but never fewer than two points (so slow rates stay
//! computable), and measures against `now` so the estimate decays during
//! stalls instead of freezing at the last burst.

use std::time::{Duration, Instant};

const HISTORY: Duration = Duration::from_secs(60);

pub struct SpeedTracker {
    /// (when, counter value) — appended only on counter change.
    points: Vec<(Instant, usize)>,
}

impl SpeedTracker {
    pub fn new() -> SpeedTracker {
        SpeedTracker { points: Vec::new() }
    }

    /// Reset between phases (each phase restarts its counter).
    pub fn reset(&mut self) {
        self.points.clear();
    }

    pub fn record(&mut self, files_processed: usize) {
        self.record_at(Instant::now(), files_processed);
    }

    fn record_at(&mut self, now: Instant, files_processed: usize) {
        match self.points.last() {
            Some(&(_, last)) if last == files_processed => return,
            // Counter went backwards — a new phase started without an
            // explicit reset.
            Some(&(_, last)) if files_processed < last => self.points.clear(),
            _ => {}
        }
        self.points.push((now, files_processed));
        // Prune old points, but always keep at least two so a slow but
        // steady rate never becomes unmeasurable.
        while self.points.len() > 2 && now.duration_since(self.points[0].0) > HISTORY {
            self.points.remove(0);
        }
    }

    /// Estimated files/sec, measured from the oldest retained progress
    /// point to *now*. `None` until two data points exist.
    pub fn files_per_sec(&self) -> Option<f64> {
        self.files_per_sec_at(Instant::now())
    }

    fn files_per_sec_at(&self, now: Instant) -> Option<f64> {
        let (t0, c0) = *self.points.first()?;
        let (_, c1) = *self.points.last()?;
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
        // One file every 2.5 s — the old 1 s window reported 0.0 here.
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
