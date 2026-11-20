//! The list of notes, and one note opened: a page of its title, speakers, text and timeline,
//! over a bar that plays the audio and holds what can be done with the note.

use super::{action, card, menu_row, role, weak, wrapped};
use crate::app::{Confirm, ScribeApp};
use crate::clock;
use crate::library::{Note, NoteState, Seg};
use egui::{Color32, RichText, Ui};
use egui_icon_pack::phosphor_regular as svg;
use egui_icon_pack::Icon;
use egui_mobile::theme::Role;
use egui_mobile::{Haptic, Host};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long an edit sits before it is written, so typing does not rewrite the file per key.
const SAVE_AFTER: Duration = Duration::from_millis(700);

/// The most of the page the text, or the timeline, fills. However long either runs, the
/// headings around it stay where a thumb finds them.
const MOST: f32 = 0.7;
/// What either keeps when the page is crowded; past that the page itself scrolls. A quarter
/// each lets the two of them, both open, share a phone's page with the headings.
const LEAST: f32 = 0.25;
/// Left clear under the last thing on the page.
const END: f32 = 8.0;
/// The side of the square buttons on the note's bar.
const BUTTON: f32 = 44.0;
/// The note's bar with its margin, for what is drawn over the foot of the screen.
pub const BAR: f32 = BUTTON + 12.0;
/// Around the text inside its card: on the left, room for a speaker's stripe.
const TEXT_EDGE: egui::Margin = egui::Margin { left: 11, right: 6, top: 3, bottom: 3 };
/// A timeline row's text starts this far in, clear of its stripe, and this far down.
const ROW_INSET: f32 = 10.0;
const ROW_EDGE: f32 = 5.0;

/// The menus on the open note's bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu {
    Share,
    More,
}

/// Where the parts of the page came to rest last frame. The text is sized by what lies
/// under it, and that is only known once the page has been laid out.
#[derive(Clone, Copy)]
struct Fit {
    /// The height the page had.
    room: f32,
    /// How much of the page lies under the text's card.
    under: f32,
    /// The text had the keyboard.
    typing: bool,
    /// How tall the card and the whole page came out, for the layout tests to hold to.
    #[cfg(test)]
    card: f32,
    #[cfg(test)]
    page: f32,
}

/// How tall each row of the timeline is at one width. Rows out of view are stepped over by
/// these instead of being laid out: an hour of speech is a thousand rows.
#[derive(Default)]
struct Rows {
    width: f32,
    /// Per segment: when it starts, the length of its text, and the height that comes to.
    known: Vec<(u32, usize, f32)>,
    /// The height the timeline was last given, for the layout tests to hold to.
    #[cfg(test)]
    tall: f32,
}

impl Rows {
    /// Bring the heights in line with `segments` at `width`, measuring only what is new.
    fn fit(&mut self, ui: &Ui, segments: &[Seg], width: f32) {
        if self.width != width {
            (self.width, self.known) = (width, Vec::new());
        }
        let kept = self.known.iter().zip(segments).take_while(|((start, bytes, _), segment)| *start == segment.start.to_bits() && *bytes == segment.text.len()).count();
        self.known.truncate(kept);
        for segment in &segments[kept..] {
            self.known.push((segment.start.to_bits(), segment.text.len(), part(ui, &segment.text, width).1));
        }
    }
}

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
    /// What the text calls each speaker, by number; empty while nothing in it is labelled.
    voices: Vec<String>,
    /// The text field has the keyboard, as of the last frame.
    body_focused: bool,
    /// Where the seek bar is being dragged to, until the finger lifts.
    scrub: Option<f64>,
    fit: Fit,
    rows: Rows,
}

impl Detail {
    pub fn open(note: &Note) -> Self {
        let text = note.text();
        // Nothing is under the text until a frame has put it there; a heading's worth is
        // the guess that frame is laid out by, and it is not shown.
        let fit = Fit {
            room: 0.0,
            under: 64.0,
            typing: false,
            #[cfg(test)]
            card: 0.0,
            #[cfg(test)]
            page: 0.0,
        };
        Self { id: note.id.clone(), title: note.title.clone(), body: text.clone(), synced: text, covers: note.segments.len(), body_edited: None, title_edited: None, unsynced: false, names: names_of(note), names_edited: None, voices: voices_of(note), body_focused: false, scrub: None, fit, rows: Rows::default() }
    }

