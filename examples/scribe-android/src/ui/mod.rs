//! Screens and the shared widgets they are built from.

pub mod notes;
pub mod record;
pub mod settings;

use egui::{Color32, Frame, Response, RichText, Ui};
use egui_icon_pack::phosphor_regular as svg;
use egui_icon_pack::Icon;
use egui_mobile::theme::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Notes,
    Record,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Notes, Tab::Record, Tab::Settings];

    pub fn icon(self) -> Icon {
        match self {
            Tab::Notes => svg::NOTEPAD,
            Tab::Record => svg::MICROPHONE,
            Tab::Settings => svg::SLIDERS,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tab::Notes => "Notes",
            Tab::Record => "Record",
            Tab::Settings => "Settings",
        }
    }
}

/// Comfortable thumb target; egui's default button height is built for a mouse.
pub const TOUCH_HEIGHT: f32 = 46.0;

pub fn role(ui: &Ui, role: Role) -> Color32 {
    egui_mobile::theme::color(ui.ctx(), role)
}

/// One Android back press this frame, taken so no other layer reacts to the same press.
pub fn back_pressed(ctx: &egui::Context) -> bool {
    ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack) || input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

/// The app's one grouping primitive.
pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::group(ui.style())
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner
}

pub fn section_title(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(ui.visuals().weak_text_color()).strong());
}

/// Body text that wraps instead of widening the page.
pub fn wrapped(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> Response {
    ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Wrap))
}

pub fn weak(ui: &mut Ui, text: impl Into<String>) -> Response {
    let color = ui.visuals().weak_text_color();
    wrapped(ui, RichText::new(text).size(13.0).color(color))
}

/// A full-width button tall enough to hit without aiming.
pub fn wide_button(ui: &mut Ui, icon: Icon, text: &str) -> Response {
    let button = egui_icon_pack::widgets::button_widget(ui, icon, RichText::new(text).size(15.0));
    ui.add_sized([ui.available_width(), TOUCH_HEIGHT], button)
}

/// An icon-and-text button sized for a row of several.
pub fn action(ui: &mut Ui, icon: Icon, text: &str) -> Response {
    let button = egui_icon_pack::widgets::button_widget(ui, icon, text).min_size(egui::vec2(0.0, 40.0));
    ui.add(button)
}

/// [`action`] in a semantic colour.
pub fn role_action(ui: &mut Ui, role: Role, icon: Icon, text: &str) -> Response {
    egui_mobile::theme::styled(ui, role, |ui| action(ui, icon, text))
}

/// A square icon button with an accessible name.
pub fn icon_button(ui: &mut Ui, icon: Icon, label: &str, size: f32) -> Response {
    let response = ui.add_sized([size, size], egui::Button::new(icon.image(size * 0.46)).image_tint_follows_text_color(true));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
    response
}

/// Label and value on one line, value pushed to the right edge. Both sides truncate: text in
/// a `horizontal` does not wrap, and a long value would otherwise widen the page.
pub fn detail_row(ui: &mut Ui, label: &str, value: impl Into<egui::WidgetText>) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(label).color(ui.visuals().weak_text_color())).truncate());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(value).truncate());
        });
    });
}

/// Lay a row of known width out in the middle of the available width.
pub fn centered_row<R>(ui: &mut Ui, width: f32, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
        add_contents(ui)
    })
    .inner
}

/// A horizontal bar showing `fraction`, without the text a `ProgressBar` centres over it.
pub fn progress(ui: &mut Ui, fraction: f32) {
    ui.add(egui::ProgressBar::new(fraction.clamp(0.0, 1.0)).desired_height(6.0));
}

