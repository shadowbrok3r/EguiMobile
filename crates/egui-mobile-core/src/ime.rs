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
}
