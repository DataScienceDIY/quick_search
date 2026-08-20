//! The process log: every line that would go to the terminal, kept in
//! memory so a windowed run — which on Windows has no console at all under
//! the GUI subsystem — can show it.
//!
//! Background reporting goes through [`crate::log_info!`] and
//! [`crate::log_warn!`] instead of `println!`/`eprintln!`: each writes the
//! same line to stderr *and* appends it to a bounded ring the GUI's Logs tab
//! reads. Command output — search hits, usage text, the errors a command
//! exits with — is a program's answer, not a background event, and is not
//! logged.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lines retained before the oldest are dropped. The ring holds the count it
/// threw away so the tab can say so instead of quietly lying.
pub const CAPACITY: usize = 5_000;

/// How loud a line is. The GUI colors by this; stderr gets the `Warning:`
/// prefix that the same messages carried when they were `eprintln!`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
}

/// One recorded line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Unix seconds when it was recorded.
    pub at: u64,
    pub level: Level,
    /// Exactly the text written to stderr, prefix included.
    pub text: String,
}

/// Record an informational line: `println!`-style formatting.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Info, ::std::format!($($arg)*))
    };
}

/// Record a warning. The stored and printed text gains a `Warning: ` prefix,
/// so call sites pass the message alone.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Warn, ::std::format!($($arg)*))
    };
}

/// Write `message` to stderr and to the ring. Prefer the macros; this is
/// what they call.
///
/// A failed stderr write is ignored rather than propagated: `eprintln!`
/// *panics* when the handle is unwritable, which on a process launched
/// without stdio would take down whichever background thread happened to
/// report something.
pub fn record(level: Level, message: String) {
    let text = match level {
        Level::Warn => format!("Warning: {}", message),
        Level::Info => message,
    };
    // Almost every line here names a path, and a path is whatever someone
    // called a file. An escape sequence in one reaches a terminal three ways:
    // the stderr write below, a user running the GUI from a shell, and the
    // Logs tab's Copy button, which puts the ring on the clipboard for pasting
    // into a bug report. Diagnostics are not data — nothing downstream needs
    // these bytes exactly — so they are scrubbed unconditionally.
    //
    // Line breaks are collapsed *first*, because `scrub_controls` counts them
    // as controls and would leave `U+FFFD` where a space belongs. One record
    // is one line — the ring renders it that way and `writeln!` adds the only
    // newline there should be — so a message that arrives multi-line, such as
    // a nested error's chain of causes, is flattened rather than boxed.
    let text = if text.contains(['\n', '\r']) {
        text.replace(['\n', '\r'], " ")
    } else {
        text
    };
    let text = crate::textenc::scrub_controls(&text).into_owned();
    let _ = writeln!(std::io::stderr(), "{}", text);
    lock().push(level, text);
}

/// A cap on how many times one *kind* of warning is allowed to speak.
///
/// Some failures are per-file and arrive in the thousands — on Windows,
/// `ERROR_SHARING_VIOLATION` from a file another process holds open is
/// routine and has no Unix equivalent. Logging each one costs a global mutex
/// and an unbuffered stderr write on the walk's hottest path, and evicts the
/// ring's warnings worth reading. So the first few speak and the rest are
/// counted; reset by whoever owns the run, so the numbers describe one run.
pub struct Throttle {
    limit: u64,
    seen: std::sync::atomic::AtomicU64,
}

