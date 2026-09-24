//! Decisions the Android IME bridge takes about the hidden EditText, kept free of JNI so the host can test them.

/// What to do when egui's settled text differs from the text last pushed to the EditText.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resync {
    /// Nothing to push.
    Keep,
    /// The EditText already holds egui's text: record it without pushing.
    Adopt,
    /// Push egui's text and restart the IME session over it.
    Push,
}

/// The resync for egui's `settled` text, given the text last pushed (`None` before the first seed)
/// and the EditText's own text (`None` while IME events are still in flight).
pub fn resync(settled: &str, synced: Option<&str>, mirror: Option<&str>) -> Resync {
    match (synced, mirror) {
        (None, _) => Resync::Keep,
        (Some(s), _) if s == settled => Resync::Keep,
        (Some(_), Some(m)) if m == settled => Resync::Adopt,
        (Some(_), None) => Resync::Keep,
        (Some(_), Some(_)) => Resync::Push,
    }
}

/// Caret inside a composition `len` chars long, counted from its start: `caret` when it lies within, else the end.
pub fn composition_caret(len: usize, caret: Option<usize>) -> usize {
    caret.map_or(len, |c| c.min(len))
}

/// Where an IME selection lands relative to the composition egui shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaretPlacement {
    /// No composition is tracked: move egui's cursor.
    Plain,
    /// Inside the composition: keep it, with this range relative to its start.
    Inside(std::ops::Range<usize>),
    /// Outside it: finish the composition where it is, then move.
    Outside,
    /// egui no longer holds the composition where it was put: stop tracking it.
    Lost,
}

/// Placement of the IME's `selection`, given the tracked preedit's length and egui's live selection.
pub fn caret_placement(preedit_len: usize, live: Option<(usize, usize)>, selection: (usize, usize)) -> CaretPlacement {
    if preedit_len == 0 {
        return CaretPlacement::Plain;
    }
    match live {
        Some((a, b)) if b >= a && b - a == preedit_len => {
            if a <= selection.0 && selection.0 <= selection.1 && selection.1 <= b {
                CaretPlacement::Inside(selection.0 - a..selection.1 - a)
            } else {
                CaretPlacement::Outside
            }
        }
        _ => CaretPlacement::Lost,
    }
}

/// What a `finishComposingText` does to egui.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    /// Commit the tracked preedit over the composition egui shows.
    Commit,
    /// End egui's composition at a collapsed caret, changing no text.
    End,
    /// Leave egui's text and selection alone.
    Drop,
}

/// The finish for a tracked preedit `preedit_len` chars long, given egui's live selection and whether
/// an earlier event in the same batch already changed egui (its live selection is then out of date).
pub fn finish(preedit_len: usize, live: Option<(usize, usize)>, batch_mutated: bool) -> Finish {
    match live {
        _ if preedit_len == 0 => Finish::Drop,
        _ if batch_mutated => Finish::Commit,
        Some((a, b)) if b >= a && b - a == preedit_len => Finish::Commit,
        Some((a, b)) if a == b => Finish::End,
        _ => Finish::Drop,
    }
}

/// Seconds a keyboard may stay up while no field wants it before the bridge hides it.
pub const STRAY_KEYBOARD_SECS: f64 = 0.4;

/// Watches for a soft keyboard that is up while no egui field wants it.
#[derive(Clone, Copy, Debug, Default)]
pub struct StrayKeyboard {
    since: Option<f64>,
    hidden: bool,
}

impl StrayKeyboard {
    /// `true` once per stray spell, after the keyboard has been up unwanted for [`STRAY_KEYBOARD_SECS`].
    pub fn update(&mut self, now: f64, wanted: bool, visible: bool) -> bool {
        if wanted || !visible {
            *self = Self::default();
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if !self.hidden && now - since >= STRAY_KEYBOARD_SECS {
            self.hidden = true;
            return true;
        }
        false
    }

    /// Whether a stray keyboard is being timed, so the caller keeps frames coming.
    pub fn armed(&self) -> bool {
        self.since.is_some() && !self.hidden
    }
}

/// Seconds a requested keyboard may take to appear before the bridge asks once more.
pub const SHOW_RETRY_SECS: f64 = 0.8;

/// Watches a keyboard show request that has not produced a keyboard yet.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShowWatch {
    requested: Option<f64>,
    retried: bool,
}

impl ShowWatch {
    /// Record a show request at `now`.
    pub fn requested(&mut self, now: f64) {
        *self = Self { requested: Some(now), retried: false };
    }

    /// Stop watching: the keyboard appeared or is no longer wanted.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `true` once when the keyboard is still not open [`SHOW_RETRY_SECS`] after the request.
    pub fn update(&mut self, now: f64, open: bool) -> bool {
        match self.requested {
            None => false,
            Some(_) if open => {
                self.clear();
                false
            }
            Some(at) if !self.retried && now - at >= SHOW_RETRY_SECS => {
                self.retried = true;
                true
            }
            Some(_) => false,
        }
    }

