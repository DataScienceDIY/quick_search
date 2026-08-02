//! Small display formatters shared across tabs.

/// Human-readable byte size: `999 B`, `1.2 KB`, `4.7 MB`, `1.3 GB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// `YYYY-MM-DD HH:MM` in local time; raw seconds if out of range.
pub fn fmt_mtime(unix_secs: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(unix_secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => unix_secs.to_string(),
    }
}

/// Relative time for recent events, absolute for old ones: "just now",
/// "5 min ago", "3 h ago", else `YYYY-MM-DD HH:MM`. Gives instant
/// feedback that an action (like a fast index run) actually happened.
pub fn fmt_ago(unix_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(unix_secs);
    if age < 60 {
        "just now".to_string()
    } else if age < 3600 {
        format!("{} min ago", age / 60)
    } else if age < 86_400 {
        format!("{} h ago", age / 3600)
    } else {
        fmt_mtime(unix_secs as i64)
    }
}

/// A configured interval as a phrase to drop after "every": `90 min`,
/// `24 h`, `3 days`. Used where the periodic reindex is the only thing
/// refreshing the index, so the user can judge how stale it may get.
pub fn fmt_interval(minutes: u64) -> String {
    if minutes == 0 {
        // The scheduler treats 0 as always-due.
        return "run".to_string();
    }
    if minutes < 60 {
        return format!("{} min", minutes);
    }
    if minutes.is_multiple_of(1440) {
        let days = minutes / 1440;
        return if days == 1 {
            // "24 h" reads better than "1 day" for the shipped default.
            "24 h".to_string()
        } else {
            format!("{} days", days)
        };
    }
    if minutes.is_multiple_of(60) {
        return format!("{} h", minutes / 60);
    }
    format!("{} h {} min", minutes / 60, minutes % 60)
}

/// Group thousands for counts: `1,234,567`.
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Files/sec display. Never renders a nonzero rate as "0.0": slow rates
/// switch to a per-minute figure.
pub fn fmt_rate(files_per_sec: f64) -> String {
    if files_per_sec <= 0.0 {
        "0 files/s".to_string()
    } else if files_per_sec >= 10.0 {
        format!("{:.0} files/s", files_per_sec)
    } else if files_per_sec >= 1.0 {
        format!("{:.1} files/s", files_per_sec)
    } else {
        format!("{:.0} files/min", (files_per_sec * 60.0).max(1.0))
    }
}

/// Search duration: milliseconds under a second, seconds above.
pub fn fmt_elapsed(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms >= 1000 {
        format!("{:.1} s", d.as_secs_f64())
    } else {
        format!("{} ms", ms)
    }
}

/// Middle-truncate a path to at most `max_chars` characters.
pub fn middle_truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars || max_chars < 5 {
        return s.to_string();
    }
    let keep = max_chars - 1;
    let head = keep / 2;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1200), "1.2 KB");
        assert_eq!(human_size(4_700_000), "4.7 MB");
        assert_eq!(human_size(1_300_000_000), "1.3 GB");
    }

    #[test]
    fn intervals() {
        assert_eq!(fmt_interval(0), "run");
        assert_eq!(fmt_interval(1), "1 min");
        assert_eq!(fmt_interval(59), "59 min");
        assert_eq!(fmt_interval(60), "1 h");
        assert_eq!(fmt_interval(90), "1 h 30 min");
        assert_eq!(fmt_interval(120), "2 h");
        assert_eq!(fmt_interval(1440), "24 h", "the shipped default");
        assert_eq!(fmt_interval(2880), "2 days");
        assert_eq!(fmt_interval(10_080), "7 days");
    }

    #[test]
    fn thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn rates_never_show_zero_for_nonzero() {
        assert_eq!(fmt_rate(0.0), "0 files/s");
        assert_eq!(fmt_rate(2543.0), "2543 files/s");
        // Not 3.14: clippy reads that as a botched `PI` and denies it.
        assert_eq!(fmt_rate(3.12), "3.1 files/s");
        assert_eq!(fmt_rate(0.4), "24 files/min");
        assert_eq!(fmt_rate(0.001), "1 files/min", "floor at 1/min, never 0.0");
    }

    #[test]
    fn elapsed_units() {
        use std::time::Duration;
        assert_eq!(fmt_elapsed(Duration::from_millis(0)), "0 ms");
        assert_eq!(fmt_elapsed(Duration::from_millis(7)), "7 ms");
        assert_eq!(fmt_elapsed(Duration::from_millis(999)), "999 ms");
        assert_eq!(fmt_elapsed(Duration::from_millis(1000)), "1.0 s");
        assert_eq!(fmt_elapsed(Duration::from_millis(2340)), "2.3 s");
    }

    #[test]
    fn ago_buckets() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(fmt_ago(now), "just now");
        assert_eq!(fmt_ago(now - 59), "just now");
        assert_eq!(fmt_ago(now - 120), "2 min ago");
        assert_eq!(fmt_ago(now - 7200), "2 h ago");
        assert!(fmt_ago(now - 200_000).contains('-'), "old = absolute date");
    }

    #[test]
    fn truncation() {
        assert_eq!(middle_truncate("short", 20), "short");
        let t = middle_truncate("/very/long/path/to/some/file.txt", 15);
        assert!(t.chars().count() <= 15);
        assert!(t.contains('…'));
        assert!(t.starts_with("/very"));
        assert!(t.ends_with("e.txt"));
    }
}