impl Throttle {
    pub const fn new(limit: u64) -> Throttle {
        Throttle {
            limit,
            seen: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Count one occurrence, and answer whether it may be logged individually.
    pub fn allow(&self) -> bool {
        self.seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < self.limit
    }

    /// Occurrences counted since the last [`Throttle::reset`].
    pub fn seen(&self) -> u64 {
        self.seen.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many went unlogged — what a summary line should report.
    pub fn suppressed(&self) -> u64 {
        self.seen().saturating_sub(self.limit)
    }

    pub fn reset(&self) {
        self.seen.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Every retained line, oldest first.
pub fn snapshot() -> Vec<LogLine> {
    lock().lines.iter().cloned().collect()
}

/// How many lines have been recorded since the process started, including
/// ones since dropped. Only ever grows, so a poll of this is the cheap way
/// to ask "anything new?" without copying the ring.
pub fn recorded() -> u64 {
    lock().recorded
}

/// How many lines the ring has evicted since the last [`clear`].
pub fn dropped() -> u64 {
    lock().dropped
}

/// Forget every retained line. [`recorded`] keeps counting.
pub fn clear() {
    lock().clear();
}

static LOG: LazyLock<Mutex<Ring>> = LazyLock::new(|| Mutex::new(Ring::new(CAPACITY)));

/// Logging must not turn one panic into a cascade of them: a thread that
/// died mid-push would otherwise poison the lock and take down every later
/// logger. The worst a poisoned guard can hold is a half-added line.
fn lock() -> MutexGuard<'static, Ring> {
    crate::lock_ok(&LOG)
}

/// The bounded line buffer. Split from the global so it can be tested on its
/// own instance — every other test in the process shares the global one.
struct Ring {
    lines: VecDeque<LogLine>,
    capacity: usize,
    recorded: u64,
    dropped: u64,
}

impl Ring {
    fn new(capacity: usize) -> Ring {
        Ring {
            lines: VecDeque::new(),
            capacity: capacity.max(1),
            recorded: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, level: Level, text: String) {
        while self.lines.len() >= self.capacity {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(LogLine {
            at: now_unix(),
            level,
            text,
        });
        self.recorded += 1;
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.dropped = 0;
    }
}

/// Seconds since the Unix epoch. A clock set before 1970 yields `0`, which
/// every caller already reads as "very long ago".
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(ring: &Ring) -> Vec<&str> {
        ring.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn lines_come_back_oldest_first() {
        let mut ring = Ring::new(8);
        ring.push(Level::Info, "one".into());
        ring.push(Level::Warn, "two".into());
        assert_eq!(texts(&ring), vec!["one", "two"]);
        assert_eq!(ring.lines[1].level, Level::Warn);
        assert_eq!(ring.recorded, 2);
        assert_eq!(ring.dropped, 0);
    }

    #[test]
    fn the_oldest_lines_are_dropped_at_capacity() {
        let mut ring = Ring::new(3);
        for i in 0..5 {
            ring.push(Level::Info, format!("line {}", i));
        }
        assert_eq!(
            texts(&ring),
            vec!["line 2", "line 3", "line 4"],
            "only the newest `capacity` lines survive"
        );
        assert_eq!(ring.dropped, 2, "and the count of the lost ones is kept");
        assert_eq!(ring.recorded, 5, "recorded counts everything ever pushed");
    }

    /// A zero capacity would spin the eviction loop forever on the first
    /// push; hand-configuring one is not possible today, but the ring should
    /// not depend on that staying true.
    #[test]
    fn a_zero_capacity_still_holds_one_line() {
        let mut ring = Ring::new(0);
        ring.push(Level::Info, "kept".into());
        assert_eq!(texts(&ring), vec!["kept"]);
    }

    #[test]
    fn clearing_empties_the_ring_but_not_the_total() {
        let mut ring = Ring::new(2);
        for i in 0..4 {
            ring.push(Level::Info, format!("line {}", i));
        }
        ring.clear();
        assert!(ring.lines.is_empty());
        assert_eq!(ring.dropped, 0, "dropped counts against what is shown");
        assert_eq!(ring.recorded, 4, "the running total survives a clear");
    }

    #[test]
    fn a_throttle_lets_the_first_few_through_and_counts_the_rest() {
        let t = Throttle::new(3);
        assert_eq!((t.seen(), t.suppressed()), (0, 0));
        for _ in 0..3 {
            assert!(t.allow(), "the first `limit` occurrences speak");
        }
        for _ in 0..7 {
            assert!(!t.allow(), "the rest are counted only");
        }
        assert_eq!(t.seen(), 10);
        assert_eq!(t.suppressed(), 7);

        t.reset();
        assert_eq!((t.seen(), t.suppressed()), (0, 0));
        assert!(t.allow(), "a reset throttle speaks again");
    }

    /// A zero limit must silence rather than divide by anything.
    #[test]
    fn a_zero_limit_throttle_logs_nothing() {
        let t = Throttle::new(0);
        assert!(!t.allow());
        assert_eq!(t.seen(), 1);
        assert_eq!(t.suppressed(), 1);
    }

    /// Through the global: the macros must land in the snapshot, and a
    /// warning must carry the prefix its terminal line has. Written to
    /// tolerate lines from tests running in parallel in this process.
    #[test]
    fn recorded_lines_reach_the_snapshot() {
        let before = recorded();
        crate::log_info!("test-marker info {}", 1);
        crate::log_warn!("test-marker warn {}", 2);
        assert!(recorded() >= before + 2);

        let lines = snapshot();
        let info = lines.iter().find(|l| l.text == "test-marker info 1");
        let warn = lines
            .iter()
            .find(|l| l.text == "Warning: test-marker warn 2");
        assert_eq!(info.map(|l| l.level), Some(Level::Info));
        assert_eq!(
            warn.map(|l| l.level),
            Some(Level::Warn),
            "a warning is stored with the prefix it printed with"
        );
        assert!(info.unwrap().at > 0, "timestamped when recorded");
    }
}