    /// How tall the text's card and the whole page came out, and the height the page had.
    #[cfg(test)]
    pub fn laid_out(&self) -> (f32, f32, f32) {
        (self.fit.card, self.fit.page, self.fit.room)
    }

    /// How many timeline rows have a height, and what they come to.
    #[cfg(test)]
    pub fn timeline_rows(&self) -> (usize, f32) {
        (self.rows.known.len(), self.rows.known.iter().map(|(.., height)| height).sum())
    }

    /// The height the timeline was last given to scroll in.
    #[cfg(test)]
    pub fn timeline_tall(&self) -> f32 {
        self.rows.tall
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
        self.voices = voices_of(note);
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
        // A name just saved reaches the draft through `take`, a frame before it reaches
        // this copy of the note; that frame is after this, so the newer names stand.
        self.voices = voices_of(note);
    }
}

/// What the text calls each speaker; nothing while there is one voice or none, when no
/// line of it is labelled.
fn voices_of(note: &Note) -> Vec<String> {
    let Some(speakers) = note.speakers.as_ref().filter(|speakers| speakers.count > 1) else { return Vec::new() };
    (0..speakers.count).map(|speaker| speakers.name(speaker)).collect()
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
    super::section(ui, "speakers", &title, app.expand_all, |ui| {
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
                ui.horizontal(|ui| {
                    // The colour their turns carry in the text and the timeline. Painted:
                    // the default fonts have no filled circle to print.
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(14.0, ui.spacing().interact_size.y), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 5.0, super::voice(speaker));
                    let field = egui::TextEdit::singleline(name).hint_text(format!("Speaker {}", speaker + 1)).desired_width(f32::INFINITY).id(egui::Id::unique(("speaker name", &note.id, speaker)));
                    typed |= ui.add(field).changed();
                });
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

/// Whether `id` is being recorded or transcribed right now: its text is still arriving.
fn busy_with(app: &ScribeApp, id: &str) -> bool {
    app.status.as_ref().is_some_and(|s| s.recording.as_ref().is_some_and(|r| r.id == id) || s.active.as_ref().is_some_and(|(active, ..)| active == id))
}

/// How tall to make the text, or the timeline, on a page `room` tall whose other parts
/// leave it `free`: that, within bounds.
fn share(room: f32, free: f32) -> f32 {
    // `max` and `min`, not `clamp`: a page height that is not a number must not panic.
    let most = (room * MOST).floor().max(0.0);
    let least = (room * LEAST).floor().max(0.0).min(most);
    // A point short, so that rounding never leaves the page a sliver to scroll by.
    (free - 1.0).floor().max(least).min(most)
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
    let busy = busy_with(app, &id);
    let queued = app.status.as_ref().is_some_and(|s| s.queue.iter().any(|q| *q == id));

    if let Some(detail) = app.detail.as_mut() {
        detail.follow(&note);
        detail.save_due(&engine);
    }

    // What the text and the timeline are sized against: the page as it stands, less the
    // keyboard when that is up.
    let room = ui.available_height();
    // The page's own bar floats over it. One that took its width from the page would
    // re-wrap the text every time the keyboard, or an unfolded section, made the page scroll.
    let bars = ui.spacing().scroll;
    ui.spacing_mut().scroll = egui::style::ScrollStyle::floating();
    egui::ScrollArea::vertical().id_salt(("detail", &id)).auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().scroll = bars;
        let page = ui.cursor().top();
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

        // What the note is waiting on, and the one thing to do about it.
        if !state.is_empty() {
            ui.add_space(4.0);
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
        }

        if !busy {
            speakers(app, ui, host, &engine, &note);
        }

        let mut placed = None;
        super::section(ui, "transcript", "Transcript", true, |ui| {
            let Some(detail) = app.detail.as_ref() else { return };
            if !busy && note.state == NoteState::Done && note.segments.is_empty() && detail.body.is_empty() {
                weak(ui, "No speech was heard in this recording.");
            }
            let above = ui.cursor().top() - page;
            // While it is typed in it runs down to the keyboard, and what lies under it
            // waits below the fold: the keyboard leaves too little page to share out.
            let tall = share(room, room - above - if detail.fit.typing { 0.0 } else { detail.fit.under });
            placed = Some(text(app, ui, &note, busy, tall));
            if let (Some(export), Some(exporter)) = (note.export.as_ref().filter(|_| app.settings.save_text), engine.exporter.as_ref()) {
                weak(ui, format!("Text file: {}/{}", exporter.label(), export.file));
            }
        });

        if !note.segments.is_empty() {
            super::section(ui, "timeline", "Timeline", app.expand_all, |ui| {
                // Under it: the gap every section is followed by, and the foot of the page.
                let free = room - (ui.cursor().top() - page) - ui.spacing().item_spacing.y - END;
                // The text was sized before this unfolded and has the room. The two share
                // it evenly, unless the text is short and wants less than its half; the text
                // then finds the difference lying under it and gives it up.
                let text = placed.map_or(0.0, |card: egui::Rect| card.height());
                let tall = share(room, free + text - text.min((free + text) / 2.0));
                timeline(app, ui, host, &note, busy, tall);
            });
        }
        ui.add_space(END);

        let Some(detail) = app.detail.as_mut() else { return };
        let was = detail.fit;
        let typing = detail.body_focused;
        let Some(card) = placed else {
            // Folded away, the text is not being typed in, whatever it was a frame ago.
            detail.body_focused = false;
            detail.fit = Fit { room, typing: false, ..was };
            return;
        };
        detail.fit = Fit {
            room,
            under: ui.cursor().top() - card.bottom(),
            typing,
            #[cfg(test)]
            card: card.height(),
            #[cfg(test)]
            page: ui.cursor().top() - page,
        };
        if was.room == 0.0 {
            // The first frame is laid out on a guess of what lies under the text. Laid out
            // again before it is shown, the text does not change height as the note opens.
            ui.ctx().request_discard("a note's first layout");
        }
        // The keyboard takes the foot of the page as it comes up, and the text is its own
        // scroll area: nothing else brings what is being typed back into view.
        if typing && (!was.typing || was.room != room) {
            ui.scroll_to_rect(card, None);
        }
    });
    ui.spacing_mut().scroll = bars;
}

