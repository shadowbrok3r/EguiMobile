//! The list of notes, and one note opened: its title, audio, text and timeline.

use super::{action, action_if, card, role, role_action, weak, wrapped};
use crate::app::{Confirm, ScribeApp};
use crate::clock;
use crate::library::{Note, NoteState};
use egui::{RichText, Ui};
use egui_icon_pack::phosphor_regular as svg;
use egui_mobile::theme::Role;
use egui_mobile::{Haptic, Host};
use std::time::{Duration, Instant};

/// How long an edit sits before it is written, so typing does not rewrite the file per key.
const SAVE_AFTER: Duration = Duration::from_millis(700);

/// A note opened for reading and editing.
pub struct Detail {
    pub id: String,
    pub title: String,
    pub body: String,
    /// The note's text as of the last time the draft and the file agreed.
    synced: String,
    /// How many of the note's segments the draft has seen. An edit stands in for those; what
    /// is transcribed after them still follows it.
    covers: usize,
    body_edited: Option<Instant>,
    title_edited: Option<Instant>,
    /// An edit has been saved to the note that its text file has not seen yet.
    unsynced: bool,
    /// The speakers' names as typed, one per speaker; empty while there is one voice or none.
    pub names: Vec<String>,
    names_edited: Option<Instant>,
    /// The text field has the keyboard, as of the last frame.
    body_focused: bool,
    /// Where the seek bar is being dragged to, until the finger lifts.
    scrub: Option<f64>,
}

impl Detail {
    pub fn open(note: &Note) -> Self {
        let text = note.text();
        Self { id: note.id.clone(), title: note.title.clone(), body: text.clone(), synced: text, covers: note.segments.len(), body_edited: None, title_edited: None, unsynced: false, names: names_of(note), names_edited: None, body_focused: false, scrub: None }
    }

    /// Treat the draft as just typed.
    #[cfg(test)]
    pub fn mark_edited(&mut self) {
        self.body_edited = Some(Instant::now());
        self.title_edited = Some(Instant::now());
        self.names_edited = Some(Instant::now());
    }

    /// Write any edit still waiting out its delay.
    pub fn flush(&mut self, app_engine: &crate::engine::Engine) {
        self.body_edited = self.body_edited.map(|_| Instant::now() - SAVE_AFTER);
        self.title_edited = self.title_edited.map(|_| Instant::now() - SAVE_AFTER);
        self.names_edited = self.names_edited.map(|_| Instant::now() - SAVE_AFTER);
        self.save_due(app_engine);
        // The text file is brought up to date once per visit, not at every pause in typing:
        // a title being typed would rename it letter by letter.
        if std::mem::take(&mut self.unsynced) {
            app_engine.export_note(&self.id);
        }
    }

    fn save_due(&mut self, engine: &crate::engine::Engine) {
        if self.title_edited.is_some_and(|at| at.elapsed() >= SAVE_AFTER) {
            self.title_edited = None;
            self.unsynced = true;
            let title = self.title.trim().to_string();
            engine.edit_note(&self.id, |note| note.title = title);
        }
        let names_due = self.names_edited.is_some_and(|at| at.elapsed() >= SAVE_AFTER);
        // A name changes the labels in the text, so text still waiting to be saved goes
        // first: saved after, it would put the old labels back.
        if self.body_edited.is_some_and(|at| names_due || at.elapsed() >= SAVE_AFTER) {
            self.body_edited = None;
            self.unsynced = true;
            let (body, covers) = (self.body.clone(), self.covers);
            let saved = engine.edit_note(&self.id, |note| {
                note.body = Some(body);
                note.body_covers = covers.min(note.segments.len());
                // Text edited back to the transcript is not an edit any more.
                if note.text() == note.transcript() {
                    (note.body, note.body_covers) = (None, 0);
                }
            });
            // Segments that landed while it was being typed now follow it in the draft too.
            if let Some(note) = saved {
                self.take(&note);
            }
        }
        if names_due {
            self.names_edited = None;
            self.unsynced = true;
            if let Some(note) = engine.name_speakers(&self.id, &self.names) {
                self.take(&note);
            }
        }
    }

