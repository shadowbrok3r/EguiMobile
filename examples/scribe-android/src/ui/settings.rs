//! Settings: the model, how recordings are transcribed, appearance, and a debug panel that
//! exercises every moving part by hand.

use super::{action, card, detail_row, role, role_action, section_title, weak, wrapped};
use crate::app::{Confirm, ScribeApp};
use crate::engine::{ModelStatus, SelfTest};
use crate::models::{self, human_bytes, Phase, CATALOG};
use crate::settings::MicPath;
use egui::{RichText, Ui};
use egui_icon_pack::phosphor_regular as svg;
use egui_mobile::theme::{Density, Role};
use egui_mobile::{Haptic, Host};

pub fn show(app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    model(app, ui, host);
    transcription(app, ui);
    storage(app, ui);
    appearance(app, ui);
    about(app, ui, host);
    debug(app, ui, host);
    ui.add_space(16.0);
}

fn model_summary(app: &ScribeApp) -> (String, Role) {
    let Some(status) = app.status.as_ref() else { return (String::new(), Role::Muted) };
    match &status.model {
        ModelStatus::Ready(loaded) if loaded.simulated => ("Simulated engine in use. Nothing is transcribed for real.".into(), Role::Warning),
        ModelStatus::Ready(loaded) => (format!("{} is loaded on the NPU.", loaded.name), Role::Success),
        ModelStatus::Loading(name) => (format!("Loading {name}…"), Role::Warning),
        ModelStatus::Failed(reason) => (format!("Could not load the model: {reason}"), Role::Danger),
        ModelStatus::Paused(reason) => (reason.clone(), Role::Warning),
        ModelStatus::Missing => ("No model is installed. Recordings are saved and wait for one.".into(), Role::Warning),
        ModelStatus::Idle if app.packs.is_empty() => ("No model is installed. Recordings are saved and wait for one.".into(), Role::Warning),
        ModelStatus::Idle => ("The model loads when there is something to transcribe.".into(), Role::Muted),
    }
}

fn model(app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    let Some(engine) = app.engine.clone() else { return };
    section_title(ui, "Speech model");
    let (summary, colour) = model_summary(app);
    wrapped(ui, RichText::new(summary).size(13.0).color(role(ui, colour)));
    ui.add_space(2.0);

    let install = app.status.as_ref().and_then(|s| s.install.clone());
    let installing = install.as_ref().filter(|i| i.result.is_none()).map(|i| i.name.clone());
    for spec in &CATALOG {
        let installed = app.packs.iter().any(|name| name == spec.name);
        let chosen = app.settings.model == spec.name;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(spec.label).strong()).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, colour) = match (installed, chosen) {
                        (true, true) => ("In use", Role::Success),
                        (true, false) => ("Installed", Role::Muted),
                        (false, _) => ("Not installed", Role::Muted),
                    };
                    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(role(ui, colour))).truncate());
                });
            });
            weak(ui, format!("{} {} on the phone.", spec.blurb, human_bytes(spec.installed_bytes())));

            if let Some(active) = install.as_ref().filter(|i| i.name == spec.name) {
                match &active.result {
                    None => {
                        let what = match active.phase {
                            Phase::Downloading => "Downloading",
                            Phase::Unpacking => "Unpacking",
                        };
                        weak(ui, format!("{what} {} of {}", human_bytes(active.done), human_bytes(active.total)));
                        super::progress(ui, if active.total > 0 { active.done as f32 / active.total as f32 } else { 0.0 });
                        if action(ui, svg::X, "Cancel").clicked() {
                            engine.cancel_install();
                        }
                    }
                    Some(Err(reason)) => {
                        wrapped(ui, RichText::new(reason).size(13.0).color(role(ui, Role::Danger)));
                        ui.horizontal_wrapped(|ui| {
                            if action(ui, svg::ARROWS_CLOCKWISE, "Resume").clicked() {
                                engine.dismiss_install();
                                engine.install(spec);
                            }
                            if action(ui, svg::X, "Dismiss").clicked() {
                                engine.dismiss_install();
                            }
                        });
                    }
                    Some(Ok(())) => {
                        engine.dismiss_install();
                        app.packs_stale = true;
                        app.say(format!("{} is installed.", spec.label));
                    }
                }
                return;
            }

            ui.horizontal_wrapped(|ui| {
                if installed {
                    if !chosen && action(ui, svg::CHECK, "Use this model").clicked() {
                        host.haptic(Haptic::Selection);
                        engine.update_settings(|s| s.model = spec.name.to_string());
                    }
                    if role_action(ui, Role::Danger, svg::TRASH, "Remove").clicked() {
                        app.confirm = Some(Confirm::RemovePack(spec.name.to_string()));
                    }
                } else {
                    ui.add_enabled_ui(installing.is_none(), |ui| {
                        if action(ui, svg::DOWNLOAD_SIMPLE, &format!("Download {}", human_bytes(spec.zip_bytes))).clicked() {
                            host.haptic(Haptic::Light);
                            engine.update_settings(|s| s.model = spec.name.to_string());
                            engine.install(spec);
                        }
                    });
                }
            });
        });
        ui.add_space(2.0);
    }

    // Packs pushed by hand under names the catalog does not know.
    for name in app.packs.clone().iter().filter(|name| models::spec(name).is_none()) {
        card(ui, |ui| {
            detail_row(ui, name, if app.settings.model == *name { "In use" } else { "Installed" });
            ui.horizontal_wrapped(|ui| {
                if app.settings.model != *name && action(ui, svg::CHECK, "Use this model").clicked() {
                    engine.update_settings(|s| s.model = name.clone());
                }
                if role_action(ui, Role::Danger, svg::TRASH, "Remove").clicked() {
                    app.confirm = Some(Confirm::RemovePack(name.clone()));
                }
            });
        });
    }
    weak(ui, "Models are Qualcomm's builds of OpenAI Whisper for this phone's NPU, downloaded once over Wi-Fi. Speech never leaves the device.");
}

