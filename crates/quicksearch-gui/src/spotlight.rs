//! Where the widgets the tour talks about are on screen, this pass.
//!
//! The tour's window is drawn last, long after the tabs that own the widgets
//! it points at, and it has no handle on any of them. So each interesting
//! widget publishes its rectangle here as it lays itself out, and the tour
//! reads them back at the end of the same pass to paint its glows.
//!
//! Two rules keep the registry honest. Nothing is recorded unless
//! [`set_active`] was called with `true` earlier in the frame — with the tour
//! closed, which is nearly always, marking costs one bool read. And every
//! entry is stamped with the pass that wrote it, so a widget that stopped
//! being drawn (a hidden column, another tab) stops being highlighted
//! immediately rather than glowing at a stale position.

use crate::app::Tab;

/// A widget the tour can point at. Not every widget: only the ones a page
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spot {
    /// The *Add folder… / path box / Add* row of Manage Index's folder list.
    IndexedFolderAdd,
    /// The box new ignore patterns are typed into, under Content filters.
    IgnorePatterns,
    /// The full-text extensions box beside it.
    ExtWhitelist,
    /// The query box on the Search tab.
    SearchBar,
    /// The `?` button that opens the query-syntax window.
    QueryHelp,
    /// The Rank column: its header cell unioned with every cell below it.
    RankColumn,
    /// The whole bottom panel.
    StatusBar,
    /// The Fuzzy label and its checkbox together.
    FuzzyToggle,
    /// One entry in the tab strip.
    TabButton(Tab),
}

/// The registry itself, parked in the context's temp store.
#[derive(Clone, Default)]
struct Spots {
    /// Whether a tour is on screen; written before the panels draw.
    active: bool,
    /// The pass `marks` describes. Marks from any other pass are discarded.
    pass: u64,
    marks: Vec<(Spot, egui::Rect)>,
}

fn id() -> egui::Id {
    egui::Id::new("quicksearch-spotlight")
}

/// `data_mut` holds the context's own lock, so everything the closure needs
/// from `ctx` — the pass number, in practice — must be read before the call:
/// asking egui a second question from inside it deadlocks.
fn with<R>(ctx: &egui::Context, f: impl FnOnce(&mut Spots) -> R) -> R {
    ctx.data_mut(|d| f(d.get_temp_mut_or_default::<Spots>(id())))
}

/// Turn marking on or off. Call once per frame, before anything that marks.
pub fn set_active(ctx: &egui::Context, active: bool) {
    with(ctx, |spots| {
        if !active {
            spots.marks.clear();
        }
        spots.active = active;
    });
}

/// Record where `spot` is. Repeat marks for one spot in a single pass are
/// unioned — that is how the Rank column adds up from its separate cells.
pub fn mark(ctx: &egui::Context, spot: Spot, rect: egui::Rect) {
    let pass = ctx.cumulative_pass_nr();
    with(ctx, |spots| {
        if !spots.active {
            return;
        }
        if spots.pass != pass {
            spots.pass = pass;
            spots.marks.clear();
        }
        match spots.marks.iter_mut().find(|(s, _)| *s == spot) {
            Some((_, known)) => *known = known.union(rect),
            None => spots.marks.push((spot, rect)),
        }
    });
}

/// Where `spot` was drawn this pass, or `None` if it was not drawn at all.
pub fn rect(ctx: &egui::Context, spot: Spot) -> Option<egui::Rect> {
    let pass = ctx.cumulative_pass_nr();
    with(ctx, |spots| {
        if spots.pass != pass {
            return None;
        }
        spots
            .marks
            .iter()
            .find(|(s, _)| *s == spot)
            .map(|(_, rect)| *rect)
    })
}

/// Mark a widget from its response, in the manner of [`crate::tips::Tipped`].
pub trait Spotlit {
    fn spot(self, spot: Spot) -> Self;
}

impl Spotlit for egui::Response {
    fn spot(self, spot: Spot) -> egui::Response {
        mark(&self.ctx, spot, self.rect);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One pass, running `body` inside it, so marks land against a real
    /// `cumulative_pass_nr` rather than the zero a bare context reports.
    fn pass(ctx: &egui::Context, body: impl FnOnce(&egui::Context)) {
        let mut body = Some(body);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            if let Some(body) = body.take() {
                body(ctx);
            }
        });
    }

    fn rect_at(x: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(x, x), egui::vec2(10.0, 10.0))
    }

    #[test]
    fn a_mark_is_readable_within_its_own_pass() {
        let ctx = egui::Context::default();
        pass(&ctx, |ctx| {
            set_active(ctx, true);
            mark(ctx, Spot::SearchBar, rect_at(4.0));
            assert_eq!(rect(ctx, Spot::SearchBar), Some(rect_at(4.0)));
            assert_eq!(rect(ctx, Spot::StatusBar), None, "never marked");
        });
    }

    /// The point of the pass stamp: a widget that is no longer drawn must
    /// stop reporting a position, not keep glowing where it used to be.
    #[test]
    fn a_mark_does_not_survive_into_the_next_pass() {
        let ctx = egui::Context::default();
        pass(&ctx, |ctx| {
            set_active(ctx, true);
            mark(ctx, Spot::SearchBar, rect_at(4.0));
        });
        pass(&ctx, |ctx| {
            assert_eq!(rect(ctx, Spot::SearchBar), None);
        });
    }

    #[test]
    fn nothing_is_recorded_while_inactive() {
        let ctx = egui::Context::default();
        pass(&ctx, |ctx| {
            mark(ctx, Spot::SearchBar, rect_at(4.0));
            assert_eq!(rect(ctx, Spot::SearchBar), None, "no tour, no marks");

            set_active(ctx, true);
            mark(ctx, Spot::SearchBar, rect_at(4.0));
            assert!(rect(ctx, Spot::SearchBar).is_some());

            // Dismissing the tour drops what was marked for it.
            set_active(ctx, false);
            assert_eq!(rect(ctx, Spot::SearchBar), None);
        });
    }

    /// A table column is marked once per cell; the glow has to cover all of them.
    #[test]
    fn repeat_marks_for_one_spot_union() {
        let ctx = egui::Context::default();
        pass(&ctx, |ctx| {
            set_active(ctx, true);
            mark(ctx, Spot::RankColumn, rect_at(0.0));
            mark(ctx, Spot::RankColumn, rect_at(40.0));
            assert_eq!(
                rect(ctx, Spot::RankColumn),
                Some(rect_at(0.0).union(rect_at(40.0)))
            );
        });
    }

    /// Tabs are distinct spots, not one shared "some tab" entry.
    #[test]
    fn tab_buttons_are_told_apart() {
        let ctx = egui::Context::default();
        pass(&ctx, |ctx| {
            set_active(ctx, true);
            mark(ctx, Spot::TabButton(Tab::Duplicates), rect_at(0.0));
            mark(ctx, Spot::TabButton(Tab::Settings), rect_at(40.0));
            assert_eq!(
                rect(ctx, Spot::TabButton(Tab::Duplicates)),
                Some(rect_at(0.0))
            );
            assert_eq!(
                rect(ctx, Spot::TabButton(Tab::Settings)),
                Some(rect_at(40.0))
            );
            assert_eq!(rect(ctx, Spot::TabButton(Tab::Help)), None);
        });
    }
}