    /// Make the draft the note's text as it now stands.
    fn take(&mut self, note: &Note) {
        let text = note.text();
        if self.body != text {
            self.body = text.clone();
            // Only the field being typed in has a keyboard holding a copy of it. Told while
            // another field has it — a speaker's name, whose saving is what changed this
            // text — the keyboard would drop the word it is in the middle of.
            if self.body_focused {
                egui_mobile_note_out_of_band();
            }
        }
        self.synced = text;
        self.covers = note.segments.len();
    }

    /// Take in transcript that arrived since the draft was loaded, unless the user has typed.
    pub fn follow(&mut self, note: &Note) {
        if note.text() != self.synced && self.body_edited.is_none() && self.body == self.synced {
            self.take(note);
        }
        // And speakers that were found, or recounted, since.
        if self.names_edited.is_none() && self.names.len() != names_of(note).len() {
            self.names = names_of(note);
        }
    }
}

/// The names to offer for editing: one per speaker once there are two or more.
fn names_of(note: &Note) -> Vec<String> {
    let Some(speakers) = note.speakers.as_ref().filter(|speakers| speakers.count > 1) else { return Vec::new() };
    (0..speakers.count as usize).map(|speaker| speakers.names.get(speaker).cloned().unwrap_or_default()).collect()
}

fn people(count: u8) -> String {
    if count == 1 { "one voice".into() } else { format!("{count} voices") }
}

/// Who speaks in the note: how many, what they are called, and the pass that finds out.
fn speakers(app: &mut ScribeApp, ui: &mut Ui, host: &Host, engine: &crate::engine::Engine, note: &Note) {
    // Nothing to offer where the pass cannot run and never has.
    let available = app.settings.simulate || engine.paths.speaker_pack.is_some();
    if note.state != NoteState::Done || note.segments.is_empty() || (!available && note.speakers.is_none()) {
        return;
    }
    let running = app.status.as_ref().and_then(|s| s.voicing.as_ref()).filter(|(id, ..)| *id == note.id).map(|(_, done, total)| (*done, *total));
    let waiting = app.status.as_ref().is_some_and(|s| s.voices.iter().any(|id| *id == note.id));
    let found = note.speakers.as_ref();
    let title = match found {
        Some(speakers) if speakers.count > 1 => format!("Speakers ({})", speakers.count),
        _ => "Speakers".to_string(),
    };
    let mut choice = None;
    egui::CollapsingHeader::new(title).id_salt(("speakers", &note.id)).default_open(app.expand_all).show(ui, |ui| {
        match (running, waiting, found) {
            (Some((done, total)), ..) => {
                weak(ui, "Telling the speakers apart…");
                super::progress(ui, done as f32 / total.max(1) as f32);
            }
            (None, true, _) => {
                weak(ui, "Waiting for the NPU to tell the speakers apart.");
            }
            (None, false, Some(speakers)) if speakers.fixed => {
                weak(ui, format!("Set to {}.", people(speakers.count)));
            }
            (None, false, Some(speakers)) => {
                weak(ui, format!("Found {}.", people(speakers.count)));
            }
            (None, false, None) => {
                weak(ui, "Not told apart yet.");
            }
        }
        let idle = running.is_none() && !waiting;
        ui.horizontal_wrapped(|ui| {
            let size = egui::vec2(40.0, 40.0);
            if ui.add_enabled(idle, egui::Button::selectable(found.is_some_and(|s| !s.fixed), "Find").min_size(size)).clicked() {
                choice = Some(None);
            }
            for count in 1..=6u8 {
                let chosen = found.is_some_and(|s| s.fixed && s.count == count);
                if ui.add_enabled(idle, egui::Button::selectable(chosen, count.to_string()).min_size(size)).clicked() {
                    choice = Some(Some(count));
                }
            }
        });
        weak(ui, "Find works out how many people speak. If it gets that wrong, tap the number there are.");

        if let Some(detail) = app.detail.as_mut().filter(|detail| !detail.names.is_empty()) {
            ui.add_space(4.0);
            let mut typed = false;
            for (speaker, name) in detail.names.iter_mut().enumerate() {
                let field = egui::TextEdit::singleline(name).hint_text(format!("Speaker {}", speaker + 1)).desired_width(f32::INFINITY).id(egui::Id::unique(("speaker name", &note.id, speaker)));
                typed |= ui.add(field).changed();
            }
            if typed {
                detail.names_edited = Some(Instant::now());
            }
            let labels: Vec<String> = detail.names.iter().enumerate().map(|(speaker, name)| if name.trim().is_empty() { format!("Speaker {}", speaker + 1) } else { name.trim().to_string() }).collect();
            if labels.iter().enumerate().any(|(i, label)| labels[..i].contains(label)) {
                weak(ui, "Two speakers cannot share a name; one that is taken is not given.");
            }
            if note.body.is_some() && note.body_covers >= note.segments.len() {
                weak(ui, "This note's text was edited by hand and is left as it is; the timeline shows who spoke. Transcribe again to rebuild the text with speakers.");
            }
        }
    });
    if let Some(fixed) = choice {
        host.haptic(Haptic::Light);
        // Names typed a moment ago belong to the count they were typed under.
        if let Some(detail) = app.detail.as_mut() {
            detail.flush(engine);
        }
        engine.find_speakers(&note.id, fixed);
    }
}

