//! Driving egui headlessly from tests: build an input frame, synthesize a
//! click, read back what was painted.

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
/// `predicted_dt` per pass, which is fine until a test is *about* what
/// something does over time — an animation, or a delay.
pub fn raw_input_at(size: egui::Vec2, events: Vec<egui::Event>, time: f64) -> egui::RawInput {
    egui::RawInput {
        time: Some(time),
        ..raw_input(size, events)
    }
}

/// Press and release at `pos`, preceded by the pointer moving there: egui
/// hit-tests against the pointer's *current* position, so a press without
/// the move lands wherever the pointer was last frame.
pub fn click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    vec![egui::Event::PointerMoved(pos), button(true), button(false)]
}

/// A context carrying the app's fonts — a default context has *no* faces,
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

fn painted_galleys(out: &egui::FullOutput) -> Vec<(&std::sync::Arc<egui::Galley>, egui::Rect)> {
    fn walk<'a>(
        shape: &'a egui::epaint::Shape,
        into: &mut Vec<(&'a std::sync::Arc<egui::Galley>, egui::Rect)>,
    ) {
        match shape {
            egui::epaint::Shape::Text(t) => {
                into.push((&t.galley, egui::Rect::from_min_size(t.pos, t.galley.size())))
            }
            egui::epaint::Shape::Vec(shapes) => {
                for s in shapes {
                    walk(s, into);
                }
            }
            _ => {}
        }
    }
    let mut galleys = Vec::new();
    for clipped in &out.shapes {
        walk(&clipped.shape, &mut galleys);
    }
    galleys
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

/// Each galley with the font size its first run was laid out at — how big
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

/// Runs with a background behind them — the distinguishing mark of a match:
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
    fn walk<'a>(shape: &'a egui::epaint::Shape, into: &mut Vec<&'a egui::epaint::RectShape>) {
        match shape {
            egui::epaint::Shape::Rect(rect) => into.push(rect),
            egui::epaint::Shape::Vec(shapes) => {
                for s in shapes {
                    walk(s, into);
                }
            }
            _ => {}
        }
    }
    let mut rects = Vec::new();
    for clipped in &out.shapes {
        walk(&clipped.shape, &mut rects);
    }
    rects
}

/// A mesh is the only way to get a gradient out of egui, and its color
/// varies across the shape, so the vertices are what a check has to read.
pub fn painted_meshes(out: &egui::FullOutput) -> Vec<&egui::Mesh> {
    fn walk<'a>(shape: &'a egui::epaint::Shape, into: &mut Vec<&'a egui::Mesh>) {
        match shape {
            egui::epaint::Shape::Mesh(mesh) => into.push(mesh),
            egui::epaint::Shape::Vec(shapes) => {
                for s in shapes {
                    walk(s, into);
                }
            }
            _ => {}
        }
    }
    let mut meshes = Vec::new();
    for clipped in &out.shapes {
        walk(&clipped.shape, &mut meshes);
    }
    meshes
}

/// The centre of `needle`'s galley, as a click target. The *last* match
/// wins, so a string painted behind a modal and on it resolves to the one
/// on top — the one a click would reach.
pub fn painted_text_center(out: &egui::FullOutput, needle: &str) -> Option<egui::Pos2> {
    painted(out)
        .iter()
        .rev()
        .find(|(text, _)| text == needle)
        .map(|(_, rect)| rect.center())
}
