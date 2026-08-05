//! Driving egui headlessly from tests: build an input frame, synthesize a
//! click, read back what was painted.
//!
//! Every tab's test module wants the same three things, and each had grown its
//! own copy. The per-tab `frame(…)` wrappers stay where they are — each drives
//! a different widget with a different return type — but they are built on
//! these.

/// A frame of input at `size`, carrying `events`.
///
/// The viewport size is explicit rather than defaulted because it is load
/// bearing: a modal is centred in it, so tests that locate a button by where
/// it was painted get different coordinates from a different size, and a panel
/// that does not fit is simply not painted at all.
pub fn raw_input(size: egui::Vec2, events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        events,
        ..Default::default()
    }
}

/// A primary-button press and release at `pos`, preceded by the pointer moving
/// there.
///
/// The move is not decoration: egui hit-tests against the pointer's *current*
/// position, so a press delivered without one lands wherever the pointer was
/// last frame — which for the first frame of a fresh context is nowhere.
pub fn click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    vec![
        egui::Event::PointerMoved(pos),
        button(true),
        button(false),
    ]
}

/// Every text galley painted this frame, each with the rectangle it occupies.
///
/// Labels carry no widget id worth recording, so reading the shapes back is
/// the only way to check the text a user actually sees — and the only way to
/// find a click target that follows the layout instead of pinning it.
pub fn painted(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
    fn walk(shape: &egui::epaint::Shape, into: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::epaint::Shape::Text(t) => into.push((
                t.galley.text().to_string(),
                egui::Rect::from_min_size(t.pos, t.galley.size()),
            )),
            egui::epaint::Shape::Vec(shapes) => {
                for s in shapes {
                    walk(s, into);
                }
            }
            _ => {}
        }
    }
    let mut out_text = Vec::new();
    for clipped in &out.shapes {
        walk(&clipped.shape, &mut out_text);
    }
    out_text
}

/// Every string painted this frame, in paint order.
pub fn painted_text(out: &egui::FullOutput) -> Vec<String> {
    painted(out).into_iter().map(|(text, _)| text).collect()
}

/// The centre of `needle`'s galley, as a click target.
///
/// The *last* match wins, so a string painted both behind a modal and on it
/// resolves to the one on top — which is the one a click would reach.
pub fn painted_text_center(out: &egui::FullOutput, needle: &str) -> Option<egui::Pos2> {
    painted(out)
        .iter()
        .rev()
        .find(|(text, _)| text == needle)
        .map(|(_, rect)| rect.center())
}
