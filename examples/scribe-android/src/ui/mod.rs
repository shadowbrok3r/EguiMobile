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

/// [`action`] that can be greyed out, without the child layout `add_enabled_ui` would open —
/// inside a wrapping row that child wraps on its own, away from its neighbours.
pub fn action_if(ui: &mut Ui, enabled: bool, icon: Icon, text: &str) -> Response {
    let button = egui_icon_pack::widgets::button_widget(ui, icon, text).min_size(egui::vec2(0.0, 40.0));
    ui.add_enabled(enabled, button)
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
