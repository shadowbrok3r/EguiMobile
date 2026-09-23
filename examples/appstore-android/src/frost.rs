//! Backdrop blur behind floating panes (the app detail sheet), via a grab-pass paint callback.
//! Needs the glow backend; without a GL context the translucent fills stand alone.

use backdrop_blur_egui::{BlurRadius, CornerRadius, GrabPassRenderer, Presence, RepaintPolicy, Surface, Tint};
use std::sync::OnceLock;
use std::time::Duration;

static RENDERER: OnceLock<Option<GrabPassRenderer>> = OnceLock::new();

/// Dark, faintly violet film over the blur.
const TINT: [u8; 4] = [11, 9, 19, 92];
const BLUR: f32 = 24.0;
/// Matches `window_corner_radius`.
const CORNER: f32 = 8.0;
/// Panes larger than this fraction of the screen on both axes are scrims, not glass.
const SCRIM: f32 = 0.9;

fn renderer() -> Option<&'static GrabPassRenderer> {
    RENDERER
        .get_or_init(|| {
            let gl = egui_mobile::glow_context()?;
            match GrabPassRenderer::new(&gl) {
                Ok(renderer) => Some(renderer),
                Err(error) => {
                    log::warn!("appstore: backdrop blur unavailable: {error}");
                    None
                }
            }
        })
        .as_ref()
}

/// Frost every Foreground/Tooltip pane that was open last frame. Call once per frame.
pub fn glass_panes(ctx: &egui::Context) {
    let Some(renderer) = renderer() else { return };

    let screen = ctx.content_rect();
    let mut panes: Vec<egui::Rect> = ctx.memory(|m| {
        m.areas()
            .visible_layer_ids()
            .iter()
            .filter(|layer| matches!(layer.order, egui::Order::Foreground | egui::Order::Tooltip))
            .filter_map(|layer| m.area_rect(layer.id))
            .collect()
    });
    panes.retain(|rect| {
        let rect = rect.intersect(screen);
        rect.width() > 1.0
            && rect.height() > 1.0
            && !(rect.width() > screen.width() * SCRIM && rect.height() > screen.height() * SCRIM)
    });
    if panes.is_empty() {
        return;
    }

    let moving = ctx.input(|i| i.pointer.any_down() || i.any_touches() || i.is_scrolling());
    let repaint = if moving {
        RepaintPolicy::Live
    } else {
        RepaintPolicy::Bounded(Duration::from_millis(400))
    };
    let tint = Tint::from_srgb_unmultiplied(TINT);

    egui::Area::new(egui::Id::new("frost-glass"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .movable(false)
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_clip_rect(screen);
            for rect in panes {
                renderer.frost(
                    ui,
                    Surface {
                        rect,
                        blur_radius: BlurRadius::new(BLUR),
                        tint,
                        corner_radius: CornerRadius::new(CORNER),
                        presence: Presence::new(1.0),
                        repaint,
                    },
                );
            }
        });
}
