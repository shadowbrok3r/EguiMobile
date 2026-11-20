//! The notes on disk: one directory per note, holding `note.json` and `audio.wav`.
//!
//! The audio is the durable half. A transcript can always be made again from it — with a
//! better model, in another language, or because the first attempt never ran — so nothing here
//! ever removes a recording except deleting the note itself.

use crate::export::Exported;
use crate::wav;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

pub const AUDIO_FILE: &str = "audio.wav";
const NOTE_FILE: &str = "note.json";
/// Where a note file that would not load is put before another is written in its place.
const UNREAD_FILE: &str = "note.unreadable.json";
/// The voice prints of the recording's windows, kept so its speakers can be regrouped.
const VOICES_FILE: &str = "voices.bin";
/// How many times a note may be the one being transcribed when the process dies before it is
/// no longer queued by itself at the next start.
const DEATHS_ALLOWED: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteState {
    /// Audio is being captured right now.
    Recording,
    /// Audio is saved and not (fully) transcribed.
    #[default]
    Pending,
    /// A transcription is running or queued in this process.
    Transcribing,
    Done,
    /// The last transcription attempt ended in `error`. The audio is intact.
    Failed,
}

/// One timestamped stretch of transcript, in seconds from the start of the audio.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Seg {
    pub start: f32,
    pub end: f32,
    pub text: String,
    /// Whose voice it is, as a number into the note's [`Speakers`]; `None` until the
    /// recording has been through the speaker pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<u8>,
}

/// The voices told apart in a recording.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Speakers {
    /// How many there are. With one, nothing in the text is labelled.
    pub count: u8,
    /// The count was set by hand rather than found.
    pub fixed: bool,
    /// Names given to them, by number; a missing or empty one reads "Speaker N".
    pub names: Vec<String>,
}

