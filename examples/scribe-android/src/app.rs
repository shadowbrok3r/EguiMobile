//! The screen-side half of the app: one activity's view onto the process-wide [`Engine`].

use crate::engine::{Engine, Paths, Status, RATE};
use crate::library::{Note, Seg};
use crate::platform::{self, Player};
use crate::settings::Settings;
use crate::ui::notes::Detail;
use crate::ui::{self, Tab};
use egui::RichText;
use egui_mobile::theme::{Density, Role};
use egui_mobile::{CreateContext, EguiApp, Haptic, Host, Permission};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a note's row in the list shows and is searched by. Each is made from the note's whole
/// text, so they are worked out when the notes are read, not for every note on every frame.
pub struct Row {
    pub title: String,
    pub snippet: String,
    /// Title and text, lowercased.
    haystack: String,
}

impl Row {
    fn of(note: &Note) -> Self {
        let text = note.text();
        let title = note.title_over(&text);
        Self { snippet: crate::library::snippet_of(&text, 110), haystack: format!("{}\n{}", title.to_lowercase(), text.to_lowercase()), title }
    }

    /// Whether the row holds `query`, which is trimmed and lowercased already.
    pub fn holds(&self, query: &str) -> bool {
        query.is_empty() || self.haystack.contains(query)
    }
}

/// A question that needs a yes before something is thrown away.
pub enum Confirm {
    DeleteNote(String),
    RemovePack(String),
    Retranscribe(String),
}

pub struct ScribeApp {
    pub engine: Option<Arc<Engine>>,
    init_error: Option<String>,
    /// This frame's copy of the engine's settings.
    pub settings: Settings,
    pub tab: Tab,
    /// This frame's snapshot of the engine.
    pub status: Option<Status>,
    pub notes: Vec<Note>,
    /// One per note, in the same order.
    pub rows: Vec<Row>,
    notes_revision: u64,
    pub library_bytes: u64,
    pub search: String,
    pub detail: Option<Detail>,
    /// The live recording's transcript, refreshed when the engine says it changed.
    pub live_segments: Vec<Seg>,
    pub live_preview: Vec<Seg>,
    live_revision: Option<(String, u64)>,
    /// Recent microphone levels for the meter, oldest first.
    pub levels: VecDeque<f32>,
    level_samples: u64,
    /// The recording that was running last frame, to open its note when it ends.
    was_recording: Option<String>,
    pub player: Player,
    /// Installed model pack names.
    pub packs: Vec<String>,
    pub packs_stale: bool,
    packs_checked: Instant,
    pub confirm: Option<Confirm>,
    /// The menu open on the note's bar, if one is.
    pub menu: Option<ui::notes::Menu>,
    toast: Option<(String, Instant)>,
    /// Since when the microphone permission dialog has been up.
    pub awaiting_mic: Option<Instant>,
    pub mic_denied: bool,
    /// A share sheet was asked for; its outcome is still to be read.
    pub sharing: bool,
    pub overflow: Vec<String>,
    /// Open every collapsible section by default, so a layout test sees all of a screen.
    pub expand_all: bool,
    /// A screen asked to go to another tab; done after it has finished drawing.
    pub leave_to: Option<Tab>,
}

impl ScribeApp {
    pub fn new(_cc: &CreateContext) -> Self {
        Self {
            engine: None,
            init_error: None,
            settings: Settings::default(),
            tab: Tab::Notes,
            status: None,
            notes: Vec::new(),
            rows: Vec::new(),
            notes_revision: 0,
            library_bytes: 0,
            search: String::new(),
            detail: None,
            live_segments: Vec::new(),
            live_preview: Vec::new(),
            live_revision: None,
            levels: VecDeque::new(),
            level_samples: 0,
            was_recording: None,
            player: Player::default(),
            packs: Vec::new(),
            packs_stale: true,
            packs_checked: Instant::now(),
            confirm: None,
            menu: None,
            toast: None,
            awaiting_mic: None,
            mic_denied: false,
            sharing: false,
            overflow: Vec::new(),
            expand_all: false,
            leave_to: None,
        }
    }

    /// A screen built around an engine that already exists, for tests.
    pub fn with_engine(engine: Arc<Engine>) -> Self {
        let mut app = Self::new(&CreateContext { width_px: 0, height_px: 0, pixels_per_point: 1.0 });
        app.settings = engine.settings();
        app.engine = Some(engine);
        app
    }

    /// Where things live: notes and models on the app's shared-storage directory, which is
    /// roomy and reachable over USB, falling back to private storage if it cannot be made.
    fn paths(host: &Host) -> Result<Paths, String> {
        let documents = host.documents_dir().ok_or("Android has not given the app a storage directory yet.")?;
        let private = PathBuf::from(&documents);
        let shared = platform::shared_files_dir(&documents).filter(|dir| std::fs::create_dir_all(dir.join("notes")).is_ok());
        let documents = shared.as_deref().and_then(platform::public_documents);
        let root = shared.unwrap_or_else(|| private.clone());
        let native_libs = platform::native_lib_dir(host);
        let speaker_pack = platform::speaker_pack(&private, native_libs.as_deref());
        Ok(Paths { settings: private.join("settings.json"), library: root.join("notes"), models: root.join("whisper"), native_libs, documents, speaker_pack })
    }

