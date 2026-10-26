//! Diagnostics stay in Settings, with a direct link from desktop pairing.
use crate::{app::PrivaxyApp, ui};
use egui::{RichText, Ui};

#[derive(Default)]
pub struct LogView {
    pub open: bool,
    /// Scroll the card into view on the next frame.
    pub reveal: bool,
    query: String,
    paused: bool,
    frozen: Vec<privaxy_diagnostics::Entry>,
}
pub fn show(app: &mut PrivaxyApp, ui: &mut Ui) {
    let export_dir = app.loaded.as_ref().map(|v| v.paths.root.join("exports"));
    let mut export = false;
    if std::mem::take(&mut app.logs.reveal) {
        ui.scroll_to_cursor(Some(egui::Align::TOP));
    }
    ui::card(ui, |ui| {
        ui::section_title(ui, "Diagnostic logs");
        ui.horizontal_wrapped(|ui| {
            let mut level = privaxy_diagnostics::level();
            egui::ComboBox::from_id_salt("diagnostic_level")
                .selected_text(format!("Level: {level}"))
                .show_ui(ui, |ui| {
                    for choice in privaxy_diagnostics::LEVELS {
                        ui.selectable_value(&mut level, choice, choice.to_string());
                    }
                });
            if level != privaxy_diagnostics::level() {
                privaxy_diagnostics::set_level(level);
            }
            ui.toggle_value(&mut app.logs.open, "View logs");
            export = ui.button("Export logs").clicked();
        });
        ui.weak("Saved across restarts, up to 10 MiB. Trace adds connection stages and heartbeat timing.");
        if let Some(error) = privaxy_diagnostics::file_error() {
            ui.colored_label(ui::BAD, error);
        }
        if !app.logs.open {
            return;
        }
        ui.add(
            egui::TextEdit::singleline(&mut app.logs.query)
                .hint_text("Search message or source")
                .desired_width(f32::INFINITY),
        );
        ui.horizontal_wrapped(|ui| {
            if ui.checkbox(&mut app.logs.paused, "Pause view").changed() && app.logs.paused {
                app.logs.frozen = privaxy_diagnostics::entries();
            }
            if ui.button("Clear view").clicked() {
                privaxy_diagnostics::clear_view();
                app.logs.frozen.clear();
            }
        });
        let entries = if app.logs.paused {
            app.logs.frozen.clone()
        } else {
            privaxy_diagnostics::entries()
        };
        let query = app.logs.query.to_lowercase();
        let entries: Vec<_> = entries
            .iter()
            .filter(|e| {
                e.level <= privaxy_diagnostics::level()
                    && (query.is_empty()
                        || e.message.to_lowercase().contains(&query)
                        || e.target.to_lowercase().contains(&query))
            })
            .collect();
        ui.weak(format!(
            "{} visible · {} disk queue drops",
            entries.len(),
            privaxy_diagnostics::dropped_lines()
        ));
        egui::ScrollArea::vertical()
            .id_salt("diagnostic_entries")
            .max_height(400.0)
            // Fixed height: deep in the page egui would shrink it to 64pt.
            .min_scrolled_height(400.0)
            .auto_shrink([false, false])
            .stick_to_bottom(!app.logs.paused)
            .show(ui, |ui| {
                for entry in entries.iter().rev().take(250).rev() {
                    let color = match entry.level {
                        log::Level::Error => ui::BAD,
                        log::Level::Warn => ui::WARN,
                        _ => ui.visuals().text_color(),
                    };
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            entry
                                .time
                                .with_timezone(&chrono::Local)
                                .format("%H:%M:%S%.3f"),
                            entry.level
                        ))
                        .small()
                        .color(color),
                    );
                    ui.label(RichText::new(&entry.message).monospace());
                    ui.weak(&entry.target);
                    ui.separator();
                }
            });
        if entries.len() > 250 {
            ui.weak("Showing the latest 250 matches. Export includes both log files.");
        }
    });
    if export && let Some(directory) = export_dir {
        app.save_file(move || {
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let path = directory.join(format!(
                "privaxy-android-{}.txt",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            ));
            std::fs::write(
                &path,
                privaxy_diagnostics::export_text().map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            Ok(path)
        });
    }
}