impl Speakers {
    pub fn name(&self, speaker: u8) -> String {
        match self.names.get(speaker as usize).map(|name| name.trim()).filter(|name| !name.is_empty()) {
            Some(name) => name.to_string(),
            None => format!("Speaker {}", speaker as u32 + 1),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Note {
    pub id: String,
    /// Empty until the user names it; see [`Note::display_title`].
    pub title: String,
    /// Unix seconds.
    pub created: i64,
    pub duration_ms: u64,
    pub state: NoteState,
    pub segments: Vec<Seg>,
    /// The text as the user left it, once they have edited it. `None` follows the segments.
    pub body: Option<String>,
    /// How many segments existed when `body` was written: it stands in for those, and
    /// whatever is transcribed after them still follows it.
    pub body_covers: usize,
    /// Samples of audio the segments cover: where a transcription resumes.
    pub seek: u64,
    pub language: Option<String>,
    /// The model pack that produced the segments.
    pub model: Option<String>,
    pub error: Option<String>,
    pub pinned: bool,
    /// Transcriptions of this note begun and not seen to end; above zero at startup means the
    /// process died during one.
    pub attempts: u32,
    /// The text file last written for this note in the documents folder, if any.
    pub export: Option<Exported>,
    /// Who speaks in the segments, once the speaker pass has been over them.
    pub speakers: Option<Speakers>,
    /// The speaker pass is owed: asked for and not seen to end.
    pub speakers_due: bool,
}

impl Note {
    /// The note as running text: the user's edit if there is one, then whatever has been
    /// transcribed since it was made. An edit of a half-finished transcript, or a line typed
    /// before there was one, must not hide the rest when it arrives.
    pub fn text(&self) -> String {
        let Some(body) = &self.body else { return self.transcript() };
        let later = self.transcript_from(self.body_covers);
        let body = body.trim_end();
        if later.is_empty() || body.is_empty() {
            return if later.is_empty() { self.body.clone().unwrap_or_default() } else { later };
        }
        // Mid-flow it reads on as the same paragraph; after a pause, after a line typed
        // ahead of any transcript, or where what follows opens with a speaker's name, as a
        // new one.
        let pause = match self.body_covers.checked_sub(1).and_then(|i| self.segments.get(i)) {
            Some(before) => self.segments[self.body_covers].start - before.end > 2.0,
            None => true,
        };
        let named = self.named().is_some() && self.segments[self.body_covers].speaker.is_some();
        format!("{body}{}{later}", if pause || named { "\n\n" } else { " " })
    }

    /// The machine transcript, one paragraph per pause.
    pub fn transcript(&self) -> String {
        self.transcript_from(0)
    }

    /// The voices to label the text with: only when more than one was told apart.
    fn named(&self) -> Option<&Speakers> {
        self.speakers.as_ref().filter(|speakers| speakers.count > 1)
    }

    /// The transcript of the segments from index `from` on. Where the recording's voices have
    /// been told apart, each change of speaker starts a paragraph under that speaker's name.
    fn transcript_from(&self, from: usize) -> String {
        let named = self.named();
        let mut out = String::new();
        let mut last_end = 0f32;
        let mut voice = None;
        for seg in self.segments.get(from..).unwrap_or_default() {
            let turn = named.zip(seg.speaker).filter(|(_, speaker)| voice != Some(*speaker));
            if !out.is_empty() {
                // A gap in speech reads as a paragraph break; so does someone else speaking.
                out.push_str(if turn.is_some() || seg.start - last_end > 2.0 { "\n\n" } else { " " });
            }
            if let Some((speakers, speaker)) = turn {
                out.push_str(&speakers.name(speaker));
                out.push_str(": ");
                voice = Some(speaker);
            }
            out.push_str(&seg.text);
            last_end = seg.end;
        }
        out
    }

    /// What identifies the segments the speaker pass ran over: voice prints saved under
    /// another key are of an older transcript.
    pub fn voices_key(&self) -> u64 {
        self.segments.iter().fold(0xcbf2_9ce4_8422_2325, |hash: u64, seg| {
            [seg.start.to_bits(), seg.end.to_bits()].iter().fold(hash, |hash, bits| (hash ^ *bits as u64).wrapping_mul(0x0000_0100_0000_01b3))
        })
    }

    /// Take the speaker pass's answer: a voice per segment, `count` of them in all. A segment
    /// too short to have been heard goes with the one before it. Names already given stay
    /// with their numbers.
    pub fn set_speakers(&mut self, voices: &[Option<u8>], count: usize, fixed: bool) {
        let first = voices.iter().flatten().next().copied().unwrap_or(0);
        let mut last = first;
        for (seg, voice) in self.segments.iter_mut().zip(voices.iter().chain(std::iter::repeat(&None))) {
            last = voice.unwrap_or(last);
            seg.speaker = Some(last);
        }
        let names = self.speakers.take().map(|speakers| speakers.names).unwrap_or_default();
        self.speakers = Some(Speakers { count: count.clamp(1, u8::MAX as usize) as u8, fixed, names });
        self.speakers_due = false;
    }

    /// Forget who speaks where; the transcript itself stays.
    pub fn clear_speakers(&mut self) {
        self.segments.iter_mut().for_each(|seg| seg.speaker = None);
        self.speakers = None;
        self.speakers_due = false;
    }

    /// Call the speakers by `names`, first to last; an empty name goes back to the number.
    ///
    /// Two speakers cannot answer to one name: a name that is taken is not given, and that
    /// speaker keeps the one they had. That is what lets text the user has edited follow —
    /// it carries the names as plain words at the start of paragraphs, and those are
    /// rewritten in a single pass, each line by the one speaker its opening words name. A
    /// rename at a time would turn Ann into Bob and then both into Ann.
    pub fn name_speakers(&mut self, names: &[String]) {
        let Some(speakers) = self.speakers.as_mut() else { return };
        let count = speakers.count as usize;
        let from: Vec<String> = (0..count).map(|speaker| speakers.name(speaker as u8)).collect();
        let label = |speaker: usize, name: &str| if name.trim().is_empty() { format!("Speaker {}", speaker + 1) } else { name.trim().to_string() };
        let mut to: Vec<String> = (0..count).map(|speaker| names.get(speaker).map_or_else(|| from[speaker].clone(), |name| label(speaker, name))).collect();
        // Give way until no two share a label: the last one changing goes back to what
        // they had.
        while let Some(taken) = (0..count).rev().find(|&i| to[i] != from[i] && (0..count).any(|j| j != i && to[j] == to[i])) {
            to[taken] = from[taken].clone();
        }
        if speakers.names.len() < count {
            speakers.names.resize(count, String::new());
        }
        for speaker in (0..count).filter(|&speaker| to[speaker] != from[speaker]) {
            speakers.names[speaker] = names.get(speaker).map(|name| name.trim().to_string()).unwrap_or_default();
        }
        let Some(body) = self.body.as_mut() else { return };
        // Only from a label that was one speaker's alone; one two of them shared says
        // nothing about whose line it opens.
        let moved: Vec<usize> = (0..count).filter(|&i| to[i] != from[i] && from.iter().filter(|label| **label == from[i]).count() == 1).collect();
        if moved.is_empty() {
            return;
        }
        let lines: Vec<String> = body.split('\n').map(|line| {
            let opens = |i: &usize| line.strip_prefix(from[*i].as_str()).is_some_and(|rest| rest.starts_with(": "));
            match moved.iter().copied().filter(opens).max_by_key(|&i| from[i].len()) {
                Some(i) => format!("{}{}", to[i], &line[from[i].len()..]),
                None => line.to_string(),
            }
        }).collect();
        *body = lines.join("\n");
    }

    /// [`Note::name_speakers`] for one of them.
    pub fn rename_speaker(&mut self, speaker: u8, name: &str) {
        let Some(speakers) = self.speakers.as_ref() else { return };
        let mut names: Vec<String> = (0..speakers.count as usize).map(|i| speakers.names.get(i).cloned().unwrap_or_default()).collect();
        if let Some(slot) = names.get_mut(speaker as usize) {
            *slot = name.to_string();
            self.name_speakers(&names);
        }
    }

    /// The user's title, else the first words spoken, else when it was recorded.
    pub fn display_title(&self) -> String {
        self.title_over(&self.text())
    }

    /// [`Note::display_title`], given the note's text.
    pub fn title_over(&self, text: &str) -> String {
        if !self.title.trim().is_empty() {
            return self.title.trim().to_string();
        }
        // The first words spoken, not the names of whoever spoke them.
        fn spoken<'t>(named: Option<&Speakers>, line: &'t str) -> &'t str {
            named.and_then(|speakers| (0..speakers.count).find_map(|speaker| line.strip_prefix(&format!("{}: ", speakers.name(speaker))))).unwrap_or(line)
        }
        let named = self.named();
        let mut words: Vec<&str> = text.lines().flat_map(|line| spoken(named, line).split_whitespace()).take(8).collect();
        let more = words.len() > 7;
        words.truncate(7);
        if !words.is_empty() {
            let mut title = words.join(" ");
            if more {
                title.push('…');
            }
            return title.trim_end_matches(['.', ',', ';', ':']).to_string();
        }
        let local = crate::clock::local(self.created);
        format!("Recording {} {}", local.date(), local.time())
    }

    /// Up to `max` characters of the text on one line, for a list row.
    pub fn snippet(&self, max: usize) -> String {
        snippet_of(&self.text(), max)
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_ms as f64 / 1000.0
    }

    /// Case-insensitive match against the title and the text.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty() || self.display_title().to_lowercase().contains(&query) || self.text().to_lowercase().contains(&query)
    }