/// One speaker's stretch of the text: the line their name opens, and what follows it.
struct Turn {
    speaker: usize,
    /// Their name and its colon, in bytes.
    label: Range<usize>,
    /// From the name to the end of what they say, in characters: a galley counts in those.
    chars: Range<usize>,
}

/// The stretches of `text` that open with one of `voices`. A paragraph without a name is
/// the same speaker going on after a pause, which is how the transcript writes it.
fn turns_in(text: &str, voices: &[String]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    if voices.is_empty() {
        return turns;
    }
    let (mut byte, mut char) = (0, 0);
    for line in text.split('\n') {
        let count = line.chars().count();
        // The longest, as renaming does: of "Al" and "Al: B", a line opening "Al: B: " is the second's.
        let opens = |name: &&String| line.strip_prefix(name.as_str()).is_some_and(|rest| rest.starts_with(": "));
        match voices.iter().enumerate().filter(|(_, name)| opens(name)).max_by_key(|(_, name)| name.len()) {
            Some((speaker, name)) => turns.push(Turn { speaker, label: byte..byte + name.len() + 1, chars: char..char + count }),
            None if !line.trim().is_empty() => {
                if let Some(turn) = turns.last_mut() {
                    turn.chars.end = char + count;
                }
            }
            None => {}
        }
        byte += line.len() + 1;
        char += count + 1;
    }
    turns
}

/// `text` ready to lay out `wrap` wide, with each speaker's name in that speaker's colour.
fn job(text: &str, turns: &[Turn], font: &egui::FontId, ink: Color32, inks: &[Color32], line: f32, wrap: f32) -> egui::text::LayoutJob {
    let plain = egui::TextFormat { font_id: font.clone(), color: ink, line_height: Some(line), ..Default::default() };
    // Trailing spaces stay, as in any text field: one that vanished as it was typed would
    // take the cursor back with it.
    let mut job = egui::text::LayoutJob { keep_trailing_whitespace: true, ..Default::default() };
    job.wrap.max_width = wrap;
    let mut at = 0;
    for turn in turns {
        if turn.label.start > at {
            job.append(&text[at..turn.label.start], 0.0, plain.clone());
        }
        job.append(&text[turn.label.clone()], 0.0, egui::TextFormat { color: inks[turn.speaker], ..plain.clone() });
        at = turn.label.end;
    }
    // Always, even when nothing is left: a job with no section has no line height.
    job.append(&text[at..], 0.0, plain);
    job
}