/// Whether "Transcribe again" is offered. A finished note with no text gets it too: audio the
/// first pass heard nothing in is exactly what another model, or another setting, is for.
pub fn can_redo(note: &Note, queued: bool) -> bool {
    !queued && (note.state == NoteState::Done || !note.segments.is_empty())
}

/// The IME holds its own copy of a focused field; tell it the text changed under it.
fn egui_mobile_note_out_of_band() {
    #[cfg(target_os = "android")]
    egui_mobile::note_ime_out_of_band_edit();
}

/// What a note is waiting on, for its row and its banner.
fn state_of(app: &ScribeApp, note: &Note) -> (String, Role) {
    let Some(status) = app.status.as_ref() else { return (String::new(), Role::Muted) };
    if status.recording.as_ref().is_some_and(|r| r.id == note.id) {
        return ("Recording".into(), Role::Danger);
    }
    if let Some((_, done, total)) = status.active.as_ref().filter(|(id, ..)| *id == note.id) {
        return (format!("Transcribing {}%", (*done as f64 * 100.0 / (*total).max(1) as f64) as u32), Role::Accent);
    }
    if status.queue.iter().any(|id| *id == note.id) {
        use crate::engine::ModelStatus;
        let waiting = match (&status.model, status.blocked) {
            (_, false) => "Queued",
            (ModelStatus::Paused(_), true) => "Transcription paused",
            (ModelStatus::Failed(_), true) => "Model unavailable",
            (_, true) => "Waiting for a model",
        };
        return (waiting.into(), Role::Warning);
    }
    match note.state {
        NoteState::Failed => ("Transcription failed".into(), Role::Danger),
        NoteState::Done => (String::new(), Role::Muted),
        _ if note.segments.is_empty() => ("Not transcribed".into(), Role::Warning),
        _ => ("Partly transcribed".into(), Role::Warning),
    }
}

fn meta(note: &Note) -> String {
    let now = clock::local(clock::now());
    format!("{} · {}", clock::local(note.created).relative_to(&now), clock::duration(note.duration_seconds()))
}