    /// Whether a request is still being watched, so the caller keeps frames coming.
    pub fn armed(&self) -> bool {
        self.requested.is_some() && !self.retried
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_the_ime_already_mirrors_needs_no_push() {
        assert_eq!(resync("hello", Some(""), Some("hello")), Resync::Adopt);
        assert_eq!(resync("hello", Some("hello"), Some("hello")), Resync::Keep);
    }

    #[test]
    fn an_app_edit_the_mirror_lacks_is_pushed() {
        assert_eq!(resync("hi", Some("hi\n"), Some("hi\n")), Resync::Push);
        assert_eq!(resync("", Some("draft"), Some("draft")), Resync::Push);
    }

    #[test]
    fn nothing_is_pushed_before_the_seed_or_while_events_are_in_flight() {
        assert_eq!(resync("hello", None, Some("")), Resync::Keep);
        assert_eq!(resync("hello", Some("hell"), None), Resync::Keep);
    }

    #[test]
    fn the_composition_caret_stays_inside_the_word() {
        assert_eq!(composition_caret(5, None), 5);
        assert_eq!(composition_caret(5, Some(2)), 2);
        assert_eq!(composition_caret(5, Some(9)), 5);
        assert_eq!(composition_caret(0, Some(3)), 0);
    }

    #[test]
    fn a_selection_without_a_composition_moves_the_cursor() {
        assert_eq!(caret_placement(0, Some((7, 7)), (3, 3)), CaretPlacement::Plain);
    }

    #[test]
    fn a_caret_inside_the_composition_keeps_it() {
        // Gboard re-composing "hellole": region 0..7, then the caret back to 7.
        assert_eq!(caret_placement(7, Some((0, 7)), (7, 7)), CaretPlacement::Inside(7..7));
        assert_eq!(caret_placement(7, Some((0, 7)), (0, 0)), CaretPlacement::Inside(0..0));
        assert_eq!(caret_placement(3, Some((4, 7)), (5, 6)), CaretPlacement::Inside(1..2));
    }

    #[test]
    fn a_caret_outside_the_composition_finishes_it_first() {
        assert_eq!(caret_placement(3, Some((4, 7)), (9, 9)), CaretPlacement::Outside);
        assert_eq!(caret_placement(3, Some((4, 7)), (2, 5)), CaretPlacement::Outside);
    }

    #[test]
    fn a_composition_egui_no_longer_holds_is_dropped() {
        assert_eq!(caret_placement(7, Some((7, 7)), (7, 7)), CaretPlacement::Lost);
        assert_eq!(caret_placement(3, None, (1, 1)), CaretPlacement::Lost);
    }

    #[test]
    fn finish_commits_only_the_composition_egui_still_shows() {
        assert_eq!(finish(7, Some((0, 7)), false), Finish::Commit);
        // The duplicate-word case: the range collapsed to the caret, committing would re-insert the word.
        assert_eq!(finish(7, Some((7, 7)), false), Finish::End);
        // A user selection of another length is not the composition.
        assert_eq!(finish(7, Some((2, 5)), false), Finish::Drop);
        assert_eq!(finish(0, Some((0, 7)), false), Finish::Drop);
    }

    #[test]
    fn finish_after_a_change_in_the_same_batch_commits() {
        assert_eq!(finish(3, Some((0, 0)), true), Finish::Commit);
        assert_eq!(finish(0, Some((0, 0)), true), Finish::Drop);
    }

    #[test]
    fn a_stray_keyboard_is_hidden_once_after_the_grace_period() {
        let mut stray = StrayKeyboard::default();
        assert!(!stray.update(10.0, false, true));
        assert!(stray.armed());
        assert!(!stray.update(10.2, false, true));
        assert!(stray.update(10.4, false, true));
        assert!(!stray.update(10.9, false, true));
        assert!(!stray.armed());
    }

    #[test]
    fn a_wanted_or_hidden_keyboard_resets_the_stray_watch() {
        let mut stray = StrayKeyboard::default();
        stray.update(1.0, false, true);
        assert!(!stray.update(1.3, true, true));
        assert!(!stray.update(1.5, false, true));
        assert!(!stray.update(1.8, false, false));
        assert!(!stray.armed());
        assert!(!stray.update(2.0, false, true));
        assert!(stray.update(2.5, false, true));
    }

    #[test]
    fn a_show_that_never_appeared_is_retried_once() {
        let mut show = ShowWatch::default();
        assert!(!show.update(0.0, false));
        show.requested(1.0);
        assert!(show.armed());
        assert!(!show.update(1.5, false));
        assert!(show.update(1.8, false));
        assert!(!show.update(3.0, false));
        assert!(!show.armed());
    }

    #[test]
    fn a_keyboard_that_opens_ends_the_show_watch() {
        let mut show = ShowWatch::default();
        show.requested(1.0);
        assert!(!show.update(1.2, true));
        assert!(!show.update(5.0, false));
        assert!(!show.armed());
    }
}
