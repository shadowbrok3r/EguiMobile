//! Preset-aware store surfaces and controls.
use egui::{Color32, CornerRadius, Stroke};
use egui_mobile_theme::{Role, color};

pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}
pub fn surface(ui: &egui::Ui) -> Color32 {
    ui.visuals().window_fill
}
pub fn surface_pressed(ui: &egui::Ui) -> Color32 {
    ui.visuals().widgets.active.bg_fill
}
pub fn surface_hovered(ui: &egui::Ui) -> Color32 {
    ui.visuals().widgets.hovered.bg_fill
}

pub fn page_ambience(ctx: &egui::Context) {
    if egui_mobile_theme::glass(ctx).blur_radius <= 0.0 {
        return;
    }
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::unique("page-ambience"))
        .order(egui::Order::Background)
        .fixed_pos(screen.min)
        .movable(false)
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_clip_rect(screen);
            let d = screen.width().min(screen.height());
            for (x, y, r, role) in [
                (0.12, 0.14, 0.46, Role::Accent),
                (0.94, 0.38, 0.38, Role::Info),
                (0.46, 0.97, 0.42, Role::Primary),
            ] {
                let c = color(ctx, role);
                for i in 0..16 {
                    ui.painter().circle_filled(
                        screen.lerp_inside(egui::vec2(x, y)),
                        d * r * (1.0 - i as f32 / 16.0),
                        rgba(c.r(), c.g(), c.b(), 1),
                    );
                }
            }
        });
}

pub fn selectable_label<'a>(
    ui: &mut egui::Ui,
    selected: bool,
    text: impl egui::IntoAtoms<'a>,
) -> egui::Response {
    let resp = ui.add(egui::Button::selectable(selected, text).frame_when_inactive(true));
    if selected {
        ui.painter().rect_stroke(
            resp.rect,
            CornerRadius::same(5),
            Stroke::new(1.6, color(ui.ctx(), Role::Accent)),
            egui::StrokeKind::Inside,
        );
    }
    resp
}

pub fn primary_button<'a>(ui: &egui::Ui, text: impl egui::IntoAtoms<'a>) -> egui::Button<'a> {
    let c = color(ui.ctx(), Role::Primary);
    egui::Button::new(text)
        .fill(c.gamma_multiply(0.2))
        .stroke(Stroke::new(1.4, c))
}

pub fn scroll_vertical() -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        .scroll_source(egui::scroll_area::ScrollSource::ALL)
        .scroll_bar_visibility(
            egui::containers::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
        )
}