fn language_label(code: &str) -> String {
    if code == "auto" {
        return "Detect automatically".into();
    }
    local_whisper::LANGUAGES.iter().find(|(c, _)| *c == code).map_or_else(|| code.to_string(), |(_, name)| (*name).to_string())
}

fn transcription(app: &mut ScribeApp, ui: &mut Ui) {
    let Some(engine) = app.engine.clone() else { return };
    section_title(ui, "Transcription");
    card(ui, |ui| {
        let mut settings = app.settings.clone();
        ui.horizontal(|ui| {
            ui.label("Language");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("language").selected_text(language_label(&settings.language)).width(170.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.language, "auto".to_string(), language_label("auto"));
                    for (code, name) in local_whisper::LANGUAGES {
                        ui.selectable_value(&mut settings.language, code.to_string(), name);
                    }
                });
            });
        });
        ui.checkbox(&mut settings.auto_transcribe, "Transcribe recordings as they are made");
        ui.add_enabled_ui(settings.auto_transcribe, |ui| {
            ui.checkbox(&mut settings.live_preview, "Show a rough preview while recording");
        });
        match engine.exporter.as_ref() {
            Some(exporter) => {
                ui.checkbox(&mut settings.save_text, "Save each transcript as a text file");
                if settings.save_text {
                    weak(ui, format!("In {} on the phone, named by date and title. The file follows its note's edits, and stays when the note is deleted.", exporter.label()));
                    if let Some(reason) = app.status.as_ref().and_then(|s| s.export_error.as_deref()) {
                        wrapped(ui, RichText::new(format!("The last one could not be saved: {reason}")).size(13.0).color(role(ui, Role::Danger)));
                    }
                }
            }
            None => {
                weak(ui, "Text files are unavailable: the phone's shared storage could not be reached.");
            }
        }
        if engine.speakers_available() {
            ui.checkbox(&mut settings.speakers, "Tell speakers apart");
            if settings.speakers {
                weak(ui, "Once a recording is transcribed its voices are compared on the NPU, and each speaker's turns are labelled Speaker 1, Speaker 2 and so on. Open Speakers on a note to name them, or to set how many there are.");
                if let Some(reason) = app.status.as_ref().and_then(|s| s.voices_paused.as_deref()) {
                    wrapped(ui, RichText::new(reason).size(13.0).color(role(ui, Role::Warning)));
                }
            }
        } else {
            weak(ui, "Telling speakers apart is unavailable: this build carries no speaker model.");
        }
        ui.horizontal(|ui| {
            ui.label("Microphone");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("mic path").selected_text(settings.mic.label()).width(120.0).show_ui(ui, |ui| {
                    for path in MicPath::ALL {
                        ui.selectable_value(&mut settings.mic, path, path.label());
                    }
                });
            });
        });
        if settings != app.settings {
            let files_on = settings.save_text && !app.settings.save_text;
            engine.update_settings(|s| *s = settings.clone());
            app.settings = settings;
            if files_on {
                // Switched on: the notes already there get their files too.
                engine.export_all();
            }
        }
        let waiting = app.notes.iter().filter(|n| matches!(n.state, crate::library::NoteState::Pending | crate::library::NoteState::Failed)).count();
        if waiting > 0 {
            ui.add_space(4.0);
            if action(ui, svg::SPARKLE, &format!("Transcribe {waiting} waiting {}", if waiting == 1 { "note" } else { "notes" })).clicked() {
                engine.transcribe_pending();
            }
        }
    });
}

