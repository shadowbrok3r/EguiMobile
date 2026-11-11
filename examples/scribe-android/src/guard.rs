//! A breadcrumb around NPU work, so that a start-up which keeps killing the process stops.
//!
//! The NPU path runs vendor code inside this process. A context that will not load or a graph
//! that aborts takes the app down with it, and nothing in Rust gets a say. Left alone, the
//! next launch would queue the same recording, load the same model and die the same way a few
//! seconds in, every time — with no way to reach Settings, and with "clear storage", which
//! also deletes the recordings, as the only thing left to try.
//!
//! So what the NPU is about to do is written down first and crossed out after. A launch that
//! finds it still written counts a strike; two in a row and transcription waits to be asked
//! for, while everything else — recording above all — carries on. A run that gets through
//! clears the count.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Processes in a row that may die mid-work before transcription stops starting by itself. One
/// is forgiven: Android kills apps for its own reasons, sometimes in the middle of a window.
pub const STRIKES_ALLOWED: u32 = 2;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// What the NPU is doing right now; `None` between runs.
    busy: Option<String>,
    /// Processes in a row that ended with `busy` set, or with the engine panicking.
    strikes: u32,
}

pub struct Guard {
    path: PathBuf,
    state: Mutex<State>,
    /// Strikes at which it trips.
    allowed: u32,
}

impl Guard {
    /// Read what the last process left behind and count it. Also returns what that process
    /// was doing, if it died doing it.
    pub fn open(path: PathBuf) -> (Self, Option<String>) {
        Self::open_allowing(path, STRIKES_ALLOWED)
    }

    /// [`Guard::open`] for work that gets no second chance: it trips at the first death.
    /// For something short and optional, one crash is one too many to risk repeating, and a
    /// wrong guess costs a tap.
    pub fn open_strict(path: PathBuf) -> (Self, Option<String>) {
        Self::open_allowing(path, 1)
    }

    fn open_allowing(path: PathBuf, allowed: u32) -> (Self, Option<String>) {
        let mut state: State = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        let died = state.busy.take();
        if died.is_some() {
            state.strikes += 1;
        }
        let guard = Self { path, state: Mutex::new(state), allowed };
        if died.is_some() {
            guard.write(&guard.lock());
        }
        (guard, died)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Not synced to storage: a process dying takes nothing in the page cache with it, and
    /// that is the only death this is here to notice.
    fn write(&self, state: &State) {
        let temporary = self.path.with_extension("json.tmp");
        let saved = serde_json::to_string(state).map_err(std::io::Error::other).and_then(|text| std::fs::write(&temporary, text)).and_then(|()| std::fs::rename(&temporary, &self.path));
        if let Err(e) = saved {
            log::warn!("crash guard not written: {e}");
        }
    }

    /// Whether transcription should wait to be asked for.
    pub fn tripped(&self) -> bool {
        self.lock().strikes >= self.allowed
    }

    pub fn strikes(&self) -> u32 {
        self.lock().strikes
    }

    /// Write down `what` until the returned value is dropped.
    pub fn enter(&self, what: impl Into<String>) -> Busy<'_> {
        let mut state = self.lock();
        state.busy = Some(what.into());
        self.write(&state);
        Busy { guard: self, proven: false }
    }

    /// Count a failure that did not take the process with it.
    pub fn strike(&self) {
        let mut state = self.lock();
        state.busy = None;
        state.strikes += 1;
        self.write(&state);
    }
}

/// NPU work in flight.
pub struct Busy<'g> {
    guard: &'g Guard,
    proven: bool,
}

impl Busy<'_> {
    /// The NPU ran and came back: the path works, and earlier deaths are forgiven.
    pub fn proven(mut self) {
        self.proven = true;
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut state = self.guard.lock();
        state.busy = None;
        if self.proven {
            state.strikes = 0;
        }
        self.guard.write(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scribe-guard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("engine-guard.json")
    }

    #[test]
    fn two_deaths_in_a_row_trip_it_and_a_good_run_clears_it() {
        let path = path("strikes");
        let (guard, died) = Guard::open(path.clone());
        assert_eq!((died, guard.tripped()), (None, false));

        // The process dies with work in flight: the value is never dropped.
        std::mem::forget(guard.enter("loading whisper-base"));
        let (guard, died) = Guard::open(path.clone());
        assert_eq!((died.as_deref(), guard.strikes(), guard.tripped()), (Some("loading whisper-base"), 1, false));
        std::mem::forget(guard.enter("transcribing a note"));
        let (guard, died) = Guard::open(path.clone());
        assert_eq!((died.as_deref(), guard.strikes(), guard.tripped()), (Some("transcribing a note"), 2, true));

        // A launch that does no NPU work leaves it tripped: only a run that works clears it.
        let (guard, died) = Guard::open(path.clone());
        assert_eq!((died, guard.tripped()), (None, true));
        // Work that ended without proving anything (a failed load, a cancel) changes nothing.
        drop(guard.enter("loading whisper-base"));
        assert!(Guard::open(path.clone()).0.tripped());
        guard.enter("the self-test").proven();
        let (guard, died) = Guard::open(path);
        assert_eq!((died, guard.strikes(), guard.tripped()), (None, 0, false));
    }

    #[test]
    fn a_panic_counts_without_a_death_and_a_damaged_file_counts_as_clean() {
        let path = path("panic");
        let (guard, _) = Guard::open(path.clone());
        guard.strike();
        guard.strike();
        assert!(Guard::open(path.clone()).0.tripped());

        std::fs::write(&path, b"{ not json").unwrap();
        let (guard, died) = Guard::open(path);
        assert_eq!((died, guard.strikes()), (None, 0));
    }

    #[test]
    fn a_strict_guard_trips_at_the_first_death() {
        let path = path("strict");
        let (guard, _) = Guard::open_strict(path.clone());
        assert!(!guard.tripped());
        std::mem::forget(guard.enter("loading the speaker model"));
        let (guard, died) = Guard::open_strict(path.clone());
        assert_eq!((died.as_deref(), guard.tripped()), (Some("loading the speaker model"), true));
        // The same file under the ordinary allowance would still be given another go.
        assert!(!Guard::open(path.clone()).0.tripped());
        guard.enter("telling the speakers apart").proven();
        assert!(!Guard::open_strict(path).0.tripped());
    }
}
