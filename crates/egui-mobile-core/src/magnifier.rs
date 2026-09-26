//! Touch magnifier for text fields, the counterpart of Android's and iOS's native text loupe.
//!
//! While a finger presses, holds or drags inside the focused `TextEdit`, a lens above the finger
//! shows the line under it, magnified — the finger itself covers the caret and the selection end
//! it is placing. A drag shows the lens at once; a still press after [`HOLD_SECS`].
//!
//! The lens repaints the field layer's own shapes, scaled, rather than sampling the framebuffer, so
//! it works on every renderer (wgpu and glow alike) and costs no GPU readback. Text is scaled from
//! the glyph atlas, so it is as soft as a native pixel-copy loupe, not re-rasterized.
//!
//! The iOS and Android runtimes install it for every app; [`set_enabled`] turns it off.

use egui::emath::TSTransform;
use egui::epaint::{ClippedShape, Shape};
use egui::{Context, Id, LayerId, Order, Pos2, Rect, StrokeKind, Ui, pos2, vec2};

/// Magnification of the lens content.
pub const ZOOM: f32 = 1.5;
/// Seconds a still press waits before the lens appears.
pub const HOLD_SECS: f64 = 0.35;
/// Lens width in points, before it is narrowed to fit the screen.
const WIDTH: f32 = 150.0;
/// Lens corner radius; content is kept out of the rounded ends.
const RADIUS: f32 = 12.0;
/// Gap between the lens and the top of the line it magnifies.
const GAP: f32 = 12.0;
/// Clearance below the line for the fingertip, when the lens has no room above the line.
const FINGER: f32 = 48.0;
/// Space kept between the lens and the content edge.
const MARGIN: f32 = 8.0;

/// Install the magnifier on `ctx`. Installing twice is harmless.
pub fn install(ctx: &Context) {
    ctx.add_plugin(Magnifier::default());
}

/// Turn the magnifier on or off for this context.
pub fn set_enabled(ctx: &Context, enabled: bool) {
    ctx.with_plugin::<Magnifier, _>(|m| m.enabled = enabled);
}

/// Set the rect the lens must stay inside for this frame; the runtime calls this with the rect it
/// hands the app. Without it the lens stays inside the viewport.
pub fn set_content_bounds(ctx: &Context, rect: Rect) {
    ctx.with_plugin::<Magnifier, _>(|m| m.bounds = Some(rect));
}

struct Magnifier {
    enabled: bool,
    bounds: Option<Rect>,
}

impl Default for Magnifier {
    fn default() -> Self {
        Self { enabled: true, bounds: None }
    }
}

impl egui::Plugin for Magnifier {
    fn debug_name(&self) -> &'static str {
        "text-magnifier"
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let bounds = self.bounds.take();
        if !self.enabled {
            return;
        }
        let ctx = ui.ctx().clone();
        if let Some(target) = target(&ctx) {
            paint(&ctx, &target, bounds.unwrap_or_else(|| ctx.viewport_rect()));
        }
    }
}

/// The focused field under a pressing finger.
struct Target {
    layer: LayerId,
    /// The field's text area, in screen coordinates.
    field: Rect,
    /// The primary caret, which follows the finger while it selects.
    caret: Rect,
    finger: Pos2,
}

fn target(ctx: &Context) -> Option<Target> {
    let ime = ctx.output(|o| o.ime)?;
    let id = ctx.memory(|m| m.focused())?;
    egui::text_edit::TextEditState::load(ctx, id)?;
    let layer = ctx.viewport(|v| v.this_pass.widgets.get(id).map(|w| w.layer_id))?;
    let (down, finger, origin, held, dragging) = ctx.input(|i| {
        let p = &i.pointer;
        (p.primary_down(), p.latest_pos(), p.press_origin(), p.press_start_time().map(|t| i.time - t), p.is_decidedly_dragging())
    });
    if !down || !ime.rect.expand(4.0).contains(origin?) {
        return None;
    }
    let held = held?;
    if !dragging && held < HOLD_SECS {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(HOLD_SECS - held));
        return None;
    }
    Some(Target { layer, field: ime.rect, caret: ime.cursor_rect, finger: finger? })
}

