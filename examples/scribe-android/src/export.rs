//! Each finished note's text as a plain file in the phone's Documents folder.
//!
//! A note lives in the app's own storage, where no other app can pick it up. A transcript is
//! most often wanted somewhere else — attached to a message, opened in an editor, handed to
//! another program — and a long one does not belong on a clipboard. So the text is also kept
//! as an ordinary `.txt` beside every other document on the phone.
//!
//! The file follows its note: an edit or a new transcript rewrites it, a new title renames
//! it. Nothing else is ever removed here. Deleting a note leaves its file, because the text is
//! small and the usual reason to delete a note is the size of its recording.
//!
//! The folder is the user's, and other notes' files are in it. A note's file is treated as
//! its own only while it still holds exactly what the note last wrote: one that was deleted,
//! edited by hand, or whose name another note has since taken is left alone, and the note
//! writes a new one.

use crate::clock;
use crate::library::Note;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Longest title carried into a file name, in characters and in bytes. A name may hold 255
/// bytes, of which the date, a stand-in's number and the extension need their share; sixty
/// characters of emoji alone would be 240.
const TITLE_CHARS: usize = 60;
const TITLE_BYTES: usize = 180;
/// How many notes may want the same name before the last one goes without a file.
const SAME_NAME: usize = 99;

/// What a note last wrote to the documents folder.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
// Every field optional on the way in: a note file from a build that did not write one of
// them has to load, or the note is taken for audio with no note at all.
#[serde(default)]
pub struct Exported {
    /// The file's name inside the folder.
    pub file: String,
    /// Of the name asked for and the text written: either changing means writing again.
    pub hash: u64,
    /// Of the bytes written: a file that no longer holds them is not this note's to touch.
    pub wrote: u64,
}

