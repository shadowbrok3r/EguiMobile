//! The part of the app that outlives its screen: the recording in progress, the transcription
//! queue, and the model.
//!
//! Android rebuilds the activity — and with it the whole UI struct — whenever it likes, while
//! the recording service keeps this process alive. So everything that must not be interrupted
//! lives here, in one process-wide [`Engine`], and each UI that comes along only looks at it.
//!
//! One worker thread owns the NPU. It transcribes the live recording a window at a time as the
//! audio arrives, then the queue of saved recordings, then tells the speakers of what it has
//! finished apart, and lets the model go after a few idle minutes. Capture never waits on it:
//! samples go from the service's thread straight to disk, so a slow or failed transcription
//! costs text, never audio.

use crate::export::Exporter;
use crate::guard::Guard;
use crate::library::{Library, Note, NoteState, Seg};
use crate::models::{self, ModelSpec};
use crate::platform::{self, MicEvent, MicSink};
use crate::resample::Resampler;
use crate::settings::Settings;
use crate::sim::{SimEmbedder, SimEngine};
use crate::wav::{WavReader, WavWriter};
use crate::{clock, npu};
use local_speaker::{Embedder, Prints, QnnEmbedder, Span, SpeakerPack, Window, DIM, WINDOW_SAMPLES};
use local_whisper::{Advance, BurstVote, DecodeOptions, Engine as Whisper, Error as WhisperError, LongForm, QnnEngine, QnnSystem, Segment, Session, Timings, CHUNK_SAMPLES};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// The rate everything is stored and transcribed at.
pub const RATE: u32 = 16_000;
/// How long the model stays resident with nothing to do.
const IDLE_UNLOAD: Duration = Duration::from_secs(180);
/// Spacing of the rough live transcript while a window fills.
const PREVIEW_EVERY: Duration = Duration::from_secs(6);
/// How long a stop waits for the service to confirm capture has ended.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// How long the NPU stays voted to full power after the last run, in case another follows.
const POWER_LINGER: Duration = Duration::from_secs(3);
/// How long one turn of the speaker pass runs before the worker looks at the recording again.
const VOICE_SLICE: Duration = Duration::from_millis(300);
const LOG_LINES: usize = 400;
/// What the screen says when the crash guard has stopped transcription starting by itself.
const PAUSED: &str = "Transcription is paused: the app closed twice in a row while the NPU was working. Your recordings are safe. Transcribe a note, or run the self-test, to try again.";

/// The clip the self-test transcribes: eleven seconds of a well-known speech.
const SELFTEST_PCM: &[u8] = include_bytes!("../../../crates/local-whisper/tests/fixtures/jfk.s16");
const SELFTEST_PHRASE: &str = "ask not what your country can do for you";
/// The speaker network's answer for the clip's first two seconds, from the fp32 weights.
const SELFTEST_PRINT: &[u8] = include_bytes!("../../../crates/local-speaker/tests/fixtures/jfk_print.f32");
/// What the screen says when the crash guard has stopped speaker passes starting by themselves.
const VOICES_PAUSED: &str = "Speaker labelling is paused: the app closed while telling speakers apart. Use Speakers on a note to try again.";