pub fn list(app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    ui.horizontal(|ui| {
        svg::MAGNIFYING_GLASS.show_tinted(ui, 18.0, ui.visuals().weak_text_color());
        ui.add(egui::TextEdit::singleline(&mut app.search).hint_text("Search notes").desired_width(f32::INFINITY).id(egui::Id::unique("note search")));
    });
    ui.add_space(4.0);

    if app.notes.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            svg::MICROPHONE.show_tinted(ui, 44.0, ui.visuals().weak_text_color());
            ui.add_space(8.0);
            ui.label(RichText::new("No notes yet").size(18.0));
            weak(ui, "Record something and it is transcribed here, on the phone.");
            ui.add_space(12.0);
            if action(ui, svg::MICROPHONE, "Record a note").clicked() {
                app.tab = super::Tab::Record;
            }
        });
        return;
    }

    let mut open = None;
    egui::ScrollArea::vertical().id_salt("notes").auto_shrink([false, false]).show(ui, |ui| {
        // A selectable label would take the tap meant for the row it sits in.
        ui.style_mut().interaction.selectable_labels = false;
        let mut shown = 0;
        let query = app.search.trim().to_lowercase();
        for (note, row) in app.notes.iter().zip(&app.rows).filter(|(_, row)| row.holds(&query)) {
            shown += 1;
            let builder = egui::UiBuilder::new().id_salt(("note", &note.id)).sense(egui::Sense::click());
            let response = ui
                .scope_builder(builder, |ui| {
                    card(ui, |ui| {
                        // Rows of text, not of controls: without this each takes a button's height.
                        ui.spacing_mut().interact_size.y = 0.0;
                        ui.horizontal(|ui| {
                            if note.pinned {
                                svg::PUSH_PIN.show_tinted(ui, 14.0, role(ui, Role::Accent));
                            }
                            ui.add(egui::Label::new(RichText::new(&row.title).size(15.5).strong()).truncate());
                        });
                        let (state, colour) = state_of(app, note);
                        ui.horizontal(|ui| {
                            ui.add(egui::Label::new(RichText::new(meta(note)).size(12.0).color(ui.visuals().weak_text_color())).truncate());
                            if !state.is_empty() {
                                ui.add(egui::Label::new(RichText::new(format!("• {state}")).size(12.0).color(role(ui, colour))).truncate());
                            }
                        });
                        if !row.snippet.is_empty() {
                            wrapped(ui, RichText::new(&row.snippet).size(13.0).color(ui.visuals().weak_text_color()));
                        }
                    });
                })
                .response;
            response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &row.title));
            if response.clicked() {
                open = Some(note.id.clone());
            }
            ui.add_space(2.0);
        }
        if shown == 0 {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| weak(ui, format!("Nothing matches “{}”.", app.search.trim())));
        }
    });
    if let Some(id) = open {
        host.haptic(Haptic::Selection);
        app.open_note(&id);
    }
}