    /// Forget the transcript so the audio is transcribed again from the start.
    pub fn reset_transcript(&mut self) {
        self.segments.clear();
        self.clear_speakers();
        self.body = None;
        self.body_covers = 0;
        self.seek = 0;
        self.language = None;
        self.model = None;
        self.error = None;
        self.state = NoteState::Pending;
    }
}

/// Up to `max` characters of `text` on one line. Only as much of it is read as that takes.
pub fn snippet_of(text: &str, max: usize) -> String {
    let mut flat = text.split_whitespace().enumerate().flat_map(|(i, word)| (i > 0).then_some(' ').into_iter().chain(word.chars()));
    let mut out: String = flat.by_ref().take(max).collect();
    if flat.next().is_some() {
        out.push('…');
    }
    out
}

/// The directory holding every note.
#[derive(Clone, Debug)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn note_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn audio_path(&self, id: &str) -> PathBuf {
        self.note_dir(id).join(AUDIO_FILE)
    }

    pub fn voices_path(&self, id: &str) -> PathBuf {
        self.note_dir(id).join(VOICES_FILE)
    }

    /// Finished notes still owed their speaker pass, oldest first.
    pub fn speakers_due(&self) -> Vec<String> {
        let mut due: Vec<Note> = self.list().into_iter().filter(|note| note.state == NoteState::Done && note.speakers_due).collect();
        // By age, not by the list's order, which puts pinned notes first.
        due.sort_by(|a, b| a.created.cmp(&b.created).then(a.id.cmp(&b.id)));
        due.into_iter().map(|note| note.id).collect()
    }

    /// Make the directory for a note about to be recorded.
    pub fn create(&self, now: i64) -> io::Result<Note> {
        let stamp = crate::clock::local(now).stamp();
        // Two recordings inside one second are possible from a double tap.
        let id = (0..100).map(|n| if n == 0 { stamp.clone() } else { format!("{stamp}-{n}") }).find(|id| !self.note_dir(id).exists());
        let id = id.ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free note id"))?;
        std::fs::create_dir_all(self.note_dir(&id))?;
        let note = Note { id, created: now, state: NoteState::Recording, ..Default::default() };
        self.save(&note)?;
        Ok(note)
    }

    /// Write `note.json` through a temporary file, so a crash never leaves half of one.
    pub fn save(&self, note: &Note) -> io::Result<()> {
        let path = self.note_dir(&note.id).join(NOTE_FILE);
        let temporary = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(note).map_err(io::Error::other)?;
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, &path)
    }

    pub fn load(&self, id: &str) -> io::Result<Note> {
        let text = std::fs::read_to_string(self.note_dir(id).join(NOTE_FILE))?;
        let mut note: Note = serde_json::from_str(&text).map_err(io::Error::other)?;
        // The directory is the identity; a copied or renamed note follows its folder.
        note.id = id.to_string();
        Ok(note)
    }

    /// The note in folder `id`: its `note.json`, or failing that one rebuilt around its audio,
    /// so a recording that lost its note file is not hidden. Reads only.
    pub fn find(&self, id: &str) -> Option<Note> {
        self.load(id).ok().or_else(|| self.adopt(id))
    }

    /// Every note, pinned first and then newest first. Reads only: a list taken while another
    /// thread deletes a note must not write that note back.
    pub fn list(&self) -> Vec<Note> {
        let mut notes: Vec<Note> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                let id = entry.file_name().to_string_lossy().into_owned();
                if id.starts_with('.') {
                    return None;
                }
                self.find(&id)
            })
            .collect();
        notes.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.created.cmp(&a.created)).then(b.id.cmp(&a.id)));
        notes
    }

    /// Rebuild a note around audio that has lost its `note.json`.
    fn adopt(&self, id: &str) -> Option<Note> {
        let info = wav::probe(&self.audio_path(id)).ok()?;
        let created = std::fs::metadata(self.audio_path(id)).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
        Some(Note { id: id.to_string(), created, duration_ms: (info.seconds() * 1000.0) as u64, state: NoteState::Pending, ..Default::default() })
    }

    pub fn delete(&self, id: &str) -> io::Result<()> {
        std::fs::remove_dir_all(self.note_dir(id))
    }

    /// Put right what a killed process left behind: recordings still marked as in progress get
    /// their WAV header repaired, and anything mid-transcription goes back to pending with what
    /// it had. A recording that never captured anything is removed; one that cannot be read
    /// right now is kept and says so. Returns the ids now waiting for transcription.
    pub fn recover(&self) -> Vec<String> {
        let mut waiting = Vec::new();
        for mut note in self.list() {
            match note.state {
                NoteState::Recording => {
                    let audio = self.audio_path(&note.id);
                    // No file, or nothing after its header: the capture never got going.
                    let empty = std::fs::metadata(&audio).map_or(true, |m| m.len() <= wav::HEADER_LEN);
                    match (!empty).then(|| wav::repair(&audio)) {
                        Some(Ok(info)) if info.samples > 0 => {
                            note.duration_ms = (info.seconds() * 1000.0) as u64;
                            note.state = NoteState::Pending;
                            let _ = self.save(&note);
                            waiting.push(note.id);
                        }
                        None | Some(Ok(_)) => {
                            let _ = self.delete(&note.id);
                        }
                        // There is audio and it could not be read. That is never a reason to
                        // remove it: the note stays, and another start may read it.
                        Some(Err(e)) => {
                            note.state = NoteState::Failed;
                            note.error = Some(format!("The recording could not be read: {e}"));
                            let _ = self.save(&note);
                        }
                    }
                }
                NoteState::Transcribing if note.attempts >= DEATHS_ALLOWED => {
                    // Not queued again by itself: whatever it does to the process, it has done
                    // twice, and a third start should get as far as the screen.
                    note.state = NoteState::Failed;
                    note.error = Some("Transcribing this recording stopped the app twice. The audio is intact; try again, or with another model.".into());
                    let _ = self.save(&note);
                }
                NoteState::Transcribing => {
                    note.state = NoteState::Pending;
                    let _ = self.save(&note);
                    waiting.push(note.id);
                }
                NoteState::Pending => {
                    // Audio adopted without a readable note file gets one now, at startup,
                    // when nothing else is touching the library.
                    if self.load(&note.id).is_err() {
                        // A file that is there and would not load may be damaged, or may
                        // be from a build this one cannot read. Either way it holds
                        // someone's title and edits: set aside, never written over.
                        let unread = self.note_dir(&note.id).join(NOTE_FILE);
                        if unread.exists() {
                            let _ = std::fs::rename(&unread, self.note_dir(&note.id).join(UNREAD_FILE));
                        }
                        let _ = self.save(&note);
                    }
                    waiting.push(note.id);
                }
                NoteState::Done | NoteState::Failed => {}
            }
        }
        // Oldest first: recordings are transcribed in the order they were made.
        waiting.reverse();
        waiting
    }

    /// Bytes of audio and notes under the root.
    pub fn total_bytes(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| {
                let path = e.path();
                if path.is_dir() { walk(&path) } else { e.metadata().map_or(0, |m| m.len()) }
            }).sum()
        }
        walk(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::WavWriter;

    fn library(name: &str) -> Library {
        let root = std::env::temp_dir().join(format!("scribe-library-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Library::open(root).unwrap()
    }

    fn seg(start: f32, end: f32, text: &str) -> Seg {
        Seg { start, end, text: text.into(), speaker: None }
    }

    #[test]
    fn notes_round_trip_and_list_newest_first_with_pins_on_top() {
        let lib = library("list");
        let mut first = lib.create(1_000_000).unwrap();
        let second = lib.create(1_000_500).unwrap();
        let third = lib.create(1_000_900).unwrap();
        first.pinned = true;
        first.segments = vec![seg(0.0, 2.0, "Hello there.")];
        lib.save(&first).unwrap();
        assert_eq!(lib.load(&first.id).unwrap(), first);
        let ids: Vec<String> = lib.list().into_iter().map(|n| n.id).collect();
        assert_eq!(ids, [first.id.clone(), third.id.clone(), second.id.clone()]);
        lib.delete(&third.id).unwrap();
        assert_eq!(lib.list().len(), 2);
    }

    #[test]
    fn two_recordings_in_one_second_get_distinct_folders() {
        let lib = library("ids");
        let (a, b) = (lib.create(5_000).unwrap(), lib.create(5_000).unwrap());
        assert_ne!(a.id, b.id);
        assert!(b.id.starts_with(&a.id));
    }

    #[test]
    fn titles_fall_back_from_name_to_first_words_to_date() {
        let mut note = Note { created: 1_794_300_000, ..Default::default() };
        assert!(note.display_title().starts_with("Recording "));
        note.segments = vec![seg(0.0, 3.0, "Remember to call the dentist about Thursday's appointment and the bill.")];
        assert_eq!(note.display_title(), "Remember to call the dentist about Thursday's…");
        note.segments = vec![seg(0.0, 1.0, "Buy milk.")];
        assert_eq!(note.display_title(), "Buy milk");
        note.title = "  Shopping  ".into();
        assert_eq!(note.display_title(), "Shopping");
    }

    #[test]
    fn text_follows_segments_until_the_user_edits_it() {
        let mut note = Note::default();
        note.segments = vec![seg(0.0, 2.0, "First thought."), seg(2.1, 4.0, "Still going."), seg(9.0, 11.0, "After a pause.")];
        assert_eq!(note.text(), "First thought. Still going.\n\nAfter a pause.");
        assert_eq!(note.snippet(20), "First thought. Still…");
        assert!(note.matches("STILL going"));
        assert!(!note.matches("absent"));
        note.body = Some("My own words".into());
        note.body_covers = 3;
        assert_eq!(note.text(), "My own words");
        assert_eq!(note.transcript(), "First thought. Still going.\n\nAfter a pause.");
        note.reset_transcript();
        assert_eq!((note.text().as_str(), note.seek, note.state, note.body_covers), ("", 0, NoteState::Pending, 0));
    }

    #[test]
    fn speakers_label_their_turns_once_there_is_more_than_one() {
        let mut note = Note::default();
        note.segments = vec![seg(0.0, 2.0, "Shall we start?"), seg(2.1, 4.0, "Yes."), seg(4.2, 6.0, "Go ahead."), seg(6.1, 8.0, "Right."), seg(12.0, 14.0, "First item.")];
        let plain = note.text();
        // One voice: the pass ran, and the text reads as it did.
        note.set_speakers(&[Some(0); 5], 1, false);
        assert_eq!(note.text(), plain);
        assert_eq!(note.speakers, Some(Speakers { count: 1, fixed: false, names: vec![] }));

        // Two: a paragraph per turn, named; a pause inside a turn is still only a paragraph.
        note.set_speakers(&[Some(0), Some(1), Some(1), Some(0), Some(0)], 2, false);
        assert_eq!(note.text(), "Speaker 1: Shall we start?\n\nSpeaker 2: Yes. Go ahead.\n\nSpeaker 1: Right.\n\nFirst item.");
        assert_eq!(note.display_title(), "Shall we start? Yes. Go ahead. Right.…");
        assert!(!note.speakers_due);

        // Names replace the numbers, and survive the count being changed.
        note.rename_speaker(1, " Maya ");
        assert_eq!(note.text(), "Speaker 1: Shall we start?\n\nMaya: Yes. Go ahead.\n\nSpeaker 1: Right.\n\nFirst item.");
        note.set_speakers(&[Some(0), Some(1), Some(2), Some(0), None], 3, true);
        assert_eq!(note.speakers.as_ref().map(|s| (s.count, s.fixed, s.name(1), s.name(2))), Some((3, true, "Maya".into(), "Speaker 3".into())));
        // A segment nobody was heard in goes with the one before it.
        assert_eq!(note.segments[4].speaker, Some(0));
        note.rename_speaker(1, "");
        assert_eq!(note.speakers.as_ref().unwrap().name(1), "Speaker 2");

        // A transcript that changes is a transcript whose speakers are unknown again.
        let key = note.voices_key();
        note.segments.push(seg(15.0, 16.0, "More."));
        assert_ne!(note.voices_key(), key);
        note.reset_transcript();
        assert_eq!((note.speakers.clone(), note.segments.len()), (None, 0));
    }

    #[test]
    fn an_edited_text_keeps_its_words_and_takes_a_new_name() {
        let mut note = Note::default();
        note.segments = vec![seg(0.0, 2.0, "Hello."), seg(2.1, 4.0, "Hi."), seg(4.2, 6.0, "Later line.")];
        note.set_speakers(&[Some(0), Some(1), Some(0)], 2, false);
        // The user tidies the first two turns; the third is transcribed after the edit.
        note.body = Some("Speaker 1: Hello!\n\nSpeaker 2: Hi there. Speaker 2: not a label mid-line".into());
        note.body_covers = 2;
        note.rename_speaker(1, "Sam");
        assert_eq!(note.body.as_deref(), Some("Speaker 1: Hello!\n\nSam: Hi there. Speaker 2: not a label mid-line"));
        assert_eq!(note.text(), "Speaker 1: Hello!\n\nSam: Hi there. Speaker 2: not a label mid-line\n\nSpeaker 1: Later line.");
        // With no speakers there is nothing to rename.
        let mut bare = Note { body: Some("Speaker 1: typed by hand".into()), ..Default::default() };
        bare.rename_speaker(0, "Me");
        assert_eq!(bare.body.as_deref(), Some("Speaker 1: typed by hand"));
    }

    #[test]
    fn names_stay_one_to_a_speaker_so_edited_text_can_follow_them() {
        let mut note = Note::default();
        note.segments = vec![seg(0.0, 2.0, "One."), seg(2.1, 4.0, "Two."), seg(4.2, 6.0, "Three.")];
        note.set_speakers(&[Some(0), Some(1), Some(2)], 3, false);
        note.name_speakers(&["Ann".into(), "Bob".into(), String::new()]);
        note.body = Some("Ann: One!\n\nBob: Two!\n\nSpeaker 3: Three!".into());
        note.body_covers = 3;
        let names = |note: &Note| (0..3).map(|i| note.speakers.as_ref().unwrap().name(i)).collect::<Vec<_>>();

        // Swapped at once: each paragraph changes hands once, not twice.
        note.name_speakers(&["Bob".into(), "Ann".into(), String::new()]);
        assert_eq!(names(&note), ["Bob", "Ann", "Speaker 3"]);
        assert_eq!(note.body.as_deref(), Some("Bob: One!\n\nAnn: Two!\n\nSpeaker 3: Three!"));

        // A name someone else has is not given, by typing it or on the way to another: the
        // speaker keeps what they had, and so do their paragraphs.
        note.rename_speaker(2, "Ann");
        assert_eq!(names(&note), ["Bob", "Ann", "Speaker 3"]);
        note.rename_speaker(2, "Annabel");
        assert_eq!(names(&note), ["Bob", "Ann", "Annabel"]);
        assert_eq!(note.body.as_deref(), Some("Bob: One!\n\nAnn: Two!\n\nAnnabel: Three!"));
        // Nor is another speaker's number.
        note.rename_speaker(0, "Speaker 2");
        note.rename_speaker(1, "");
        assert_eq!(names(&note), ["Speaker 2", "Ann", "Annabel"], "the number is taken as a name, so the second keeps theirs");
        note.name_speakers(&[String::new(), String::new(), String::new()]);
        assert_eq!(names(&note), ["Speaker 1", "Speaker 2", "Speaker 3"]);
        assert_eq!(note.body.as_deref(), Some("Speaker 1: One!\n\nSpeaker 2: Two!\n\nSpeaker 3: Three!"));
        // Two given the same name at once: the first has it.
        note.name_speakers(&["Sam".into(), "Sam".into(), "Kim".into()]);
        assert_eq!(names(&note), ["Sam", "Speaker 2", "Kim"]);
        assert_eq!(note.body.as_deref(), Some("Sam: One!\n\nSpeaker 2: Two!\n\nKim: Three!"));
    }

    #[test]
    fn what_is_transcribed_after_an_edit_follows_it() {
        let mut note = Note::default();
        note.segments = vec![seg(0.0, 2.0, "Frist thought.")];
        // A correction made while the rest was still to come.
        note.body = Some("First thought.\n".into());
        note.body_covers = 1;
        assert_eq!(note.text(), "First thought.\n");
        note.segments.push(seg(2.2, 4.0, "Still going."));
        assert_eq!(note.text(), "First thought. Still going.");
        note.segments.push(seg(9.0, 11.0, "After a pause."));
        assert_eq!(note.text(), "First thought. Still going.\n\nAfter a pause.");
        assert!(note.matches("after a pause"));
        // The pause can fall right at the edit's edge.
        note.segments[1].start = 6.0;
        assert_eq!(note.text(), "First thought.\n\nStill going.\n\nAfter a pause.");

        // A line typed before anything was transcribed stays on top of all of it.
        let mut typed = Note { body: Some("Meeting with Bob".into()), ..Default::default() };
        assert_eq!(typed.text(), "Meeting with Bob");
        typed.segments = vec![seg(0.0, 2.0, "Hello Bob.")];
        assert_eq!(typed.text(), "Meeting with Bob\n\nHello Bob.");
        assert_eq!(typed.snippet(40), "Meeting with Bob Hello Bob.");
        // Text cleared by hand does not swallow what comes next either.
        typed.body = Some("  ".into());
        assert_eq!(typed.text(), "Hello Bob.");
    }

    #[test]
    fn recovery_repairs_cut_off_recordings_and_requeues_unfinished_work() {
        let lib = library("recover");
        // Killed while recording, with audio.
        let cut = lib.create(100).unwrap();
        let mut writer = WavWriter::create(&lib.audio_path(&cut.id), 16_000).unwrap();
        writer.write(&vec![5i16; 32_000]).unwrap();
        std::mem::forget(writer);
        // Killed while recording, before any audio.
        let empty = lib.create(200).unwrap();
        // Killed mid-transcription.
        let mut half = lib.create(300).unwrap();
        half.state = NoteState::Transcribing;
        half.seek = 4_000;
        half.segments = vec![seg(0.0, 0.25, "Partial")];
        lib.save(&half).unwrap();
        // Finished long ago.
        let mut done = lib.create(400).unwrap();
        done.state = NoteState::Done;
        lib.save(&done).unwrap();

        assert_eq!(lib.recover(), [cut.id.clone(), half.id.clone()]);
        let cut = lib.load(&cut.id).unwrap();
        assert_eq!((cut.state, cut.duration_ms), (NoteState::Pending, 2_000));
        assert!(!lib.note_dir(&empty.id).exists());
        let half = lib.load(&half.id).unwrap();
        // What was already transcribed is kept; the job resumes from its seek point.
        assert_eq!((half.state, half.seek, half.segments.len()), (NoteState::Pending, 4_000, 1));
        assert_eq!(lib.load(&done.id).unwrap().state, NoteState::Done);
    }

    #[test]
    fn recovery_never_deletes_audio_it_could_not_tidy_or_read() {
        use std::os::unix::fs::PermissionsExt;
        let lib = library("keep");
        // Killed while recording; the volume then refuses writes, so the header cannot be put
        // right. The samples are all there and that is all that matters.
        let locked = lib.create(100).unwrap();
        let mut writer = WavWriter::create(&lib.audio_path(&locked.id), 16_000).unwrap();
        writer.write(&vec![5i16; 48_000]).unwrap();
        std::mem::forget(writer);
        std::fs::set_permissions(lib.audio_path(&locked.id), std::fs::Permissions::from_mode(0o444)).unwrap();
        // Killed while recording; what is on disk does not parse as audio at all.
        let mangled = lib.create(200).unwrap();
        std::fs::write(lib.audio_path(&mangled.id), vec![0xa5u8; 4_000]).unwrap();

        assert_eq!(lib.recover(), [locked.id.clone()]);
        let kept = lib.load(&locked.id).unwrap();
        assert_eq!((kept.state, kept.duration_ms), (NoteState::Pending, 3_000));
        let unread = lib.load(&mangled.id).unwrap();
        assert_eq!(unread.state, NoteState::Failed);
        assert!(unread.error.as_deref().is_some_and(|e| e.starts_with("The recording could not be read")), "{:?}", unread.error);
        assert_eq!(std::fs::metadata(lib.audio_path(&mangled.id)).unwrap().len(), 4_000);
        // And a second start leaves the unreadable one alone rather than trying again.
        assert_eq!(lib.recover(), [locked.id.clone()]);
        assert!(lib.audio_path(&mangled.id).exists());
    }

    #[test]
    fn a_note_that_was_running_when_the_process_died_twice_is_not_queued_again() {
        let lib = library("deaths");
        let mut once = lib.create(100).unwrap();
        (once.state, once.attempts) = (NoteState::Transcribing, 1);
        lib.save(&once).unwrap();
        let mut twice = lib.create(200).unwrap();
        (twice.state, twice.attempts, twice.seek) = (NoteState::Transcribing, 2, 16_000);
        twice.segments = vec![seg(0.0, 1.0, "Kept.")];
        lib.save(&twice).unwrap();

        assert_eq!(lib.recover(), [once.id.clone()]);
        let stopped = lib.load(&twice.id).unwrap();
        assert_eq!((stopped.state, stopped.seek, stopped.segments.len()), (NoteState::Failed, 16_000, 1));
        assert!(stopped.error.as_deref().is_some_and(|e| e.contains("stopped the app twice")));
        // Failed notes wait for a person, at this start and every later one.
        assert_eq!(lib.recover(), [once.id]);
    }

    #[test]
    fn audio_without_a_note_file_is_adopted_not_hidden() {
        let lib = library("adopt");
        let dir = lib.note_dir("orphan");
        std::fs::create_dir_all(&dir).unwrap();
        let mut writer = WavWriter::create(&dir.join(AUDIO_FILE), 16_000).unwrap();
        writer.write(&vec![1i16; 16_000]).unwrap();
        writer.finish().unwrap();
        std::fs::write(lib.root().join("stray.txt"), b"ignored").unwrap();
        let notes = lib.list();
        assert_eq!(notes.len(), 1);
        assert_eq!((notes[0].id.as_str(), notes[0].duration_ms, notes[0].state), ("orphan", 1_000, NoteState::Pending));
        assert!(lib.total_bytes() > 32_000);
        // Listing wrote nothing; recovery at startup is what gives the audio its note file.
        assert!(lib.load("orphan").is_err());
        assert_eq!(lib.recover(), ["orphan"]);
        assert_eq!(lib.load("orphan").unwrap().duration_ms, 1_000);
    }

    #[test]
    fn a_note_file_that_will_not_load_is_set_aside_not_written_over() {
        let lib = library("unread");
        let note = lib.create(100).unwrap();
        let mut writer = WavWriter::create(&lib.audio_path(&note.id), 16_000).unwrap();
        writer.write(&vec![3i16; 16_000]).unwrap();
        writer.finish().unwrap();
        // Valid JSON this build cannot make a note of, with someone's words in it.
        let odd = r#"{"title": "Keep me", "segments": "not a list"}"#;
        std::fs::write(lib.note_dir(&note.id).join(NOTE_FILE), odd).unwrap();
        assert_eq!(lib.recover(), [note.id.clone()]);
        // The audio is a note again, and what was there is beside it, untouched.
        assert_eq!(lib.load(&note.id).unwrap().state, NoteState::Pending);
        assert_eq!(std::fs::read_to_string(lib.note_dir(&note.id).join(UNREAD_FILE)).unwrap(), odd);
    }

    #[test]
    fn notes_written_by_earlier_builds_load_whole() {
        let lib = library("older");
        // As v0.1.0 wrote them: no speakers, no text file.
        let first = r#"{"id": "20261110-081522", "title": "Standup", "created": 1794300000, "duration_ms": 4000, "state": "done",
            "segments": [{"start": 0.0, "end": 2.0, "text": "Hello there."}, {"start": 2.1, "end": 4.0, "text": "Hi."}],
            "body": "Hello there, all.", "body_covers": 1, "seek": 64000, "language": "en", "model": "whisper-large-v3-turbo",
            "error": null, "pinned": true, "attempts": 0}"#;
        std::fs::create_dir_all(lib.note_dir("20261110-081522")).unwrap();
        std::fs::write(lib.note_dir("20261110-081522").join(NOTE_FILE), first).unwrap();
        let note = lib.load("20261110-081522").unwrap();
        assert_eq!((note.title.as_str(), note.state, note.pinned, note.segments.len()), ("Standup", NoteState::Done, true, 2));
        assert_eq!(note.text(), "Hello there, all. Hi.");
        assert_eq!((note.export.clone(), note.speakers.clone(), note.speakers_due, note.segments[0].speaker), (None, None, false, None));

        // And one from a build that knew less about a text file than this one does.
        let partial = r#"{"state": "done", "segments": [{"start": 0.0, "end": 1.0, "text": "Kept.", "speaker": 1}],
            "export": {"file": "2026-11-11 14.32 Kept.txt", "hash": 7}, "speakers": {"count": 2}, "a_field_from_the_future": [1, 2]}"#;
        std::fs::create_dir_all(lib.note_dir("later")).unwrap();
        std::fs::write(lib.note_dir("later").join(NOTE_FILE), partial).unwrap();
        let note = lib.load("later").unwrap();
        assert_eq!(note.export.as_ref().map(|e| (e.file.as_str(), e.hash, e.wrote)), Some(("2026-11-11 14.32 Kept.txt", 7, 0)));
        assert_eq!(note.speakers.as_ref().map(|s| (s.count, s.fixed, s.names.len())), Some((2, false, 0)));
        assert_eq!(note.text(), "Speaker 2: Kept.");
    }

    #[test]
    fn a_corrupt_note_file_does_not_take_the_list_down() {
        let lib = library("corrupt");
        let good = lib.create(10).unwrap();
        let bad = lib.create(20).unwrap();
        std::fs::write(lib.note_dir(&bad.id).join(NOTE_FILE), b"{ not json").unwrap();
        let ids: Vec<String> = lib.list().into_iter().map(|n| n.id).collect();
        assert_eq!(ids, [good.id]);
    }
}