/// The lens for a line whose caret is `caret`, centred on `x`, inside `bounds`: above the line,
/// or below the fingertip when the line sits too close to the top.
pub fn lens_rect(caret: Rect, x: f32, bounds: Rect) -> Rect {
    let height = (caret.height() * ZOOM + 16.0).max(44.0);
    let width = WIDTH.min(bounds.width() - 2.0 * MARGIN).max(height);
    let above = caret.top() - GAP - height;
    let top = if above >= bounds.top() + MARGIN {
        above
    } else {
        (caret.bottom() + FINGER).min(bounds.bottom() - MARGIN - height)
    };
    let left = (x - width * 0.5).min(bounds.right() - MARGIN - width).max(bounds.left() + MARGIN);
    Rect::from_min_size(pos2(left, top), vec2(width, height))
}

/// The screen area the lens shows: the caret's line around `x`, `lens` shrunk by [`ZOOM`].
pub fn source_rect(lens: Rect, caret: Rect, x: f32) -> Rect {
    Rect::from_center_size(pos2(x, caret.center().y), lens.size() / ZOOM)
}

/// Maps `source` onto `lens`, scaled by [`ZOOM`] about their centres.
pub fn lens_transform(source: Rect, lens: Rect) -> TSTransform {
    TSTransform::new(lens.center().to_vec2() - ZOOM * source.center().to_vec2(), ZOOM)
}

