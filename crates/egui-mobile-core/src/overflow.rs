//! Debug-only guard that reports UI crossing the left or right content edge.
//!
//! egui clips a widget that lands past the edge instead of failing, so a row that fits a desktop
//! harness silently loses its last button on a phone. The guard walks every widget rect egui
//! registered this pass, paints the offenders red with a label naming them, and logs one line per
//! widget under the `ui_overflow` target (logcat tag `ui_overflow`) so a build/run loop can grep
//! for it: `cargo egui-mobile logcat -a --check-ui` exits non-zero when there is one.
//!
//! The iOS and Android runtimes install it themselves, so an app gets the check without opting in.
//! Everything here is behind `cfg!(debug_assertions)`: a release build registers no plugin and
//! every entry point returns immediately.
//!
//! Only the horizontal edges are checked. A vertically scrolled list always has a half-cut row at
//! the top and bottom of the viewport, so a vertical check is noise; a widget cut by the right
//! edge is a bug unless it sits in a horizontal [`egui::ScrollArea`], which [`allow`] excuses.

use egui::epaint::Shape;
use egui::{
    Align2, Color32, Context, FontId, FullOutput, Id, Rect, Stroke, StrokeKind, Ui, WidgetRects,
    pos2,
};
use std::collections::HashMap;

/// What the guard does when a widget crosses the content edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Policy {
    /// Paint the offender and log it. The default.
    #[default]
    Report,
    /// Paint, log, then panic on the frame the overflow appears.
    Panic,
    /// Check nothing.
    Off,
}

/// Overflow below this many points is rounding, not a layout bug.
const TOLERANCE: f32 = 0.5;

/// Offenders painted and logged per frame, so one broken row cannot bury the log.
const MAX_PER_FRAME: usize = 8;

/// Undrained reports kept before the oldest is dropped.
const MAX_REPORTS: usize = 64;

/// Marks the guard's own on-screen labels so they are never mistaken for app text.
const CHIP_PREFIX: &str = "clipped ";

/// Install the guard on `ctx`. No-op in a release build, and installing twice is harmless.
///
/// Reads `EGUI_UI_OVERFLOW` (`report`, `panic`, `off`) for the policy; defaults to
/// [`Policy::Report`].
pub fn install(ctx: &Context) {
    install_with(ctx, policy_from_env());
}

/// [`install`] with an explicit policy, ignoring `EGUI_UI_OVERFLOW`.
pub fn install_with(ctx: &Context, policy: Policy) {
    if !cfg!(debug_assertions) {
        return;
    }
    ctx.add_plugin(Guard { policy, ..Guard::default() });
}

/// Set the rect the UI is expected to stay inside for this frame; the runtime calls this with the
/// rect it hands the app. Without it the guard falls back to the viewport rect.
pub fn set_content_bounds(ctx: &Context, rect: Rect) {
    with_guard(ctx, |g| g.bounds = Some(rect));
}

/// Drain the overflow messages logged since the last call. Empty in a release build. Lets an app
/// surface them in its own debug UI, and a host-side test assert a screen size lays out clean.
pub fn take_reports(ctx: &Context) -> Vec<String> {
    let mut out = Vec::new();
    with_guard(ctx, |g| out = std::mem::take(&mut g.reports));
    out
}

/// Change the policy on an installed guard.
pub fn set_policy(ctx: &Context, policy: Policy) {
    with_guard(ctx, |g| g.policy = policy);
}

/// Excuse everything drawn in `ui`'s clip rect this frame. Call it inside a deliberately
/// horizontally scrolling area, whose content is meant to run past the edge.
pub fn allow(ui: &Ui) {
    allow_rect(ui.ctx(), ui.clip_rect());
}

/// [`allow`] for an explicit rect.
pub fn allow_rect(ctx: &Context, rect: Rect) {
    with_guard(ctx, |g| g.allow.push(rect));
}

fn with_guard(ctx: &Context, f: impl FnOnce(&mut Guard)) {
    if !cfg!(debug_assertions) {
        return;
    }
    ctx.with_plugin::<Guard, _>(f);
}

fn policy_from_env() -> Policy {
    match std::env::var("EGUI_UI_OVERFLOW").as_deref() {
        Ok("panic") => Policy::Panic,
        Ok("off") => Policy::Off,
        _ => Policy::Report,
    }
}

#[derive(Default)]
struct Guard {
    policy: Policy,
    bounds: Option<Rect>,
    allow: Vec<Rect>,
    /// Last overflow logged per widget, to log a steady bug once instead of every frame.
    reported: HashMap<Id, f32>,
    /// Names resolved from painted text, reused on later frames for the on-screen label.
    names: HashMap<Id, String>,
    reports: Vec<String>,
    /// Found while closing the pass, reported once the frame's shapes can name them.
    pending: Vec<Offender>,
}

