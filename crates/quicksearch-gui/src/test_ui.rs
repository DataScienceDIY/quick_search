//! Driving egui headlessly from tests: build an input frame, synthesize
//! input, read back what was painted.

use egui::epaint::Shape;

/// The viewport size is load-bearing: a modal is centred in it, and a panel
/// that does not fit is simply not painted at all.
pub fn raw_input(size: egui::Vec2, events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        events,
        ..Default::default()
    }
}

/// The same, with the clock pinned. Without a time egui advances by
/// `predicted_dt` per pass, which is fine until a test is about what
/// something does over time (an animation, or a delay).
pub fn raw_input_at(size: egui::Vec2, events: Vec<egui::Event>, time: f64) -> egui::RawInput {
    egui::RawInput {
        time: Some(time),
        ..raw_input(size, events)
    }
}

/// One frame of `draw` in a `CentralPanel` over a `size` viewport. `FnMut`
/// because egui may run a pass more than once per frame.
pub fn run_panel(
    ctx: &egui::Context,
    size: egui::Vec2,
    events: Vec<egui::Event>,
    mut draw: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    ctx.run(raw_input(size, events), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| draw(ui));
    })
}

/// Two frames of `run`, `events` in the second, which is returned: an
/// `egui::Window` is measured on its first frame and placed on the next, so
/// the first paints nothing to read back.
pub fn two_pass(
    ctx: &egui::Context,
    size: egui::Vec2,
    events: Vec<egui::Event>,
    mut run: impl FnMut(&egui::Context),
) -> egui::FullOutput {
    let _ = ctx.run(raw_input(size, Vec::new()), &mut run);
    ctx.run(raw_input(size, events), &mut run)
}

/// `which` pressed (or released) at `pos`.
pub fn button(pos: egui::Pos2, which: egui::PointerButton, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: which,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

/// `which` pressed and released at `pos`, after the pointer moves there:
/// egui hit-tests against the pointer's *current* position, so a press
/// without the move lands wherever the pointer was last frame.
pub fn click_with(pos: egui::Pos2, which: egui::PointerButton) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(pos),
        button(pos, which, true),
        button(pos, which, false),
    ]
}

/// A primary-button [`click_with`].
pub fn click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    click_with(pos, egui::PointerButton::Primary)
}

/// `key` pressed with `modifiers` held; egui acts on the press alone.
pub fn key_press(key: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
    vec![egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }]
}

/// Tooltips with no delay that show while the pointer moves, so a test
/// checks that a tooltip is wired up, not egui's hover timing.
pub fn instant_tooltips(ctx: &egui::Context) {
    ctx.style_mut(|s| {
        s.interaction.tooltip_delay = 0.0;
        s.interaction.show_tooltips_only_when_still = false;
    });
}

/// Whether hovering `pos` paints `text` somewhere. `frame` runs one frame
/// with the events it is given; the tooltip is its own area, so it may land
/// a frame or two behind the pointer.
pub fn hover_shows(
    mut frame: impl FnMut(Vec<egui::Event>) -> egui::FullOutput,
    pos: egui::Pos2,
    text: &str,
) -> bool {
    let mut out = frame(vec![egui::Event::PointerMoved(pos)]);
    for _ in 0..3 {
        if painted_text(&out).join("\n").contains(text) {
            return true;
        }
        out = frame(Vec::new());
    }
    false
}

/// A context carrying the app's fonts. A default context has *no* faces,
/// and that does not fail loudly: epaint hands back `row_height` 0.0 and
/// zero-advance glyphs, so every measurement quietly stops meaning anything.
pub fn ctx() -> egui::Context {
    let ctx = egui::Context::default();
    crate::fonts::install(&ctx);
    // The shipped text greys, so what a test measures is what the app paints.
    crate::color::apply_text_contrast(&ctx);
    ctx
}

/// Assert nothing painted this frame is a `◻`. Whitespace and controls are
/// skipped: epaint maps those to invisible glyphs on purpose, and `\n` is
/// documented to report as the replacement. Only sees what this frame
/// painted, so its reach is the calling test's reach.
pub fn assert_no_tofu(ctx: &egui::Context, out: &egui::FullOutput) {
    let mut missing: Vec<(char, String, egui::FontId)> = Vec::new();
    ctx.fonts(|fonts| {
        for (galley, _) in painted_galleys(out) {
            for section in &galley.job.sections {
                for c in galley.job.text[section.byte_range.clone()].chars() {
                    if c.is_whitespace() || c.is_control() {
                        continue;
                    }
                    if !fonts.has_glyph(&section.format.font_id, c) {
                        missing.push((
                            c,
                            galley.text().to_string(),
                            section.format.font_id.clone(),
                        ));
                    }
                }
            }
        }
    });
    assert!(missing.is_empty(), "no glyph for: {missing:#?}");
}

