//! Shared SVG controls, tinted by the current theme and labelled for accessibility.
use egui::{Response, Ui};
use egui_icon_pack::phosphor_regular as svg;

#[derive(Clone, Copy)]
pub enum Icon {
    Filter,
    Play,
    Pause,
    Trash,
    Download,
    Refresh,
}

pub fn button(ui: &mut Ui, icon: Icon, label: &str, selected: bool, size: f32) -> Response {
    let icon = match icon {
        Icon::Filter => svg::FUNNEL,
        Icon::Play => svg::PLAY,
        Icon::Pause => svg::PAUSE,
        Icon::Trash => svg::TRASH,
        Icon::Download => svg::DOWNLOAD_SIMPLE,
        Icon::Refresh => svg::ARROWS_CLOCKWISE,
    };
    let response = ui.add_sized(
        [size, size],
        egui::Button::selectable(selected, icon.image(18.0)).image_tint_follows_text_color(true),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
    response.on_hover_text(label)
}