/// A band of the speaker's colour behind each turn of `galley`, which is laid out at
/// `origin` and `across` the card.
fn bands(galley: &egui::Galley, origin: egui::Pos2, across: egui::Rangef, clip: egui::Rect, turns: &[Turn], hues: &[Color32]) -> Vec<egui::Shape> {
    // The character each row starts at: rows are what have a place on the page.
    let mut starts = Vec::with_capacity(galley.rows.len());
    let mut count = 0;
    for row in &galley.rows {
        starts.push(count);
        count += row.char_count_including_newline().0;
    }
    let mut shapes = Vec::new();
    for turn in turns {
        let first = starts.partition_point(|start| *start <= turn.chars.start).saturating_sub(1);
        let last = starts.partition_point(|start| *start < turn.chars.end).saturating_sub(1);
        let (Some(top), Some(bottom), Some(hue)) = (galley.rows.get(first), galley.rows.get(last), hues.get(turn.speaker)) else { continue };
        let band = egui::Rect::from_x_y_ranges(across, origin.y + top.min_y() - 2.0..=origin.y + bottom.max_y() + 2.0);
        // A long conversation is hundreds of turns, nearly all of them scrolled out of view.
        if !clip.intersects(band) {
            continue;
        }
        shapes.push(egui::Shape::rect_filled(band, 4.0, hue.gamma_multiply(0.16)));
        shapes.push(egui::Shape::rect_filled(band.with_max_x(band.left() + 3.0), egui::CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 }, *hue));
    }
    shapes
}

/// The note's text in a card that scrolls once the text is taller than `tall`, with each
/// speaker's turns set off in that speaker's colour. Returns where the card is.
fn text(app: &mut ScribeApp, ui: &mut Ui, note: &Note, busy: bool, tall: f32) -> egui::Rect {
    let Some(detail) = app.detail.as_mut() else { return egui::Rect::NOTHING };
    let font = egui::FontId::proportional(15.5);
    let ink = ui.visuals().text_color();
    let hues: Vec<Color32> = (0..detail.voices.len()).map(super::voice).collect();
    let inks: Vec<Color32> = hues.iter().map(|hue| super::legible(*hue, ui.visuals().panel_fill)).collect();
    let line = ui.fonts_mut(|fonts| fonts.row_height(&font)) + ui.spacing().extra_text_line_spacing;
    let voices = detail.voices.clone();
    // The colours go in as the text is laid out, so they follow every keystroke.
    let mut layouter = |ui: &Ui, text: &dyn egui::TextBuffer, wrap: f32| {
        let turns = turns_in(text.as_str(), &voices);
        ui.fonts_mut(|fonts| fonts.layout_job(job(text.as_str(), &turns, &font, ink, &inks, line, wrap)))
    };
    let mut frame = egui::Frame::group(ui.style()).inner_margin(3);
    if detail.body_focused {
        frame = frame.stroke(ui.visuals().selection.stroke);
    }
    let within = (tall - frame.total_margin().sum().y).max(0.0);
    let inside = |ui: &mut Ui| {
        frame.show(ui, |ui| {
            // While the text is still arriving, the newest of it stays in view.
            egui::ScrollArea::vertical().id_salt(("text", &note.id)).max_height(within).auto_shrink([false, true]).stick_to_bottom(busy).show(ui, |ui| {
                // Held for the bands: they go behind text that is laid out after them.
                let behind = ui.painter().add(egui::Shape::Noop);
                let (galley, origin, across) = if busy {
                    detail.body_focused = false;
                    if detail.body.is_empty() {
                        ui.set_width(ui.available_width());
                        weak(ui, "The transcript will appear here.");
                        return;
                    }
                    // Not a field: an editor here would fight the transcript for the buffer.
                    let across = ui.available_width();
                    let galley = layouter(ui, &detail.body, across - TEXT_EDGE.sum().x);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(across, galley.size().y + TEXT_EDGE.sum().y), egui::Sense::hover());
                    let origin = rect.min + TEXT_EDGE.left_top();
                    ui.painter().galley(origin, galley.clone(), ink);
                    (galley, origin, rect.x_range())
                } else {
                    let field = egui::TextEdit::multiline(&mut detail.body)
                        .hint_text("No text yet. Transcribe the recording, or type here.")
                        .font(font.clone())
                        .frame(egui::Frame::NONE.inner_margin(TEXT_EDGE))
                        .desired_width(f32::INFINITY)
                        .desired_rows(4)
                        .id(egui::Id::unique(("note body", &note.id)))
                        .layouter(&mut layouter)
                        .show(ui);
                    // On a touch screen the tap that focuses a field leaves the caret at
                    // its end, which here is pages from the word that was tapped.
                    if field.response.clicked() && !detail.body_focused {
                        if let Some(tapped) = field.response.interact_pointer_pos() {
                            let mut state = field.state.clone();
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(field.galley.cursor_from_pos(tapped - field.galley_pos))));
                            state.store(ui.ctx(), field.response.id);
                        }
                    }
                    detail.body_focused = field.response.has_focus();
                    if field.response.changed() {
                        detail.body_edited = Some(Instant::now());
                    }
                    (field.galley, field.galley_pos, field.response.rect.x_range())
                };
                let turns = turns_in(&detail.body, &voices);
                ui.painter().set(behind, egui::Shape::Vec(bands(&galley, origin, across, ui.clip_rect(), &turns, &hues)));
            });
        });
    };
    sized(ui, tall, inside).response.rect
}