/// A `Ui` from a real headless pass, so helpers see the app's own fonts.
pub fn with_ui<R>(f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let ctx = ctx();
    let mut f = Some(f);
    let mut out = None;
    let _ = ctx.run(egui::RawInput::default(), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(f) = f.take() {
                out = Some(f(ui));
            }
        });
    });
    out.expect("the central panel ran")
}

/// Every shape painted this frame, nested `Shape::Vec`s flattened.
fn flat_shapes(out: &egui::FullOutput) -> Vec<&Shape> {
    fn walk<'a>(shape: &'a Shape, into: &mut Vec<&'a Shape>) {
        match shape {
            Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, into)),
            other => into.push(other),
        }
    }
    let mut shapes = Vec::new();
    for clipped in &out.shapes {
        walk(&clipped.shape, &mut shapes);
    }
    shapes
}

fn painted_galleys(out: &egui::FullOutput) -> Vec<(&std::sync::Arc<egui::Galley>, egui::Rect)> {
    flat_shapes(out)
        .into_iter()
        .filter_map(|shape| match shape {
            Shape::Text(t) => Some((&t.galley, egui::Rect::from_min_size(t.pos, t.galley.size()))),
            _ => None,
        })
        .collect()
}

/// Labels carry no widget id worth recording, so reading the shapes back is
/// the only way to check the text a user actually sees.
pub fn painted(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
    painted_galleys(out)
        .into_iter()
        .map(|(g, rect)| (g.text().to_string(), rect))
        .collect()
}

pub fn painted_text(out: &egui::FullOutput) -> Vec<String> {
    painted(out).into_iter().map(|(text, _)| text).collect()
}

/// Where `text` was first painted; panics if it was not.
pub fn painted_rect(out: &egui::FullOutput, text: &str) -> egui::Rect {
    painted(out)
        .into_iter()
        .find(|(painted, _)| painted == text)
        .map(|(_, rect)| rect)
        .unwrap_or_else(|| panic!("nothing painted for {text:?}"))
}

/// Every styled *run* with its color: runs are the layout job's own
/// sections, so a single-color label yields exactly one entry.
pub fn painted_spans(out: &egui::FullOutput) -> Vec<(String, egui::Color32)> {
    painted_galleys(out)
        .into_iter()
        .flat_map(|(g, _)| {
            g.job
                .sections
                .iter()
                .map(|s| (g.job.text[s.byte_range.clone()].to_string(), s.format.color))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Each galley with the font size its first run was laid out at: how big
/// text actually came out, which the style alone cannot say once a widget
/// has overridden it.
pub fn painted_sizes(out: &egui::FullOutput) -> Vec<(String, f32)> {
    painted_galleys(out)
        .into_iter()
        .filter_map(|(g, _)| {
            let size = g.job.sections.first()?.format.font_id.size;
            Some((g.text().to_string(), size))
        })
        .collect()
}

/// Runs with a background behind them, the distinguishing mark of a match:
/// headers use the same *text* color, so [`painted_spans`] cannot tell them apart.
pub fn painted_backgrounds(out: &egui::FullOutput) -> Vec<(String, egui::Color32)> {
    painted_galleys(out)
        .into_iter()
        .flat_map(|(g, _)| {
            g.job
                .sections
                .iter()
                .filter(|s| s.format.background != egui::Color32::TRANSPARENT)
                .map(|s| {
                    (
                        g.job.text[s.byte_range.clone()].to_string(),
                        s.format.background,
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Every *visible* row: a galley's `text()` includes the rows epaint
/// dropped at `wrap.max_rows`, so the laid-out rows are the only place a
/// truncation is visible.
pub fn painted_rows(out: &egui::FullOutput) -> Vec<String> {
    painted_galleys(out)
        .into_iter()
        .flat_map(|(g, _)| g.rows.iter().map(|r| r.text()).collect::<Vec<_>>())
        .collect()
}

/// Every rectangle painted this frame. Fills and strokes both arrive as
/// `RectShape`, so a caller checking for one has to look at which of the two
/// the shape actually carries.
pub fn painted_rects(out: &egui::FullOutput) -> Vec<&egui::epaint::RectShape> {
    flat_shapes(out)
        .into_iter()
        .filter_map(|shape| match shape {
            Shape::Rect(rect) => Some(rect),
            _ => None,
        })
        .collect()
}

/// A mesh is the only way to get a gradient out of egui, and its color
/// varies across the shape, so the vertices are what a check has to read.
pub fn painted_meshes(out: &egui::FullOutput) -> Vec<&egui::Mesh> {
    flat_shapes(out)
        .into_iter()
        .filter_map(|shape| match shape {
            Shape::Mesh(mesh) => Some(&**mesh),
            _ => None,
        })
        .collect()
}

/// The centre of `needle`'s galley, as a click target. The *last* match
/// wins, so a string painted behind a modal and on it resolves to the one
/// on top, which a click would reach.
pub fn painted_text_center(out: &egui::FullOutput, needle: &str) -> Option<egui::Pos2> {
    painted(out)
        .iter()
        .rev()
        .find(|(text, _)| text == needle)
        .map(|(_, rect)| rect.center())
}