fn storage(app: &mut ScribeApp, ui: &mut Ui) {
    let Some(engine) = app.engine.clone() else { return };
    section_title(ui, "Storage");
    card(ui, |ui| {
        detail_row(ui, "Notes", format!("{} · {}", app.notes.len(), human_bytes(app.library_bytes)));
        detail_row(ui, "Free space", models::free_bytes(&engine.paths.library).map_or_else(|| "?".into(), human_bytes));
        weak(ui, format!("Recordings are 16 kHz WAV files in {}", engine.paths.library.display()));
        weak(ui, "They stay on the phone until you delete the note, so any note can be transcribed again. Uninstalling Scribe removes them.");
    });
}

fn appearance(app: &mut ScribeApp, ui: &mut Ui) {
    let Some(engine) = app.engine.clone() else { return };
    section_title(ui, "Appearance");
    egui::CollapsingHeader::new("Theme").id_salt("theme").default_open(app.expand_all).show(ui, |ui| {
        let mut theme = app.settings.theme.clone();
        if egui_mobile::theme::editor(ui, &mut theme, Density::Touch) {
            theme.apply(ui.ctx(), Density::Touch);
            engine.update_settings(|s| s.theme = theme.clone());
            app.settings.theme = theme;
        }
    });
}

fn about(_app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    section_title(ui, "About");
    card(ui, |ui| {
        detail_row(ui, "Scribe", format!("v{}", env!("CARGO_PKG_VERSION")));
        if crate::platform::store_available(host) {
            weak(ui, "Updates are managed by App Store.");
            ui.horizontal_wrapped(|ui| {
                if action(ui, svg::ARROWS_CLOCKWISE, "Check for updates").clicked() {
                    crate::platform::check_store_update(host);
                }
                if action(ui, svg::DOWNLOAD_SIMPLE, "Open App Store").clicked() {
                    crate::platform::open_app_store(host);
                }
            });
        }
    });
}