pub fn detail(app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    let Some(engine) = app.engine.clone() else { return };
    let Some(id) = app.detail.as_ref().map(|d| d.id.clone()) else { return };
    let Some(note) = app.notes.iter().find(|n| n.id == id).cloned() else {
        // Deleted from under the screen.
        app.close_note();
        return;
    };
    let (state, state_colour) = state_of(app, &note);
    let busy = app.status.as_ref().is_some_and(|s| s.recording.as_ref().is_some_and(|r| r.id == id) || s.active.as_ref().is_some_and(|(active, ..)| *active == id));
    let queued = app.status.as_ref().is_some_and(|s| s.queue.iter().any(|q| *q == id));

    if let Some(detail) = app.detail.as_mut() {
        detail.follow(&note);
        detail.save_due(&engine);
    }

    egui::ScrollArea::vertical().id_salt(("detail", &id)).auto_shrink([false, false]).show(ui, |ui| {
        let Some(detail) = app.detail.as_mut() else { return };
        let title = egui::TextEdit::singleline(&mut detail.title)
            .hint_text(note.display_title())
            .font(egui::FontId::proportional(19.0))
            .desired_width(f32::INFINITY)
            .id(egui::Id::unique(("note title", &id)));
        if ui.add(title).changed() {
            detail.title_edited = Some(Instant::now());
        }
        let mut line = meta(&note);
        if let Some(language) = note.language.as_deref() {
            line.push_str(&format!(" · {language}"));
        }
        ui.add(egui::Label::new(RichText::new(line).size(12.5).color(ui.visuals().weak_text_color())).truncate());
        ui.add_space(4.0);

        player(app, ui, host, &note, busy);
        ui.add_space(4.0);

        // What the note is waiting on, and the one thing to do about it.
        if !state.is_empty() {
            card(ui, |ui| {
                wrapped(ui, RichText::new(&state).color(role(ui, state_colour)));
                if let Some((_, done, total)) = app.status.as_ref().and_then(|s| s.active.clone()).filter(|(active, ..)| *active == id) {
                    super::progress(ui, done as f32 / total.max(1) as f32);
                }
                if let Some(error) = note.error.as_deref() {
                    weak(ui, error);
                }
                let recording = app.status.as_ref().is_some_and(|s| s.recording.as_ref().is_some_and(|r| r.id == id));
                // Queued behind a model that is not there: say which kind of not there.
                let stuck = queued && app.status.as_ref().is_some_and(|s| s.blocked);
                // A model that is there and would not start can be asked again; a missing
                // one has to be fetched first.
                let mut retry = false;
                if stuck {
                    match app.status.as_ref().map(|s| &s.model) {
                        Some(crate::engine::ModelStatus::Failed(reason)) => {
                            retry = true;
                            weak(ui, format!("The model could not be loaded: {reason}"))
                        }
                        Some(crate::engine::ModelStatus::Paused(reason)) => {
                            retry = true;
                            weak(ui, reason)
                        }
                        _ => weak(ui, "No speech model is installed. The recording is safe and is transcribed once one is."),
                    };
                }
                let mut to_settings = false;
                ui.horizontal_wrapped(|ui| {
                    if retry && action(ui, svg::ARROWS_CLOCKWISE, "Try again").clicked() {
                        host.haptic(Haptic::Light);
                        engine.transcribe(&id, false);
                    }
                    if stuck && action(ui, svg::DOWNLOAD_SIMPLE, "Speech models").clicked() {
                        to_settings = true;
                    }
                    if busy || queued {
                        if !recording && action(ui, svg::X, "Cancel").clicked() {
                            engine.cancel(&id);
                        }
                    } else if action(ui, svg::SPARKLE, if note.segments.is_empty() { "Transcribe" } else { "Continue" }).clicked() {
                        host.haptic(Haptic::Light);
                        engine.transcribe(&id, false);
                    }
                });
                if to_settings {
                    app.leave_to = Some(super::Tab::Settings);
                }
            });
            ui.add_space(4.0);
        }

        if !busy {
            speakers(app, ui, host, &engine, &note);
        }

        let Some(detail) = app.detail.as_mut() else { return };
        if busy {
            detail.body_focused = false;
            // Text is still arriving; an editor here would fight it for the buffer.
            if detail.body.is_empty() {
                weak(ui, "The transcript will appear here.");
            } else {
                wrapped(ui, RichText::new(&detail.body).size(15.5));
            }
        } else {
            if note.state == NoteState::Done && note.segments.is_empty() && detail.body.is_empty() {
                weak(ui, "No speech was heard in this recording.");
            }
            let body = egui::TextEdit::multiline(&mut detail.body)
                .hint_text("No text yet. Transcribe the recording, or type here.")
                .font(egui::FontId::proportional(15.5))
                .desired_width(f32::INFINITY)
                .desired_rows(6)
                .id(egui::Id::unique(("note body", &id)));
            let field = ui.add(body);
            detail.body_focused = field.has_focus();
            if field.changed() {
                detail.body_edited = Some(Instant::now());
            }
        }
        ui.add_space(6.0);

        ui.horizontal_wrapped(|ui| {
            let text = app.detail.as_ref().map(|d| d.body.clone()).unwrap_or_default();
            let has_text = !text.trim().is_empty();
            if action_if(ui, has_text, svg::COPY, "Copy").clicked() {
                host.copy_text(text.clone());
                app.say("Copied.");
            }
            if action_if(ui, has_text, svg::SHARE_NETWORK, "Share text").clicked() {
                host.share_text(text);
            }
            // A transcript is usually too long to paste; most apps take it as an attachment.
            if action_if(ui, has_text, svg::FILE_TEXT, "Share file").clicked() {
                host.haptic(Haptic::Light);
                app.share_file(host, &id);
            }
            if action_if(ui, !busy, svg::FILE_AUDIO, "Share audio").clicked() {
                crate::platform::share_audio(host, &engine.library.audio_path(&id), &format!("{}.wav", note.id));
                app.sharing = true;
            }
            if can_redo(&note, queued) && action_if(ui, !busy, svg::ARROWS_CLOCKWISE, "Transcribe again").clicked() {
                if note.segments.is_empty() && note.body.is_none() {
                    // Nothing would be replaced, so there is nothing to confirm.
                    host.haptic(Haptic::Light);
                    engine.transcribe(&id, true);
                } else {
                    app.confirm = Some(Confirm::Retranscribe(id.clone()));
                }
            }
            if action(ui, if note.pinned { svg::PUSH_PIN_SLASH } else { svg::PUSH_PIN }, if note.pinned { "Unpin" } else { "Pin" }).clicked() {
                engine.edit_note(&id, |note| note.pinned = !note.pinned);
            }
            if !busy && role_action(ui, Role::Danger, svg::TRASH, "Delete").clicked() {
                app.confirm = Some(Confirm::DeleteNote(id.clone()));
            }
        });

        if let (Some(export), Some(exporter)) = (note.export.as_ref().filter(|_| app.settings.save_text), engine.exporter.as_ref()) {
            weak(ui, format!("Text file: {}/{}", exporter.label(), export.file));
        }

        if !note.segments.is_empty() {
            ui.add_space(6.0);
            egui::CollapsingHeader::new("Timeline").id_salt(("timeline", &id)).default_open(app.expand_all).show(ui, |ui| {
                let mut jump = None;
                let named = note.speakers.as_ref().filter(|speakers| speakers.count > 1);
                for segment in &note.segments {
                    let who = named.zip(segment.speaker).map(|(speakers, speaker)| format!(" · {}", speakers.name(speaker))).unwrap_or_default();
                    let stamp = format!("{} – {}{who}", clock::duration(segment.start as f64), clock::duration(segment.end as f64));
                    if ui.add_enabled(!busy, egui::Button::new(RichText::new(stamp).size(12.0)).small().truncate()).clicked() {
                        jump = Some(segment.start as f64);
                    }
                    wrapped(ui, RichText::new(&segment.text).size(14.0));
                    ui.add_space(4.0);
                }
                if let Some(seconds) = jump {
                    app.play_from(host, &note, seconds);
                }
            });
        }
        ui.add_space(12.0);
    });
}

