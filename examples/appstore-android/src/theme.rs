//! AMOLED galactic theme shared with comfyui-android and zc-codex: a true-black page lit by three
//! soft light pools, violet glass surfaces, and two neon accents.
//!
//! Hot pink ([`PINK`]) is the primary — selected, pressed, or needing action (an update). Aqua
//! ([`AQUA`]) is the secondary — hover, links, and "up to date". Violet ([`VIOLET`]) is ambient
//! light only. Surface edges are faint white ([`RIM`]), never coloured.

use egui::containers::scroll_area::ScrollBarVisibility;
use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

pub const PINK: Color32 = Color32::from_rgb(255, 61, 139);
pub const PINK_BRIGHT: Color32 = Color32::from_rgb(255, 110, 168);
pub const AQUA: Color32 = Color32::from_rgb(43, 226, 214);
pub const AQUA_BRIGHT: Color32 = Color32::from_rgb(120, 240, 232);
pub const VIOLET: Color32 = Color32::from_rgb(163, 140, 255);
/// Dim white hairline for a pane's edge.
pub const RIM: Color32 = Color32::from_rgba_premultiplied(46, 46, 52, 46);
/// Brighter hairline for surfaces closest to the eye.
pub const RIM_BRIGHT: Color32 = Color32::from_rgba_premultiplied(72, 72, 80, 72);
pub const INK: Color32 = Color32::from_rgb(233, 233, 239);
pub const MUTED: Color32 = Color32::from_rgb(150, 148, 166);

/// Resting glass fill for a surface on the page.
pub fn surface() -> Color32 {
    rgba(24, 21, 38, 150)
}

/// Glass fill for a pressed surface.
pub fn surface_pressed() -> Color32 {
    rgba(255, 61, 139, 54)
}

/// Glass fill for a hovered surface.
pub fn surface_hovered() -> Color32 {
    rgba(43, 226, 214, 36)
}

/// Three low-alpha pools of light, one per accent.
pub fn ambience(painter: &egui::Painter, rect: egui::Rect, ring_alpha: u8) {
    let d = rect.width().min(rect.height()).max(1.0);
    for (fx, fy, fr, color) in [
        (0.12, 0.14, 0.46, VIOLET),
        (0.94, 0.38, 0.38, AQUA),
        (0.46, 0.97, 0.42, PINK),
    ] {
        light_pool(painter, rect.lerp_inside(egui::vec2(fx, fy)), d * fr, color, ring_alpha);
    }
}

/// Paint [`ambience`] beneath every panel.
pub fn page_ambience(ctx: &egui::Context) {
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("page-ambience"))
        .order(egui::Order::Background)
        .fixed_pos(screen.min)
        .movable(false)
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_clip_rect(screen);
            ambience(ui.painter(), screen, 1);
        });
}

/// Nested discs of a constant low alpha, largest first.
fn light_pool(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32, ring_alpha: u8) {
    const RINGS: usize = 16;
    let fill = rgba(color.r(), color.g(), color.b(), ring_alpha);
    for i in 0..RINGS {
        let t = 1.0 - i as f32 / RINGS as f32;
        painter.circle_filled(center, radius * t, fill);
    }
}

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    v.override_text_color = Some(INK);
    // Not fully opaque so `page_ambience` shows through.
    v.panel_fill = rgba(0, 0, 0, 232);
    v.window_fill = rgba(19, 17, 30, 120);
    v.window_stroke = Stroke::new(1.2, RIM);
    v.faint_bg_color = rgb(11, 10, 16);
    v.extreme_bg_color = rgb(8, 7, 13);
    v.code_bg_color = rgb(6, 5, 10);
    v.hyperlink_color = AQUA;
    v.warn_fg_color = AQUA_BRIGHT;
    v.error_fg_color = PINK;
    v.selection.bg_fill = rgba(255, 61, 139, 140);
    v.selection.stroke = Stroke::new(1.4, PINK_BRIGHT);
    v.window_shadow = egui::epaint::Shadow { offset: [0, 2], blur: 12, spread: 2, color: rgba(0, 0, 0, 200) };
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 2], blur: 10, spread: 1, color: rgba(0, 0, 0, 170) };
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    widget_palette(&mut v.widgets);
    ctx.set_visuals(v);

    let styles = [
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.5, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ];
    ctx.all_styles_mut(|s| {
        for (style, id) in &styles {
            s.text_styles.insert(style.clone(), id.clone());
        }
        s.interaction.selectable_labels = false;
        s.spacing.item_spacing = egui::vec2(6.0, 6.0);
        s.spacing.button_padding = egui::vec2(8.0, 6.0);
        let mut scroll = egui::style::ScrollStyle::solid();
        scroll.bar_width = 10.0;
        scroll.handle_min_length = 28.0;
        scroll.bar_inner_margin = 2.0;
        s.spacing.scroll = scroll;
    });
}

/// Rest = dark glass, hover = aqua edge, press/active = pink.
fn widget_palette(w: &mut egui::style::Widgets) {
    let radius = CornerRadius::same(5);

    w.noninteractive.bg_fill = rgba(18, 16, 28, 132);
    w.noninteractive.weak_bg_fill = rgba(14, 12, 22, 120);
    w.noninteractive.bg_stroke = Stroke::new(1.0, RIM);
    w.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    w.noninteractive.corner_radius = radius;

    w.inactive.bg_fill = rgba(31, 28, 47, 165);
    w.inactive.weak_bg_fill = rgba(25, 23, 38, 150);
    w.inactive.bg_stroke = Stroke::new(1.0, RIM_BRIGHT);
    w.inactive.fg_stroke = Stroke::new(1.0, INK);
    w.inactive.corner_radius = radius;

    w.hovered.bg_fill = rgba(43, 226, 214, 42);
    w.hovered.weak_bg_fill = rgba(43, 226, 214, 42);
    w.hovered.bg_stroke = Stroke::new(1.5, rgba(43, 226, 214, 240));
    w.hovered.fg_stroke = Stroke::new(1.5, rgb(248, 250, 252));
    w.hovered.corner_radius = radius;

    w.active.bg_fill = rgba(255, 61, 139, 54);
    w.active.weak_bg_fill = rgba(255, 61, 139, 54);
    w.active.bg_stroke = Stroke::new(1.7, rgba(255, 61, 139, 245));
    w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    w.active.corner_radius = radius;

    w.open.bg_fill = rgba(31, 28, 47, 165);
    w.open.weak_bg_fill = rgba(25, 23, 38, 150);
    w.open.bg_stroke = Stroke::new(1.3, rgba(43, 226, 214, 205));
    w.open.fg_stroke = Stroke::new(1.0, INK);
    w.open.corner_radius = radius;
}

/// Selectable button with a persistent frame and a pink rim when selected.
pub fn selectable_label<'a>(ui: &mut egui::Ui, selected: bool, text: impl egui::IntoAtoms<'a>) -> egui::Response {
    let resp = ui.add(egui::Button::selectable(selected, text).frame_when_inactive(true));
    if selected {
        ui.painter().rect_stroke(resp.rect, CornerRadius::same(5), Stroke::new(1.6, PINK), egui::StrokeKind::Inside);
    }
    resp
}

/// Pink call-to-action button.
pub fn primary_button<'a>(text: impl egui::IntoAtoms<'a>) -> egui::Button<'a> {
    egui::Button::new(text).fill(rgba(255, 61, 139, 96)).stroke(Stroke::new(1.4, PINK))
}

pub fn scroll_vertical() -> egui::ScrollArea {
    egui::ScrollArea::vertical().scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
}