    fn connect(&mut self, ctx: &egui::Context, host: &Host) {
        if self.engine.is_some() {
            return;
        }
        let paths = match Self::paths(host) {
            Ok(paths) => paths,
            Err(reason) => {
                self.init_error = Some(reason);
                return;
            }
        };
        match Engine::global(|| paths) {
            Ok(engine) => {
                engine.attach_ui(ctx.clone());
                self.settings = engine.settings();
                self.settings.theme.apply(ctx, Density::Touch);
                // A recording may already be running: this activity can be a replacement.
                if engine.status().recording.is_some() {
                    self.tab = Tab::Record;
                }
                self.engine = Some(engine);
                self.init_error = None;
            }
            Err(e) => self.init_error = Some(format!("Scribe could not open its storage: {e}")),
        }
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    pub fn open_note(&mut self, id: &str) {
        self.close_note();
        if let Some(note) = self.notes.iter().find(|n| n.id == id) {
            self.detail = Some(Detail::open(note));
            self.tab = Tab::Notes;
        }
    }

    pub fn close_note(&mut self) {
        if let (Some(mut detail), Some(engine)) = (self.detail.take(), self.engine.as_ref()) {
            detail.flush(engine);
        }
        self.player.close();
        self.menu = None;
    }

    /// Ask for the microphone if need be, then start.
    pub fn begin_recording(&mut self, host: &Host) {
        self.mic_denied = false;
        if platform::mic_permission() {
            self.start_recording(host);
        } else {
            host.request_permission(Permission::Microphone);
            self.awaiting_mic = Some(Instant::now());
        }
    }

    fn start_recording(&mut self, host: &Host) {
        let Some(engine) = self.engine.clone() else { return };
        self.player.set_playing(false);
        match engine.start_recording() {
            Ok(()) => {
                self.tab = Tab::Record;
                self.levels.clear();
                if !self.settings.notifications_asked {
                    // Without it the recording notification, and its Stop button, stay invisible.
                    platform::request_notifications(host);
                    engine.update_settings(|s| s.notifications_asked = true);
                }
            }
            Err(reason) => {
                host.haptic(Haptic::Error);
                self.say(reason);
            }
        }
    }

    fn poll_mic_permission(&mut self, host: &Host) {
        let Some(since) = self.awaiting_mic else { return };
        if platform::mic_permission() {
            self.awaiting_mic = None;
            self.start_recording(host);
        } else if host.permission(Permission::Microphone) == Some(false) || since.elapsed() > Duration::from_secs(90) {
            self.awaiting_mic = None;
            self.mic_denied = true;
        }
    }

    /// Hand a note's text to the share sheet as a file.
    pub fn share_file(&mut self, host: &Host, id: &str) {
        let Some(engine) = self.engine.clone() else { return };
        // What is on screen is what goes: an edit still waiting out its delay is written first.
        if let Some(detail) = self.detail.as_mut().filter(|detail| detail.id == id) {
            detail.flush(&engine);
        }
        let file = match engine.text_file(id) {
            Ok(file) => file,
            Err(reason) => {
                host.haptic(Haptic::Error);
                self.say(reason);
                return;
            }
        };
        if let (true, Some(exporter)) = (file.stored, engine.exporter.as_ref()) {
            match platform::share_stored(&exporter.label(), &file.name) {
                Ok(()) => return,
                // Android would not pass the file on as it is. A copy still can be.
                Err(reason) => engine.log.warn(format!("{} could not be shared where it is: {reason}", file.name)),
            }
        }
        platform::share_copy(host, &file.path, &file.name);
        self.sharing = true;
    }

    /// Start or move playback of `note` at `seconds`.
    pub fn play_from(&mut self, host: &Host, note: &Note, seconds: f64) {
        let Some(engine) = self.engine.as_ref() else { return };
        if self.player.note() != Some(note.id.as_str()) && !self.player.open(&note.id, &engine.library.audio_path(&note.id)) {
            host.haptic(Haptic::Error);
            self.say("The audio could not be opened.");
            return;
        }
        self.player.seek(seconds);
        self.player.set_playing(true);
    }

    /// Bring this frame's copies of the engine's state up to date.
    fn refresh(&mut self, ctx: &egui::Context) {
        let Some(engine) = self.engine.clone() else { return };
        let status = engine.status();
        self.settings = engine.settings();
        if status.revision != self.notes_revision {
            self.notes = engine.library.list();
            self.rows = self.notes.iter().map(Row::of).collect();
            self.notes_revision = status.revision;
            self.library_bytes = engine.library.total_bytes();
        }
        if self.packs_stale || self.packs_checked.elapsed() > Duration::from_secs(3) {
            self.packs = crate::models::installed(&engine.paths.models).iter().map(|pack| pack.name()).collect();
            self.packs_checked = Instant::now();
            self.packs_stale = false;
        }

        match &status.recording {
            Some(live) => {
                let key = (live.id.clone(), live.revision);
                if self.live_revision.as_ref() != Some(&key) {
                    (self.live_segments, self.live_preview) = engine.live_transcript();
                    self.live_revision = Some(key);
                }
                // One bar per tenth of a second of audio, whatever the frame rate.
                if live.samples >= self.level_samples + RATE as u64 / 10 || live.samples < self.level_samples {
                    self.level_samples = live.samples;
                    self.levels.push_back(live.level);
                    while self.levels.len() > 64 {
                        self.levels.pop_front();
                    }
                }
                self.was_recording = Some(live.id.clone());
            }
            None => {
                self.live_segments.clear();
                self.live_preview.clear();
                self.live_revision = None;
                self.level_samples = 0;
                // The recording just ended: show the note it became.
                if let Some(id) = self.was_recording.take() {
                    if self.notes.iter().any(|n| n.id == id) {
                        self.open_note(&id);
                    }
                }
            }
        }
        if let Some(notice) = engine.take_notice() {
            self.say(notice);
        }
        self.overflow.extend(egui_mobile::overflow::take_reports(ctx));
        let extra = self.overflow.len().saturating_sub(40);
        self.overflow.drain(..extra);
        self.status = Some(status);
    }

    fn header(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.horizontal(|ui| {
            let in_note = self.tab == Tab::Notes && self.detail.is_some();
            if in_note {
                if ui::icon_button(ui, egui_icon_pack::phosphor_regular::ARROW_LEFT, "Back to notes", 40.0).clicked() {
                    host.haptic(Haptic::Light);
                    self.close_note();
                }
            } else {
                ui.add_space(2.0);
                ui.label(RichText::new("Scribe").size(19.0).strong());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, colour) = self.chip();
                let chip = egui::Button::selectable(self.tab == Tab::Settings, RichText::new(text).size(12.0).color(ui::role(ui, colour))).truncate();
                if ui.add(chip).clicked() {
                    self.close_note();
                    self.tab = Tab::Settings;
                }
            });
        });
    }

    /// The model's state in two words, for the header.
    fn chip(&self) -> (String, Role) {
        use crate::engine::ModelStatus;
        let Some(status) = self.status.as_ref() else { return (String::new(), Role::Muted) };
        if let Some((_, done, total)) = &status.active {
            return (format!("• Transcribing {}%", (*done as f64 * 100.0 / (*total).max(1) as f64) as u32), Role::Accent);
        }
        if let Some((_, done, total)) = &status.voicing {
            return (format!("• Speakers {}%", (*done as f64 * 100.0 / (*total).max(1) as f64) as u32), Role::Accent);
        }
        match &status.model {
            ModelStatus::Ready(loaded) if loaded.simulated => ("• Simulated".into(), Role::Warning),
            ModelStatus::Ready(_) => ("• NPU ready".into(), Role::Success),
            ModelStatus::Loading(_) => ("• Loading model".into(), Role::Warning),
            ModelStatus::Failed(_) => ("• NPU unavailable".into(), Role::Danger),
            ModelStatus::Paused(_) => ("• Paused".into(), Role::Warning),
            ModelStatus::Missing => ("• No model".into(), Role::Warning),
            ModelStatus::Idle if self.packs.is_empty() && !self.settings.simulate => ("• No model".into(), Role::Warning),
            ModelStatus::Idle => ("• NPU idle".into(), Role::Muted),
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui, host: &Host) {
        let recording = self.status.as_ref().is_some_and(|s| s.recording.is_some());
        ui.horizontal(|ui| {
            let gaps = ui.spacing().item_spacing.x * (Tab::ALL.len() - 1) as f32;
            let width = (ui.available_width() - gaps) / Tab::ALL.len() as f32;
            for tab in Tab::ALL {
                let selected = self.tab == tab;
                let tint = if tab == Tab::Record && recording {
                    ui::role(ui, Role::Danger)
                } else if selected {
                    ui.visuals().strong_text_color()
                } else {
                    ui.visuals().weak_text_color()
                };
                let response = ui.add_sized([width, ui::TOUCH_HEIGHT], egui::Button::selectable(selected, tab.icon().image(22.0).tint(tint)));
                response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, ui.is_enabled(), selected, tab.label()));
                if response.clicked() && !selected {
                    self.close_note();
                    self.tab = tab;
                    host.haptic(Haptic::Selection);
                }
            }
        });
    }

    fn confirmation(&mut self, ctx: &egui::Context, host: &Host) {
        let Some(confirm) = self.confirm.as_ref() else { return };
        let (question, detail, yes) = match confirm {
            Confirm::DeleteNote(id) => {
                let mut detail = "The recording and its text are removed from the phone.".to_string();
                // Where its text has also been saved as a file, that copy is not part of this.
                let folder = self.engine.as_ref().and_then(|engine| engine.exporter.as_ref()).map(|exporter| exporter.label());
                if let Some(folder) = folder.filter(|_| self.notes.iter().any(|note| note.id == *id && note.export.is_some())) {
                    detail.push_str(&format!(" Its text file in {folder} stays."));
                }
                ("Delete this note?", detail, "Delete")
            }
            Confirm::RemovePack(_) => ("Remove this model?", "It can be downloaded again. Notes are not affected.".to_string(), "Remove"),
            Confirm::Retranscribe(_) => ("Transcribe again?", "The current text, including your edits, is replaced.".to_string(), "Transcribe"),
        };
        let mut decided = None;
        let modal = egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_width(ui.available_width().min(300.0));
            ui.label(RichText::new(question).size(17.0).strong());
            ui::weak(ui, detail);
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if ui::role_action(ui, Role::Danger, egui_icon_pack::phosphor_regular::CHECK, yes).clicked() {
                    decided = Some(true);
                }
                if ui::action(ui, egui_icon_pack::phosphor_regular::X, "Cancel").clicked() {
                    decided = Some(false);
                }
            });
        });
        if modal.should_close() {
            decided.get_or_insert(false);
        }
        let Some(yes) = decided else { return };
        let Some(confirm) = self.confirm.take() else { return };
        let Some(engine) = self.engine.clone() else { return };
        if !yes {
            return;
        }
        host.haptic(Haptic::Medium);
        match confirm {
            Confirm::DeleteNote(id) => {
                if self.detail.as_ref().is_some_and(|d| d.id == id) {
                    // Dropped, not flushed: a pending edit must not recreate the note's file.
                    self.detail = None;
                    self.player.close();
                }
                if let Err(reason) = engine.delete_note(&id) {
                    self.say(reason);
                }
            }
            Confirm::RemovePack(name) => {
                match engine.remove_pack(&name) {
                    Ok(()) => self.say("Model removed."),
                    Err(reason) => self.say(reason),
                }
                self.packs_stale = true;
            }
            Confirm::Retranscribe(id) => {
                if let Some(detail) = self.detail.as_mut().filter(|d| d.id == id) {
                    *detail = Detail::open(&Note { id: id.clone(), title: detail.title.clone(), ..Default::default() });
                }
                engine.transcribe(&id, true);
            }
        }
    }

    fn toast(&mut self, ctx: &egui::Context, host: &Host) {
        let Some((text, since)) = self.toast.as_ref() else { return };
        if since.elapsed() > Duration::from_secs(4) {
            self.toast = None;
            return;
        }
        let insets = host.safe_area_insets();
        // Clear of an open note's bar too: Copy is on it, and its toast would cover the player.
        let bar = if self.tab == Tab::Notes && self.detail.is_some() { ui::notes::BAR } else { 0.0 };
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -(insets.bottom + ui::TOUCH_HEIGHT + 24.0 + bar)))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                // Opaque whatever the theme's glass setting: it sits on top of other text.
                egui::Frame::popup(ui.style()).fill(ui.visuals().window_fill.to_opaque()).show(ui, |ui| {
                    ui.set_max_width((ctx.content_rect().width() - 48.0).max(120.0));
                    ui::wrapped(ui, RichText::new(text).size(14.0));
                });
            });
    }

    /// How soon the next frame is needed with nothing touching the screen.
    fn next_frame(&self) -> Duration {
        let Some(status) = self.status.as_ref() else { return Duration::from_millis(500) };
        if status.recording.is_some() || self.awaiting_mic.is_some() || self.player.is_playing() {
            Duration::from_millis(80)
        } else if status.active.is_some() || status.voicing.is_some() || status.install.is_some() || self.toast.is_some() || self.detail.is_some() {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(2)
        }
    }

    pub fn screen(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.connect(ui.ctx(), host);
        self.refresh(ui.ctx());
        self.poll_mic_permission(host);
        if self.sharing {
            // The outcome arrives a frame or more after the request; a failure is the only
            // one worth interrupting for.
            if let Some(reason) = platform::share_error(host) {
                self.sharing = false;
                self.say(format!("Could not share: {reason}"));
            }
        }

        // As the frame began: a menu takes Back's key itself and is closed by the time the
        // press is looked at below, where it would then go on to close the note as well.
        let menu_open = self.menu.is_some();

        egui::Panel::top("header").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 4))).show(ui, |ui| self.header(ui, host));

        // Hidden while typing: focus leads the keyboard in and its inset trails it out.
        let mut tabs_open = !(host.keyboard_height() > 1.0 || ui.ctx().text_edit_focused());
        egui::Panel::bottom("tabs")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 4)))
            .drag_to_open(false)
            .show_collapsible(ui, &mut tabs_open, |ui| self.tab_bar(ui, host));

        // An open note's player and menus ride on the tab bar, and stay when the keyboard
        // takes that away: they are the one part of a note that is always in reach.
        if self.tab == Tab::Notes && self.detail.is_some() && self.engine.is_some() {
            egui::Panel::bottom("note bar").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6))).show(ui, |ui| ui::notes::bar(self, ui, host));
        }

        egui::CentralPanel::default().frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 6))).show(ui, |ui| {
            if let Some(error) = self.init_error.clone() {
                ui::card(ui, |ui| {
                    ui::wrapped(ui, RichText::new(error).color(ui::role(ui, Role::Danger)));
                });
                return;
            }
            if self.engine.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.spinner();
                });
                return;
            }
            match self.tab {
                Tab::Notes if self.detail.is_some() => ui::notes::detail(self, ui, host),
                Tab::Notes => ui::notes::list(self, ui, host),
                // The recorder lays its own scroll area under a fixed header.
                Tab::Record => ui::record::show(self, ui, host),
                Tab::Settings => {
                    egui::ScrollArea::vertical().id_salt("settings").auto_shrink([false, false]).show(ui, |ui| ui::settings::show(self, ui, host));
                }
            }
        });

        if let Some(tab) = self.leave_to.take() {
            self.close_note();
            self.tab = tab;
        }
        self.confirmation(ui.ctx(), host);
        self.toast(ui.ctx(), host);

        // Back unwinds one layer per press, innermost first. Checked last so a layer opened
        // this frame is not closed by the press that opened it.
        if ui::back_pressed(ui.ctx()) {
            if self.confirm.is_some() {
                self.confirm = None;
            } else if menu_open {
                self.menu = None;
            } else if self.detail.is_some() {
                self.close_note();
            } else if self.tab != Tab::Notes {
                self.tab = Tab::Notes;
            }
            host.haptic(Haptic::Light);
        }
        ui.ctx().request_repaint_after(self.next_frame());
    }
}