/// Play, pause and seek through the note's audio.
fn player(app: &mut ScribeApp, ui: &mut Ui, host: &Host, note: &Note, busy: bool) {
    let duration = note.duration_seconds().max(0.1);
    let loaded = app.player.note() == Some(note.id.as_str());
    let playing = loaded && app.player.is_playing();
    let position = if loaded { app.player.position().min(duration) } else { 0.0 };
    let mut seek_to = None;
    card(ui, |ui| {
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                let (icon, label) = if playing { (svg::PAUSE, "Pause") } else { (svg::PLAY, "Play") };
                if super::icon_button(ui, icon, label, 44.0).clicked() {
                    host.haptic(Haptic::Light);
                    if playing {
                        app.player.set_playing(false);
                    } else {
                        app.play_from(host, note, position);
                    }
                }
                let Some(detail) = app.detail.as_mut() else { return };
                let stamp = format!("{} / {}", clock::duration(detail.scrub.unwrap_or(position)), clock::duration(duration));
                // The clock is laid out first, from the right, so the bar gets what is left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(stamp).size(12.5).color(ui.visuals().weak_text_color()));
                    let mut value = detail.scrub.unwrap_or(position);
                    ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(40.0);
                    let slider = ui.add(egui::Slider::new(&mut value, 0.0..=duration).show_value(false));
                    if slider.dragged() {
                        detail.scrub = Some(value);
                    } else if slider.drag_stopped() || slider.changed() {
                        // One seek when the finger lifts: MediaPlayer blocks on each.
                        detail.scrub = None;
                        seek_to = Some(value);
                    }
                });
            });
        });
        if busy {
            weak(ui, "Playback is available once the recording is saved.");
        }
    });
    if let Some(seconds) = seek_to {
        if loaded {
            app.player.seek(seconds);
        } else {
            app.play_from(host, note, seconds);
            app.player.set_playing(false);
        }
    }
}