/// FNV-1a. Its value is stored in note files, so it has to be the same in every build, which
/// the standard library's hasher does not promise.
fn fingerprint(name: &str, text: &[u8]) -> u64 {
    name.bytes().chain([0]).chain(text.iter().copied()).fold(0xcbf2_9ce4_8422_2325, |hash: u64, byte| (hash ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// What a note's file holds: its text, ending in a newline.
pub fn content(text: &str) -> String {
    if text.ends_with('\n') { text.to_string() } else { format!("{text}\n") }
}

/// A title as part of a file name: nothing a FAT volume or Android's media store refuses, on
/// one line, and short enough to read in a file picker.
fn clean(title: &str) -> String {
    let allowed: String = title.chars().map(|c| if c.is_control() || r#"/\:*?"<>|"#.contains(c) { ' ' } else { c }).collect();
    let words: Vec<&str> = allowed.split_whitespace().collect();
    let mut cut = String::new();
    for c in words.join(" ").chars().take(TITLE_CHARS) {
        if cut.len() + c.len_utf8() > TITLE_BYTES {
            break;
        }
        cut.push(c);
    }
    // Trailing dots are dropped by some filesystems, and a leading one hides the file.
    let cut = cut.trim().trim_end_matches(['.', '…']).trim_start_matches('.').trim();
    if cut.is_empty() { "Note".to_string() } else { cut.to_string() }
}

/// `2026-11-11 14.32`: when the note was made, on the phone's clock.
///
/// Once a note has a file the stamp is read back from that file's name. Worked out afresh it
/// would follow the phone's time zone, and a flight would rename every file in the folder —
/// bringing back the ones that had been thrown away.
fn stamp(note: &Note) -> String {
    let dated = |name: &&str| name.len() == 16 && name.bytes().enumerate().all(|(at, byte)| match at {
        4 | 7 => byte == b'-',
        10 => byte == b' ',
        13 => byte == b'.',
        _ => byte.is_ascii_digit(),
    });
    if let Some(kept) = note.export.as_ref().and_then(|export| export.file.get(..16)).filter(dated) {
        return kept.to_string();
    }
    let at = clock::local(note.created);
    format!("{:04}-{:02}-{:02} {:02}.{:02}", at.year, at.month, at.day, at.hour, at.minute)
}

/// `2026-11-11 14.32 Call the dentist.txt`: the date first, so the folder sorts by time.
pub fn file_name(note: &Note, text: &str) -> String {
    format!("{} {}.txt", stamp(note), clean(&note.title_over(text)))
}

/// Whether `file` is `stem.txt` or one of its numbered stand-ins, `stem (2).txt`.
fn is_named(file: &str, stem: &str) -> bool {
    let Some(rest) = file.strip_prefix(stem).and_then(|rest| rest.strip_suffix(".txt")) else { return false };
    rest.is_empty() || rest.strip_prefix(" (").and_then(|n| n.strip_suffix(')')).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn write(path: &Path, content: &str, new: bool) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    // `create_new` rather than a look first: Android hides other apps' documents from a
    // lookup, and a file that cannot be seen can still be in the way.
    let mut file = if new { options.write(true).create_new(true).open(path)? } else { options.write(true).truncate(true).open(path)? };
    let written = file.write_all(content.as_bytes());
    drop(file);
    if written.is_err() {
        // A full disk. What is there now is empty or cut short, and was either made a moment
        // ago or was this note's own: gone, rather than one more stray at every attempt.
        let _ = std::fs::remove_file(path);
    }
    written
}

/// The folder the text files are kept in.
#[derive(Clone, Debug)]
pub struct Exporter {
    dir: PathBuf,
}

impl Exporter {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The folder as a person would look for it: `Documents/Scribe`.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = self.dir.components().rev().take(2).map(|part| part.as_os_str().to_string_lossy().into_owned()).collect();
        parts.reverse();
        parts.join("/")
    }

    /// Bring the note's file up to date with its text. True if `note.export` changed and the
    /// note needs saving. A note with no text has no file, and keeps whatever it had.
    pub fn sync(&self, note: &mut Note) -> io::Result<bool> {
        let text = note.text();
        if text.trim().is_empty() {
            return Ok(false);
        }
        let name = file_name(note, &text);
        let content = content(&text);
        let hash = fingerprint(&name, content.as_bytes());
        if note.export.as_ref().is_some_and(|export| export.hash == hash) {
            return Ok(false);
        }
        std::fs::create_dir_all(&self.dir)?;
        let previous = note.export.as_ref().filter(|export| self.owns(export)).map(|export| export.file.clone());
        let file = self.place(&name, previous.as_deref(), &content)?;
        if let Some(old) = previous.filter(|old| *old != file) {
            // Renamed. The old name going wrong leaves a stray copy, not a missing one.
            if let Err(e) = std::fs::remove_file(self.dir.join(&old)) {
                log::warn!("old text file {old} not removed: {e}");
            }
        }
        note.export = Some(Exported { file, hash, wrote: fingerprint("", content.as_bytes()) });
        Ok(true)
    }

    /// The note's file, if it has one and that file holds its text as it now stands: the
    /// one to hand on when the text is shared as a file.
    pub fn current(&self, note: &Note) -> Option<String> {
        let export = note.export.as_ref()?;
        let text = note.text();
        let hash = fingerprint(&file_name(note, &text), content(&text).as_bytes());
        (export.hash == hash && self.owns(export)).then(|| export.file.clone())
    }

    /// Whether the file a note recorded is still that note's: in this folder under a plain
    /// name, and holding exactly what the note wrote there.
    fn owns(&self, export: &Exported) -> bool {
        let plain = Path::new(&export.file).file_name().is_some_and(|name| name == export.file.as_str());
        plain && std::fs::read(self.dir.join(&export.file)).is_ok_and(|bytes| fingerprint("", &bytes) == export.wrote)
    }

    /// Write `content` under `name`, or under the first numbered stand-in that is free. `own`
    /// is the note's present file, the only one that is ever overwritten.
    fn place(&self, name: &str, own: Option<&str>, content: &str) -> io::Result<String> {
        let stem = name.strip_suffix(".txt").unwrap_or(name);
        if let Some(own) = own.filter(|own| is_named(own, stem)) {
            // It was read a moment ago, so it is this app's to write; what fails here is the
            // disk, and a stand-in would fail the same way.
            return write(&self.dir.join(own), content, false).map(|()| own.to_string());
        }
        let mut taken = None;
        for n in 1..=SAME_NAME {
            let candidate = if n == 1 { format!("{stem}.txt") } else { format!("{stem} ({n}).txt") };
            match write(&self.dir.join(&candidate), content, true) {
                Ok(()) => return Ok(candidate),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => taken = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(taken.unwrap_or_else(|| io::Error::other("no free file name")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Seg;

    fn exporter(name: &str) -> Exporter {
        let dir = std::env::temp_dir().join(format!("scribe-export-{name}-{}", std::process::id())).join("Documents").join("Scribe");
        let _ = std::fs::remove_dir_all(&dir);
        Exporter::new(dir)
    }

    fn note(created: i64, text: &str) -> Note {
        Note { id: format!("n{created}"), created, segments: vec![Seg { start: 0.0, end: 2.0, text: text.into(), ..Default::default() }], ..Default::default() }
    }

    fn files(exporter: &Exporter) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(exporter.dir()).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn names_carry_the_date_and_a_title_any_filesystem_takes() {
        let mut note = note(1_794_300_000, "Is this: a \"good\" name? Yes/no.");
        let at = clock::local(note.created);
        let stamp = format!("{:04}-{:02}-{:02} {:02}.{:02}", at.year, at.month, at.day, at.hour, at.minute);
        assert_eq!(file_name(&note, &note.text()), format!("{stamp} Is this a good name Yes no.txt"));
        note.title = "  Q3 / roadmap\nreview...  ".into();
        assert_eq!(file_name(&note, &note.text()), format!("{stamp} Q3 roadmap review.txt"));
        note.title = "…".into();
        assert_eq!(file_name(&note, &note.text()), format!("{stamp} Note.txt"));
        note.title = "é".repeat(200);
        assert_eq!(file_name(&note, &note.text()), format!("{stamp} {}.txt", "é".repeat(TITLE_CHARS)));
        note.title = ".hidden".into();
        assert_eq!(file_name(&note, &note.text()), format!("{stamp} hidden.txt"));
        // Four bytes a character: the cut is where the name would stop fitting a filesystem.
        note.title = "😀".repeat(200);
        let name = file_name(&note, &note.text());
        assert_eq!(name, format!("{stamp} {}.txt", "😀".repeat(TITLE_BYTES / 4)));
        assert!(name.len() + " (99)".len() <= 255, "{} bytes", name.len());
    }

    #[test]
    fn a_note_gets_one_file_that_follows_its_text_and_its_title() {
        let exporter = exporter("follow");
        assert_eq!(exporter.label(), "Documents/Scribe");
        let mut note = note(1_794_300_000, "Buy milk.");
        // Nothing to save, nothing written, not even the folder.
        let mut empty = Note { created: 5, ..Default::default() };
        assert!(!exporter.sync(&mut empty).unwrap());
        assert!(!exporter.dir().exists());

        assert!(exporter.sync(&mut note).unwrap());
        let first = note.export.clone().unwrap();
        assert!(first.file.ends_with(" Buy milk.txt"), "{}", first.file);
        assert_eq!(std::fs::read_to_string(exporter.dir().join(&first.file)).unwrap(), "Buy milk.\n");
        assert_eq!(exporter.current(&note).as_ref(), Some(&first.file));
        // Unchanged: left alone, even with the file gone, so one thrown away stays away.
        std::fs::remove_file(exporter.dir().join(&first.file)).unwrap();
        assert!(!exporter.sync(&mut note).unwrap());
        assert!(files(&exporter).is_empty());
        // And a file that is gone, or behind the note, is not the note's text to hand on.
        assert_eq!(exporter.current(&note), None);

        // New text under the same title rewrites the same file.
        note.title = "Shopping".into();
        assert!(exporter.sync(&mut note).unwrap());
        let named = note.export.clone().unwrap().file;
        note.body = Some("Buy milk.\nAnd eggs.".into());
        note.body_covers = 1;
        assert_eq!(exporter.current(&note), None, "the file has yet to see the edit");
        assert!(exporter.sync(&mut note).unwrap());
        assert_eq!(exporter.current(&note), Some(named.clone()));
        assert_eq!(note.export.as_ref().unwrap().file, named);
        assert_eq!(std::fs::read_to_string(exporter.dir().join(&named)).unwrap(), "Buy milk.\nAnd eggs.\n");

        // A new title moves it: one file, under the new name.
        note.title = "Groceries".into();
        assert!(exporter.sync(&mut note).unwrap());
        let renamed = note.export.clone().unwrap().file;
        assert!(renamed.ends_with(" Groceries.txt"));
        assert_eq!(files(&exporter), [renamed]);
    }

    #[test]
    fn notes_that_want_the_same_name_each_keep_their_own_file() {
        let exporter = exporter("same");
        let (mut a, mut b) = (note(1_794_300_000, "Same words."), note(1_794_300_010, "Same words."));
        assert!(exporter.sync(&mut a).unwrap() && exporter.sync(&mut b).unwrap());
        let (first, second) = (a.export.clone().unwrap().file, b.export.clone().unwrap().file);
        assert!(first.ends_with(" Same words.txt") && second.ends_with(" Same words (2).txt"), "{first} / {second}");

        // The second keeps its stand-in when its text changes, and never touches the first.
        b.segments[0].text = "Same words. And more.".into();
        b.title = "Same words".into();
        assert!(exporter.sync(&mut b).unwrap());
        assert_eq!(b.export.as_ref().unwrap().file, second);
        assert_eq!(std::fs::read_to_string(exporter.dir().join(&first)).unwrap(), "Same words.\n");
        assert_eq!(std::fs::read_to_string(exporter.dir().join(&second)).unwrap(), "Same words. And more.\n");
        assert_eq!(files(&exporter).len(), 2);
    }

    #[test]
    fn a_file_that_is_not_the_notes_own_is_never_overwritten() {
        let exporter = exporter("foreign");
        let mut note = note(1_794_300_000, "Mine.");
        std::fs::create_dir_all(exporter.dir()).unwrap();
        let wanted = file_name(&note, &note.text());
        std::fs::write(exporter.dir().join(&wanted), "someone else's").unwrap();
        assert!(exporter.sync(&mut note).unwrap());
        assert_eq!(std::fs::read_to_string(exporter.dir().join(&wanted)).unwrap(), "someone else's");
        assert!(note.export.as_ref().unwrap().file.ends_with(" Mine (2).txt"));

        // A folder that cannot be written says so, and the note is left as it was.
        let blocked = Exporter::new(exporter.dir().join(&wanted).join("under-a-file"));
        let mut other = note.clone();
        other.export = None;
        assert!(blocked.sync(&mut other).is_err());
        assert_eq!(other.export, None);
    }

    #[test]
    fn a_file_is_the_notes_own_only_while_it_holds_what_the_note_wrote() {
        let exporter = exporter("owned");
        let (mut a, mut b) = (note(1_794_300_000, "Same words."), note(1_794_300_010, "Same words."));
        assert!(exporter.sync(&mut a).unwrap());
        let name = a.export.clone().unwrap().file;
        // A's file is thrown away, and B, wanting the same name, takes it.
        std::fs::remove_file(exporter.dir().join(&name)).unwrap();
        b.segments[0].text = "Same words. B's own.".into();
        b.title = "Same words".into();
        assert!(exporter.sync(&mut b).unwrap());
        assert_eq!(b.export.as_ref().unwrap().file, name);

        // A changes: what is under its old name is B's now, and stays B's.
        a.segments[0].text = "Same words. A's own.".into();
        a.title = "Same words".into();
        assert!(exporter.sync(&mut a).unwrap());
        let stand_in = a.export.clone().unwrap().file;
        assert!(stand_in.ends_with(" Same words (2).txt"), "{stand_in}");
        assert_eq!(std::fs::read_to_string(exporter.dir().join(&name)).unwrap(), "Same words. B's own.\n");
        // A new title for A removes A's file, never B's.
        a.title = "Renamed".into();
        assert!(exporter.sync(&mut a).unwrap());
        let mut both = [name.clone(), a.export.clone().unwrap().file];
        both.sort();
        assert_eq!(files(&exporter), both);
        assert_eq!(std::fs::read_to_string(exporter.dir().join(&name)).unwrap(), "Same words. B's own.\n");

        // A file someone has written in by hand is theirs: the note's next text goes beside it.
        std::fs::write(exporter.dir().join(&name), "Same words. B's own.\nAnd a line added in an editor.\n").unwrap();
        b.segments[0].text = "Same words. B's second version.".into();
        assert!(exporter.sync(&mut b).unwrap());
        assert!(b.export.as_ref().unwrap().file.ends_with(" Same words (2).txt"));
        assert!(std::fs::read_to_string(exporter.dir().join(&name)).unwrap().contains("added in an editor"));

        // The stamp is the one the file was first given, whatever the clock says since: a
        // note made in another time zone is not a note that changed.
        let before = a.export.clone().unwrap();
        a.created += 7 * 3600;
        assert!(!exporter.sync(&mut a).unwrap());
        a.title = "Renamed again".into();
        assert!(exporter.sync(&mut a).unwrap());
        assert_eq!(a.export.as_ref().unwrap().file[..16], before.file[..16]);
        a.created -= 7 * 3600;

        // A name that points out of the folder is never followed.
        let outside = exporter.dir().parent().unwrap().join("outside.txt");
        std::fs::write(&outside, "Mine.\n").unwrap();
        let mut odd = note(1_794_300_600, "Mine.");
        odd.export = Some(Exported { file: "../outside.txt".into(), hash: 1, wrote: fingerprint("", b"Mine.\n") });
        odd.title = "Elsewhere".into();
        assert!(exporter.sync(&mut odd).unwrap());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "Mine.\n");
        assert!(odd.export.unwrap().file.ends_with(" Elsewhere.txt"));
    }

    #[test]
    fn stand_in_names_are_recognised_exactly() {
        assert!(is_named("a b.txt", "a b") && is_named("a b (2).txt", "a b") && is_named("a b (17).txt", "a b"));
        assert!(!is_named("a b c.txt", "a b") && !is_named("a b ().txt", "a b") && !is_named("a b (x).txt", "a b") && !is_named("a b (2)", "a b"));
        assert_ne!(fingerprint("a", b"bc"), fingerprint("ab", b"c"));
        // Stored in note files: the value may not drift between builds.
        assert_eq!(fingerprint("", b""), 0xaf63_bd4c_8601_b7df);
    }
}
