//! The Logs tab: every line background threads write to
//! [`quicksearch_core::log`] instead of printing. Launched from a desktop
//! launcher there is no terminal, so this is the only place they show.

use quicksearch_core::log::{self, Level, LogLine};

use crate::format::group_thousands;
use crate::ui_util::hint;

/// Repaint cadence: log lines arrive on threads that never wake the UI.
const REFRESH_MS: u64 = 500;

pub struct LogsTab {
    lines: Vec<LogLine>,
    /// [`log::recorded`] as of the last refresh.
    seen: u64,
    dropped: u64,
    filter: String,
    warnings_only: bool,
    /// Keep the newest line in view. egui already unsticks a scroll area the
    /// user drags and re-sticks it at the bottom; this stops it following.
    follow: bool,
    /// Indices into `lines` that pass both filters. Measured: rebuilding this
    /// per frame lowercased up to [`log::CAPACITY`] lines on every frame.
    shown: Vec<usize>,
    /// What `shown` was computed from: filter and warnings-only; `None`
    /// after a refresh.
    shown_key: Option<(String, bool)>,
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
            shown: Vec::new(),
            shown_key: None,
        }
    }

    fn resync_shown(&mut self) {
        if matches!(
            &self.shown_key,
            Some((f, w)) if *f == self.filter && *w == self.warnings_only
        ) {
            return;
        }
        let needle = self.filter.to_lowercase();
        self.shown.clear();
        self.shown.extend(
            self.lines
                .iter()
                .enumerate()
                .filter(|(_, l)| keep(l, &needle, self.warnings_only))
                .map(|(i, _)| i),
        );
        self.shown_key = Some((self.filter.clone(), self.warnings_only));
    }

    fn refresh(&mut self) {
        self.lines = log::snapshot();
        self.seen = log::recorded();
        self.dropped = log::dropped();
        self.shown_key = None;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if log::recorded() != self.seen {
            self.refresh();
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(REFRESH_MS));

        self.resync_shown();

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
                .add_enabled(!self.shown.is_empty(), egui::Button::new("Copy"))
                .on_hover_text("Copy the lines shown below to the clipboard")
                .clicked()
            {
                let joined = self
                    .shown
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
                self.refresh();
                self.resync_shown();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let count = if self.shown.len() == self.lines.len() {
                    format!("{} lines", group_thousands(self.lines.len() as u64))
                } else {
                    format!(
                        "{} of {} lines",
                        group_thousands(self.shown.len() as u64),
                        group_thousands(self.lines.len() as u64)
                    )
                };
                ui.label(hint(count));
            });
        });
        if self.dropped > 0 {
            ui.label(hint(format!(
                "{} earlier lines were dropped; the newest {} are kept.",
                group_thousands(self.dropped),
                group_thousands(log::CAPACITY as u64),
            )));
        }
        ui.separator();

        if self.lines.is_empty() {
            ui.label(
                egui::RichText::new(
                    "Nothing logged yet. Warnings from indexing, watching folders and \
                     opening files appear here, as they would in a terminal.",
                )
                .weak(),
            );
            return;
        }
        if self.shown.is_empty() {
            ui.label(egui::RichText::new("No lines match the filter.").weak());
            return;
        }

        // `show_rows` assumes every row is exactly one line tall.
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let scroll = egui::ScrollArea::both()
            .auto_shrink([false; 2])
            .stick_to_bottom(self.follow)
            .show_rows(ui, row_height, self.shown.len(), |ui, range| {
                for &i in &self.shown[range] {
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

/// Whether a line survives both filters; `needle` must already be lowercased.
fn keep(line: &LogLine, needle: &str, warnings_only: bool) -> bool {
    if warnings_only && line.level != Level::Warn {
        return false;
    }
    needle.is_empty() || line.text.to_lowercase().contains(needle)
}

/// `HH:MM:SS` local time.
fn fmt_clock(unix_secs: u64) -> String {
    use chrono::TimeZone;
    // Not `as`: that cast wraps a huge value into a plausible 1969 timestamp.
    let secs = i64::try_from(unix_secs).unwrap_or(i64::MAX);
    match chrono::Local.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%H:%M:%S").to_string(),
        _ => "--:--:--".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{fmt_clock, keep, LogsTab};
    use crate::format::group_thousands;
    use crate::test_ui::{click_at, painted_rect, painted_text, run_panel};
    use quicksearch_core::log::{self, Level, LogLine};

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

    #[test]
    fn an_out_of_range_stamp_falls_back() {
        assert_eq!(fmt_clock(u64::MAX), "--:--:--");
    }

    /// The ring is process-wide and these tests clear it, so they take
    /// turns. Lines other tests log meanwhile are tolerated.
    static RING_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Ignore poisoning: one failed test must not fail the others.
    fn ring_guard() -> std::sync::MutexGuard<'static, ()> {
        RING_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One frame of the tab. `shown` must never index past `lines`: that
    /// stale index is what Copy panics on.
    fn frame(ctx: &egui::Context, tab: &mut LogsTab, events: Vec<egui::Event>) -> egui::FullOutput {
        let out = run_panel(ctx, egui::vec2(900.0, 400.0), events, |ui| tab.ui(ui));
        assert!(
            tab.shown.iter().all(|&i| i < tab.lines.len()),
            "shown {:?} indexes past {} lines",
            tab.shown,
            tab.lines.len()
        );
        out
    }

    /// Click the widget painted as `label`; egui reports it on this frame.
    fn click(
        ctx: &egui::Context,
        tab: &mut LogsTab,
        out: &egui::FullOutput,
        label: &str,
    ) -> egui::FullOutput {
        let at = painted_rect(out, label).center();
        frame(ctx, tab, click_at(at))
    }

    fn holds(tab: &LogsTab, text: &str) -> bool {
        tab.lines.iter().any(|l| l.text == text)
    }

    fn shown(tab: &LogsTab) -> Vec<&str> {
        tab.shown
            .iter()
            .map(|&i| tab.lines[i].text.as_str())
            .collect()
    }

    #[test]
    fn copy_after_clear_does_not_panic() {
        let _ring = ring_guard();
        let text = "logs tab: copy after clear";
        log::record(Level::Info, text.to_string());
        let ctx = crate::test_ui::ctx();
        let mut tab = LogsTab::new();

        let out = frame(&ctx, &mut tab, vec![]);
        assert!(holds(&tab, text));
        click(&ctx, &mut tab, &out, "Clear");
        assert!(!holds(&tab, text), "the click cleared the ring");
        let out = frame(&ctx, &mut tab, vec![]);
        click(&ctx, &mut tab, &out, "Copy");
        frame(&ctx, &mut tab, vec![]);
    }

    /// The count painted this frame is the one `tab` now holds, never "N of 0".
    fn assert_count(out: &egui::FullOutput, tab: &LogsTab) {
        // No filter is set, so every retained line is shown.
        let want = format!("{} lines", group_thousands(tab.lines.len() as u64));
        let painted = painted_text(out);
        assert!(painted.contains(&want), "want {want:?} in {painted:?}");
        assert!(
            !painted.iter().any(|t| t.ends_with(" of 0 lines")),
            "{painted:?}"
        );
    }

    #[test]
    fn after_clear_the_count_is_not_n_of_0() {
        let _ring = ring_guard();
        log::record(Level::Info, "logs tab: count, first".to_string());
        log::record(Level::Info, "logs tab: count, second".to_string());
        let ctx = crate::test_ui::ctx();
        let mut tab = LogsTab::new();

        let out = frame(&ctx, &mut tab, vec![]);
        assert_count(&out, &tab);
        let clicked = click(&ctx, &mut tab, &out, "Clear");
        assert!(!holds(&tab, "logs tab: count, second"), "the click cleared");
        // The label is drawn after the button, so the click frame already
        // counts what is left.
        assert_count(&clicked, &tab);
        let next = frame(&ctx, &mut tab, vec![]);
        assert_count(&next, &tab);
    }

    #[test]
    fn a_filter_edit_after_clear_rebuilds_from_the_new_lines() {
        let _ring = ring_guard();
        let before = "logs tab: filter mark, before the clear";
        let after = "logs tab: filter mark, after the clear";
        let later = "logs tab: filter mark, under the same filter";
        log::record(Level::Info, before.to_string());
        let ctx = crate::test_ui::ctx();
        let mut tab = LogsTab::new();

        let out = frame(&ctx, &mut tab, vec![]);
        click(&ctx, &mut tab, &out, "Clear");
        log::record(Level::Info, after.to_string());
        let out = frame(&ctx, &mut tab, vec![]);
        click(&ctx, &mut tab, &out, "Filter");
        let typed = vec![egui::Event::Text("FILTER MARK".into())];
        frame(&ctx, &mut tab, typed);
        assert_eq!(tab.filter, "FILTER MARK", "the typing reached the field");
        // `shown` follows an edit from the next frame on.
        frame(&ctx, &mut tab, vec![]);
        assert!(!holds(&tab, before), "cleared lines stay gone");
        assert_eq!(
            tab.shown,
            (0..tab.lines.len())
                .filter(|&i| keep(&tab.lines[i], "filter mark", false))
                .collect::<Vec<_>>()
        );
        assert!(shown(&tab).contains(&after), "{:?}", shown(&tab));

        // A refresh rebuilds `shown` even though the filter did not change.
        log::record(Level::Info, later.to_string());
        frame(&ctx, &mut tab, vec![]);
        assert!(shown(&tab).contains(&later), "{:?}", shown(&tab));
    }
}
