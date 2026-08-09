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

/// A `Ui` from a real (headless) egui pass, so measuring helpers see the same
/// fonts the app paints with — the whole point of `middle_elide` and of the
/// snippet row arithmetic is that they agree with egui's own layout.
pub fn with_ui<R>(f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let ctx = egui::Context::default();
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

/// Every text galley painted this frame, in paint order, each with the
/// rectangle it occupies.
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

/// Every text galley painted this frame, each with the rectangle it occupies.
///
/// Labels carry no widget id worth recording, so reading the shapes back is
/// the only way to check the text a user actually sees — and the only way to
/// find a click target that follows the layout instead of pinning it.
pub fn painted(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
    painted_galleys(out)
        .into_iter()
        .map(|(g, rect)| (g.text().to_string(), rect))
        .collect()
}

/// Every string painted this frame, in paint order.
pub fn painted_text(out: &egui::FullOutput) -> Vec<String> {
    painted(out).into_iter().map(|(text, _)| text).collect()
}

/// Every styled *run* of text painted this frame with the color it was
/// painted in, in paint order.
///
/// [`painted`] and [`painted_text`] are color-blind, and a galley can hold
/// several colors at once (a status line whose phase word is hinted and whose
/// counters are not), so a color hint can only be checked one section at a
/// time. Runs are the layout job's own sections, so a single-color label
/// yields exactly one entry.
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

/// Every *visible* row of every galley painted this frame, in paint order.
///
/// Not the same thing as [`painted_text`]: a galley's `text()` is the job it
/// was laid out from, including the rows epaint dropped at `wrap.max_rows`. A
/// label that silently truncated away the very thing it was meant to show
/// still reads as complete there; the laid-out rows are the only place the
/// loss is visible.
pub fn painted_rows(out: &egui::FullOutput) -> Vec<String> {
    painted_galleys(out)
        .into_iter()
        .flat_map(|(g, _)| g.rows.iter().map(|r| r.text()).collect::<Vec<_>>())
        .collect()
}

/// Every mesh painted this frame, in paint order.
///
/// The app paints text and rectangles; a mesh means a shape assembled
/// vertex by vertex, which is the only way to get a gradient out of egui.
/// Reading the vertices back is the only way to check one, since the colour
/// that matters varies across the shape rather than being a property of it.
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