impl EguiApp for ScribeApp {
    fn theme(&self, ctx: &egui::Context) {
        egui_icon_pack::install(ctx);
        self.settings.theme.apply(ctx, Density::Touch);
    }

    fn on_start(&mut self, ctx: &egui::Context, host: &Host) {
        self.connect(ctx, host);
    }

    fn on_pause(&mut self, _host: &Host) {
        // Edits are written now: a paused activity may never get another frame.
        if let (Some(detail), Some(engine)) = (self.detail.as_mut(), self.engine.as_ref()) {
            detail.flush(engine);
        }
        self.player.set_playing(false);
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.screen(ui, host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Paths;
    use crate::library::NoteState;
    use egui_mobile::overflow;

    /// A phone-width screen in points: an S26 Ultra is about 411 wide, a small phone 360.
    const WIDTHS: [f32; 2] = [411.0, 360.0];

    fn app(name: &str) -> (ScribeApp, egui::Context, Arc<Engine>) {
        app_after(name, |_| {})
    }

    /// An app whose storage `seed` has had a go at before the engine opens it.
    fn app_after(name: &str, seed: impl FnOnce(&std::path::Path)) -> (ScribeApp, egui::Context, Arc<Engine>) {
        let root = std::env::temp_dir().join(format!("scribe-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        seed(&root);
        let mut settings = Settings::default();
        settings.simulate = true;
        settings.save(&root.join("settings.json")).unwrap();
        let engine = Engine::start(Paths { settings: root.join("settings.json"), library: root.join("notes"), models: root.join("whisper"), native_libs: None, documents: Some(root.join("Documents").join("Scribe")), speaker_pack: None }).unwrap();
        let ctx = egui::Context::default();
        overflow::install_with(&ctx, overflow::Policy::Report);
        let mut app = ScribeApp::with_engine(engine.clone());
        app.expand_all = true;
        app.theme(&ctx);
        (app, ctx, engine)
    }

    /// Lay the current screen out at each phone width and fail on anything clipped.
    fn lays_out(app: &mut ScribeApp, ctx: &egui::Context, what: &str) {
        let host = Host::new();
        for width in WIDTHS {
            // A few passes: sizes settle over the first frames, and the guard reads the last.
            for _ in 0..4 {
                let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(width, 860.0))), ..Default::default() };
                ctx.run_ui(input, |ui| app.screen(ui, &host)).textures_delta.clear();
                let _ = overflow::take_reports(ctx);
            }
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(width, 860.0))), ..Default::default() };
            ctx.run_ui(input, |ui| app.screen(ui, &host)).textures_delta.clear();
            let reports = overflow::take_reports(ctx);
            assert!(reports.is_empty(), "{what} at {width} pt: {reports:#?}");
        }
    }

    fn record(engine: &Engine, seconds: usize) {
        let mut sink = engine.prepare_recording().unwrap();
        let pcm: Vec<i16> = (0..seconds * RATE as usize).map(|i| ((i as f32 * 0.05).sin() * 9_000.0) as i16).collect();
        for chunk in pcm.chunks(1_600) {
            sink(chunk, RATE);
        }
    }

    fn settle(engine: &Engine) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = engine.status();
            if status.idle() {
                return;
            }
            assert!(Instant::now() < deadline, "engine never settled: {status:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn rows_carry_what_the_list_shows_and_searches() {
        let spoken = Seg { start: 0.0, end: 2.0, text: "Ship the Recorder on Friday.".into(), speaker: None };
        let note = Note { title: " Standup ".into(), segments: vec![spoken], ..Default::default() };
        let row = Row::of(&note);
        assert_eq!((row.title.as_str(), row.snippet.as_str()), ("Standup", "Ship the Recorder on Friday."));
        assert!(row.holds("") && row.holds("recorder") && row.holds("standup") && !row.holds("saturday"));
        // The same cut the note itself would make.
        let long = Note { segments: vec![Seg { start: 0.0, end: 9.0, text: "word ".repeat(60), speaker: None }], ..Default::default() };
        assert_eq!(Row::of(&long).snippet, long.snippet(110));
        assert!(Row::of(&long).snippet.ends_with('…'));
    }

    #[test]
    fn every_screen_fits_a_phone() {
        let (mut app, ctx, engine) = app("screens");
        lays_out(&mut app, &ctx, "empty notes list");
        app.tab = Tab::Record;
        lays_out(&mut app, &ctx, "idle recorder");
        app.mic_denied = true;
        lays_out(&mut app, &ctx, "recorder after a refused permission");
        app.mic_denied = false;
        app.tab = Tab::Settings;
        lays_out(&mut app, &ctx, "settings with no model");

        // A recording in progress, long enough to have live text.
        record(&engine, 40);
        let deadline = Instant::now() + Duration::from_secs(20);
        while engine.status().recording.is_none_or(|r| r.revision == 0) {
            assert!(Instant::now() < deadline, "no live window");
            std::thread::sleep(Duration::from_millis(20));
        }
        app.tab = Tab::Record;
        lays_out(&mut app, &ctx, "recorder with a live transcript");
        assert!(!app.live_segments.is_empty());
        assert!(!app.levels.is_empty());
        engine.set_paused(true);
        lays_out(&mut app, &ctx, "paused recorder");
        app.tab = Tab::Notes;
        lays_out(&mut app, &ctx, "notes list during a recording");

        engine.stop_recording();
        engine.finalize_recording();
        settle(&engine);
        // The finished recording opens as a note by itself.
        lays_out(&mut app, &ctx, "note that just finished");
        let detail = app.detail.as_ref().expect("the new note opens");
        assert!(detail.body.contains("simulated transcript"));
        assert_eq!(app.tab, Tab::Notes);

        app.close_note();
        lays_out(&mut app, &ctx, "notes list with a note");
        app.search = "nothing like this".into();
        lays_out(&mut app, &ctx, "search with no matches");
        app.search.clear();
        app.tab = Tab::Settings;
        lays_out(&mut app, &ctx, "settings after transcribing");
        assert_eq!(app.chip().0, "• Simulated");
    }

    #[test]
    fn note_states_and_dialogs_fit_too() {
        let (mut app, ctx, engine) = app("states");
        engine.update_settings(|s| s.auto_transcribe = false);
        record(&engine, 6);
        lays_out(&mut app, &ctx, "recorder with transcription off");
        engine.stop_recording();
        engine.finalize_recording();
        lays_out(&mut app, &ctx, "note that was only recorded");
        let id = app.detail.as_ref().expect("the new note opens").id.clone();
        assert_eq!(app.notes[0].state, NoteState::Pending);

        // A failure with a long reason, and a title long enough to need cutting.
        engine.edit_note(&id, |note| {
            note.state = NoteState::Failed;
            note.error = Some("The audio is 44100 Hz; only 16 kHz recordings can be transcribed, and this sentence keeps going to be sure it wraps.".into());
            note.title = "A deliberately long title that could not possibly fit on one line of a phone".into();
            note.pinned = true;
        });
        lays_out(&mut app, &ctx, "failed note");
        app.confirm = Some(Confirm::DeleteNote(id.clone()));
        lays_out(&mut app, &ctx, "delete confirmation");
        app.confirm = Some(Confirm::Retranscribe(id.clone()));
        lays_out(&mut app, &ctx, "retranscribe confirmation");
        app.confirm = None;
        app.say("Something worth saying at a length that has to wrap across the toast.");
        lays_out(&mut app, &ctx, "toast");
        app.close_note();
        lays_out(&mut app, &ctx, "list with a pinned, failed, long-titled note");

        engine.transcribe(&id, true);
        settle(&engine);
        app.open_note(&id);
        lays_out(&mut app, &ctx, "transcribed note with its timeline");
        assert!(app.detail.as_ref().unwrap().body.contains("simulated transcript"));
    }

    #[test]
    fn a_start_held_back_by_the_crash_guard_says_so_everywhere() {
        let (mut app, ctx, engine) = app_after("paused", |root| {
            std::fs::write(root.join("engine-guard.json"), r#"{"busy":"loading whisper-large-v3-turbo","strikes":1}"#).unwrap();
        });
        lays_out(&mut app, &ctx, "empty list, paused");
        assert_eq!(app.chip().0, "• Paused");
        app.tab = Tab::Settings;
        lays_out(&mut app, &ctx, "settings, paused");
        app.tab = Tab::Record;
        record(&engine, 5);
        lays_out(&mut app, &ctx, "recorder, paused");
        engine.stop_recording();
        engine.finalize_recording();
        lays_out(&mut app, &ctx, "note waiting on a paused engine");
        let id = app.detail.as_ref().expect("the new note opens").id.clone();
        assert_eq!(engine.status().queue, [id.clone()]);
        assert_eq!(app.notes[0].state, NoteState::Pending);

        // The note's own "Try again".
        engine.transcribe(&id, false);
        settle(&engine);
        lays_out(&mut app, &ctx, "note after trying again");
        assert_eq!(engine.library.load(&id).unwrap().state, NoteState::Done);
        assert_eq!(app.chip().0, "• Simulated");
    }

    #[test]
    fn an_open_note_follows_its_transcript_window_by_window() {
        let (mut app, ctx, engine) = app("follow");
        engine.update_settings(|s| s.auto_transcribe = false);
        record(&engine, 70);
        lays_out(&mut app, &ctx, "recorder");
        engine.stop_recording();
        engine.finalize_recording();
        lays_out(&mut app, &ctx, "untranscribed note");
        let id = app.detail.as_ref().expect("the new note opens").id.clone();
        assert!(app.detail.as_ref().unwrap().body.is_empty());

        // Three windows' worth: the screen has to pick up each as it lands, not just the first.
        engine.transcribe(&id, false);
        let mut lengths = vec![0];
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            lays_out(&mut app, &ctx, "note while its transcript arrives");
            let shown = app.detail.as_ref().unwrap().body.len();
            if shown > *lengths.last().unwrap() {
                lengths.push(shown);
            }
            let status = engine.status();
            if status.queue.is_empty() && status.active.is_none() {
                break;
            }
            assert!(Instant::now() < deadline, "transcription never finished");
        }
        lays_out(&mut app, &ctx, "finished note");
        let saved = engine.library.load(&id).unwrap();
        assert_eq!(saved.state, NoteState::Done);
        assert_eq!(app.detail.as_ref().unwrap().body, saved.text());
        assert!(saved.segments.len() >= 10, "{} segments", saved.segments.len());
    }

    #[test]
    fn a_conversation_shows_its_speakers_and_takes_their_names() {
        let (mut app, ctx, engine) = app("speakers");
        // Two tones taking turns of six seconds: two voices, to the scripted speaker network.
        let mut sink = engine.prepare_recording().unwrap();
        let pcm: Vec<i16> = (0..36 * RATE as usize).map(|i| ((i as f32 * if (i / (6 * RATE as usize)) % 2 == 0 { 0.05 } else { 0.2 }).sin() * 9_000.0) as i16).collect();
        for chunk in pcm.chunks(1_600) {
            sink(chunk, RATE);
        }
        lays_out(&mut app, &ctx, "recorder");
        engine.stop_recording();
        engine.finalize_recording();
        settle(&engine);
        lays_out(&mut app, &ctx, "conversation with its speakers section open");
        let id = app.detail.as_ref().expect("the new note opens").id.clone();
        let detail = app.detail.as_ref().unwrap();
        assert!(detail.body.starts_with("Speaker 1: ") && detail.body.contains("\n\nSpeaker 2: "), "{}", detail.body);
        assert_eq!(detail.names, ["", ""]);
        assert_eq!(app.notes[0].display_title(), "This is a simulated transcript. No speech…");

        // A name typed into the section reaches the text on screen, the note and its file.
        let detail = app.detail.as_mut().unwrap();
        detail.names[1] = "A rather long name for a speaker, to see it fit".into();
        detail.mark_edited();
        app.close_note();
        app.open_note(&id);
        lays_out(&mut app, &ctx, "conversation with a long name");
        detail_named(&app, "A rather long name for a speaker, to see it fit: ");
        app.close_note();
        let saved = engine.library.load(&id).unwrap();
        assert_eq!(saved.body, None, "naming a speaker is not an edit of the text");
        assert_eq!(saved.speakers.as_ref().map(|s| s.name(1)), Some("A rather long name for a speaker, to see it fit".into()));
        let dir = engine.exporter.as_ref().unwrap().dir().to_path_buf();
        assert_eq!(std::fs::read_to_string(dir.join(&saved.export.as_ref().unwrap().file)).unwrap(), format!("{}\n", saved.text()));

        // Setting the count by hand is immediate, and one voice takes the labels away.
        engine.find_speakers(&id, Some(1));
        app.open_note(&id);
        lays_out(&mut app, &ctx, "conversation set to one voice");
        assert!(app.detail.as_ref().unwrap().names.is_empty() && !app.detail.as_ref().unwrap().body.contains("Speaker"));
        engine.find_speakers(&id, Some(2));
        lays_out(&mut app, &ctx, "conversation set to two voices");
        assert_eq!(app.detail.as_ref().unwrap().names.len(), 2);
        app.close_note();
        lays_out(&mut app, &ctx, "list with a conversation");
        app.tab = Tab::Settings;
        lays_out(&mut app, &ctx, "settings with speakers on");
        assert_eq!(app.chip().0, "• Simulated");
    }

    /// A finished note made long: `parts` segments, three voices taking turns in them.
    fn long_note(engine: &Engine, parts: usize) -> String {
        record(engine, 6);
        engine.stop_recording();
        engine.finalize_recording();
        settle(engine);
        let id = engine.library.list()[0].id.clone();
        engine.edit_note(&id, |note| {
            let part = |i: usize| Seg { start: i as f32 * 4.0, end: i as f32 * 4.0 + 3.5, text: format!("Part {i} of a long conversation, with enough words in it to wrap on a phone."), speaker: Some((i / 3 % 3) as u8) };
            note.segments = (0..parts).map(part).collect();
            note.speakers = Some(crate::library::Speakers { count: 3, fixed: false, names: vec![String::new(), "Bo".into()] });
        });
        id
    }

    #[test]
    fn a_long_note_keeps_its_text_to_a_share_of_the_page() {
        let (mut app, ctx, engine) = app("long");
        let id = long_note(&engine, 600);
        // As a phone shows it: only the text's section open.
        app.expand_all = false;
        lays_out(&mut app, &ctx, "list with a long note");
        app.open_note(&id);
        lays_out(&mut app, &ctx, "long note");
        let detail = app.detail.as_ref().unwrap();
        assert!(detail.body.len() > 40_000 && detail.body.contains("\n\nBo: Part 3 "), "{}", &detail.body[..200]);
        let (card, page, room) = detail.laid_out();
        assert!(room > 500.0, "{room}");
        assert!(card <= room * 0.7 && card >= room * 0.55, "the text takes {card} of {room}");
        // Nothing is left over to scroll the page by: the headings and the bar stay put.
        assert!(page <= room && page > room - 4.0, "the page is {page} in {room}");

        // The menus open over it, and stay open until something is chosen.
        for menu in [ui::notes::Menu::Share, ui::notes::Menu::More] {
            app.menu = Some(menu);
            lays_out(&mut app, &ctx, "long note with a menu open");
            assert_eq!(app.menu, Some(menu));
        }
        app.menu = None;

        // With the timeline unfolded under it the text gives up room, and the two still
        // share one page between them.
        let mut timeline = egui::collapsing_header::CollapsingState::load_with_default_open(&ctx, egui::Id::unique(("section", "timeline")), false);
        timeline.set_open(true);
        timeline.store(&ctx);
        lays_out(&mut app, &ctx, "long note with its timeline open");
        let (card, page, room) = app.detail.as_ref().unwrap().laid_out();
        assert!(card >= (room * 0.25).floor() && card < room * 0.5, "the text takes {card} of {room}");
        assert!(page <= room && page > room - 4.0, "the page is {page} in {room}");
        // Evenly: neither is left the scraps of the other.
        let timeline = app.detail.as_ref().unwrap().timeline_tall();
        assert!((card - timeline).abs() <= 2.0, "text {card}, timeline {timeline}");

        // Leaving the note closes whichever menu was open.
        app.menu = Some(ui::notes::Menu::Share);
        app.close_note();
        assert_eq!(app.menu, None);
        lays_out(&mut app, &ctx, "list after a long note");
    }

    #[test]
    fn with_every_section_open_the_text_gives_way_and_the_timeline_stays_short() {
        let (mut app, ctx, engine) = app("crowded");
        let id = long_note(&engine, 600);
        lays_out(&mut app, &ctx, "list with a long note");
        app.open_note(&id);
        lays_out(&mut app, &ctx, "long note with every section open");
        let detail = app.detail.as_ref().unwrap();
        let (card, page, room) = detail.laid_out();
        // Down to its floor, and no further.
        assert!((card - (room * 0.25).floor()).abs() <= 1.0, "the text takes {card} of {room}");
        // Every row has a height, and all but the few in view are stepped over: laid out
        // in full the timeline alone would be dozens of pages.
        let (rows, tall) = detail.timeline_rows();
        assert_eq!(rows, 600);
        assert!(tall > room * 20.0, "{tall}");
        assert!(page > room && page < room * 2.0, "the page is {page} in {room}");

        // Fewer segments, as after transcribing again, leave no rows behind.
        engine.edit_note(&id, |note| note.segments.truncate(4));
        lays_out(&mut app, &ctx, "the same note, short");
        assert_eq!(app.detail.as_ref().unwrap().timeline_rows().0, 4);
    }

    #[test]
    fn back_closes_a_menu_before_it_closes_the_note() {
        let (mut app, ctx, engine) = app("back");
        let id = long_note(&engine, 12);
        lays_out(&mut app, &ctx, "list");
        app.open_note(&id);
        app.menu = Some(ui::notes::Menu::More);
        lays_out(&mut app, &ctx, "note with a menu open");
        let host = Host::new();
        let back = |app: &mut ScribeApp| {
            let press = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(411.0, 860.0))), events: vec![press], ..Default::default() };
            ctx.run_ui(input, |ui| app.screen(ui, &host)).textures_delta.clear();
        };
        back(&mut app);
        assert!(app.menu.is_none() && app.detail.is_some());
        back(&mut app);
        assert!(app.detail.is_none());
    }

    /// The open note's text carries `label` at the start of a paragraph.
    fn detail_named(app: &ScribeApp, label: &str) {
        let body = &app.detail.as_ref().expect("a note is open").body;
        assert!(body.contains(&format!("\n\n{label}")), "{body}");
    }

    #[test]
    fn an_edit_is_saved_and_survives_reopening() {
        let (mut app, ctx, engine) = app("edit");
        record(&engine, 6);
        lays_out(&mut app, &ctx, "recorder");
        engine.stop_recording();
        engine.finalize_recording();
        settle(&engine);
        lays_out(&mut app, &ctx, "new note");
        let id = app.detail.as_ref().expect("the new note opens").id.clone();

        // What typing does, without a keyboard to do it.
        let detail = app.detail.as_mut().unwrap();
        detail.body = "My own words".into();
        detail.title = "Renamed".into();
        detail.mark_edited();
        app.close_note();
        let saved = engine.library.load(&id).unwrap();
        assert_eq!((saved.title.as_str(), saved.body.as_deref()), ("Renamed", Some("My own words")));
        // The machine transcript is still there underneath the edit.
        assert!(saved.transcript().contains("simulated"));

        // Leaving the note is what brings its text file up to date.
        let dir = engine.exporter.as_ref().unwrap().dir().to_path_buf();
        let files: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with(" Renamed.txt"), "{files:?}");
        assert_eq!(std::fs::read_to_string(dir.join(&files[0])).unwrap(), "My own words\n");

        lays_out(&mut app, &ctx, "list after an edit");
        app.open_note(&id);
        assert_eq!(app.detail.as_ref().unwrap().body, "My own words");
        assert_eq!(app.notes[0].display_title(), "Renamed");
        lays_out(&mut app, &ctx, "note with a text file");
        // Shared as a file, it is the note's own file that goes, with the edit in it.
        let host = Host::new();
        app.detail.as_mut().unwrap().body = "My own words, and more".into();
        app.detail.as_mut().unwrap().mark_edited();
        app.share_file(&host, &id);
        assert_eq!(std::fs::read_to_string(dir.join(&files[0])).unwrap(), "My own words, and more\n");
        assert!(app.toast.is_none());
        app.confirm = Some(Confirm::DeleteNote(id.clone()));
        lays_out(&mut app, &ctx, "delete confirmation naming the text file");
        app.confirm = None;
        app.tab = Tab::Settings;
        lays_out(&mut app, &ctx, "settings with text files on");
        assert!(app.settings.save_text);
    }
}