impl egui::Plugin for Guard {
    fn debug_name(&self) -> &'static str {
        "ui-overflow-guard"
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        if self.policy == Policy::Off {
            return;
        }
        let ctx = ui.ctx().clone();
        let screen = ctx.input(|i| i.viewport_rect());
        let bounds = self.bounds.take().unwrap_or(screen);
        let allow = std::mem::take(&mut self.allow);
        // `this_pass` still holds the pass being closed; the swap into `prev_pass` happens after.
        let mut found = ctx.viewport(|v| offenders(&v.this_pass.widgets, bounds, &allow));
        found.truncate(MAX_PER_FRAME);
        if found.is_empty() {
            return;
        }

        let painter = ctx.debug_painter();
        let red = Color32::from_rgb(230, 40, 40);
        let mut chip_y = screen.top() + 2.0;
        for o in &found {
            let visible = o.rect.intersect(screen);
            if visible.is_positive() {
                painter.rect_stroke(visible, 0.0, Stroke::new(2.0, red), StrokeKind::Inside);
            }
            let edge_x = if o.edge == Edge::Right { bounds.right() } else { bounds.left() };
            painter.line_segment(
                [pos2(edge_x, o.rect.top()), pos2(edge_x, o.rect.bottom())],
                Stroke::new(1.0, red),
            );

            let name = o.name.clone().or_else(|| self.names.get(&o.id).cloned());
            let text = format!(
                "{CHIP_PREFIX}{:.0}pt {} - {}",
                o.over,
                o.edge.as_str(),
                name.unwrap_or_else(|| o.id.short_debug_format())
            );
            let galley = painter.layout_no_wrap(text, FontId::proportional(10.0), Color32::WHITE);
            let anchor = pos2(bounds.right().min(screen.right() - 2.0), o.rect.top().max(chip_y));
            let chip = Align2::RIGHT_TOP.anchor_size(anchor, galley.size()).expand(2.0);
            painter.rect_filled(chip, 2.0, Color32::from_rgba_unmultiplied(120, 0, 0, 230));
            painter.galley(chip.shrink(2.0).min, galley, Color32::WHITE);
            chip_y = chip.bottom() + 2.0;
        }
        self.pending = found;
    }

    fn output_hook(&mut self, _ctx: &Context, out: &mut FullOutput) {
        for o in std::mem::take(&mut self.pending) {
            // egui records widget labels only under `debug.show_interactive_widgets`, so the name
            // normally comes from the text painted inside the widget this same frame.
            let name = o
                .name
                .clone()
                .or_else(|| text_inside(&out.shapes, o.rect))
                .or_else(|| self.names.get(&o.id).cloned());
            if let Some(name) = &name {
                self.names.insert(o.id, name.clone());
            }
            self.report(&o, name);
        }
    }
}