/// Everything needed to see why transcription is or is not working, from the phone itself.
fn debug(app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    let Some(engine) = app.engine.clone() else { return };
    let Some(status) = app.status.clone() else { return };
    section_title(ui, "Debug");
    egui::CollapsingHeader::new("NPU and engine").id_salt("debug").default_open(app.expand_all).show(ui, |ui| {
        card(ui, |ui| {
            let model = match &status.model {
                ModelStatus::Idle => "not loaded".to_string(),
                ModelStatus::Missing => "no pack installed".to_string(),
                ModelStatus::Loading(name) => format!("loading {name}"),
                ModelStatus::Ready(loaded) => format!("{} ({} ms)", loaded.name, loaded.load_ms),
                ModelStatus::Failed(_) => "failed".to_string(),
                ModelStatus::Paused(_) => "paused by the crash guard".to_string(),
            };
            detail_row(ui, "Model", model);
            if let ModelStatus::Ready(loaded) = &status.model {
                weak(ui, format!("{}. {}", loaded.summary, loaded.provenance));
            }
            if let ModelStatus::Failed(reason) | ModelStatus::Paused(reason) = &status.model {
                wrapped(ui, RichText::new(reason).size(13.0).color(role(ui, Role::Danger)));
            }
            detail_row(ui, "Qualcomm NPU", if crate::platform::has_npu() { "present" } else { "not found" });
            detail_row(ui, "Speaker model", if engine.paths.speaker_pack.is_some() { "bundled" } else if app.settings.simulate { "simulated" } else { "not in this build" });
            detail_row(ui, "QNN runtime", crate::npu::describe().unwrap_or_else(|| "not loaded".into()));
            detail_row(ui, "Microphone", if crate::platform::mic_permission() { "permitted" } else { "not permitted" });
            let stats = status.stats;
            if stats.windows > 0 {
                detail_row(ui, "Speed", format!("{:.0}x real time over {} window{}", stats.speed(), stats.windows, if stats.windows == 1 { "" } else { "s" }));
                detail_row(ui, "Last window", format!("encoder {:.0} ms, {:.1} ms/token, {} tokens", stats.last_encode_ms, stats.last_ms_per_token, stats.last_tokens));
            }
            if !status.queue.is_empty() || status.active.is_some() {
                detail_row(ui, "Queue", format!("{} waiting{}", status.queue.len(), if status.active.is_some() { ", 1 running" } else { "" }));
            }
            ui.horizontal_wrapped(|ui| {
                if action(ui, svg::CPU, "Load model").clicked() {
                    engine.request_load();
                }
                if action(ui, svg::EJECT, "Unload").clicked() {
                    engine.request_unload();
                }
                let running = matches!(status.selftest, SelfTest::Requested | SelfTest::Running);
                ui.add_enabled_ui(!running, |ui| {
                    if action(ui, svg::WAVEFORM, if running { "Testing…" } else { "Self-test" }).clicked() {
                        host.haptic(Haptic::Light);
                        engine.request_selftest();
                    }
                });
                if action(ui, svg::GEAR, "App settings").clicked() {
                    crate::platform::open_app_settings(host);
                }
            });
            weak(ui, "The self-test transcribes a bundled 11 s clip of a 1961 speech and reports the timings, then checks the speaker model's answer for it against a reference.");
            match &status.selftest {
                SelfTest::Finished { text, detail, matched } => {
                    let colour = role(ui, if *matched { Role::Success } else { Role::Warning });
                    wrapped(ui, RichText::new(if *matched { "Self-test passed" } else { "Self-test ran, but the text is not the clip's" }).color(colour));
                    wrapped(ui, RichText::new(text).size(13.5));
                    weak(ui, detail.clone());
                }
                SelfTest::Failed(reason) => {
                    wrapped(ui, RichText::new(format!("Self-test failed: {reason}")).color(role(ui, Role::Danger)));
                }
                _ => {}
            }

            let mut simulate = app.settings.simulate;
            if ui.checkbox(&mut simulate, "Simulated engine (no NPU)").changed() {
                engine.update_settings(|s| s.simulate = simulate);
                app.settings.simulate = simulate;
            }
            weak(ui, "Answers every recording with placeholder sentences, to exercise the app on a device without an NPU.");
        });

        card(ui, |ui| {
            weak(ui, format!("Models: {}", engine.paths.models.display()));
            weak(ui, format!("Native libraries: {}", engine.paths.native_libs.as_ref().map_or_else(|| "unknown".into(), |p| p.display().to_string())));
        });

        if !app.overflow.is_empty() {
            card(ui, |ui| {
                wrapped(ui, RichText::new("Layout overflow reports").color(role(ui, Role::Warning)));
                for line in app.overflow.iter().rev().take(8) {
                    weak(ui, line.clone());
                }
            });
        }

        let lines = engine.log.lines();
        card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Log").strong());
                if action(ui, svg::COPY, "Copy log").clicked() {
                    host.copy_text(lines.join("\n"));
                    app.say("Log copied.");
                }
            });
            for line in lines.iter().rev().take(60) {
                let colour = if line.contains(" ! ") { role(ui, Role::Warning) } else { ui.visuals().weak_text_color() };
                wrapped(ui, RichText::new(line).size(11.0).monospace().color(colour));
            }
        });
    });
}
