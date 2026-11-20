//! The recorder: one big button, a clock, a level meter, and the transcript as it forms.

use super::{centered_row, role, weak, wrapped};
use crate::app::ScribeApp;
use crate::clock;
use crate::engine::{LiveStatus, ModelStatus, RATE};
use egui::{Color32, RichText, Sense, Ui};
use egui_icon_pack::phosphor_regular as svg;
use egui_mobile::theme::Role;
use egui_mobile::{Haptic, Host};

const BUTTON: f32 = 136.0;

/// The round record / stop control. `level` swells the halo behind it while recording.
fn big_button(ui: &mut Ui, recording: bool, level: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(BUTTON, BUTTON), Sense::click());
    let colour = if recording { role(ui, Role::Danger) } else { ui.visuals().selection.bg_fill };
    let centre = rect.center();
    let pressed = if response.is_pointer_button_down_on() { 2.0 } else { 0.0 };
    let painter = ui.painter();
    painter.circle_filled(centre, 50.0 + level.sqrt() * 16.0, colour.gamma_multiply(0.22));
    painter.circle_filled(centre, 46.0 - pressed, colour);
    let icon = if recording { svg::STOP } else { svg::MICROPHONE };
    icon.paint(ui, rect, 38.0, Color32::WHITE);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, if recording { "Stop recording" } else { "Start recording" }));
    response
}

/// Recent levels as bars, newest on the right.
fn meter(ui: &mut Ui, levels: &std::collections::VecDeque<f32>, paused: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(320.0), 44.0), Sense::hover());
    let colour = if paused { ui.visuals().weak_text_color() } else { role(ui, Role::Accent) };
    let slots = 48;
    let pitch = rect.width() / slots as f32;
    for slot in 0..slots {
        let level = levels.len().checked_sub(slot + 1).map(|index| levels[index]);
        // Square root: speech sits low in the linear range and would barely move a bar.
        let height = level.map_or(2.0, |level| (level.sqrt() * rect.height()).clamp(2.0, rect.height()));
        let x = rect.right() - (slot as f32 + 0.5) * pitch;
        let bar = egui::Rect::from_center_size(egui::pos2(x, rect.center().y), egui::vec2(pitch * 0.55, height));
        // Slots with no audio behind them yet stay drawn, dimmed, so the meter keeps its width.
        ui.painter().rect_filled(bar, 2.0, if level.is_some() { colour } else { colour.gamma_multiply(0.25) });
    }
}

fn model_line(app: &ScribeApp) -> (String, Role) {
    let Some(status) = app.status.as_ref() else { return (String::new(), Role::Muted) };
    let settings = &app.settings;
    if !settings.auto_transcribe {
        return ("Audio only. Transcribe from the note when you want the text.".into(), Role::Muted);
    }
    match &status.model {
        ModelStatus::Ready(loaded) if loaded.simulated => ("Simulated engine: the text will be placeholder sentences.".into(), Role::Warning),
        ModelStatus::Ready(loaded) => (format!("Transcribing on the NPU with {}.", loaded.name), Role::Success),
        ModelStatus::Loading(name) => (format!("Loading {name} onto the NPU…"), Role::Warning),
        ModelStatus::Failed(reason) => (format!("The audio is being saved. Transcription is unavailable: {reason}"), Role::Danger),
        ModelStatus::Paused(_) => ("The audio is being saved. Transcription is paused; start it from a note or from Settings.".into(), Role::Warning),
        ModelStatus::Missing => ("No model installed. The audio is saved and can be transcribed later.".into(), Role::Warning),
        ModelStatus::Idle if app.packs.is_empty() && !settings.simulate => ("No model installed. The audio is saved and can be transcribed later.".into(), Role::Warning),
        ModelStatus::Idle => ("Transcribes on the NPU as you speak.".into(), Role::Muted),
    }
}

pub fn show(app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    let live = app.status.as_ref().and_then(|s| s.recording.clone());
    ui.vertical_centered(|ui| {
        ui.add_space(10.0);
        match &live {
            Some(live) => recording(app, ui, host, live),
            None => idle(app, ui, host),
        }
    });
    if live.is_some() {
        transcript(app, ui);
    }
}

