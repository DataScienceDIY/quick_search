//! The process log: every line that would go to the terminal, kept in
//! memory so a windowed run can show it.
//!
//! Launched from a desktop launcher — or on Windows, where the GUI binary is
//! built for the window subsystem and has no console at all — the process has
//! nowhere to print. The warnings the walker, indexer and watcher emit are
//! exactly the ones a user needs when something looks wrong, and they were
//! going nowhere.
//!
//! So background reporting goes through [`log_info!`] and [`log_warn!`]
//! instead of `println!`/`eprintln!`: each writes the same line to stderr
//! *and* appends it to a bounded ring the GUI's Logs tab reads. A terminal
//! run looks exactly as it did; a windowed run gains the tab.
//!
//! Command output — search hits from `quicksearch-cli`, usage text, the
//! errors a command exits with — is not logged. That is a program's answer to
//! what it was asked, not a background event, and it belongs on stdout.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lines retained before the oldest are dropped.
///
/// A run over a tree full of unreadable files can log per file, so this is
/// bounded rather than complete: the newest few thousand lines are what
/// diagnosing anything actually needs, and the ring holds the count it threw
/// away so the tab can say so instead of quietly lying.
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

/// Write `message` to stderr and to the ring.
///
/// Prefer the [`log_info!`] / [`log_warn!`] macros; this is what they call.
///
/// A failed stderr write is ignored rather than propagated: `eprintln!`
/// *panics* when the handle is unwritable, which on a process launched
/// without stdio would take down whichever background thread happened to
/// report something. Losing the terminal copy is acceptable — that is
/// precisely the case where the in-memory copy is the one that matters.
pub fn record(level: Level, message: String) {
    let text = match level {
        Level::Warn => format!("Warning: {}", message),
        Level::Info => message,
    };
    let _ = writeln!(std::io::stderr(), "{}", text);
    lock().push(level, text);
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
/// logger. The ring is a `VecDeque` of owned strings, so the worst a poisoned
/// guard can hold is a line that was half-added.
fn lock() -> MutexGuard<'static, Ring> {
    LOG.lock().unwrap_or_else(|e| e.into_inner())
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

fn now_unix() -> u64 {
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
