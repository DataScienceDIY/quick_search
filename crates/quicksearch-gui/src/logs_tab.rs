//! The Logs tab: what the terminal would have shown.
//!
//! Everything here comes from [`quicksearch_core::log`], which background
//! threads write through instead of printing. Launched from a desktop
//! launcher there is no terminal to read, and this is the only place a
//! "cannot read that folder" warning is visible.

use quicksearch_core::log::{self, Level, LogLine};

use crate::format::group_thousands;
use crate::ui_util::hint;

/// How often to repaint while the tab is open: log lines arrive on threads
/// that never wake the UI, so an idle window would sit on a stale list.
const REFRESH_MS: u64 = 500;

pub struct LogsTab {
    /// Copy of the ring, refreshed only when the recorded count moves.
    lines: Vec<LogLine>,
    /// [`log::recorded`] as of the last refresh.
    seen: u64,
    dropped: u64,
    filter: String,
    warnings_only: bool,
    /// Keep the newest line in view. Scrolling up releases the view anyway
    /// (egui unsticks a scroll area the user moves, and re-sticks it when
    /// they return to the bottom); unticking this stops it following at all.
    follow: bool,
}

impl LogsTab {
    pub fn new() -> LogsTab {
        LogsTab {
            lines: Vec::new(),
            seen: 0,
            dropped: 0,
            filter: String::new(),
            warnings_only: false,
            follow: true,
        }
    }

    fn refresh(&mut self) {
        self.lines = log::snapshot();
        self.seen = log::recorded();
        self.dropped = log::dropped();
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if log::recorded() != self.seen {
            self.refresh();
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(REFRESH_MS));

        let needle = self.filter.to_lowercase();
        let shown: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| keep(l, &needle, self.warnings_only))
            .map(|(i, _)| i)
            .collect();

        let mut cleared = false;
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.follow, "Follow")
                .on_hover_text("Scroll to the newest line as it arrives");
            ui.checkbox(&mut self.warnings_only, "Warnings only");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .desired_width(200.0)
                    .hint_text("Filter"),
            );
            if ui
                .add_enabled(!shown.is_empty(), egui::Button::new("Copy"))
                .on_hover_text("Copy the lines shown below to the clipboard")
                .clicked()
            {
                let joined = shown
                    .iter()
                    .map(|&i| self.lines[i].text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.ctx().copy_text(joined);
            }
            if ui
                .add_enabled(!self.lines.is_empty(), egui::Button::new("Clear"))
                .clicked()
            {
                log::clear();
                cleared = true;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let count = if shown.len() == self.lines.len() {
                    format!("{} lines", group_thousands(self.lines.len() as u64))
                } else {
                    format!(
                        "{} of {} lines",
                        group_thousands(shown.len() as u64),
                        group_thousands(self.lines.len() as u64)
                    )
                };
                ui.label(hint(count));
            });
        });
        if cleared {
            // `shown` indexes lines that no longer exist.
            self.refresh();
            return;
        }
        if self.dropped > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "{} earlier lines were dropped; the newest {} are kept.",
                    group_thousands(self.dropped),
                    group_thousands(log::CAPACITY as u64),
                ))
                .small()
                .weak(),
            );
        }
        ui.separator();

        if self.lines.is_empty() {
            ui.label(
                egui::RichText::new(
                    "Nothing logged yet. Warnings from indexing, watching folders and \
                     opening files appear here — the same lines the terminal would show.",
                )
                .weak(),
            );
            return;
        }
        if shown.is_empty() {
            ui.label(egui::RichText::new("No lines match the filter.").weak());
            return;
        }

        // `show_rows` assumes every row is exactly one line tall, so long
        // paths extend into a horizontal scroll rather than wrapping.
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let scroll = egui::ScrollArea::both()
            .auto_shrink([false; 2])
            .stick_to_bottom(self.follow)
            .show_rows(ui, row_height, shown.len(), |ui, range| {
                for &i in &shown[range] {
                    let line = &self.lines[i];
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(fmt_clock(line.at)).monospace().weak());
                        let text = egui::RichText::new(&line.text).monospace();
                        match line.level {
                            Level::Warn => {
                                ui.colored_label(ui.visuals().warn_fg_color, text);
                            }
                            Level::Info => {
                                ui.label(text);
                            }
                        }
                    });
                }
            });
        crate::ui_util::more_below_hint(ui, &scroll);
    }
}

/// Whether a line survives the tab's two filters. `needle` must already be
/// lowercased.
fn keep(line: &LogLine, needle: &str, warnings_only: bool) -> bool {
    if warnings_only && line.level != Level::Warn {
        return false;
    }
    needle.is_empty() || line.text.to_lowercase().contains(needle)
}

/// `HH:MM:SS` local time.
fn fmt_clock(unix_secs: u64) -> String {
    use chrono::TimeZone;
    // Not `as`: that cast wraps a huge value into a negative one — a valid
    // 1969 timestamp that would render as a plausible time.
    let secs = i64::try_from(unix_secs).unwrap_or(i64::MAX);
    match chrono::Local.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%H:%M:%S").to_string(),
        _ => "--:--:--".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{fmt_clock, keep};
    use quicksearch_core::log::{Level, LogLine};

    fn line(level: Level, text: &str) -> LogLine {
        LogLine {
            at: 1_700_000_000,
            level,
            text: text.to_string(),
        }
    }

    #[test]
    fn an_empty_filter_keeps_everything() {
        assert!(keep(&line(Level::Info, "anything"), "", false));
        assert!(keep(&line(Level::Warn, "anything"), "", false));
    }

    #[test]
    fn the_filter_ignores_case_on_both_sides() {
        let l = line(Level::Warn, "Warning: cannot read /Home/Photos");
        assert!(keep(&l, "photos", false), "needle case must not matter");
        assert!(keep(&l, "cannot read", false), "nor the line's");
        assert!(!keep(&l, "videos", false));
    }

    #[test]
    fn warnings_only_hides_informational_lines() {
        assert!(!keep(&line(Level::Info, "shutting down"), "", true));
        assert!(keep(&line(Level::Warn, "cannot read"), "", true));
    }

    /// Both filters apply, not either.
    #[test]
    fn the_two_filters_compose() {
        assert!(!keep(&line(Level::Info, "cannot read"), "cannot", true));
        assert!(!keep(&line(Level::Warn, "cannot read"), "missing", true));
        assert!(keep(&line(Level::Warn, "cannot read"), "cannot", true));
    }

    #[test]
    fn a_clock_stamp_is_fixed_width() {
        assert_eq!(fmt_clock(0).len(), 8, "epoch renders as a time, not a date");
        assert_eq!(fmt_clock(1_700_000_000).len(), 8);
    }

    /// Beyond what a local calendar can represent, the row still lines up.
    #[test]
    fn an_out_of_range_stamp_falls_back() {
        assert_eq!(fmt_clock(u64::MAX), "--:--:--");
    }
}