fn idle(app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    ui.add_space(24.0);
    let waiting = app.awaiting_mic.is_some();
    if big_button(ui, false, 0.0).clicked() && !waiting {
        host.haptic(Haptic::Medium);
        app.begin_recording(host);
    }
    ui.add_space(6.0);
    ui.label(RichText::new(if waiting { "Waiting for microphone access…" } else { "Tap to record" }).size(17.0));
    ui.add_space(10.0);
    let (text, colour) = model_line(app);
    wrapped(ui, RichText::new(text).size(13.0).color(role(ui, colour)));
    let no_model = app.packs.is_empty() && !app.settings.simulate;
    if no_model && app.settings.auto_transcribe {
        ui.add_space(8.0);
        if super::action(ui, svg::DOWNLOAD_SIMPLE, "Get a model").clicked() {
            app.tab = super::Tab::Settings;
        }
    }
    if app.mic_denied {
        ui.add_space(10.0);
        weak(ui, "Scribe cannot record without the microphone permission.");
        if super::action(ui, svg::GEAR, "Open app settings").clicked() {
            crate::platform::open_app_settings(host);
        }
    }
}

fn recording(app: &mut ScribeApp, ui: &mut Ui, host: &Host, live: &LiveStatus) {
    let seconds = live.samples as f64 / RATE as f64;
    ui.label(RichText::new(clock::duration(seconds)).size(46.0).strong());
    let state = if live.stopping {
        "Saving…"
    } else if live.paused {
        "Paused"
    } else {
        "Recording"
    };
    ui.label(RichText::new(state).size(14.0).color(role(ui, if live.paused { Role::Warning } else { Role::Danger })));
    ui.add_space(4.0);
    meter(ui, &app.levels, live.paused);
    ui.add_space(2.0);

    let side = 56.0;
    centered_row(ui, side + 18.0 + BUTTON + 18.0 + side, |ui| {
        ui.spacing_mut().item_spacing.x = 18.0;
        ui.add_enabled_ui(!live.stopping, |ui| {
            let (icon, label) = if live.paused { (svg::PLAY, "Resume recording") } else { (svg::PAUSE, "Pause recording") };
            // Centred against the taller button beside it.
            ui.vertical(|ui| {
                ui.add_space((BUTTON - side) / 2.0);
                if super::icon_button(ui, icon, label, side).clicked() {
                    host.haptic(Haptic::Light);
                    if let Some(engine) = app.engine.as_ref() {
                        engine.set_paused(!live.paused);
                    }
                }
            });
            if big_button(ui, true, if live.paused { 0.0 } else { live.level }).clicked() {
                host.haptic(Haptic::Medium);
                if let Some(engine) = app.engine.as_ref() {
                    engine.stop_recording();
                }
            }
            // Keeps the stop button centred under the clock.
            ui.allocate_exact_size(egui::vec2(side, side), Sense::hover());
        });
    });
    let (text, colour) = model_line(app);
    wrapped(ui, RichText::new(text).size(12.5).color(role(ui, colour)));
    ui.add_space(4.0);
}

/// The live transcript: final text, then the rough preview of what is still being said.
fn transcript(app: &mut ScribeApp, ui: &mut Ui) {
    let (segments, preview) = (&app.live_segments, &app.live_preview);
    egui::ScrollArea::vertical().id_salt("live transcript").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        if segments.is_empty() && preview.is_empty() {
            if app.settings.auto_transcribe {
                weak(ui, "The transcript appears here in half-minute pieces, with a rough preview in between.");
            }
            return;
        }
        let text = segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            wrapped(ui, RichText::new(text).size(15.5));
        }
        if !preview.is_empty() {
            let text = preview.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ");
            let colour = ui.visuals().weak_text_color();
            wrapped(ui, RichText::new(text).size(15.5).italics().color(colour));
        }
    });
}