/// A stretch of a page under a heading that folds it away. The heading is one target across
/// the page: a thumb does not have to find the arrow. Which stretches are open is kept by
/// `name`, not per note, so a page reads the same way from one note to the next.
pub fn section<R>(ui: &mut Ui, name: &str, title: &str, open: bool, body: impl FnOnce(&mut Ui) -> R) -> Option<egui::InnerResponse<R>> {
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), egui::Id::unique(("section", name)), open);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::click());
    if response.clicked() {
        state.toggle(ui);
    }
    if ui.is_rect_visible(rect) {
        if response.is_pointer_button_down_on() {
            ui.painter().rect_filled(rect, ui.visuals().widgets.active.corner_radius, ui.visuals().widgets.active.weak_bg_fill);
        }
        let arrow = egui::Rect::from_min_size(rect.min, egui::vec2(16.0, rect.height()));
        let icon = if state.is_open() { svg::CARET_DOWN } else { svg::CARET_RIGHT };
        icon.paint(ui, arrow, 14.0, ui.visuals().weak_text_color());
        ui.painter().text(egui::pos2(arrow.right() + 6.0, rect.center().y), egui::Align2::LEFT_CENTER, title, egui::FontId::proportional(15.0), ui.visuals().strong_text_color());
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::DisclosureTriangle, true, title));
    state.show_body_unindented(ui, body)
}

/// One line of a menu: an icon and what it does, as wide as the menu and as tall as a thumb.
pub fn menu_row(ui: &mut Ui, enabled: bool, icon: Icon, text: &str) -> Response {
    let button = egui_icon_pack::widgets::button_widget(ui, icon, RichText::new(text).size(15.0)).min_size(egui::vec2(0.0, TOUCH_HEIGHT));
    ui.add_enabled(enabled, button)
}

/// The colour that stands for speaker number `voice` wherever that speaker shows. These are
/// the theme kit's series colours, in the order checked for colour-blind readers.
pub fn voice(voice: usize) -> Color32 {
    let palette = egui_mobile::theme::colors::SERIES_PALETTE;
    palette[voice % palette.len()]
}

/// `colour`, moved toward white or black until text in it reads against `ground`. The series
/// colours are mid-tones picked to sit on either theme, which is not enough for small text.
pub fn legible(colour: Color32, ground: Color32) -> Color32 {
    let light = |colour: egui::Rgba| 0.2126 * colour.r() + 0.7152 * colour.g() + 0.0722 * colour.b();
    let under = light(egui::Rgba::from(ground.to_opaque()));
    let toward = if under < 0.18 { egui::Rgba::WHITE } else { egui::Rgba::BLACK };
    let from = egui::Rgba::from(colour);
    (0..=10)
        .map(|step| from * (1.0 - step as f32 / 10.0) + toward * (step as f32 / 10.0))
        .find(|mixed| {
            let (a, b) = (light(*mixed) + 0.05, under + 0.05);
            a.max(b) / a.min(b) >= 4.5
        })
        .map_or(Color32::from(toward), Color32::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contrast(a: Color32, b: Color32) -> f32 {
        let light = |colour: Color32| {
            let colour = egui::Rgba::from(colour);
            0.2126 * colour.r() + 0.7152 * colour.g() + 0.0722 * colour.b() + 0.05
        };
        light(a).max(light(b)) / light(a).min(light(b))
    }

    #[test]
    fn a_speakers_colour_reads_as_text_on_dark_and_on_light() {
        for ground in [Color32::from_rgb(10, 10, 14), Color32::from_rgb(38, 40, 48), Color32::from_rgb(236, 238, 242), Color32::WHITE] {
            for speaker in 0..6 {
                let ink = legible(voice(speaker), ground);
                assert!(contrast(ink, ground) >= 4.4, "speaker {speaker} on {ground:?}: {ink:?}");
                // Moved toward the far end, not all the way: it is still that speaker's colour.
                assert!(ink != Color32::WHITE && ink != Color32::BLACK, "speaker {speaker} on {ground:?}");
            }
        }
        // Past the six it starts again rather than running out.
        assert_eq!(voice(6), voice(0));
    }
}