impl Guard {
    fn report(&mut self, o: &Offender, name: Option<String>) {
        let last = self.reported.get(&o.id).copied().unwrap_or(f32::MIN);
        if (o.over - last).abs() <= TOLERANCE {
            return;
        }
        self.reported.insert(o.id, o.over);
        let msg = format!(
            "UI_OVERFLOW {} +{:.1}pt: {} at x {:.0}..{:.0} y {:.0}..{:.0}, content x {:.0}..{:.0}. \
             The widget is clipped on this screen - shrink it, truncate or wrap its text, \
             give the row a max width, or call egui_mobile::overflow::allow() if the overflow \
             is a deliberate horizontal scroll.",
            o.edge.as_str(),
            o.over,
            name.unwrap_or_else(|| format!("unnamed widget {}", o.id.short_debug_format())),
            o.rect.left(),
            o.rect.right(),
            o.rect.top(),
            o.rect.bottom(),
            o.bounds.left(),
            o.bounds.right(),
        );
        log::error!(target: "ui_overflow", "{msg}");
        if self.reports.len() >= MAX_REPORTS {
            self.reports.remove(0);
        }
        self.reports.push(msg.clone());
        if self.policy == Policy::Panic {
            panic!("{msg}");
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Left,
    Right,
}

impl Edge {
    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

struct Offender {
    id: Id,
    rect: Rect,
    bounds: Rect,
    over: f32,
    edge: Edge,
    name: Option<String>,
}

fn offenders(widgets: &WidgetRects, bounds: Rect, allow: &[Rect]) -> Vec<Offender> {
    let mut found: Vec<Offender> = Vec::new();
    for (_, layer) in widgets.layers() {
        for w in layer {
            let rect = w.rect;
            if !rect.is_finite() || !rect.is_positive() {
                continue;
            }
            // Scrolled fully out of view, so nothing of it is cut off on screen.
            if !bounds.intersects(rect) {
                continue;
            }
            if allow.iter().any(|a| a.intersects(rect)) {
                continue;
            }
            let (over, edge) = if rect.right() > bounds.right() + TOLERANCE {
                (rect.right() - bounds.right(), Edge::Right)
            } else if rect.left() < bounds.left() - TOLERANCE {
                (bounds.left() - rect.left(), Edge::Left)
            } else {
                continue;
            };
            let name = name_of(widgets, w.id);
            found.push(Offender { id: w.id, rect, bounds, over, edge, name });
        }
    }
    // Worst first to the nearest point, then the narrowest of an equally clipped stack: that is
    // the widget itself rather than the row and the panel around it.
    found.sort_by(|a, b| {
        b.over.round().total_cmp(&a.over.round()).then(a.rect.width().total_cmp(&b.rect.width()))
    });
    // An ancestor is only as wide as the child that stretched it, so reporting both says the same
    // thing twice - up to and including the root ui, which would stroke the whole screen.
    let rects: Vec<Rect> = found.iter().map(|o| o.rect).collect();
    let mut index = 0;
    found.retain(|_| {
        let rect = rects[index];
        index += 1;
        !rects.iter().any(|other| {
            *other != rect
                && rect.contains_rect(*other)
                && (rect.right() - other.right()).abs() <= TOLERANCE
        })
    });
    found
}

/// `Button "Generate"`, only when the app turned on `debug.show_interactive_widgets`.
fn name_of(widgets: &WidgetRects, id: Id) -> Option<String> {
    let info = widgets.info(id)?;
    Some(match &info.label {
        Some(label) => format!("{:?} {label:?}", info.typ),
        None => format!("{:?}", info.typ),
    })
}

/// The text painted inside `rect` this frame, as a name for the widget there. Prefers the text
/// reaching furthest right, which is the part actually cut off. Text filling most of the widget is
/// taken as its label; anything smaller only says what the widget sits next to.
fn text_inside(shapes: &[egui::epaint::ClippedShape], rect: Rect) -> Option<String> {
    let mut best: Option<(f32, String)> = None;
    let mut visit = |shape: &Shape| {
        let Shape::Text(text) = shape else { return };
        let at = text.visual_bounding_rect();
        if !at.intersects(rect) {
            return;
        }
        let content = text.galley.text().trim();
        if content.is_empty() || content.starts_with(CHIP_PREFIX) {
            return;
        }
        if best.as_ref().is_none_or(|(right, _)| at.right() > *right) {
            let label: String = content.chars().take(40).collect();
            let own = at.width() > rect.width() * 0.5;
            best = Some((at.right(), if own { format!("{label:?}") } else { format!("near {label:?}") }));
        }
    };
    for clipped in shapes {
        match &clipped.shape {
            Shape::Vec(shapes) => shapes.iter().for_each(&mut visit),
            shape => visit(shape),
        }
    }
    best.map(|(_, text)| text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(ctx: &Context, width: f32, build: impl FnMut(&mut Ui)) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(width, 600.0))),
            ..Default::default()
        };
        // epaint panics on an unapplied font-texture delta; nothing paints here.
        ctx.run_ui(input, build).textures_delta.clear();
    }

    #[test]
    fn row_past_the_right_edge_is_reported() {
        let ctx = Context::default();
        install_with(&ctx, Policy::Report);
        pass(&ctx, 300.0, |ui| {
            ui.horizontal(|ui| {
                for i in 0..6 {
                    let _ = ui.button(format!("button {i}"));
                }
            });
        });
        let reports = take_reports(&ctx);
        assert!(!reports.is_empty(), "an over-wide row should be reported");
        assert!(reports[0].contains("UI_OVERFLOW right"), "{}", reports[0]);
        assert!(reports.iter().any(|r| r.contains("button")), "{reports:?}");
    }

    #[test]
    fn a_row_that_fits_is_not_reported() {
        let ctx = Context::default();
        install_with(&ctx, Policy::Report);
        pass(&ctx, 300.0, |ui| {
            ui.horizontal(|ui| {
                let _ = ui.button("ok");
            });
        });
        assert!(take_reports(&ctx).is_empty());
    }

    #[test]
    fn allow_excuses_a_deliberate_horizontal_scroll() {
        let ctx = Context::default();
        install_with(&ctx, Policy::Report);
        pass(&ctx, 300.0, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                allow(ui);
                ui.horizontal(|ui| {
                    for i in 0..6 {
                        let _ = ui.button(format!("button {i}"));
                    }
                });
            });
        });
        assert!(take_reports(&ctx).is_empty());
    }

    #[test]
    fn content_bounds_narrow_the_check() {
        let ctx = Context::default();
        install_with(&ctx, Policy::Report);
        set_content_bounds(&ctx, Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 600.0)));
        pass(&ctx, 300.0, |ui| {
            ui.horizontal(|ui| {
                let _ = ui.button("wider than the inset");
            });
        });
        assert!(!take_reports(&ctx).is_empty());
    }
}