/// Lay `inside` out with `tall` to its name. A scroll area in a page that itself scrolls
/// is otherwise offered only what is left of the page in view, and makes itself that
/// small, whatever height it was told it may have.
fn sized<R>(ui: &mut Ui, tall: f32, inside: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    ui.allocate_ui(egui::vec2(ui.available_width(), tall), inside)
}

/// A timeline row's words wrapped for a row `width` wide, and how tall that makes the row.
fn part(ui: &Ui, text: &str, width: f32) -> (Arc<egui::Galley>, f32) {
    let colour = ui.visuals().text_color();
    let (words, stamp) = ui.fonts_mut(|fonts| (fonts.layout(text.to_owned(), egui::FontId::proportional(14.0), colour, width - ROW_INSET), fonts.row_height(&egui::FontId::proportional(12.0))));
    let height = ROW_EDGE + stamp + words.size().y + ROW_EDGE;
    (words, height)
}

/// The note part by part, in an area that scrolls past `tall`. A tap on a part plays the
/// recording from where it starts, and the part being played is lit.
fn timeline(app: &mut ScribeApp, ui: &mut Ui, host: &Host, note: &Note, busy: bool, tall: f32) {
    let named = note.speakers.as_ref().filter(|speakers| speakers.count > 1);
    let playing = (app.player.note() == Some(note.id.as_str())).then(|| app.player.position() as f32);
    let ground = ui.visuals().panel_fill;
    let weak_ink = ui.visuals().weak_text_color();
    let stamp_font = egui::FontId::proportional(12.0);
    let mut jump = None;
    let area = egui::ScrollArea::vertical().id_salt(("timeline", &note.id)).max_height(tall).auto_shrink([false, true]);
    sized(ui, tall, |ui| area.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let gap = ui.spacing().item_spacing.y;
        let width = ui.available_width();
        let Some(detail) = app.detail.as_mut() else { return };
        detail.rows.fit(ui, &note.segments, width);
        #[cfg(test)]
        {
            detail.rows.tall = tall;
        }
        let rows = ui.scope_id().with("part");
        let left = ui.cursor().left();
        let mut top = ui.cursor().top();
        // Rows above and below the view are allocated as one gap each, never laid out.
        let mut unseen = 0.0;
        for (index, (segment, (.., height))) in note.segments.iter().zip(&detail.rows.known).enumerate() {
            let place = egui::Rect::from_min_size(egui::pos2(left, top), egui::vec2(width, *height));
            top += height + gap;
            if !ui.is_rect_visible(place) {
                unseen += height + gap;
                continue;
            }
            if unseen > 0.0 {
                ui.allocate_space(egui::vec2(width, unseen - gap));
                unseen = 0.0;
            }
            let (_, rect) = ui.allocate_space(egui::vec2(width, *height));
            // By number, not by order: how many rows were laid out before this one changes
            // as the list scrolls.
            let response = ui.interact(rect, rows.with(index), if busy { egui::Sense::hover() } else { egui::Sense::click() });
            let live = playing.is_some_and(|at| segment.start <= at && at < segment.end);
            if live || response.is_pointer_button_down_on() {
                ui.painter().rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(if live { 0.4 } else { 0.2 }));
            }
            let who = named.zip(segment.speaker);
            if let Some((_, speaker)) = who {
                ui.painter().rect_filled(rect.with_max_x(rect.left() + 3.0), 1.5, super::voice(speaker as usize));
            }
            let times = format!("{} – {}", clock::duration(segment.start as f64), clock::duration(segment.end as f64));
            let mut stamp = egui::text::LayoutJob::default();
            stamp.append(&times, 0.0, egui::TextFormat { font_id: stamp_font.clone(), color: weak_ink, ..Default::default() });
            if let Some((speakers, speaker)) = who {
                stamp.append(&format!(" · {}", speakers.name(speaker)), 0.0, egui::TextFormat { font_id: stamp_font.clone(), color: super::legible(super::voice(speaker as usize), ground), ..Default::default() });
            }
            // One line: a long name ends in an ellipsis rather than pushing the row wider.
            stamp.wrap = egui::text::TextWrapping::truncate_at_width(width - ROW_INSET);
            let label = format!("{}. {}", stamp.text, segment.text);
            let stamp = ui.fonts_mut(|fonts| fonts.layout_job(stamp));
            let words = part(ui, &segment.text, width).0;
            let at = rect.min + egui::vec2(ROW_INSET, ROW_EDGE);
            ui.painter().galley(at + egui::vec2(0.0, stamp.size().y), words, ui.visuals().text_color());
            ui.painter().galley(at, stamp, weak_ink);
            response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, !busy, &label));
            if response.clicked() {
                jump = Some(segment.start as f64);
            }
        }
        if unseen > 0.0 {
            ui.allocate_space(egui::vec2(width, unseen - gap));
        }
    }));
    if let Some(seconds) = jump {
        host.haptic(Haptic::Light);
        app.play_from(host, note, seconds);
    }
}

