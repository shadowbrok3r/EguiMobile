//! Android demo — the same `impl EguiApp` shape as the iOS examples, built as an Android cdylib.
//! Exercises the common Host capabilities plus the Android-only `HostExt` (self-update, install /
//! overlay permission, number keypad, large-file sharing).

use egui_mobile::egui;
use egui_mobile::{CreateContext, EguiApp, Haptic, Host, HostExt, Permission, app, keyboard};

/// Size of the generated file the large-share button hands to the share sheet.
const BIG_SHARE_MIB: usize = 220;

struct Demo {
    count: u32,
    slider: f32,
    text: String,
    width_mm: f64,
    quantity: String,
    note: String,
    enter_down: u32,
    enter_up: u32,
    share_status: String,
}

impl Demo {
    fn new(_cc: &CreateContext) -> Self {
        Demo {
            count: 0,
            slider: 0.5,
            text: "edit me".to_owned(),
            width_mm: 2.5,
            quantity: "12".to_owned(),
            note: String::new(),
            enter_down: 0,
            enter_up: 0,
            share_status: String::new(),
        }
    }

    fn keyboard_section(&mut self, ui: &mut egui::Ui) {
        ui.label("Keyboard:");
        ui.horizontal(|ui| {
            ui.label("Width mm");
            let r = ui.add(egui::DragValue::new(&mut self.width_mm).speed(0.1));
            keyboard::number(&r);
            if r.changed() {
                log::info!("hello: width_mm = {}", self.width_mm);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Quantity");
            let r = ui.add(egui::TextEdit::singleline(&mut self.quantity).desired_width(120.0));
            keyboard::number(&r);
        });
        ui.horizontal(|ui| {
            ui.label("Note");
            ui.add(egui::TextEdit::singleline(&mut self.note).desired_width(160.0));
        });
        let enters: Vec<bool> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key: egui::Key::Enter, pressed, .. } => Some(*pressed),
                    _ => None,
                })
                .collect()
        });
        for pressed in enters {
            if pressed {
                self.enter_down += 1;
            } else {
                self.enter_up += 1;
            }
            log::info!("hello: Enter {}", if pressed { "down" } else { "up" });
        }
        ui.label(format!("Enter: {} down, {} up", self.enter_down, self.enter_up));
    }

    fn share_section(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.label("Share a file:");
        let dir = std::path::PathBuf::from(host.documents_dir().unwrap_or_default());
        ui.horizontal_wrapped(|ui| {
            if ui.button(format!("{BIG_SHARE_MIB} MB")).clicked() {
                let path = dir.join("big.bin");
                match write_big_file(&path) {
                    Ok(()) => host.share_media(
                        path.to_string_lossy().into_owned(),
                        "egui-big-220mb.bin",
                        "application/octet-stream",
                    ),
                    Err(e) => self.share_status = format!("could not write {}: {e}", path.display()),
                }
            }
            if ui.button("Removed file").clicked() {
                let path = dir.join("gone.txt");
                let _ = std::fs::write(&path, "gone before the share runs");
                host.share_media(path.to_string_lossy().into_owned(), "egui-gone.txt", "text/plain");
                let _ = std::fs::remove_file(&path);
            }
            if ui.button("Folder").clicked() {
                host.share_media(
                    dir.to_string_lossy().into_owned(),
                    "egui-folder.bin",
                    "application/octet-stream",
                );
            }
        });
        if let Some(outcome) = host.take_share_outcome() {
            log::info!("hello: share outcome {outcome:?}");
            self.share_status = match outcome {
                Ok(folder) => format!("shared, saved in {folder}"),
                Err(reason) => format!("share failed: {reason}"),
            };
        }
        if !self.share_status.is_empty() {
            ui.label(&self.share_status);
        }
    }
}

/// Write `BIG_SHARE_MIB` MiB where MiB `i` is every byte `i % 251`, unless the file is already that size.
fn write_big_file(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    let len = (BIG_SHARE_MIB << 20) as u64;
    if std::fs::metadata(path).is_ok_and(|m| m.len() == len) {
        return Ok(());
    }
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    for i in 0..BIG_SHARE_MIB {
        file.write_all(&vec![(i % 251) as u8; 1 << 20])?;
    }
    file.flush()
}

impl EguiApp for Demo {
    fn theme(&self, ctx: &egui::Context) {
        ctx.set_visuals(egui::Visuals::dark());
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.heading("egui on Android");
        ui.separator();

        if ui.button(format!("Tapped {} times", self.count)).clicked() {
            self.count += 1;
            host.haptic(Haptic::Light);
        }
        ui.add(egui::Slider::new(&mut self.slider, 0.0..=1.0).text("slider"));
        ui.text_edit_singleline(&mut self.text);

        ui.separator();
        self.keyboard_section(ui);

        ui.separator();
        self.share_section(ui, host);

        ui.separator();
        ui.label("System:");
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open URL").clicked() {
                host.open_url("https://github.com/emilk/egui");
            }
            if ui.button("Copy").clicked() {
                host.copy_text(&self.text);
            }
            if ui.button("Share").clicked() {
                host.share_text(&self.text);
            }
            if ui.button("Notify").clicked() {
                host.notify("Android Hello", "From Rust via JNI.");
            }
        });

        ui.separator();
        ui.label("Permissions:");
        ui.horizontal_wrapped(|ui| {
            if ui.button("Camera").clicked() {
                host.request_permission(Permission::Camera);
            }
            if ui.button("Notifications").clicked() {
                host.request_notification_permission();
            }
        });
        if let Some(granted) = host.permission(Permission::Camera) {
            ui.label(format!("camera: {granted}"));
        }

        ui.separator();
        ui.label(format!("Android-only (versionCode {})", host.current_version_code()));
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("can install: {}", host.can_install_packages()));
            if ui.button("Grant install").clicked() {
                host.request_install_permission();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("can overlay: {}", host.can_draw_overlays()));
            if ui.button("Grant overlay").clicked() {
                host.request_overlay_permission();
            }
        });

        ui.ctx().request_repaint();
    }
}

app!(Demo::new, egui_mobile::Backend::Glow);
