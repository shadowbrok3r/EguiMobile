//! The shared Mastertech theme for plugin guests and desktop hosts.
pub use egui_mobile_theme::desktop_style as mastertech_style;

pub fn apply(ctx: &egui::Context) {
    egui_mobile_theme::apply(ctx, egui_mobile_theme::Density::Compact);
}