/// A square icon button at `rect`, with a name for a screen reader.
fn square(ui: &mut Ui, rect: egui::Rect, enabled: bool, selected: bool, icon: Icon, label: &str) -> egui::Response {
    let button = egui::Button::new(icon.image(rect.height() * 0.46)).image_tint_follows_text_color(true).selected(selected);
    let layout = egui::Layout::centered_and_justified(egui::Direction::TopDown);
    let response = ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(layout), |ui| ui.add_enabled(enabled, button)).inner;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    response
}

/// A button on the bar and the menu it opens. Upward, always: egui opens a menu downward
/// wherever that fits, and its screen runs on under Android's navigation bar.
fn menu(app: &mut ScribeApp, ui: &mut Ui, which: Menu, at: egui::Rect, icon: Icon, label: &str, rows: impl FnOnce(&mut ScribeApp, &mut Ui)) {
    let was_open = app.menu == Some(which);
    let button = square(ui, at, true, was_open, icon, label);
    // Which menu is open is the app's to know, so that Back can close it.
    let mut open = was_open != button.clicked();
    // Opaque whatever the theme's glass setting: it opens over the note's text.
    let frame = egui::Frame::popup(ui.style()).fill(ui.visuals().window_fill.to_opaque());
    egui::Popup::from_response(&button)
        .kind(egui::PopupKind::Menu)
        .layout(egui::Layout::top_down_justified(egui::Align::Min))
        .style(egui::containers::menu::menu_style)
        .align(egui::RectAlign::TOP_END)
        .align_alternatives(&[egui::RectAlign::TOP_START])
        .gap(6.0)
        .frame(frame)
        .open_bool(&mut open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
        .show(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(12.0, 0.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            rows(app, ui);
        });
    if open {
        app.menu = Some(which);
    } else if was_open {
        app.menu = None;
    }
}

/// The open note's bar: its audio to play and seek through, then the menus of what can be
/// done with the note. It lies under the page rather than in it, so all of it stays in
/// reach however long the text runs.
pub fn bar(app: &mut ScribeApp, ui: &mut Ui, host: &Host) {
    let Some(engine) = app.engine.clone() else { return };
    let Some(note) = app.detail.as_ref().and_then(|open| app.notes.iter().find(|note| note.id == open.id)).cloned() else { return };
    let id = note.id.clone();
    let busy = busy_with(app, &id);
    let queued = app.status.as_ref().is_some_and(|s| s.queue.iter().any(|q| *q == id));

    // While a menu is open, a tap anywhere else only closes it. egui would also hand the tap
    // to what lies under it, focusing the text and raising the keyboard, or playing a row.
    if app.menu.is_some() {
        let screen = ui.ctx().content_rect();
        egui::Area::new(egui::Id::unique("menu scrim")).order(egui::Order::Middle).fixed_pos(screen.min).show(ui.ctx(), |ui| {
            ui.allocate_rect(screen, egui::Sense::click());
        });
    }

    // Placed by hand: the seek bar is whatever the three buttons leave.
    let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), BUTTON), egui::Sense::hover());
    let gap = ui.spacing().item_spacing.x;
    let play = egui::Rect::from_min_size(bar.min, egui::Vec2::splat(BUTTON));
    let more = egui::Rect::from_min_size(egui::pos2(bar.right() - BUTTON, bar.top()), egui::Vec2::splat(BUTTON));
    let share = more.translate(egui::vec2(-(BUTTON + gap), 0.0));
    let seek = egui::Rect::from_min_max(egui::pos2(play.right() + gap, bar.top()), egui::pos2(share.left() - gap, bar.bottom()));
    player(app, ui, host, &note, busy, play, seek);

    menu(app, ui, Menu::Share, share, svg::SHARE_NETWORK, "Share", |app, ui| {
        let text = app.detail.as_ref().map(|d| d.body.clone()).unwrap_or_default();
        let has_text = !text.trim().is_empty();
        if menu_row(ui, has_text, svg::COPY, "Copy").clicked() {
            host.copy_text(text.clone());
            app.say("Copied.");
        }
        if menu_row(ui, has_text, svg::SHARE_NETWORK, "Share text").clicked() {
            host.share_text(text);
        }
        // A transcript is usually too long to paste; most apps take it as an attachment.
        if menu_row(ui, has_text, svg::FILE_TEXT, "Share file").clicked() {
            host.haptic(Haptic::Light);
            app.share_file(host, &id);
        }
        if menu_row(ui, !busy, svg::FILE_AUDIO, "Share audio").clicked() {
            crate::platform::share_audio(host, &engine.library.audio_path(&id), &format!("{id}.wav"));
            app.sharing = true;
        }
    });
    menu(app, ui, Menu::More, more, svg::DOTS_THREE_VERTICAL, "More", |app, ui| {
        let (icon, pin) = if note.pinned { (svg::PUSH_PIN_SLASH, "Unpin") } else { (svg::PUSH_PIN, "Pin") };
        if menu_row(ui, true, icon, pin).clicked() {
            engine.edit_note(&id, |note| note.pinned = !note.pinned);
        }
        if menu_row(ui, !busy && can_redo(&note, queued), svg::ARROWS_CLOCKWISE, "Transcribe again").clicked() {
            if note.segments.is_empty() && note.body.is_none() {
                // Nothing would be replaced, so there is nothing to confirm.
                host.haptic(Haptic::Light);
                engine.transcribe(&id, true);
            } else {
                app.confirm = Some(Confirm::Retranscribe(id.clone()));
            }
        }
        ui.separator();
        // In the colour of what it does, and otherwise a row like the others: tinted and
        // outlined as a button it reads as the one thing in the menu to press.
        let danger = role(ui, Role::Danger);
        let visuals = ui.visuals_mut();
        visuals.override_text_color = Some(danger);
        // The icon takes its colour from the widget, not from the text's override.
        for widget in [&mut visuals.widgets.inactive, &mut visuals.widgets.hovered, &mut visuals.widgets.active] {
            widget.fg_stroke.color = danger;
        }
        if menu_row(ui, !busy, svg::TRASH, "Delete").clicked() {
            app.confirm = Some(Confirm::DeleteNote(id.clone()));
        }
    });
}