fn paint(ctx: &Context, t: &Target, bounds: Rect) {
    let x = t.finger.x.max(t.field.left()).min(t.field.right());
    let lens = lens_rect(t.caret, x, bounds);
    let source = source_rect(lens, t.caret, x);
    let to_global = ctx.layer_transform_to_global(t.layer).unwrap_or_default();
    let local = to_global.inverse() * source;
    let transform = lens_transform(source, lens) * to_global;
    // Paint callbacks draw at their own rect whatever the transform, so they are left out.
    let shapes: Vec<ClippedShape> = ctx.graphics(|g| {
        g.get(t.layer)
            .map(|list| {
                list.all_entries()
                    .filter(|c| {
                        !matches!(c.shape, Shape::Callback(_))
                            && c.clip_rect.intersects(local)
                            && c.shape.visual_bounding_rect().intersects(local)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    });
    let visuals = ctx.global_style().visuals.clone();
    let layer = LayerId::new(Order::Tooltip, Id::new("egui-mobile-magnifier"));
    let painter = ctx.layer_painter(layer);
    painter.add(visuals.popup_shadow.as_shape(lens, RADIUS));
    painter.rect_filled(lens, RADIUS, visuals.text_edit_bg_color());
    let band = lens.shrink2(vec2(RADIUS, 1.0));
    ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        for mut c in shapes {
            c.transform(transform);
            let clip = c.clip_rect.intersect(band);
            if clip.is_positive() {
                list.add(clip, c.shape);
            }
        }
    });
    painter.rect_stroke(lens, RADIUS, visuals.window_stroke, StrokeKind::Inside);
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, PointerButton, RawInput, TextEdit, TouchDeviceId, TouchId, TouchPhase};

    fn screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(400.0, 800.0))
    }

    #[test]
    fn the_lens_sits_above_the_line_centred_on_the_finger() {
        let caret = Rect::from_min_size(pos2(200.0, 400.0), vec2(2.0, 18.0));
        let lens = lens_rect(caret, 200.0, screen());
        assert!(lens.bottom() <= caret.top() - GAP + 0.01);
        assert!((lens.center().x - 200.0).abs() < 0.01);
    }

    #[test]
    fn a_line_near_the_top_puts_the_lens_below_the_finger() {
        let caret = Rect::from_min_size(pos2(200.0, 20.0), vec2(2.0, 18.0));
        let lens = lens_rect(caret, 200.0, screen());
        assert!(lens.top() >= caret.bottom() + FINGER - 0.01);
        assert!(screen().contains_rect(lens));
    }

    #[test]
    fn the_lens_stays_on_screen_at_either_edge() {
        let caret = Rect::from_min_size(pos2(0.0, 400.0), vec2(2.0, 18.0));
        for x in [-50.0, 0.0, 3.0, 397.0, 400.0, 450.0] {
            let lens = lens_rect(caret, x, screen());
            assert!(screen().shrink(MARGIN - 0.01).contains_rect(lens), "{x}: {lens:?}");
        }
        let narrow = Rect::from_min_size(Pos2::ZERO, vec2(120.0, 800.0));
        assert!(lens_rect(caret, 60.0, narrow).width() <= 120.0 - 2.0 * MARGIN);
    }

    #[test]
    fn the_source_line_fills_the_lens() {
        let caret = Rect::from_min_size(pos2(120.0, 400.0), vec2(2.0, 18.0));
        let lens = lens_rect(caret, 120.0, screen());
        let source = source_rect(lens, caret, 120.0);
        let t = lens_transform(source, lens);
        assert!((t * source).min.distance(lens.min) < 0.01);
        assert!((t * source).max.distance(lens.max) < 0.01);
        assert!(source.contains(caret.center()));
    }

    fn touch(pos: Pos2, pressed: bool) -> Vec<Event> {
        vec![
            Event::PointerMoved(pos),
            Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Default::default() },
            Event::Touch {
                device_id: TouchDeviceId(0),
                id: TouchId(1),
                phase: if pressed { TouchPhase::Start } else { TouchPhase::End },
                pos,
                force: None,
            },
        ]
    }

    fn frame(ctx: &Context, text: &mut String, time: f64, events: Vec<Event>) -> bool {
        let input = RawInput { screen_rect: Some(screen()), time: Some(time), events, ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            ui.add_space(300.0);
            ui.add(TextEdit::singleline(text).id(Id::new("field")));
        });
        out.textures_delta.clear();
        // Lens text: clipped to a band above the field.
        out.shapes.iter().any(|c| c.clip_rect.bottom() < 300.0 && matches!(c.shape, Shape::Text(_)))
    }

    #[test]
    fn a_held_press_on_the_focused_field_shows_the_lens_and_lifting_hides_it() {
        let ctx = Context::default();
        install(&ctx);
        let mut text = String::from("hello magnified world");
        let at = pos2(40.0, 310.0);
        ctx.memory_mut(|m| m.request_focus(Id::new("field")));
        assert!(!frame(&ctx, &mut text, 0.0, Vec::new()));
        assert!(!frame(&ctx, &mut text, 0.1, touch(at, true)));
        assert!(!frame(&ctx, &mut text, 0.2, Vec::new()));
        assert!(frame(&ctx, &mut text, 0.1 + HOLD_SECS + 0.05, Vec::new()));
        assert!(!frame(&ctx, &mut text, 1.0, touch(at, false)));
    }

    #[test]
    fn a_drag_shows_the_lens_before_the_hold_time() {
        let ctx = Context::default();
        install(&ctx);
        let mut text = String::from("hello magnified world");
        ctx.memory_mut(|m| m.request_focus(Id::new("field")));
        frame(&ctx, &mut text, 0.0, Vec::new());
        frame(&ctx, &mut text, 0.1, touch(pos2(20.0, 310.0), true));
        assert!(frame(&ctx, &mut text, 0.15, vec![Event::PointerMoved(pos2(60.0, 310.0))]));
    }

    #[test]
    fn a_press_outside_the_field_shows_nothing() {
        let ctx = Context::default();
        install(&ctx);
        let mut text = String::from("hello");
        ctx.memory_mut(|m| m.request_focus(Id::new("field")));
        frame(&ctx, &mut text, 0.0, Vec::new());
        frame(&ctx, &mut text, 0.1, touch(pos2(20.0, 100.0), true));
        assert!(!frame(&ctx, &mut text, 1.0, Vec::new()));
    }

    #[test]
    fn a_disabled_magnifier_draws_nothing() {
        let ctx = Context::default();
        install(&ctx);
        set_enabled(&ctx, false);
        let mut text = String::from("hello");
        let at = pos2(20.0, 310.0);
        ctx.memory_mut(|m| m.request_focus(Id::new("field")));
        frame(&ctx, &mut text, 0.0, Vec::new());
        frame(&ctx, &mut text, 0.1, touch(at, true));
        assert!(!frame(&ctx, &mut text, 1.0, Vec::new()));
    }
}