pub struct Paths {
    pub settings: PathBuf,
    pub library: PathBuf,
    pub models: PathBuf,
    /// Where the APK's bundled `.so` files were unpacked; `None` off Android.
    pub native_libs: Option<PathBuf>,
    /// The folder finished notes are also kept in as text files; `None` where the phone's
    /// shared storage could not be reached.
    pub documents: Option<PathBuf>,
    /// The speaker network's pack, unpacked from the APK; `None` where there is none.
    pub speaker_pack: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Loaded {
    pub name: String,
    pub summary: String,
    pub provenance: String,
    pub load_ms: u64,
    pub simulated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelStatus {
    /// Not loaded, and nothing has needed it.
    Idle,
    /// No pack is installed.
    Missing,
    Loading(String),
    Ready(Loaded),
    Failed(String),
    /// Held back by the crash guard until someone asks for it; see [`crate::guard`].
    Paused(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub windows: u64,
    pub audio_seconds: f64,
    pub work_seconds: f64,
    pub last_encode_ms: f32,
    pub last_ms_per_token: f32,
    pub last_tokens: u32,
}

impl Stats {
    /// Seconds of audio transcribed per second of work.
    pub fn speed(&self) -> f64 {
        if self.work_seconds > 0.0 { self.audio_seconds / self.work_seconds } else { 0.0 }
    }

    fn add(&mut self, audio_samples: u64, timings: &Timings) {
        // A window too quiet to decode cost no work, and says nothing about speed.
        if timings.steps == 0 {
            return;
        }
        self.windows += 1;
        self.audio_seconds += audio_samples as f64 / RATE as f64;
        self.work_seconds += (timings.encode + timings.decode).as_secs_f64();
        self.last_encode_ms = timings.encode.as_secs_f32() * 1000.0;
        self.last_ms_per_token = timings.ms_per_step();
        self.last_tokens = timings.steps;
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum SelfTest {
    #[default]
    Idle,
    Requested,
    Running,
    Finished {
        text: String,
        detail: String,
        /// Whether the text is the clip's.
        matched: bool,
    },
    Failed(String),
}

/// A bounded in-app log, mirrored to logcat.
#[derive(Default)]
pub struct Log {
    lines: Mutex<VecDeque<String>>,
}

impl Log {
    fn push(&self, line: String) {
        let stamp = clock::local(clock::now());
        let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() >= LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(format!("{:02}:{:02}:{:02} {line}", stamp.hour, stamp.minute, stamp.second));
    }

    pub fn info(&self, line: impl Into<String>) {
        let line = line.into();
        log::info!("{line}");
        self.push(line);
    }

    pub fn warn(&self, line: impl Into<String>) {
        let line = line.into();
        log::warn!("{line}");
        self.push(format!("! {line}"));
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
    }
}

/// What the capture thread does with each piece of audio: convert it to 16 kHz if the device
/// recorded at something else, append it to the WAV, and publish a level.
pub struct Recorder {
    wav: WavWriter,
    resampler: Option<(u32, Resampler)>,
    scratch: Vec<i16>,
    samples: Arc<AtomicU64>,
    level: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    /// The first write that failed; a full disk ends up here.
    pub error: Option<String>,
}

impl Recorder {
    pub fn new(wav: WavWriter, samples: Arc<AtomicU64>, level: Arc<AtomicU32>, paused: Arc<AtomicBool>) -> Self {
        Self { wav, resampler: None, scratch: Vec::new(), samples, level, paused, error: None }
    }

    pub fn push(&mut self, pcm: &[i16], rate: u32) {
        if self.paused.load(Ordering::Relaxed) {
            self.level.store(0f32.to_bits(), Ordering::Relaxed);
            return;
        }
        let pcm = if Resampler::needed(rate, RATE) {
            if self.resampler.as_ref().is_none_or(|(from, _)| *from != rate) {
                self.resampler = Some((rate, Resampler::new(rate, RATE)));
            }
            self.scratch.clear();
            if let Some((_, resampler)) = self.resampler.as_mut() {
                resampler.process(pcm, &mut self.scratch);
            }
            &self.scratch[..]
        } else {
            pcm
        };
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0;
        self.level.store(peak.to_bits(), Ordering::Relaxed);
        if let Err(e) = self.wav.write(pcm) {
            self.error.get_or_insert_with(|| e.to_string());
        }
        self.samples.store(self.wav.samples(), Ordering::Relaxed);
    }

    pub fn finish(self) -> std::io::Result<u64> {
        self.wav.finish()
    }
}

/// The NPU's full-power vote, wanted only while it is actually running something.
trait Power {
    fn set(&mut self, on: bool);
}

/// The scripted engine draws no power worth managing.
struct NoPower;

impl Power for NoPower {
    fn set(&mut self, _on: bool) {}
}

/// Holds the HTP's burst vote while on. Without the vote long graphs outrun FastRPC's timeout
/// on this chip; with it held for a whole two-hour recording the DSP never rests.
struct Burst<'s> {
    session: &'s Session<'s>,
    vote: Option<BurstVote<'s>>,
    log: &'s Log,
    warned: bool,
}

impl Power for Burst<'_> {
    fn set(&mut self, on: bool) {
        if !on {
            self.vote = None;
        } else if self.vote.is_none() {
            match self.session.burst() {
                Ok(vote) => self.vote = Some(vote),
                Err(e) if !self.warned => {
                    self.warned = true;
                    self.log.warn(format!("burst mode unavailable: {e}"));
                }
                Err(_) => {}
            }
        }
    }
}

/// Where a session gets its speaker network: brought up the first time a recording needs
/// it, since most sessions end without telling anyone apart.
trait Voices {
    fn embedder(&mut self, guard: &Guard, log: &Log) -> Result<&mut dyn Embedder, String>;
}

/// The scripted network has nothing to load.
struct SimVoices(SimEmbedder);

impl Voices for SimVoices {
    fn embedder(&mut self, _guard: &Guard, _log: &Log) -> Result<&mut dyn Embedder, String> {
        Ok(&mut self.0)
    }
}

/// The network on the NPU, in the session the transcription model is in.
struct NpuVoices<'s, 'b> {
    session: &'s Session<'b>,
    system: &'s QnnSystem,
    pack: Option<PathBuf>,
    loaded: Option<QnnEmbedder<'s>>,
    /// Why it would not load; asked once per session.
    failed: Option<String>,
}

impl Voices for NpuVoices<'_, '_> {
    fn embedder(&mut self, guard: &Guard, log: &Log) -> Result<&mut dyn Embedder, String> {
        if let Some(reason) = &self.failed {
            return Err(reason.clone());
        }
        if self.loaded.is_none() {
            let started = Instant::now();
            let loaded = (|| {
                let pack = SpeakerPack::open(self.pack.as_ref().ok_or("the speaker model is not part of this build")?).map_err(|e| e.to_string())?;
                // Vendor code, like any other load: written down first.
                let _busy = guard.enter("loading the speaker model");
                QnnEmbedder::load(self.session, self.system, &pack).map_err(|e| e.to_string())
            })();
            match loaded {
                Ok(embedder) => {
                    log.info(format!("speaker model loaded in {} ms: {}", started.elapsed().as_millis(), embedder.provenance()));
                    self.loaded = Some(embedder);
                }
                Err(reason) => {
                    self.failed = Some(reason.clone());
                    return Err(reason);
                }
            }
        }
        match self.loaded.as_mut() {
            Some(embedder) => Ok(embedder),
            None => Err("the speaker model is not loaded".into()),
        }
    }
}

/// The capture in progress.
struct Recording {
    id: String,
    recorder: Arc<Mutex<Option<Recorder>>>,
    samples: Arc<AtomicU64>,
    level: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    /// When stop was asked for; the service confirms shortly after.
    stopping: Option<Instant>,
    /// Set once the recording is ending, so the live pass lets go of the NPU at its next step
    /// instead of finishing a window whose audio is about to be decoded again as the tail.
    halt: Arc<AtomicBool>,
    segments: Vec<Seg>,
    preview: Vec<Seg>,
    seek: u64,
    language: Option<String>,
    revision: u64,
}

/// A queued transcription under way.
struct Active {
    id: String,
    done: u64,
    total: u64,
    cancel: Arc<AtomicBool>,
}

/// A speaker pass under way.
struct Voicing {
    id: String,
    done: u32,
    total: u32,
    cancel: Arc<AtomicBool>,
}

struct Install {
    name: String,
    progress: Arc<models::Progress>,
    /// Set when the worker is done, either way.
    result: Option<Result<(), String>>,
}

struct Shared {
    recording: Option<Recording>,
    queue: VecDeque<String>,
    active: Option<Active>,
    model: ModelStatus,
    /// The model could not be loaded; nothing retries until something changes.
    blocked: bool,
    unload: bool,
    load: bool,
    /// The load was asked for by hand, and is attempted even where no NPU was detected.
    forced: bool,
    /// A pack was installed: the one in use may no longer be the one that should be.
    reselect: bool,
    stats: Stats,
    selftest: SelfTest,
    install: Option<Install>,
    notice: Option<String>,
    /// Why the last text file could not be written; cleared by the next that is.
    export_error: Option<String>,
    /// Notes waiting for their speaker pass, each with the count to find if one was given.
    voices: VecDeque<(String, Option<u8>)>,
    voicing: Option<Voicing>,
    /// Why speaker passes do not start by themselves, when they do not.
    voices_paused: Option<String>,
    /// Bumped whenever a note on disk changed.
    revision: u64,
}

/// A note's text as a file, ready to be handed to another app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFile {
    pub path: PathBuf,
    /// The name it should carry wherever it goes.
    pub name: String,
    /// It is the note's own file in the documents folder, which Android has indexed and can
    /// pass on as it is. Otherwise a copy written for the occasion.
    pub stored: bool,
}

/// A frame's worth of what the engine is doing.
#[derive(Clone, Debug)]
pub struct Status {
    pub model: ModelStatus,
    pub blocked: bool,
    pub stats: Stats,
    pub queue: Vec<String>,
    /// Id, samples done and samples in all of the note being transcribed.
    pub active: Option<(String, u64, u64)>,
    pub recording: Option<LiveStatus>,
    pub selftest: SelfTest,
    pub install: Option<InstallStatus>,
    pub export_error: Option<String>,
    /// Notes waiting for their speaker pass.
    pub voices: Vec<String>,
    /// Id, windows done and windows in all of the note whose speakers are being told apart.
    pub voicing: Option<(String, u32, u32)>,
    pub voices_paused: Option<String>,
    pub revision: u64,
}

impl Status {
    /// Nothing running and nothing waiting to.
    pub fn idle(&self) -> bool {
        self.queue.is_empty() && self.active.is_none() && self.recording.is_none() && self.voices.is_empty() && self.voicing.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct LiveStatus {
    pub id: String,
    pub samples: u64,
    /// Peak of the last 100 ms, 0..1.
    pub level: f32,
    pub paused: bool,
    pub stopping: bool,
    pub language: Option<String>,
    /// Changes whenever the transcript does; see [`Engine::live_transcript`].
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub struct InstallStatus {
    pub name: String,
    pub phase: models::Phase,
    pub done: u64,
    pub total: u64,
    pub result: Option<Result<(), String>>,
}

pub struct Engine {
    pub paths: Paths,
    pub library: Library,
    pub log: Log,
    /// Where note text is also kept as files; see [`crate::export`].
    pub exporter: Option<Exporter>,
    shared: Mutex<Shared>,
    wake: Condvar,
    settings: Mutex<Settings>,
    ui: Mutex<Option<egui::Context>>,
    /// Serializes read-modify-write of note files between the worker and the UI.
    notes: Mutex<()>,
    /// Held while one service event is taken and acted on, while a capture is started, and
    /// while the service is told to hold or go: nothing decided about the last capture may be
    /// acted on once the next recording exists.
    mic: Mutex<()>,
    guard: Guard,
    /// The same breadcrumb for the speaker network, kept apart: a network that takes the
    /// process down must cost speaker labels, not transcription.
    voice_guard: Guard,
    /// Milliseconds the scripted engine spends on each decoder run.
    #[cfg(test)]
    sim_step: AtomicU64,
    /// Milliseconds the scripted speaker network spends on each window.
    #[cfg(test)]
    sim_voice: AtomicU64,
    /// Service events a test stands in for the service with.
    #[cfg(test)]
    injected: Mutex<VecDeque<MicEvent>>,
}

static GLOBAL: OnceLock<Arc<Engine>> = OnceLock::new();

fn seg(segment: &Segment) -> Seg {
    Seg { start: segment.start as f32, end: segment.end as f32, text: segment.text.clone(), speaker: None }
}

impl Engine {
    /// The process's engine, created on first call. Later calls — a rebuilt activity — get the
    /// same one, with whatever it was in the middle of.
    pub fn global(paths: impl FnOnce() -> Paths) -> std::io::Result<Arc<Engine>> {
        if let Some(engine) = GLOBAL.get() {
            return Ok(engine.clone());
        }
        let engine = Self::start(paths())?;
        Ok(GLOBAL.get_or_init(|| engine).clone())
    }

    /// Open the library, tidy up after a previous process, and start the worker.
    pub fn start(paths: Paths) -> std::io::Result<Arc<Engine>> {
        let library = Library::open(&paths.library)?;
        let _ = std::fs::create_dir_all(&paths.models);
        let settings = Settings::load(&paths.settings);
        let waiting = library.recover();
        let (guard, died) = Guard::open(paths.settings.with_file_name("engine-guard.json"));
        // Two processes in a row died with the NPU at work. This one records, plays and
        // edits as ever, and leaves transcription until it is asked for.
        let paused = guard.tripped();
        // Strict: speaker labels are an extra, and a network that took the process down once
        // is not run again until someone asks for it.
        let (voice_guard, voice_died) = Guard::open_strict(paths.settings.with_file_name("speaker-guard.json"));
        let voices_paused = voice_guard.tripped().then(|| VOICES_PAUSED.to_string());
        // Passes owed from before: the process ended between a transcript and its speakers.
        let able = settings.speakers && !paused && voices_paused.is_none() && (settings.simulate || paths.speaker_pack.is_some());
        let owed: VecDeque<(String, Option<u8>)> = if able { library.speakers_due().into_iter().map(|id| (id, None)).collect() } else { VecDeque::new() };
        let engine = Arc::new(Engine {
            library,
            log: Log::default(),
            exporter: paths.documents.as_ref().map(Exporter::new),
            shared: Mutex::new(Shared {
                recording: None,
                queue: if settings.auto_transcribe && !paused { waiting.into() } else { VecDeque::new() },
                active: None,
                model: if paused { ModelStatus::Paused(PAUSED.into()) } else { ModelStatus::Idle },
                blocked: paused,
                unload: false,
                load: false,
                forced: false,
                reselect: false,
                stats: Stats::default(),
                selftest: SelfTest::Idle,
                install: None,
                notice: if paused { Some(PAUSED.to_string()) } else { voices_paused.clone() },
                export_error: None,
                voices: owed,
                voicing: None,
                voices_paused,
                revision: 1,
            }),
            wake: Condvar::new(),
            settings: Mutex::new(settings),
            ui: Mutex::new(None),
            notes: Mutex::new(()),
            mic: Mutex::new(()),
            guard,
            voice_guard,
            #[cfg(test)]
            sim_step: AtomicU64::new(0),
            #[cfg(test)]
            sim_voice: AtomicU64::new(0),
            #[cfg(test)]
            injected: Mutex::new(VecDeque::new()),
            paths,
        });
        engine.log.info(format!("library at {}", engine.paths.library.display()));
        if let Some(what) = died {
            engine.log.warn(format!("the last run ended while {what} (strike {})", engine.guard.strikes()));
        }
        if let Some(what) = voice_died {
            engine.log.warn(format!("the last run ended while {what} (strike {})", engine.voice_guard.strikes()));
        }
        let worker = engine.clone();
        std::thread::Builder::new().name("scribe-engine".into()).spawn(move || worker.work())?;
        let watcher = engine.clone();
        std::thread::Builder::new().name("scribe-mic".into()).spawn(move || watcher.watch())?;
        // Notes finished by an earlier build, or while text files were switched off.
        engine.export_all();
        Ok(engine)
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The activity now on screen; state changes repaint it.
    pub fn attach_ui(&self, ctx: egui::Context) {
        *self.ui.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
    }

    fn repaint(&self) {
        if let Some(ctx) = self.ui.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            ctx.request_repaint();
        }
    }

    fn changed(&self) {
        self.lock().revision += 1;
        self.repaint();
    }

    fn notice(&self, text: impl Into<String>) {
        let text = text.into();
        self.log.warn(text.clone());
        self.lock().notice = Some(text);
        self.repaint();
    }

    /// The latest thing that went wrong, once.
    pub fn take_notice(&self) -> Option<String> {
        self.lock().notice.take()
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Change settings and save them. A different model or engine takes effect at once.
    pub fn update_settings(&self, edit: impl FnOnce(&mut Settings)) {
        let (before, after) = {
            let mut settings = self.settings.lock().unwrap_or_else(|e| e.into_inner());
            let before = settings.clone();
            edit(&mut settings);
            (before, settings.clone())
        };
        if before == after {
            return;
        }
        if let Err(e) = after.save(&self.paths.settings) {
            self.log.warn(format!("settings not saved: {e}"));
        }
        if before.model != after.model || before.simulate != after.simulate {
            // Whatever stopped the old choice from loading says nothing about the new one.
            self.lock().blocked = false;
        }
        self.wake.notify_all();
    }

    pub fn status(&self) -> Status {
        let shared = self.lock();
        Status {
            model: shared.model.clone(),
            blocked: shared.blocked,
            stats: shared.stats,
            queue: shared.queue.iter().cloned().collect(),
            active: shared.active.as_ref().map(|a| (a.id.clone(), a.done, a.total)),
            recording: shared.recording.as_ref().map(|r| LiveStatus {
                id: r.id.clone(),
                samples: r.samples.load(Ordering::Relaxed),
                level: f32::from_bits(r.level.load(Ordering::Relaxed)),
                paused: r.paused.load(Ordering::Relaxed),
                stopping: r.stopping.is_some(),
                language: r.language.clone(),
                revision: r.revision,
            }),
            selftest: shared.selftest.clone(),
            install: shared.install.as_ref().map(|i| {
                let (done, total) = i.progress.bytes();
                InstallStatus { name: i.name.clone(), phase: i.progress.phase(), done, total, result: i.result.clone() }
            }),
            export_error: shared.export_error.clone(),
            voices: shared.voices.iter().map(|(id, _)| id.clone()).collect(),
            voicing: shared.voicing.as_ref().map(|v| (v.id.clone(), v.done, v.total)),
            voices_paused: shared.voices_paused.clone(),
            revision: shared.revision,
        }
    }

    /// The live recording's final segments and its rough preview of what follows them.
    pub fn live_transcript(&self) -> (Vec<Seg>, Vec<Seg>) {
        let shared = self.lock();
        shared.recording.as_ref().map(|r| (r.segments.clone(), r.preview.clone())).unwrap_or_default()
    }

    // ── Notes ────────────────────────────────────────────────────────────────

    /// Load a note, change it, and save it, without losing a change made on another thread.
    pub fn edit_note(&self, id: &str, edit: impl FnOnce(&mut Note)) -> Option<Note> {
        let note = {
            let _guard = self.notes.lock().unwrap_or_else(|e| e.into_inner());
            // `find`, so audio that lost its note file can still be worked on. A deleted note
            // has no audio either, and stays deleted.
            let mut note = self.library.find(id)?;
            edit(&mut note);
            self.library.save(&note).ok()?;
            note
        };
        self.changed();
        Some(note)
    }

    /// Bring a finished note's text file up to date; see [`crate::export`]. Unfinished notes
    /// are left alone: a transcript still arriving would be written once per window.
    pub fn export_note(&self, id: &str) {
        let Some(exporter) = self.exporter.as_ref() else { return };
        if !self.settings().save_text {
            return;
        }
        let outcome = {
            let _guard = self.notes.lock().unwrap_or_else(|e| e.into_inner());
            let Some(mut note) = self.library.find(id).filter(|note| note.state == NoteState::Done) else { return };
            match exporter.sync(&mut note) {
                Ok(false) => return,
                Ok(true) => self.library.save(&note).map(|()| note.export.map(|export| export.file).unwrap_or_default()),
                Err(e) => Err(e),
            }
        };
        match outcome {
            Ok(file) => {
                platform::index_shared(&exporter.dir().join(&file));
                self.log.info(format!("text saved as {}/{file}", exporter.label()));
                self.lock().export_error = None;
                self.changed();
            }
            Err(e) => {
                let reason = e.to_string();
                // Said once on screen; after that it is in Settings and the log until it clears.
                let first = self.lock().export_error.replace(reason.clone()).is_none();
                if first {
                    self.notice(format!("The text file could not be saved in {}: {reason}", exporter.label()));
                } else {
                    self.log.warn(format!("text file for {id} not saved: {reason}"));
                }
            }
        }
    }

    /// The note's text as a file to share. A long transcript does not belong in a message
    /// box, and most apps take an attachment where they would choke on forty pages of paste.
    ///
    /// The note's own file in the documents folder is used where it has one, brought up to
    /// date first. A note without — unfinished, or with text files switched off — gets a copy
    /// written beside the library, replaced by the next one.
    pub fn text_file(&self, id: &str) -> Result<TextFile, String> {
        let note = self.library.find(id).ok_or("The note is gone.")?;
        let text = note.text();
        if text.trim().is_empty() {
            return Err("There is no text to share yet.".into());
        }
        self.export_note(id);
        if let (Some(exporter), Some(note)) = (self.exporter.as_ref(), self.library.find(id)) {
            if let Some(name) = exporter.current(&note) {
                return Ok(TextFile { path: exporter.dir().join(&name), name, stored: true });
            }
        }
        let dir = self.paths.library.with_file_name("shared");
        // Last time's copy; whoever it was shared with has long since read it.
        let _ = std::fs::remove_dir_all(&dir);
        let name = crate::export::file_name(&note, &text);
        let path = dir.join(&name);
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, crate::export::content(&text))).map_err(|e| format!("The text file could not be written: {e}"))?;
        Ok(TextFile { path, name, stored: false })
    }

    /// Write the text file of every finished note that lacks an up-to-date one, off the
    /// calling thread. Notes whose file is current cost one read each.
    pub fn export_all(self: &Arc<Self>) {
        if self.exporter.is_none() || !self.settings().save_text {
            return;
        }
        let engine = self.clone();
        let spawned = std::thread::Builder::new().name("scribe-export".into()).spawn(move || {
            for note in engine.library.list() {
                if note.state == NoteState::Done {
                    engine.export_note(&note.id);
                }
            }
            // And any file written while there was no screen to ask the scanner through.
            if let Some(dir) = engine.exporter.as_ref().map(|exporter| exporter.dir()).filter(|dir| dir.is_dir()) {
                platform::index_shared(dir);
            }
        });
        if let Err(e) = spawned {
            self.log.warn(format!("text files not caught up: {e}"));
        }
    }

    // ── Speakers ─────────────────────────────────────────────────────────────

    /// Whether the speaker pass can run here at all.
    pub fn speakers_available(&self) -> bool {
        self.settings().simulate || self.paths.speaker_pack.is_some()
    }

    /// Whether a recording that has just been transcribed gets its speaker pass unasked.
    fn auto_speakers(&self) -> bool {
        self.settings().speakers && self.speakers_available() && self.lock().voices_paused.is_none()
    }

    /// Take a note out of the speaker queue and stop its pass if that is the one running.
    fn drop_voices(shared: &mut Shared, id: &str) {
        shared.voices.retain(|(queued, _)| queued != id);
        if let Some(voicing) = shared.voicing.as_ref().filter(|v| v.id == id) {
            voicing.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Queue a note for its speaker pass, unless it is waiting or running already.
    fn queue_voices(&self, id: &str, fixed: Option<u8>) {
        let mut shared = self.lock();
        // A pass that has been cancelled is still there until the worker lets go of it, and
        // is no reason to refuse the one that replaces it.
        let under_way = shared.voicing.as_ref().is_some_and(|v| v.id == id && !v.cancel.load(Ordering::Relaxed));
        if under_way || shared.voices.iter().any(|(queued, _)| queued == id) {
            return;
        }
        shared.voices.push_back((id.to_string(), fixed));
    }

    /// Tell a note's speakers apart: `fixed` of them if given, as many as there are if not.
    /// Where the recording's voice prints are on disk this is a regrouping and done at once;
    /// otherwise the note waits its turn on the NPU.
    pub fn find_speakers(&self, id: &str, fixed: Option<u8>) {
        let Some(note) = self.library.find(id).filter(|note| note.state == NoteState::Done && !note.segments.is_empty()) else { return };
        let key = note.voices_key();
        if let Some(prints) = Prints::load(&self.library.voices_path(id), key) {
            // Grouped with no lock held: a long recording takes a moment, and the worker
            // must not wait on the screen's thread to save a note.
            let labels = local_speaker::cluster(&prints, note.segments.len(), fixed.map(usize::from));
            let applied = self.edit_note(id, |note| {
                if note.state == NoteState::Done && note.voices_key() == key {
                    note.set_speakers(&labels.spans, labels.speakers, fixed.is_some());
                }
            });
            if applied.is_some() {
                self.export_note(id);
            }
            return;
        }
        self.edit_note(id, |note| note.speakers_due = true);
        {
            // Asking is what lifts a pause, and what gets a model that would not load tried
            // again, as it is for transcription.
            let mut shared = self.lock();
            shared.voices_paused = None;
            shared.blocked = false;
        }
        self.queue_voices(id, fixed);
        self.changed();
        self.wake.notify_all();
    }

    /// Give speaker number `speaker` of a note a name; an empty one goes back to the number.
    pub fn rename_speaker(&self, id: &str, speaker: u8, name: &str) {
        self.edit_note(id, |note| note.rename_speaker(speaker, name));
        self.export_note(id);
    }

    /// Set a note's speaker names, first to last, from the screen that edits them. Like any
    /// edit made there it reaches the text file when the note is left.
    pub fn name_speakers(&self, id: &str, names: &[String]) -> Option<Note> {
        self.edit_note(id, |note| note.name_speakers(names))
    }

    pub fn delete_note(&self, id: &str) -> Result<(), String> {
        {
            let mut shared = self.lock();
            if shared.recording.as_ref().is_some_and(|r| r.id == id) {
                return Err("Stop the recording before deleting it.".into());
            }
            shared.queue.retain(|queued| queued != id);
            if let Some(active) = shared.active.as_ref().filter(|a| a.id == id) {
                active.cancel.store(true, Ordering::Relaxed);
            }
            Self::drop_voices(&mut shared, id);
        }
        let _guard = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let result = self.library.delete(id).map_err(|e| format!("Could not delete the note: {e}"));
        drop(_guard);
        // It may have been the last thing the service was being kept for.
        self.sync_service();
        self.changed();
        result
    }

    // ── Recording ────────────────────────────────────────────────────────────

    /// Create the note and its audio file, and return the sink that fills it.
    pub(crate) fn prepare_recording(&self) -> Result<MicSink, String> {
        let mut shared = self.lock();
        if shared.recording.is_some() {
            return Err("Already recording.".into());
        }
        let note = self.library.create(clock::now()).map_err(|e| format!("Could not create the note: {e}"))?;
        let wav = match WavWriter::create(&self.library.audio_path(&note.id), RATE) {
            Ok(wav) => wav,
            Err(e) => {
                let _ = self.library.delete(&note.id);
                return Err(format!("Could not create the audio file: {e}"));
            }
        };
        let (samples, level, paused) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU32::new(0)), Arc::new(AtomicBool::new(false)));
        let recorder = Arc::new(Mutex::new(Some(Recorder::new(wav, samples.clone(), level.clone(), paused.clone()))));
        let writer = recorder.clone();
        let sink: MicSink = Box::new(move |pcm, rate| {
            if let Ok(mut slot) = writer.lock() {
                if let Some(recorder) = slot.as_mut() {
                    recorder.push(pcm, rate);
                }
            }
        });
        self.log.info(format!("recording {}", note.id));
        shared.recording = Some(Recording { id: note.id, recorder, samples, level, paused, stopping: None, halt: Arc::new(AtomicBool::new(false)), segments: Vec::new(), preview: Vec::new(), seek: 0, language: None, revision: 0 });
        shared.revision += 1;
        Ok(sink)
    }

    /// Start capturing into a new note. The microphone permission must already be granted.
    pub fn start_recording(&self) -> Result<(), String> {
        let _turn = self.mic.lock().unwrap_or_else(|e| e.into_inner());
        if platform::mic_capturing() {
            return Err("The last recording is still closing. Try again in a moment.".into());
        }
        let sink = self.prepare_recording()?;
        if !platform::mic_start("Scribe", "Recording", self.settings().mic, sink) {
            if let Some(recording) = self.lock().recording.take() {
                let _ = self.library.delete(&recording.id);
            }
            self.changed();
            return Err("Android would not start the recording service.".into());
        }
        self.wake.notify_all();
        Ok(())
    }

    /// Ask the capture to end. The note is finished off once the service confirms it has.
    pub fn stop_recording(&self) {
        {
            let mut shared = self.lock();
            let Some(recording) = shared.recording.as_mut().filter(|r| r.stopping.is_none()) else { return };
            recording.stopping = Some(Instant::now());
            recording.halt.store(true, Ordering::Relaxed);
        }
        platform::mic_hold("Scribe", "Saving the recording");
        self.wake.notify_all();
        self.repaint();
    }

    pub fn set_paused(&self, paused: bool) {
        if let Some(recording) = self.lock().recording.as_ref() {
            recording.paused.store(paused, Ordering::Relaxed);
        }
        self.repaint();
    }

    /// Close the audio file and turn the capture into an ordinary note.
    pub(crate) fn finalize_recording(&self) {
        let _turn = self.mic.lock().unwrap_or_else(|e| e.into_inner());
        self.finalize(None);
    }

    /// [`Engine::finalize_recording`], after the service reported `failure`. Called with the
    /// `mic` turn held, from the watcher thread: the worker may be deep in an NPU call, and a
    /// live window of its that lands after this finds no recording and is dropped.
    fn finalize(&self, failure: Option<String>) {
        let Some(recording) = self.lock().recording.take() else { return };
        recording.halt.store(true, Ordering::Relaxed);
        let recorder = recording.recorder.lock().ok().and_then(|mut slot| slot.take());
        let mut write_error = None;
        let samples = match recorder {
            Some(recorder) => {
                write_error = recorder.error.clone();
                recorder.finish().unwrap_or_else(|e| {
                    write_error.get_or_insert(e.to_string());
                    recording.samples.load(Ordering::Relaxed)
                })
            }
            None => recording.samples.load(Ordering::Relaxed),
        };
        if samples == 0 {
            let _ = self.library.delete(&recording.id);
            self.notice(failure.map_or_else(|| "Nothing was recorded.".to_string(), |reason| format!("Recording failed: {reason}")));
        } else {
            let complete = recording.seek >= samples;
            let speakers = complete && !recording.segments.is_empty() && self.auto_speakers();
            self.edit_note(&recording.id, |note| {
                note.duration_ms = samples * 1000 / RATE as u64;
                note.segments = recording.segments.clone();
                note.seek = recording.seek.min(samples);
                note.language = recording.language.clone();
                note.state = if complete { NoteState::Done } else { NoteState::Pending };
                note.speakers_due = speakers;
            });
            self.log.info(format!("recorded {} ({})", recording.id, clock::duration(samples as f64 / RATE as f64)));
            if complete {
                self.export_note(&recording.id);
            }
            if speakers {
                self.queue_voices(&recording.id, None);
            }
            if !complete && self.settings().auto_transcribe {
                // The tail of this recording comes before older work.
                self.lock().queue.push_front(recording.id.clone());
            }
            // One notice at a time reaches the screen; a cut-short capture outranks the rest.
            if let Some(reason) = failure {
                self.notice(format!("Recording stopped early: {reason}"));
            } else if let Some(error) = write_error {
                self.notice(format!("The recording may be incomplete: {error}"));
            }
        }
        self.changed();
        self.sync_service_held();
        self.wake.notify_all();
    }

    /// Keep the foreground service exactly as long as there is work it protects.
    fn sync_service(&self) {
        // Deciding and telling the service are one step under the turn: a recording cannot
        // start in between and then be stopped by what was decided before it existed.
        let _turn = self.mic.lock().unwrap_or_else(|e| e.into_inner());
        self.sync_service_held();
    }

    fn sync_service_held(&self) {
        let working = {
            let shared = self.lock();
            if shared.recording.is_some() {
                return;
            }
            !shared.blocked && (!shared.queue.is_empty() || shared.active.is_some() || !shared.voices.is_empty() || shared.voicing.is_some())
        };
        if working { platform::mic_hold("Scribe", "Transcribing") } else { platform::mic_release() }
    }

    /// React to what the recording service has reported.
    fn next_event(&self) -> Option<MicEvent> {
        #[cfg(test)]
        if let Some(event) = self.injected.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
            return Some(event);
        }
        platform::mic_event()
    }

    fn pump_mic(&self) {
        loop {
            let _turn = self.mic.lock().unwrap_or_else(|e| e.into_inner());
            let Some(event) = self.next_event() else { break };
            match event {
                MicEvent::Started(rate) => self.log.info(format!("microphone open at {rate} Hz")),
                MicEvent::Stopped => self.finalize(None),
                MicEvent::Failed(reason) => self.finalize(Some(reason)),
                // The notification's Stop: the service has ended the capture already, and the
                // rest is what a tap on the screen's Stop does.
                MicEvent::StopRequested => self.stop_recording(),
            }
        }
        // A service that died never confirms; the audio on disk is still the recording.
        let overdue = self.lock().recording.as_ref().and_then(|r| r.stopping).is_some_and(|at| at.elapsed() > STOP_GRACE);
        if overdue {
            self.finalize_recording();
        }
    }

    // ── Transcription requests ───────────────────────────────────────────────

    /// Queue a note. `restart` discards its transcript first; otherwise it resumes.
    pub fn transcribe(&self, id: &str, restart: bool) {
        {
            let mut shared = self.lock();
            if shared.recording.as_ref().is_some_and(|r| r.id == id) || shared.active.as_ref().is_some_and(|a| a.id == id) {
                return;
            }
            if restart {
                // Wiped under the queue's own lock: the worker can only take the note up after
                // this, never between reading where it had got to and that being reset.
                Self::drop_voices(&mut shared, id);
                let _guard = self.notes.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(mut note) = self.library.find(id) {
                    note.reset_transcript();
                    let _ = self.library.save(&note);
                }
                // Prints of the old transcript's windows; the new one gets its own.
                let _ = std::fs::remove_file(self.library.voices_path(id));
            }
            if !shared.queue.iter().any(|queued| queued == id) {
                shared.queue.push_back(id.to_string());
            }
            // Asking again is how a failed model load gets retried.
            shared.blocked = false;
        }
        self.changed();
        self.wake.notify_all();
    }

    /// Queue every note that still has untranscribed audio.
    pub fn transcribe_pending(&self) {
        for note in self.library.list().into_iter().rev() {
            if matches!(note.state, NoteState::Pending | NoteState::Failed) {
                self.transcribe(&note.id, false);
            }
        }
    }

    /// Take a note out of the queue, or stop it if it is the one running. What it has stays.
    pub fn cancel(&self, id: &str) {
        {
            let mut shared = self.lock();
            shared.queue.retain(|queued| queued != id);
            if let Some(active) = shared.active.as_ref().filter(|a| a.id == id) {
                active.cancel.store(true, Ordering::Relaxed);
            }
        }
        // A queue emptied by hand has no job whose end would let the service go.
        self.sync_service();
        self.changed();
        self.wake.notify_all();
    }

    /// Load the model now, whatever was concluded about this device having an NPU: the check
    /// is a guess, and this is the way to find out from the device itself that it guessed wrong.
    pub fn request_load(&self) {
        {
            let mut shared = self.lock();
            shared.load = true;
            shared.forced = true;
            shared.blocked = false;
        }
        self.wake.notify_all();
    }

    pub fn request_unload(&self) {
        self.lock().unload = true;
        self.wake.notify_all();
    }

    pub fn request_selftest(&self) {
        {
            let mut shared = self.lock();
            shared.selftest = SelfTest::Requested;
            shared.blocked = false;
        }
        self.wake.notify_all();
    }

    // ── Model packs ──────────────────────────────────────────────────────────

    /// Download and install a pack in the background.
    pub fn install(self: &Arc<Self>, spec: &'static ModelSpec) {
        let progress = Arc::new(models::Progress::default());
        {
            let mut shared = self.lock();
            if shared.install.as_ref().is_some_and(|i| i.result.is_none()) {
                return;
            }
            shared.install = Some(Install { name: spec.name.to_string(), progress: progress.clone(), result: None });
        }
        self.log.info(format!("installing {}", spec.name));
        let engine = self.clone();
        std::thread::spawn(move || {
            let result = models::install(spec, &engine.paths.models, &progress).map(|_| ());
            match &result {
                Ok(()) => engine.log.info(format!("installed {}", spec.name)),
                Err(e) => engine.log.warn(format!("install of {}: {e}", spec.name)),
            }
            let installed = result.is_ok();
            {
                let mut shared = engine.lock();
                if let Some(install) = shared.install.as_mut() {
                    install.result = Some(result);
                }
                if installed {
                    shared.blocked = false;
                    shared.reselect = true;
                }
            }
            if installed && engine.settings().auto_transcribe {
                engine.transcribe_pending();
            }
            engine.wake.notify_all();
            engine.repaint();
        });
    }

    pub fn cancel_install(&self) {
        if let Some(install) = self.lock().install.as_ref() {
            install.progress.cancel();
        }
    }

    /// Forget a finished install's result once the screen has shown it.
    pub fn dismiss_install(&self) {
        let mut shared = self.lock();
        if shared.install.as_ref().is_some_and(|i| i.result.is_some()) {
            shared.install = None;
        }
    }

    pub fn remove_pack(&self, name: &str) -> Result<(), String> {
        if matches!(&self.lock().model, ModelStatus::Ready(loaded) if loaded.name == name) {
            return Err("Unload the model before removing it.".into());
        }
        let pack = models::installed(&self.paths.models).into_iter().find(|p| p.name() == name).ok_or("That model is not installed.")?;
        models::remove(&pack).map_err(|e| format!("Could not remove {name}: {e}"))?;
        self.repaint();
        Ok(())
    }

    // ── The worker ───────────────────────────────────────────────────────────

    fn wait(&self, duration: Duration) {
        let guard = self.lock();
        let _ = self.wake.wait_timeout(guard, duration);
    }

    fn wants_model(&self) -> bool {
        let auto = self.settings().auto_transcribe;
        let shared = self.lock();
        !shared.blocked && ((shared.recording.is_some() && auto) || !shared.queue.is_empty() || !shared.voices.is_empty() || shared.selftest == SelfTest::Requested || shared.load)
    }

    /// The worker: everything that touches the NPU.
    fn work(self: Arc<Self>) {
        loop {
            if self.wants_model() {
                // A bug in the decode path may cost transcription, never the recorder: the
                // watcher finishes recordings whatever becomes of this thread's work.
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.session())) {
                    self.after_panic(panic);
                }
                continue;
            }
            self.wait(Duration::from_secs(2));
        }
    }

    /// The watcher: reacts to the recording service on a thread of its own, so that stopping
    /// a recording never waits for whatever the NPU is in the middle of — a window can take
    /// seconds, a model load longer, and a hung call for ever.
    fn watch(self: Arc<Self>) {
        loop {
            self.pump_mic();
            let recording = self.lock().recording.is_some();
            self.wait(if recording { Duration::from_millis(100) } else { Duration::from_secs(1) });
        }
    }

    /// Put things in order after the session unwound. Transcription then waits to be asked
    /// for, so whatever input did this cannot do it again by itself.
    fn after_panic(&self, panic: Box<dyn std::any::Any + Send>) {
        let what = panic.downcast_ref::<&str>().map(|s| (*s).to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_else(|| "no message".into());
        let (orphan, voice, testing) = {
            let mut shared = self.lock();
            let testing = matches!(shared.selftest, SelfTest::Requested | SelfTest::Running);
            if testing {
                shared.selftest = SelfTest::Failed("The engine stopped unexpectedly.".into());
            }
            (shared.active.take().map(|active| active.id), shared.voicing.take().map(|voicing| voicing.id), testing)
        };
        // With no transcription under way it was the speaker pass that failed, and the
        // speaker pass that pays: labels stop, transcription does not.
        if let (None, Some(id), false) = (&orphan, &voice, testing) {
            self.log.warn(format!("the speaker pass panicked: {what}"));
            self.voice_guard.strike();
            self.edit_note(id, |note| note.speakers_due = false);
            self.pause_voices("Speaker labelling stopped unexpectedly and is paused. The log under Settings, Debug says where. Use Speakers on a note to try again.".into());
            self.sync_service();
            self.changed();
            return;
        }
        self.log.warn(format!("the transcription engine panicked: {what}"));
        self.guard.strike();
        if let Some(id) = orphan {
            self.edit_note(&id, |note| {
                note.state = NoteState::Failed;
                note.error = Some("Transcription stopped unexpectedly. The audio is intact.".into());
                note.attempts = 0;
            });
        }
        self.block(ModelStatus::Failed("The transcription engine stopped unexpectedly. The log under Settings, Debug says where.".into()));
    }

    fn set_model(&self, status: ModelStatus) {
        self.lock().model = status;
        self.repaint();
    }

    /// Record why the model is unavailable and stop asking for it.
    fn block(&self, status: ModelStatus) {
        if let ModelStatus::Failed(reason) | ModelStatus::Paused(reason) = &status {
            self.log.warn(format!("model unavailable: {reason}"));
        }
        {
            let mut shared = self.lock();
            shared.model = status;
            shared.blocked = true;
            shared.load = false;
            // Speaker passes ride on the same session. Their notes stay owed, and are taken
            // up at the next start or when asked for; none sits waiting on the screen.
            shared.voices.clear();
            if shared.selftest == SelfTest::Requested {
                shared.selftest = SelfTest::Failed("No model could be loaded.".into());
            }
        }
        self.sync_service();
        self.repaint();
    }

    /// Run one piece of NPU work with the crash breadcrumb down around it; `ran` says whether
    /// the NPU really was used and came back. `what` is `None` for the scripted engine, which
    /// has nothing to crash in and proves nothing by working.
    fn guarded<T>(&self, what: Option<String>, work: impl FnOnce() -> T, ran: impl FnOnce(&T) -> bool) -> T {
        Self::guarded_by(&self.guard, what, work, ran)
    }

    fn guarded_by<T>(guard: &Guard, what: Option<String>, work: impl FnOnce() -> T, ran: impl FnOnce(&T) -> bool) -> T {
        let Some(what) = what else { return work() };
        let busy = guard.enter(what);
        let out = work();
        if ran(&out) {
            busy.proven();
        }
        out
    }

    /// Bring a model up, serve work with it until there is none, and let it go.
    fn session(&self) {
        let settings = self.settings();
        let forced = {
            let mut shared = self.lock();
            shared.load = false;
            // Both were about a session that is over: an Unload tapped with nothing loaded
            // must not be what ends this one.
            shared.unload = false;
            shared.reselect = false;
            std::mem::take(&mut shared.forced)
        };
        if settings.simulate {
            let mut engine = SimEngine::new();
            #[cfg(test)]
            {
                engine.step_delay = Duration::from_millis(self.sim_step.load(Ordering::Relaxed));
            }
            self.set_model(ModelStatus::Ready(Loaded {
                name: "simulated".into(),
                summary: engine.dims().summary(),
                provenance: "scripted sentences, no NPU".into(),
                load_ms: 0,
                simulated: true,
            }));
            #[allow(unused_mut)]
            let mut voices = SimVoices(SimEmbedder::default());
            #[cfg(test)]
            {
                voices.0.delay = Duration::from_millis(self.sim_voice.load(Ordering::Relaxed));
            }
            self.serve(&mut engine, &mut NoPower, &mut voices, "simulated", &settings);
            self.set_model(ModelStatus::Idle);
            return;
        }
        let Some(pack) = models::choose(&self.paths.models, &settings.model) else {
            self.block(ModelStatus::Missing);
            return;
        };
        let name = pack.name();
        self.set_model(ModelStatus::Loading(name.clone()));
        let started = Instant::now();
        let outcome = (|| -> Result<(), String> {
            let lib_dir = self.paths.native_libs.as_ref().ok_or("the app's native library directory is unknown")?;
            if !forced && !platform::has_npu() {
                return Err("This device has no Qualcomm NPU.".into());
            }
            // Vendor code from here on. A load that works proves nothing yet; a window does.
            let loading = self.guard.enter(format!("loading {name}"));
            let libs = npu::libs(lib_dir)?;
            let session = Session::new(&libs.backend).map_err(|e| format!("NPU session: {e}"))?;
            // Declared before the engine, so the vote outlives the contexts and not the session.
            let mut power = Burst { session: &session, vote: None, log: &self.log, warned: false };
            power.set(true);
            let mut engine = QnnEngine::load(&session, &libs.system, &pack).map_err(|e| format!("{name}: {e}"))?;
            drop(loading);
            let load_ms = started.elapsed().as_millis() as u64;
            self.log.info(format!("{name} loaded in {load_ms} ms: {}; {}", engine.dims().summary(), engine.provenance()));
            self.set_model(ModelStatus::Ready(Loaded { name: name.clone(), summary: engine.dims().summary(), provenance: engine.provenance(), load_ms, simulated: false }));
            // Declared last, so the speaker network's context goes before the session does.
            let mut voices = NpuVoices { session: &session, system: &libs.system, pack: self.paths.speaker_pack.clone(), loaded: None, failed: None };
            self.serve(&mut engine, &mut power, &mut voices, &name, &settings);
            Ok(())
        })();
        match outcome {
            Ok(()) => {
                self.log.info(format!("{name} unloaded"));
                self.set_model(ModelStatus::Idle);
            }
            Err(reason) => self.block(ModelStatus::Failed(reason)),
        }
    }

    /// A transcription pass that carries on from `seek`. With no language forced it stays in
    /// the one the text so far is in, unless the speech plainly switches; a language chosen in
    /// Settings outranks whatever an earlier pass detected.
    fn long_form(&self, model: &dyn Whisper, forced: Option<String>, seek: u64, language: Option<&str>) -> Result<LongForm, WhisperError> {
        let detected = if forced.is_none() { language } else { None };
        let mut long = LongForm::new(model.dims(), DecodeOptions { language: forced, ..Default::default() })?;
        long.set_seek(seek);
        if let Some(code) = detected {
            let _ = long.pin_language(code);
        }
        Ok(long)
    }

    fn serve(&self, model: &mut dyn Whisper, power: &mut dyn Power, voices: &mut dyn Voices, name: &str, session: &Settings) {
        let real = !session.simulate;
        let mut live: Option<Live> = None;
        // A recording whose live pass failed; it is finished from the queue instead.
        let mut live_off: Option<String> = None;
        let mut job: Option<Job> = None;
        let mut voice: Option<VoiceJob> = None;
        let mut preferred = session.model.clone();
        let mut idle_since = Instant::now();
        let (mut previewed_at, mut previewed_samples) = (Instant::now(), 0u64);
        loop {
            let settings = self.settings();
            if settings.simulate != session.simulate {
                break;
            }
            let (unload, reselect) = {
                let mut shared = self.lock();
                // Asked for while already loaded: granted, and good for another idle spell.
                if std::mem::take(&mut shared.load) {
                    idle_since = Instant::now();
                }
                (std::mem::take(&mut shared.unload), std::mem::take(&mut shared.reselect))
            };
            if unload {
                break;
            }
            if settings.model != preferred || reselect {
                preferred = settings.model.clone();
                // A preference matters once it changes which pack would be loaded: one still
                // downloading leaves this one serving, and takes over when it has landed.
                if real && models::choose(&self.paths.models, &preferred).is_none_or(|pack| pack.name() != name) {
                    break;
                }
            }
            let mut worked = false;

            // The recording in progress comes first: its text is what someone is watching.
            let forced = settings.forced_language();
            let recording = self.lock().recording.as_ref().map(|r| (r.id.clone(), r.samples.load(Ordering::Relaxed), r.seek, r.language.clone(), r.halt.clone()));
            match recording {
                Some((id, samples, seek, language, halt)) if settings.auto_transcribe && live_off.as_deref() != Some(id.as_str()) && !halt.load(Ordering::Relaxed) => {
                    if live.as_ref().is_none_or(|l| l.id != id || l.forced != forced) {
                        // From where the recording's text ends, not from zero: a model swapped
                        // in mid-recording must not transcribe the start a second time.
                        live = match (self.long_form(model, forced.clone(), seek, language.as_deref()), WavReader::open(&self.library.audio_path(&id))) {
                            (Ok(long), Ok(reader)) => Some(Live { id: id.clone(), forced: forced.clone(), long, reader }),
                            _ => None,
                        };
                        (previewed_at, previewed_samples) = (Instant::now(), 0);
                    }
                    if let Some(Live { long, reader, .. }) = live.as_mut() {
                        if samples.saturating_sub(long.seek()) >= CHUNK_SAMPLES as u64 {
                            power.set(true);
                            let label = real.then(|| format!("transcribing {id} as it records"));
                            let advanced = self.guarded(label, || long.advance(model, reader, samples, false, &halt), |r| matches!(r, Ok(Advance::Window(w)) if w.timings.steps > 0));
                            match advanced {
                                Ok(Advance::Window(window)) => {
                                    self.live_window(&id, long, &window, name);
                                    worked = true;
                                }
                                // Stopped under it: what is left is for the note's own job.
                                Ok(_) | Err(WhisperError::Cancelled) => {}
                                Err(e) => {
                                    // The audio keeps recording; this note is finished from the queue.
                                    self.log.warn(format!("live transcription stopped: {e}"));
                                    live = None;
                                    live_off = Some(id.clone());
                                }
                            }
                        } else if settings.live_preview && samples.saturating_sub(previewed_samples) > 2 * RATE as u64 && previewed_at.elapsed() >= PREVIEW_EVERY {
                            power.set(true);
                            let label = real.then(|| format!("previewing {id}"));
                            let shown = self.guarded(label, || long.preview(model, reader, samples, &halt), |r| matches!(r, Ok((_, timings)) if timings.steps > 0));
                            if let Ok((segments, _)) = shown {
                                self.live_preview(&id, &segments);
                            }
                            (previewed_at, previewed_samples) = (Instant::now(), samples);
                            worked = true;
                        }
                    }
                }
                _ => live = None,
            }

            if !worked {
                if job.is_none() {
                    job = self.next_job(model, name);
                }
                if let Some(running) = job.as_mut() {
                    power.set(true);
                    let label = real.then(|| format!("transcribing {}", running.id));
                    let advanced = self.guarded(label, || running.long.advance(model, &mut running.reader, running.total, true, &running.cancel), |r| matches!(r, Ok(Advance::Window(w)) if w.timings.steps > 0));
                    match advanced {
                        Ok(Advance::Window(window)) => self.job_window(running, &window),
                        Ok(_) => {
                            self.finish_job(running, Ok(()), false);
                            job = None;
                        }
                        Err(e) => {
                            self.finish_job(running, Err(e), false);
                            job = None;
                        }
                    }
                    worked = true;
                }
            }

            // A pass cancelled under it is let go at once, whatever else is running: its
            // note may be about to want another.
            if let Some(running) = voice.take_if(|running| running.cancel.load(Ordering::Relaxed)) {
                self.finish_voice(&running, Err(VoiceEnd::Cancelled));
            }
            // Speakers come after every transcript that is waiting: text first, then whose.
            // And never while recording: this network is the newest thing on the NPU, and a
            // fault in it must not be able to cost a capture.
            if !worked && self.lock().recording.is_none() {
                if voice.is_none() {
                    voice = self.next_voice_job();
                }
                if let Some(running) = voice.as_mut() {
                    power.set(true);
                    let turn = match voices.embedder(&self.voice_guard, &self.log) {
                        Ok(embedder) => {
                            let label = real.then(|| format!("telling the speakers of {} apart", running.id));
                            Self::guarded_by(&self.voice_guard, label, || running.advance(embedder), |r| matches!(r, Ok(done) if *done > 0))
                        }
                        // No network, no passes: this one and every one behind it.
                        Err(reason) => {
                            self.pause_voices(format!("The speaker model could not be loaded: {reason}"));
                            Err(VoiceEnd::Failed(reason))
                        }
                    };
                    match turn {
                        Ok(_) if running.next < running.windows.len() => {
                            if let Some(voicing) = self.lock().voicing.as_mut() {
                                voicing.done = running.next as u32;
                            }
                            self.repaint();
                        }
                        Ok(_) => {
                            self.finish_voice(running, Ok(()));
                            voice = None;
                        }
                        Err(end) => {
                            self.finish_voice(running, Err(end));
                            voice = None;
                        }
                    }
                    worked = true;
                }
            }

            if !worked && self.lock().selftest == SelfTest::Requested {
                power.set(true);
                let mut result = self.guarded(real.then(|| "running the self-test".to_string()), || self.run_selftest(model), |result| matches!(result, SelfTest::Finished { .. }));
                // The speaker network on the same clip, outside transcription's breadcrumb:
                // a fault in it is charged to speaker labels, not to transcription.
                if let (SelfTest::Finished { detail, .. }, true, true) = (&mut result, real, self.lock().recording.is_none()) {
                    if let Some(verdict) = self.check_voices(voices) {
                        self.log.info(format!("self-test: {verdict}"));
                        detail.push(' ');
                        detail.push_str(&verdict);
                    }
                }
                self.lock().selftest = result;
                self.repaint();
                worked = true;
            }

            if worked {
                idle_since = Instant::now();
                continue;
            }
            if idle_since.elapsed() >= POWER_LINGER {
                power.set(false);
            }
            let recording = self.lock().recording.is_some();
            if !recording && idle_since.elapsed() >= IDLE_UNLOAD {
                break;
            }
            self.wait(if recording { Duration::from_millis(250) } else { Duration::from_secs(1) });
        }
        // Leaving with a note half done: what it has is saved, and it goes back in the queue.
        if let Some(running) = job.as_ref() {
            // Unless it was cancelled or deleted in the meantime.
            self.finish_job(running, Err(WhisperError::Cancelled), !running.cancel.load(Ordering::Relaxed));
        }
        // A speaker pass cut short starts over next session; its windows are cheap.
        if let Some(running) = voice.as_ref() {
            if !running.cancel.load(Ordering::Relaxed) {
                self.lock().voices.push_front((running.id.clone(), running.fixed));
            }
            self.finish_voice(running, Err(VoiceEnd::Cancelled));
        }
    }

    /// Stop speaker passes starting by themselves, and let go of the ones waiting.
    fn pause_voices(&self, reason: String) {
        self.log.warn(reason.clone());
        let mut shared = self.lock();
        shared.voices.clear();
        shared.voices_paused = Some(reason);
    }

    /// Open the next note waiting for its speaker pass.
    fn next_voice_job(&self) -> Option<VoiceJob> {
        loop {
            let cancel = Arc::new(AtomicBool::new(false));
            let (id, fixed) = {
                // Leaving the queue and becoming the pass under way is one step, as for a
                // transcription: in between, the engine would look idle.
                let mut shared = self.lock();
                let (id, fixed) = shared.voices.pop_front()?;
                shared.voicing = Some(Voicing { id: id.clone(), done: 0, total: 0, cancel: cancel.clone() });
                (id, fixed)
            };
            let opened = self.library.find(&id).filter(|note| note.state == NoteState::Done && !note.segments.is_empty()).ok_or_else(|| "it has no finished transcript".to_string()).and_then(|note| {
                let reader = WavReader::open(&self.library.audio_path(&id)).map_err(|e| format!("the audio could not be opened: {e}"))?;
                let info = reader.info();
                if info.rate != RATE {
                    return Err(format!("the audio is {} Hz", info.rate));
                }
                let sample = |seconds: f32| (seconds.max(0.0) as f64 * RATE as f64).round() as u64;
                let spans: Vec<Span> = note.segments.iter().map(|seg| Span { start: sample(seg.start), end: sample(seg.end) }).collect();
                let windows = local_speaker::plan(&spans, info.samples);
                Ok(VoiceJob { id: id.clone(), fixed, key: note.voices_key(), spans: spans.len(), windows, next: 0, prints: Prints::default(), reader, cancel: cancel.clone(), started: Instant::now(), audio: vec![0.0; WINDOW_SAMPLES], window: vec![0.0; WINDOW_SAMPLES] })
            });
            match opened {
                Ok(job) => {
                    if let Some(voicing) = self.lock().voicing.as_mut() {
                        voicing.total = job.windows.len() as u32;
                    }
                    self.log.info(format!("telling the speakers of {id} apart over {} windows", job.windows.len()));
                    self.sync_service();
                    self.repaint();
                    return Some(job);
                }
                Err(reason) => {
                    self.log.info(format!("{id}: no speaker pass, {reason}"));
                    self.edit_note(&id, |note| note.speakers_due = false);
                    self.lock().voicing = None;
                    self.sync_service();
                }
            }
        }
    }

    /// Record how a speaker pass ended. As with a transcription the note is put right first
    /// and the pass stops being under way last.
    fn finish_voice(&self, job: &VoiceJob, result: Result<(), VoiceEnd>) {
        match result {
            Ok(()) => {
                let labels = local_speaker::cluster(&job.prints, job.spans, job.fixed.map(usize::from));
                let applied = self.edit_note(&job.id, |note| {
                    // A transcript replaced while this ran has other speakers, and its own pass.
                    if note.state == NoteState::Done && note.voices_key() == job.key {
                        note.set_speakers(&labels.spans, labels.speakers, job.fixed.is_some());
                    }
                });
                if applied.is_some_and(|note| note.voices_key() == job.key) {
                    // Kept, so the count can be changed later without the NPU.
                    if let Err(e) = job.prints.save(&self.library.voices_path(&job.id), job.key) {
                        self.log.warn(format!("voice prints of {} not saved: {e}", job.id));
                    }
                    self.log.info(format!("{}: {} speaker{} in {:.1} s", job.id, labels.speakers, if labels.speakers == 1 { "" } else { "s" }, job.started.elapsed().as_secs_f32()));
                    self.export_note(&job.id);
                }
            }
            Err(VoiceEnd::Cancelled) => {}
            Err(VoiceEnd::Failed(reason)) => {
                self.log.warn(format!("speaker pass of {} failed: {reason}", job.id));
                self.edit_note(&job.id, |note| note.speakers_due = false);
            }
        }
        self.lock().voicing = None;
        self.sync_service();
        self.changed();
    }

    fn live_window(&self, id: &str, long: &LongForm, window: &local_whisper::Window, model: &str) {
        let language = long.language().map(str::to_string);
        let segments = {
            let mut shared = self.lock();
            shared.stats.add(window.to - window.from, &window.timings);
            let Some(recording) = shared.recording.as_mut().filter(|r| r.id == id) else { return };
            recording.segments.extend(window.segments.iter().map(seg));
            recording.preview.clear();
            recording.seek = window.to;
            recording.language = language.clone();
            recording.revision += 1;
            recording.segments.clone()
        };
        // Saved as it goes, so a crash mid-recording loses at most the window being decoded.
        self.edit_note(id, |note| {
            note.segments = segments;
            note.seek = window.to;
            note.language = language;
            note.model = Some(model.to_string());
        });
    }

    fn live_preview(&self, id: &str, segments: &[Segment]) {
        if let Some(recording) = self.lock().recording.as_mut().filter(|r| r.id == id) {
            recording.preview = segments.iter().map(seg).collect();
            recording.revision += 1;
        }
        self.repaint();
    }

    /// Open the next queued note that can actually be transcribed.
    fn next_job(&self, model: &dyn Whisper, name: &str) -> Option<Job> {
        loop {
            let cancel = Arc::new(AtomicBool::new(false));
            let id = {
                // Leaving the queue and becoming the active job is one step: in between, the
                // engine would look idle, and the recording service would be let go.
                let mut shared = self.lock();
                let id = shared.queue.pop_front()?;
                shared.active = Some(Active { id: id.clone(), done: 0, total: 0, cancel: cancel.clone() });
                id
            };
            let opened = self.library.find(&id).ok_or_else(|| "The note could not be read.".to_string()).and_then(|note| {
                let reader = WavReader::open(&self.library.audio_path(&id)).map_err(|e| format!("The audio could not be opened: {e}"))?;
                let info = reader.info();
                if info.rate != RATE {
                    return Err(format!("The audio is {} Hz; only 16 kHz recordings can be transcribed.", info.rate));
                }
                let seek = note.seek.min(info.samples);
                let long = self.long_form(model, self.settings().forced_language(), seek, note.language.as_deref()).map_err(|e| e.to_string())?;
                Ok((long, reader, info.samples, seek))
            });
            match opened {
                Ok((long, reader, total, seek)) => {
                    if let Some(active) = self.lock().active.as_mut() {
                        (active.done, active.total) = (seek, total);
                    }
                    self.edit_note(&id, |note| {
                        note.state = NoteState::Transcribing;
                        note.error = None;
                        note.duration_ms = total * 1000 / RATE as u64;
                        // Counted until the job is seen to end; see `Library::recover`.
                        note.attempts += 1;
                    });
                    self.log.info(format!("transcribing {id} from {}", clock::duration(seek as f64 / RATE as f64)));
                    self.sync_service();
                    return Some(Job { id, long, reader, total, cancel, model: name.to_string(), started: Instant::now() });
                }
                Err(reason) => {
                    self.lock().active = None;
                    self.log.warn(format!("{id}: {reason}"));
                    // A note deleted while it waited has nothing left to mark.
                    self.edit_note(&id, |note| {
                        note.state = NoteState::Failed;
                        note.error = Some(reason.clone());
                    });
                    self.sync_service();
                }
            }
        }
    }

    fn job_window(&self, job: &Job, window: &local_whisper::Window) {
        {
            let mut shared = self.lock();
            shared.stats.add(window.to - window.from, &window.timings);
            if let Some(active) = shared.active.as_mut().filter(|a| a.id == job.id) {
                active.done = window.to;
            }
        }
        let language = job.long.language().map(str::to_string);
        self.edit_note(&job.id, |note| {
            // More transcript: whoever was told apart before was told apart in less of it.
            if note.speakers.is_some() {
                note.clear_speakers();
            }
            note.segments.extend(window.segments.iter().map(seg));
            note.seek = window.to;
            note.language = language;
            note.model = Some(job.model.clone());
        });
    }

    /// Record how a job ended. The note is put right first and the job stops being active
    /// last, so the engine never looks finished while the note, or its text file, is not;
    /// `requeue` puts it back at the front in that same last step.
    fn finish_job(&self, job: &Job, result: Result<(), WhisperError>, requeue: bool) {
        match result {
            Ok(()) => {
                let speakers = self.auto_speakers();
                let done = self.edit_note(&job.id, |note| {
                    note.state = NoteState::Done;
                    note.seek = job.total;
                    note.error = None;
                    note.attempts = 0;
                    note.speakers_due = speakers && !note.segments.is_empty();
                });
                self.log.info(format!("transcribed {} in {:.1} s", job.id, job.started.elapsed().as_secs_f32()));
                self.export_note(&job.id);
                if done.is_some_and(|note| note.speakers_due) {
                    self.queue_voices(&job.id, None);
                }
            }
            Err(WhisperError::Cancelled) => {
                self.edit_note(&job.id, |note| (note.state, note.attempts) = (NoteState::Pending, 0));
            }
            Err(e) => {
                self.log.warn(format!("transcription of {} failed: {e}", job.id));
                self.edit_note(&job.id, |note| {
                    note.state = NoteState::Failed;
                    note.error = Some(e.to_string());
                    note.attempts = 0;
                });
            }
        }
        {
            let mut shared = self.lock();
            shared.active = None;
            if requeue {
                shared.queue.push_front(job.id.clone());
            }
        }
        self.sync_service();
        self.changed();
    }

    /// Transcribe the bundled clip and report what came out and how fast.
    fn run_selftest(&self, model: &mut dyn Whisper) -> SelfTest {
        self.lock().selftest = SelfTest::Running;
        self.repaint();
        let audio: Vec<f32> = SELFTEST_PCM.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
        let started = Instant::now();
        let outcome = (|| -> Result<(String, Timings, Option<&'static str>), WhisperError> {
            let mut long = LongForm::new(model.dims(), DecodeOptions::default())?;
            let never = AtomicBool::new(false);
            let (mut source, mut segments, mut timings) = (&audio[..], Vec::new(), Timings::default());
            while let Advance::Window(window) = long.advance(model, &mut source, audio.len() as u64, true, &never)? {
                timings.add(&window.timings);
                segments.extend(window.segments);
            }
            Ok((local_whisper::join_text(&segments), timings, long.language()))
        })();
        let result = match outcome {
            Ok((text, timings, language)) => {
                let seconds = audio.len() as f64 / RATE as f64;
                let detail = format!(
                    "{seconds:.1} s of audio in {:.2} s. Encoder {:.0} ms, decoder {:.1} ms/token over {} tokens. Language {}.",
                    started.elapsed().as_secs_f32(),
                    timings.encode.as_secs_f32() * 1000.0,
                    timings.ms_per_step(),
                    timings.steps,
                    language.unwrap_or("?"),
                );
                self.log.info(format!("self-test: {text:?}; {detail}"));
                let matched = text.to_lowercase().contains(SELFTEST_PHRASE);
                SelfTest::Finished { text, detail, matched }
            }
            Err(e) => {
                self.log.warn(format!("self-test failed: {e}"));
                SelfTest::Failed(e.to_string())
            }
        };
        result
    }
}

impl Engine {
    /// Run the speaker network over the start of the self-test clip and say how close its
    /// print is to the reference. `None` where there is no network to check.
    fn check_voices(&self, voices: &mut dyn Voices) -> Option<String> {
        self.paths.speaker_pack.as_ref()?;
        let audio: Vec<f32> = SELFTEST_PCM.chunks_exact(2).take(WINDOW_SAMPLES).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
        let embedder = match voices.embedder(&self.voice_guard, &self.log) {
            Ok(embedder) => embedder,
            Err(reason) => return Some(format!("Speaker model: could not be loaded ({reason}).")),
        };
        let reference: Vec<f32> = SELFTEST_PRINT.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let mut print = [0f32; DIM];
        let started = Instant::now();
        let ran = Self::guarded_by(&self.voice_guard, Some("running the speaker self-test".into()), || embedder.embed(&audio, &mut print), |r| r.is_ok());
        if let Err(e) = ran {
            return Some(format!("Speaker model: failed ({e})."));
        }
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let likeness = dot(&print, &reference) / (dot(&print, &print) * dot(&reference, &reference)).sqrt().max(1e-9);
        // fp16 on the NPU against fp32 on a desktop agree to four places when all is well.
        let verdict = if likeness > 0.99 { "matches its reference" } else { "does NOT match its reference; speaker labels will be wrong" };
        Some(format!("Speaker model: {verdict} ({likeness:.4}, {:.0} ms a window).", started.elapsed().as_secs_f32() * 1000.0))
    }
}

/// Why a speaker pass stopped before its last window.
enum VoiceEnd {
    Cancelled,
    Failed(String),
}

/// A finished note having its speakers told apart.
struct VoiceJob {
    id: String,
    /// The number of voices to find, if it was given.
    fixed: Option<u8>,
    /// What the transcript was when the pass began; see [`Note::voices_key`].
    key: u64,
    spans: usize,
    windows: Vec<Window>,
    next: usize,
    prints: Prints,
    reader: WavReader,
    cancel: Arc<AtomicBool>,
    started: Instant,
    audio: Vec<f32>,
    window: Vec<f32>,
}

impl VoiceJob {
    /// Take the next windows through the network for about [`VOICE_SLICE`], and say how many.
    fn advance(&mut self, embedder: &mut dyn Embedder) -> Result<usize, VoiceEnd> {
        let started = Instant::now();
        let mut done = 0;
        let mut print = [0f32; DIM];
        while let Some(window) = self.windows.get(self.next).copied() {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(VoiceEnd::Cancelled);
            }
            let wanted = window.len as usize;
            let got = self.reader.read(window.start, &mut self.audio[..wanted]).map_err(|e| VoiceEnd::Failed(format!("reading the audio: {e}")))?;
            local_speaker::fill(&self.audio[..got], &mut self.window);
            embedder.embed(&self.window, &mut print).map_err(|e| VoiceEnd::Failed(e.to_string()))?;
            self.prints.push(Window { len: got as u32, ..window }, &print);
            self.next += 1;
            done += 1;
            if started.elapsed() >= VOICE_SLICE {
                break;
            }
        }
        Ok(done)
    }
}

/// The pass following the recording in progress.
struct Live {
    id: String,
    /// The language forced in Settings when it was set up; a change sets it up afresh.
    forced: Option<String>,
    long: LongForm,
    reader: WavReader,
}

/// A queued note being transcribed.
struct Job {
    id: String,
    long: LongForm,
    reader: WavReader,
    total: u64,
    cancel: Arc<AtomicBool>,
    model: String,
    started: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(name: &str, edit: impl FnOnce(&mut Settings)) -> Arc<Engine> {
        let root = std::env::temp_dir().join(format!("scribe-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = Settings::default();
        settings.simulate = true;
        edit(&mut settings);
        settings.save(&root.join("settings.json")).unwrap();
        Engine::start(Paths { settings: root.join("settings.json"), library: root.join("notes"), models: root.join("whisper"), native_libs: None, documents: Some(root.join("Documents").join("Scribe")), speaker_pack: None }).unwrap()
    }

    fn speech(seconds: usize) -> Vec<i16> {
        (0..seconds * RATE as usize).map(|i| ((i as f32 * 0.05).sin() * 9_000.0) as i16).collect()
    }

    fn wait_for(engine: &Engine, what: &str, done: impl Fn(&Status) -> bool) -> Status {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = engine.status();
            if done(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {status:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn only_note(engine: &Engine) -> Note {
        let mut notes = engine.library.list();
        assert_eq!(notes.len(), 1);
        notes.remove(0)
    }

    #[test]
    fn a_recording_becomes_a_transcribed_note() {
        let engine = engine("record", |_| {});
        let mut sink = engine.prepare_recording().unwrap();
        assert!(engine.prepare_recording().is_err(), "one recording at a time");
        for chunk in speech(8).chunks(1_600) {
            sink(chunk, RATE);
        }
        let live = engine.status().recording.unwrap();
        assert_eq!(live.samples, 8 * RATE as u64);
        assert!(live.level > 0.2);

        engine.stop_recording();
        assert!(engine.status().recording.unwrap().stopping);
        // On a phone the service confirms; here the test stands in for it.
        engine.finalize_recording();
        wait_for(&engine, "the transcript", |s| s.queue.is_empty() && s.active.is_none() && s.recording.is_none());

        let note = only_note(&engine);
        assert_eq!((note.state, note.duration_ms, note.seek, note.attempts), (NoteState::Done, 8_000, 8 * RATE as u64, 0));
        assert_eq!(note.segments[0].text, "This is a simulated transcript.");
        assert_eq!(note.language.as_deref(), Some("en"));
        assert_eq!(note.model.as_deref(), Some("simulated"));
        assert!(note.segments.iter().all(|s| s.end <= 8.0));
        let info = crate::wav::probe(&engine.library.audio_path(&note.id)).unwrap();
        assert_eq!(info.samples, 8 * RATE as u64);
        assert!(engine.status().stats.windows >= 1);
    }

    /// The text files in the engine's documents folder, by name.
    fn text_files(engine: &Engine) -> Vec<(String, String)> {
        let dir = engine.exporter.as_ref().unwrap().dir();
        let mut files: Vec<(String, String)> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(e.path()).unwrap())).collect();
        files.sort();
        files
    }

    #[test]
    fn a_finished_note_is_also_kept_as_a_text_file() {
        let engine = engine("textfile", |_| {});
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(8), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        wait_for(&engine, "the transcript", |s| s.idle());

        // Idle means done: the file is there by the time nothing is running.
        let note = only_note(&engine);
        let export = note.export.clone().expect("the note records its file");
        // Named for when it was made and its first words.
        assert!(export.file.ends_with(" This is a simulated transcript. No speech.txt"), "{}", export.file);
        assert_eq!(text_files(&engine), [(export.file.clone(), format!("{}\n", note.text()))]);
        assert!(engine.log.lines().iter().any(|line| line.contains("text saved as Documents/Scribe/")));
        assert_eq!(engine.status().export_error, None);

        // An edit reaches the file when the screen asks, and a title renames it.
        engine.edit_note(&note.id, |note| (note.title, note.body, note.body_covers) = ("Standup".into(), Some("My own words".into()), note.segments.len()));
        assert_eq!(text_files(&engine)[0].0, export.file, "not before it is asked for");
        engine.export_note(&note.id);
        let files = text_files(&engine);
        assert_eq!(files.len(), 1);
        assert!(files[0].0.ends_with(" Standup.txt") && files[0].1 == "My own words\n", "{files:?}");

        // Transcribing again replaces the text in the same file.
        engine.transcribe(&note.id, true);
        wait_for(&engine, "the second pass", |s| s.idle());
        let files = text_files(&engine);
        let again = engine.library.load(&note.id).unwrap();
        assert!(again.text().contains("simulated") && again.body.is_none());
        assert_eq!(files, [(again.export.clone().unwrap().file, format!("{}\n", again.text()))]);

        // The file outlives its note.
        engine.delete_note(&note.id).unwrap();
        assert_eq!(text_files(&engine), files);
    }

    #[test]
    fn a_notes_text_can_be_had_as_a_file_whatever_state_it_is_in() {
        let engine = engine("sharefile", |_| {});
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(8), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        wait_for(&engine, "the transcript", |s| s.idle());
        let note = only_note(&engine);

        // A finished note: its own file in the documents folder, as it stands.
        let file = engine.text_file(&note.id).unwrap();
        assert!(file.stored && file.name == note.export.clone().unwrap().file);
        assert_eq!(file.path, engine.exporter.as_ref().unwrap().dir().join(&file.name));
        // An edit the file has not seen is written before it is handed on.
        engine.edit_note(&note.id, |note| (note.body, note.body_covers) = (Some("Tidied by hand.".into()), note.segments.len()));
        let file = engine.text_file(&note.id).unwrap();
        assert!(file.stored);
        assert_eq!(std::fs::read_to_string(&file.path).unwrap(), "Tidied by hand.\n");

        // With text files switched off there is no such file, and a copy is made instead:
        // the documents folder is left exactly as it was.
        engine.update_settings(|s| s.save_text = false);
        engine.edit_note(&note.id, |note| note.body = Some("Changed again.".into()));
        let copy = engine.text_file(&note.id).unwrap();
        assert!(!copy.stored && copy.name.ends_with(" Changed again.txt"), "{copy:?}");
        assert_eq!(std::fs::read_to_string(&copy.path).unwrap(), "Changed again.\n");
        assert!(copy.path.starts_with(engine.paths.library.parent().unwrap().join("shared")));
        assert_eq!(text_files(&engine).len(), 1);
        assert_eq!(text_files(&engine)[0].1, "Tidied by hand.\n");
        // One copy at a time.
        engine.edit_note(&note.id, |note| note.title = "Retitled".into());
        let next = engine.text_file(&note.id).unwrap();
        assert!(next.path.exists() && !copy.path.exists());

        // A note still waiting for its transcript has only what was typed into it.
        engine.update_settings(|s| (s.save_text, s.auto_transcribe) = (true, false));
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(3), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        let waiting = engine.library.list().into_iter().find(|n| n.state == NoteState::Pending).unwrap();
        assert_eq!(engine.text_file(&waiting.id), Err("There is no text to share yet.".into()));
        engine.edit_note(&waiting.id, |note| note.body = Some("Typed before any transcript.".into()));
        let typed = engine.text_file(&waiting.id).unwrap();
        assert!(!typed.stored);
        assert_eq!(std::fs::read_to_string(&typed.path).unwrap(), "Typed before any transcript.\n");
        assert_eq!(engine.text_file("no-such-note"), Err("The note is gone.".into()));
    }

    #[test]
    fn a_recording_whose_text_was_complete_when_it_stopped_gets_its_file_at_once() {
        let engine = engine("textlive", |_| {});
        let mut sink = engine.prepare_recording().unwrap();
        // Exactly one window: the live pass has all of it before the stop.
        for chunk in speech(30).chunks(1_600) {
            sink(chunk, RATE);
        }
        wait_for(&engine, "the live window", |s| s.recording.as_ref().is_some_and(|r| r.revision > 0));
        engine.stop_recording();
        engine.finalize_recording();
        let note = only_note(&engine);
        assert_eq!(note.state, NoteState::Done);
        assert_eq!(text_files(&engine), [(note.export.clone().unwrap().file, format!("{}\n", note.text()))]);
    }

    #[test]
    fn text_files_can_be_off_and_catch_up_when_switched_on() {
        let engine = engine("textoff", |s| s.save_text = false);
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(8), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        wait_for(&engine, "the transcript", |s| s.idle());
        assert_eq!(only_note(&engine).export, None);
        assert!(text_files(&engine).is_empty());
        engine.export_all();

        engine.update_settings(|s| s.save_text = true);
        engine.export_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        while only_note(&engine).export.is_none() {
            assert!(Instant::now() < deadline, "the note never got its file");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(text_files(&engine).len(), 1);
    }

    #[test]
    fn a_folder_that_cannot_be_written_is_reported_once_and_costs_nothing_else() {
        let root = std::env::temp_dir().join(format!("scribe-engine-textblocked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = Settings::default();
        settings.simulate = true;
        settings.save(&root.join("settings.json")).unwrap();
        // A file where the folder should be, as on a phone that refuses the app its Documents.
        std::fs::write(root.join("Documents"), b"").unwrap();
        let engine = Engine::start(Paths { settings: root.join("settings.json"), library: root.join("notes"), models: root.join("whisper"), native_libs: None, documents: Some(root.join("Documents").join("Scribe")), speaker_pack: None }).unwrap();
        for _ in 0..2 {
            let mut sink = engine.prepare_recording().unwrap();
            sink(&speech(4), RATE);
            engine.stop_recording();
            engine.finalize_recording();
            wait_for(&engine, "the transcript", |s| s.idle());
        }
        assert!(engine.library.list().iter().all(|note| note.state == NoteState::Done && note.export.is_none()));
        assert!(engine.status().export_error.is_some());
        assert!(engine.take_notice().is_some_and(|notice| notice.starts_with("The text file could not be saved in Documents/Scribe")));
        // The second failure is for the log, not the screen.
        assert_eq!(engine.take_notice(), None);
    }

    /// Two "people" taking turns of six seconds, which is how long each scripted sentence
    /// lasts: a low tone and a high one, which is all the scripted speaker network hears.
    fn conversation(turns: usize) -> Vec<i16> {
        (0..turns * 6 * RATE as usize).map(|i| ((i as f32 * if (i / (6 * RATE as usize)) % 2 == 0 { 0.05 } else { 0.2 }).sin() * 9_000.0) as i16).collect()
    }

    fn record(engine: &Engine, pcm: &[i16]) -> String {
        let mut sink = engine.prepare_recording().unwrap();
        for chunk in pcm.chunks(1_600) {
            sink(chunk, RATE);
        }
        engine.stop_recording();
        engine.finalize_recording();
        wait_for(engine, "the engine to finish", |s| s.idle());
        engine.library.list().into_iter().max_by_key(|note| note.id.clone()).unwrap().id
    }

    #[test]
    fn the_voices_in_a_recording_are_told_apart_and_can_be_named() {
        let engine = engine("voices", |_| {});
        let id = record(&engine, &conversation(8));
        let note = engine.library.load(&id).unwrap();
        assert_eq!(note.state, NoteState::Done);
        assert_eq!(note.speakers, Some(crate::library::Speakers { count: 2, fixed: false, names: vec![] }));
        assert!(!note.speakers_due);
        // Whoever speaks first is speaker one, and the turns alternate from there. The
        // script's last sentences fall past the end of the audio; with nothing to hear in
        // them they go with the turn before.
        let turns: Vec<Option<u8>> = note.segments.iter().map(|seg| seg.speaker).collect();
        assert_eq!(turns, [0, 1, 0, 1, 0, 1, 0, 1, 1, 1].map(Some));
        assert!(note.text().starts_with("Speaker 1: This is a simulated transcript.\n\nSpeaker 2: "), "{}", note.text());
        // The file follows, and the title is still the first words rather than a name.
        assert_eq!(text_files(&engine), [(note.export.clone().unwrap().file, format!("{}\n", note.text()))]);
        assert!(note.export.unwrap().file.ends_with(" This is a simulated transcript. No speech.txt"));
        assert!(engine.log.lines().iter().any(|line| line.contains("2 speakers")));

        // A name reaches the text and the file.
        engine.rename_speaker(&id, 1, "Maya");
        let named = engine.library.load(&id).unwrap();
        assert!(named.text().contains("\n\nMaya: ") && !named.text().contains("Speaker 2"));
        assert_eq!(text_files(&engine)[0].1, format!("{}\n", named.text()));

        // The count can be set by hand, at once and without the engine: the prints are kept.
        assert!(engine.library.voices_path(&id).exists());
        engine.find_speakers(&id, Some(1));
        let one = engine.library.load(&id).unwrap();
        assert_eq!(one.speakers.as_ref().map(|s| (s.count, s.fixed)), Some((1, true)));
        assert!(!one.text().contains("Speaker") && !one.text().contains("Maya"));
        assert_eq!(text_files(&engine)[0].1, format!("{}\n", one.text()));
        engine.find_speakers(&id, None);
        let back = engine.library.load(&id).unwrap();
        assert_eq!(back.speakers.as_ref().map(|s| (s.count, s.fixed, s.name(1))), Some((2, false, "Maya".into())));
        assert!(engine.status().idle());
    }

    #[test]
    fn a_new_transcript_gets_its_own_speakers_and_an_old_note_gets_them_when_asked() {
        let engine = engine("voices-again", |s| s.speakers = false);
        let id = record(&engine, &conversation(6));
        // Switched off: nothing is told apart and nothing is owed.
        let plain = engine.library.load(&id).unwrap();
        assert_eq!((plain.speakers.clone(), plain.speakers_due), (None, false));
        assert!(!engine.library.voices_path(&id).exists());

        // Asked for on that one note, it runs all the same.
        engine.find_speakers(&id, None);
        wait_for(&engine, "the speaker pass", |s| s.idle());
        let found = engine.library.load(&id).unwrap();
        assert_eq!(found.speakers.as_ref().map(|s| s.count), Some(2));
        let key = found.voices_key();

        // Transcribing again throws the speakers away with the text, and finds them afresh.
        engine.update_settings(|s| s.speakers = true);
        engine.transcribe(&id, true);
        assert_eq!(engine.library.load(&id).unwrap().speakers, None);
        wait_for(&engine, "the second pass", |s| s.idle());
        let again = engine.library.load(&id).unwrap();
        assert_eq!((again.state, again.speakers.as_ref().map(|s| s.count)), (NoteState::Done, Some(2)));
        assert!(local_speaker::Prints::load(&engine.library.voices_path(&id), again.voices_key()).is_some());
        assert_eq!(again.voices_key(), key, "the scripted transcript has the same timings");

        // One voice all the way through is one speaker, and the text carries no labels.
        let solo = record(&engine, &speech(20));
        let memo = engine.library.load(&solo).unwrap();
        assert_eq!(memo.speakers.as_ref().map(|s| s.count), Some(1));
        assert!(!memo.text().contains("Speaker"));
        // A note deleted while its pass waits or runs is simply gone. Without its prints
        // the pass needs the network, slow here, so the delete lands on it.
        std::fs::remove_file(engine.library.voices_path(&solo)).unwrap();
        engine.sim_voice.store(30, Ordering::Relaxed);
        engine.request_unload();
        wait_for(&engine, "the slow network's session", |s| s.model == ModelStatus::Idle);
        engine.find_speakers(&solo, Some(2));
        wait_for(&engine, "the pass to start", |s| s.voicing.is_some());
        engine.delete_note(&solo).unwrap();
        wait_for(&engine, "the pass to be let go", |s| s.idle());
        assert_eq!(engine.library.list().len(), 1);
    }

    #[test]
    fn a_pass_waits_out_a_recording_and_survives_its_note_being_transcribed_again() {
        let engine = engine("voices-busy", |_| {});
        engine.sim_voice.store(25, Ordering::Relaxed);
        // A minute of conversation: thirty windows, most of a second at this pace.
        let mut sink = engine.prepare_recording().unwrap();
        for chunk in conversation(10).chunks(1_600) {
            sink(chunk, RATE);
        }
        engine.stop_recording();
        engine.finalize_recording();
        let id = only_note(&engine).id;
        wait_for(&engine, "the pass to start", |s| s.voicing.as_ref().is_some_and(|(_, done, _)| *done > 0));

        // A new recording starts. The pass stands still for as long as it runs.
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(1), RATE);
        std::thread::sleep(Duration::from_millis(400));
        let held = engine.status().voicing.expect("the pass is still under way").1;
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(engine.status().voicing.map(|(_, done, _)| done), Some(held), "no window while recording");
        assert_eq!(engine.library.load(&id).unwrap().speakers, None);

        // The first note is transcribed again under the pass: the old pass is let go, and
        // the new transcript gets one of its own once the recording is over.
        engine.transcribe(&id, true);
        sink(&speech(1), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        wait_for(&engine, "everything to finish", |s| s.idle());
        let again = engine.library.load(&id).unwrap();
        assert_eq!((again.state, again.speakers.as_ref().map(|s| s.count), again.speakers_due), (NoteState::Done, Some(2), false));
        assert_eq!(engine.library.list().len(), 2);
    }

    #[test]
    fn speakers_asked_for_with_no_model_do_not_wait_for_ever() {
        let engine = engine("voices-blocked", |s| s.simulate = false);
        // A finished note from a time there was a model.
        let mut note = engine.library.create(1_000).unwrap();
        let mut writer = WavWriter::create(&engine.library.audio_path(&note.id), RATE).unwrap();
        writer.write(&conversation(4)).unwrap();
        writer.finish().unwrap();
        note.segments = (0..4).map(|i| Seg { start: i as f32 * 6.0, end: i as f32 * 6.0 + 5.5, text: format!("Turn {i}."), speaker: None }).collect();
        (note.state, note.duration_ms) = (NoteState::Done, 24_000);
        engine.library.save(&note).unwrap();

        // No pack and no scripted engine: there is nothing to run the pass on, and the
        // screen offers nothing to ask with. Asked anyway, it does not sit in a queue.
        assert!(!engine.speakers_available());
        engine.find_speakers(&note.id, None);
        let status = wait_for(&engine, "the model to be found missing", |s| s.blocked);
        assert!(status.voices.is_empty() && status.voicing.is_none(), "{status:?}");
        assert_eq!(engine.library.load(&note.id).unwrap().speakers, None);

        // With something to run it on, asking again is all it takes.
        engine.update_settings(|s| s.simulate = true);
        engine.find_speakers(&note.id, None);
        wait_for(&engine, "the pass", |s| s.idle() && !s.blocked);
        assert_eq!(engine.library.load(&note.id).unwrap().speakers.map(|s| s.count), Some(2));
    }

    #[test]
    fn a_pass_owed_from_the_last_run_is_made_up_and_a_death_pauses_them() {
        let root = std::env::temp_dir().join(format!("scribe-engine-voices-owed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let library = Library::open(root.join("notes")).unwrap();
        let mut note = library.create(1_000).unwrap();
        let mut writer = WavWriter::create(&library.audio_path(&note.id), RATE).unwrap();
        writer.write(&conversation(4)).unwrap();
        writer.finish().unwrap();
        // Transcribed by the last process, which ended before telling anyone apart.
        note.segments = (0..4).map(|i| Seg { start: i as f32 * 6.0, end: i as f32 * 6.0 + 5.5, text: format!("Turn {i}."), speaker: None }).collect();
        (note.state, note.speakers_due, note.duration_ms) = (NoteState::Done, true, 24_000);
        library.save(&note).unwrap();
        let mut settings = Settings::default();
        settings.simulate = true;
        settings.save(&root.join("settings.json")).unwrap();
        let paths = || Paths { settings: root.join("settings.json"), library: root.join("notes"), models: root.join("whisper"), native_libs: None, documents: Some(root.join("Documents").join("Scribe")), speaker_pack: None };

        let engine = Engine::start(paths()).unwrap();
        wait_for(&engine, "the owed pass", |s| s.idle() && engine.library.load(&note.id).is_ok_and(|n| n.speakers.is_some()));
        let told = engine.library.load(&note.id).unwrap();
        assert_eq!(told.speakers.as_ref().map(|s| s.count), Some(2));
        assert_eq!(told.text(), "Speaker 1: Turn 0.\n\nSpeaker 2: Turn 1.\n\nSpeaker 1: Turn 2.\n\nSpeaker 2: Turn 3.");
        drop(engine);

        // The next process dies telling speakers apart. The one after leaves it alone.
        let mut owed = told.clone();
        owed.clear_speakers();
        owed.speakers_due = true;
        library.save(&owed).unwrap();
        std::fs::write(root.join("speaker-guard.json"), r#"{"busy":"telling the speakers of x apart","strikes":0}"#).unwrap();
        let engine = Engine::start(paths()).unwrap();
        let status = engine.status();
        assert!(status.voices_paused.as_deref().is_some_and(|why| why.starts_with("Speaker labelling is paused")), "{status:?}");
        assert!(status.voices.is_empty() && status.model == ModelStatus::Idle && !status.blocked);
        assert!(engine.take_notice().is_some_and(|notice| notice.starts_with("Speaker labelling is paused")));
        // Transcription is untouched by it, and its notes are not queued for speakers.
        let memo = record(&engine, &conversation(4));
        assert_eq!(engine.library.load(&memo).unwrap().speakers, None);
        // Asking for one note's speakers is what lifts it, once that takes the network: the
        // prints of the first run would otherwise answer without it.
        std::fs::remove_file(engine.library.voices_path(&note.id)).unwrap();
        engine.find_speakers(&note.id, None);
        wait_for(&engine, "the pass that was asked for", |s| s.idle());
        assert_eq!(engine.library.load(&note.id).unwrap().speakers.map(|s| s.count), Some(2));
        assert_eq!(engine.status().voices_paused, None);
    }

    #[test]
    fn long_recordings_are_transcribed_while_they_are_still_running() {
        let engine = engine("live", |_| {});
        let mut sink = engine.prepare_recording().unwrap();
        for chunk in speech(40).chunks(1_600) {
            sink(chunk, RATE);
        }
        // 40 s recorded and capture still open: the first 30 s window is final already.
        let status = wait_for(&engine, "a live window", |s| s.recording.as_ref().is_some_and(|r| r.revision > 0));
        assert!(!status.recording.unwrap().stopping);
        let (segments, _) = engine.live_transcript();
        assert_eq!(segments.len(), 5);
        let saved = only_note(&engine);
        assert_eq!((saved.state, saved.segments.len(), saved.seek), (NoteState::Recording, 5, 30 * RATE as u64));

        engine.stop_recording();
        engine.finalize_recording();
        wait_for(&engine, "the tail", |s| s.idle());
        let note = only_note(&engine);
        assert_eq!((note.state, note.seek), (NoteState::Done, 40 * RATE as u64));
        // The tail was decoded from where the live pass stopped, not from the start.
        assert!(note.segments.len() > 5);
        assert!(note.segments[5].start >= 30.0);
    }

    #[test]
    fn capture_at_another_rate_is_stored_at_16k() {
        let engine = engine("rate", |s| s.auto_transcribe = false);
        let mut sink = engine.prepare_recording().unwrap();
        let tone: Vec<i16> = (0..48_000 * 2).map(|i| ((i as f32 * 0.02).sin() * 9_000.0) as i16).collect();
        for chunk in tone.chunks(4_800) {
            sink(chunk, 48_000);
        }
        engine.stop_recording();
        engine.finalize_recording();
        let note = only_note(&engine);
        // Two seconds either way, within the filter's delay.
        assert!((note.duration_ms as i64 - 2_000).abs() < 20, "{} ms", note.duration_ms);
        // Without auto-transcribe the audio simply waits.
        assert_eq!(note.state, NoteState::Pending);
        assert!(engine.status().queue.is_empty());
        assert_eq!(engine.status().model, ModelStatus::Idle);
    }

    #[test]
    fn an_empty_recording_leaves_no_note_and_pausing_drops_audio() {
        let engine = engine("empty", |s| s.auto_transcribe = false);
        let _sink = engine.prepare_recording().unwrap();
        engine.stop_recording();
        engine.finalize_recording();
        assert!(engine.library.list().is_empty());
        assert_eq!(engine.take_notice().as_deref(), Some("Nothing was recorded."));
        assert_eq!(engine.take_notice(), None);

        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(1), RATE);
        engine.set_paused(true);
        sink(&speech(1), RATE);
        assert!(engine.status().recording.unwrap().paused);
        engine.set_paused(false);
        sink(&speech(1), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        assert_eq!(only_note(&engine).duration_ms, 2_000);
    }

    #[test]
    fn transcribing_later_resumes_restarts_and_cancels() {
        let engine = engine("later", |s| s.auto_transcribe = false);
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(12), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        let id = only_note(&engine).id;

        engine.transcribe(&id, false);
        wait_for(&engine, "the first pass", |s| s.idle());
        let first = engine.library.load(&id).unwrap();
        assert_eq!(first.state, NoteState::Done);

        // A user edit survives until they ask for the transcript to be redone.
        engine.edit_note(&id, |note| note.body = Some("my version".into()));
        engine.transcribe(&id, true);
        wait_for(&engine, "the second pass", |s| s.idle());
        let second = engine.library.load(&id).unwrap();
        assert_eq!((second.state, second.body.clone()), (NoteState::Done, None));
        assert_eq!(second.segments.len(), first.segments.len());

        engine.transcribe(&id, true);
        engine.cancel(&id);
        wait_for(&engine, "the cancel", |s| s.idle());
        assert_ne!(engine.library.load(&id).unwrap().state, NoteState::Transcribing);
        engine.delete_note(&id).unwrap();
        assert!(engine.library.list().is_empty());
    }

    #[test]
    fn with_no_model_the_audio_is_kept_and_the_note_waits() {
        let engine = engine("nomodel", |s| s.simulate = false);
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(5), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        let status = wait_for(&engine, "the model to be found missing", |s| s.blocked);
        assert_eq!(status.model, ModelStatus::Missing);
        let note = only_note(&engine);
        assert_eq!((note.state, note.duration_ms), (NoteState::Pending, 5_000));
        // Still queued, for when a pack arrives.
        assert_eq!(status.queue, [note.id.clone()]);

        // Switching the scripted engine on is a new choice, so the queue runs.
        engine.update_settings(|s| s.simulate = true);
        wait_for(&engine, "the queue to drain", |s| s.idle());
        assert_eq!(engine.library.load(&note.id).unwrap().state, NoteState::Done);
    }

    #[test]
    fn a_session_that_ends_mid_recording_is_followed_from_where_the_text_ends() {
        let engine = engine("swap", |_| {});
        let mut sink = engine.prepare_recording().unwrap();
        for chunk in speech(40).chunks(1_600) {
            sink(chunk, RATE);
        }
        wait_for(&engine, "the first window", |s| s.recording.as_ref().is_some_and(|r| r.revision > 0));
        let (before, _) = engine.live_transcript();
        assert_eq!(before.len(), 5);

        // The model is let go, as a change of pack would: this session ends and a new one
        // takes the recording over.
        engine.request_unload();
        for chunk in speech(35).chunks(1_600) {
            sink(chunk, RATE);
        }
        wait_for(&engine, "the second window", |_| engine.live_transcript().0.len() > before.len());
        let (after, _) = engine.live_transcript();
        assert_eq!(after[..5], before[..], "the first half minute is not transcribed twice");
        assert!(after[5].start >= 30.0);
        assert!(after.windows(2).all(|pair| pair[0].start <= pair[1].start));
        engine.stop_recording();
        engine.finalize_recording();
        wait_for(&engine, "the tail", |s| s.idle());
        assert_eq!(only_note(&engine).state, NoteState::Done);
    }

    #[test]
    fn a_capture_that_fails_says_why() {
        let engine = engine("failure", |s| s.auto_transcribe = false);
        let _sink = engine.prepare_recording().unwrap();
        engine.finalize(Some("the microphone is in use or not permitted".into()));
        assert!(engine.library.list().is_empty());
        assert_eq!(engine.take_notice().as_deref(), Some("Recording failed: the microphone is in use or not permitted"));

        // Audio captured before the failure is a note like any other.
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(2), RATE);
        engine.finalize(Some("microphone read failed (-6)".into()));
        assert_eq!(only_note(&engine).duration_ms, 2_000);
        assert_eq!(engine.take_notice().as_deref(), Some("Recording stopped early: microphone read failed (-6)"));
    }

    #[test]
    fn audio_that_lost_its_note_file_can_still_be_transcribed() {
        let engine = engine("orphan", |s| s.auto_transcribe = false);
        // Dropped into the library while the app runs, as `adb push` would.
        std::fs::create_dir_all(engine.library.note_dir("pushed")).unwrap();
        let mut writer = WavWriter::create(&engine.library.audio_path("pushed"), RATE).unwrap();
        writer.write(&speech(6)).unwrap();
        writer.finish().unwrap();
        assert_eq!(only_note(&engine).state, NoteState::Pending);
        assert!(engine.library.load("pushed").is_err(), "listing wrote nothing");

        engine.transcribe("pushed", false);
        wait_for(&engine, "the transcript", |s| s.idle());
        let note = engine.library.load("pushed").unwrap();
        assert_eq!((note.state, note.duration_ms), (NoteState::Done, 6_000));
        assert!(!note.segments.is_empty());

        // Deleting is final: a late edit does not bring the folder back.
        engine.delete_note("pushed").unwrap();
        assert!(engine.edit_note("pushed", |note| note.pinned = true).is_none());
        assert!(engine.library.list().is_empty());
    }

    #[test]
    fn after_two_deaths_at_the_npu_transcription_waits_to_be_asked() {
        let root = std::env::temp_dir().join(format!("scribe-engine-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let library = Library::open(root.join("notes")).unwrap();
        let note = library.create(1_000).unwrap();
        let mut writer = WavWriter::create(&library.audio_path(&note.id), RATE).unwrap();
        writer.write(&speech(6)).unwrap();
        std::mem::forget(writer);
        let mut settings = Settings::default();
        settings.simulate = true;
        settings.save(&root.join("settings.json")).unwrap();
        // The last process died with the NPU busy, for the second time running.
        std::fs::write(root.join("engine-guard.json"), r#"{"busy":"loading whisper-base","strikes":1}"#).unwrap();

        let engine = Engine::start(Paths { settings: root.join("settings.json"), library: root.join("notes"), models: root.join("whisper"), native_libs: None, documents: Some(root.join("Documents").join("Scribe")), speaker_pack: None }).unwrap();
        let status = engine.status();
        assert!(status.blocked && status.queue.is_empty(), "{status:?}");
        assert!(matches!(status.model, ModelStatus::Paused(_)));
        assert!(engine.take_notice().is_some_and(|notice| notice.starts_with("Transcription is paused")));
        assert!(engine.log.lines().iter().any(|line| line.contains("loading whisper-base")));
        // The recording was recovered all the same, and nothing starts on it by itself.
        std::thread::sleep(Duration::from_millis(300));
        let waiting = engine.library.load(&note.id).unwrap();
        assert_eq!((waiting.state, waiting.duration_ms), (NoteState::Pending, 6_000));
        assert_eq!(engine.status().stats.windows, 0);

        // Recording carries on regardless, as audio.
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(2), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        assert_eq!(engine.library.list().len(), 2);
        assert_eq!(engine.status().model, ModelStatus::Paused(PAUSED.into()));

        // Asking is all it takes.
        engine.transcribe(&note.id, false);
        wait_for(&engine, "the transcript", |s| s.idle());
        assert_eq!(engine.library.load(&note.id).unwrap().state, NoteState::Done);
    }

    #[test]
    fn stopping_a_recording_does_not_wait_for_the_engine() {
        let engine = engine("busy", |s| s.auto_transcribe = false);
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(30), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        let long = only_note(&engine).id;
        // Slow enough that one window keeps the engine busy for a couple of seconds.
        engine.sim_step.store(40, Ordering::Relaxed);
        engine.transcribe(&long, false);
        wait_for(&engine, "the long note to start", |s| s.active.is_some());

        // Meanwhile a memo is recorded and stopped, and the service confirms the stop.
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(2), RATE);
        let stopped = Instant::now();
        engine.stop_recording();
        engine.injected.lock().unwrap().push_back(MicEvent::Stopped);
        let status = wait_for(&engine, "the memo to be saved", |s| s.recording.is_none());
        assert!(stopped.elapsed() < Duration::from_millis(1_200), "saving took {:?}", stopped.elapsed());
        assert!(status.active.is_some(), "the engine was still inside the long note's window");
        assert_eq!(engine.library.list().len(), 2);

        engine.cancel(&long);
        wait_for(&engine, "the cancel", |s| s.idle());
    }

    #[test]
    fn a_language_chosen_in_settings_outranks_the_one_a_note_was_in() {
        let engine = engine("forced", |_| {});
        let model = SimEngine::new();
        assert_eq!(engine.long_form(&model, None, 0, Some("cy")).unwrap().language(), Some("cy"));
        assert_eq!(engine.long_form(&model, Some("de".into()), 0, Some("cy")).unwrap().language(), Some("de"));
        assert_eq!(engine.long_form(&model, Some("de".into()), 0, None).unwrap().language(), Some("de"));
    }

    #[test]
    fn a_line_typed_before_the_transcript_stays_on_top_of_it() {
        let engine = engine("typed", |s| s.auto_transcribe = false);
        let mut sink = engine.prepare_recording().unwrap();
        sink(&speech(8), RATE);
        engine.stop_recording();
        engine.finalize_recording();
        let id = only_note(&engine).id;
        // What the editor saves for a line typed into a note that has no transcript yet.
        engine.edit_note(&id, |note| (note.body, note.body_covers) = (Some("Meeting with Bob".into()), 0));

        engine.transcribe(&id, false);
        wait_for(&engine, "the transcript", |s| s.idle());
        let note = engine.library.load(&id).unwrap();
        assert_eq!(note.state, NoteState::Done);
        assert!(note.text().starts_with("Meeting with Bob\n\nThis is a simulated transcript."), "{}", note.text());
        assert!(note.matches("simulated") && note.snippet(200).contains("simulated"));
    }

    #[test]
    fn the_self_test_reports_text_and_timings() {
        let engine = engine("selftest", |_| {});
        // Tapped with nothing loaded: it must not be what unloads the model about to load.
        engine.request_unload();
        engine.request_selftest();
        let status = wait_for(&engine, "the self-test", |s| matches!(s.selftest, SelfTest::Finished { .. } | SelfTest::Failed(_)));
        match status.selftest {
            SelfTest::Finished { text, detail, matched } => {
                assert!(text.contains("simulated"));
                assert!(detail.contains("11.0 s of audio"), "{detail}");
                // The scripted engine does not hear the clip.
                assert!(!matched);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(engine.status().model, ModelStatus::Ready(Loaded { simulated: true, .. })));
        engine.request_unload();
        wait_for(&engine, "the unload", |s| s.model == ModelStatus::Idle);
        assert!(engine.log.lines().iter().any(|line| line.contains("self-test")));
    }

    #[test]
    fn a_restart_picks_up_what_the_last_process_left() {
        let root = std::env::temp_dir().join(format!("scribe-engine-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let library = Library::open(root.join("notes")).unwrap();
        let note = library.create(1_000).unwrap();
        let mut writer = WavWriter::create(&library.audio_path(&note.id), RATE).unwrap();
        writer.write(&speech(6)).unwrap();
        std::mem::forget(writer);
        let mut settings = Settings::default();
        settings.simulate = true;
        settings.save(&root.join("settings.json")).unwrap();

        // And a note an earlier build finished, before there were text files.
        let mut old = library.create(500).unwrap();
        (old.state, old.segments) = (NoteState::Done, vec![Seg { start: 0.0, end: 2.0, text: "From before.".into(), speaker: None }]);
        library.save(&old).unwrap();

        let engine = Engine::start(Paths { settings: root.join("settings.json"), library: root.join("notes"), models: root.join("whisper"), native_libs: None, documents: Some(root.join("Documents").join("Scribe")), speaker_pack: None }).unwrap();
        wait_for(&engine, "the recovered note", |s| s.queue.is_empty() && s.active.is_none() && s.stats.windows > 0);
        let note = engine.library.load(&note.id).unwrap();
        assert_eq!((note.state, note.duration_ms), (NoteState::Done, 6_000));
        // Both end up with a file: one for being transcribed now, one caught up at startup.
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.library.load(&old.id).unwrap().export.is_none() {
            assert!(Instant::now() < deadline, "the older note never got its file");
            std::thread::sleep(Duration::from_millis(20));
        }
        let files = text_files(&engine);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|(name, text)| name.ends_with(" From before.txt") && text == "From before.\n"), "{files:?}");
    }
}