/// Play, pause and seek through the note's audio: the button at `play`, the rest in `seek`.
fn player(app: &mut ScribeApp, ui: &mut Ui, host: &Host, note: &Note, busy: bool, play: egui::Rect, seek: egui::Rect) {
    let duration = note.duration_seconds().max(0.1);
    let loaded = app.player.note() == Some(note.id.as_str());
    let playing = loaded && app.player.is_playing();
    let position = if loaded { app.player.position().min(duration) } else { 0.0 };
    let (icon, label) = if playing { (svg::PAUSE, "Pause") } else { (svg::PLAY, "Play") };
    if square(ui, play, !busy, false, icon, label).clicked() {
        host.haptic(Haptic::Light);
        if playing {
            app.player.set_playing(false);
        } else {
            app.play_from(host, note, position);
        }
    }
    let mut seek_to = None;
    ui.scope_builder(egui::UiBuilder::new().max_rect(seek).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
        if busy {
            ui.add_space(5.0);
            wrapped(ui, RichText::new("Playback is available once the recording is saved.").size(12.0).color(ui.visuals().weak_text_color()));
            return;
        }
        let Some(detail) = app.detail.as_mut() else { return };
        // The rail over the clock, the two no taller than the buttons beside them.
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.spacing_mut().interact_size.y = 28.0;
        ui.spacing_mut().slider_width = ui.available_width();
        ui.add_space(1.0);
        let mut value = detail.scrub.unwrap_or(position);
        let slider = ui.add(egui::Slider::new(&mut value, 0.0..=duration).show_value(false));
        if slider.dragged() {
            detail.scrub = Some(value);
        } else if slider.drag_stopped() || slider.changed() {
            // One seek when the finger lifts: MediaPlayer blocks on each.
            detail.scrub = None;
            seek_to = Some(value);
        }
        let (clock, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), egui::Sense::hover());
        let (font, colour) = (egui::FontId::proportional(11.5), ui.visuals().weak_text_color());
        ui.painter().text(clock.left_center(), egui::Align2::LEFT_CENTER, clock::duration(detail.scrub.unwrap_or(position)), font.clone(), colour);
        ui.painter().text(clock.right_center(), egui::Align2::RIGHT_CENTER, clock::duration(duration), font, colour);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_runs_from_a_name_to_the_next_name() {
        let voices = ["Speaker 1".to_string(), "Zoë".to_string()];
        let text = "Before anyone.\n\nSpeaker 1: Hello there.\n\nStill me, after a pause.\n\nZoë: Hi.\nSpeaker 1 said hello: and so on.\n\n";
        let turns = turns_in(text, &voices);
        let letters: Vec<char> = text.chars().collect();
        let said: Vec<(usize, &str, String)> = turns.iter().map(|turn| (turn.speaker, &text[turn.label.clone()], letters[turn.chars.clone()].iter().collect())).collect();
        assert_eq!(said, [
            (0, "Speaker 1:", "Speaker 1: Hello there.\n\nStill me, after a pause.".to_string()),
            // A name in the middle of a line is only a word, and the blank lines after the last one are nobody's.
            (1, "Zoë:", "Zoë: Hi.\nSpeaker 1 said hello: and so on.".to_string()),
        ]);
        // With one voice, or none, nothing is labelled and nothing is looked for.
        assert!(turns_in(text, &[]).is_empty());
        assert!(turns_in("", &voices).is_empty());
    }

    #[test]
    fn a_line_goes_to_the_longest_name_that_opens_it() {
        let voices = ["Al".to_string(), "Al: B".to_string()];
        let turns = turns_in("Al: B: mine\nAl: yours", &voices);
        assert_eq!(turns.iter().map(|turn| turn.speaker).collect::<Vec<_>>(), [1, 0]);
    }

    #[test]
    fn the_text_takes_what_the_page_leaves_it_within_bounds() {
        // What is left, a point short of it.
        assert_eq!(share(600.0, 400.0), 399.0);
        // Never more than seven tenths of the page, nor less than a quarter.
        assert_eq!(share(600.0, 560.0), 420.0);
        assert_eq!(share(600.0, -100.0), 150.0);
        // A page with no height yet, as on a first frame, is not a panic.
        assert_eq!(share(0.0, -100.0), 0.0);
        assert_eq!(share(-8.0, -8.0), 0.0);
        assert_eq!(share(f32::NAN, 100.0), 0.0);
    }
}
