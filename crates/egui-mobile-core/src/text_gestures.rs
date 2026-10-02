//! Touch selection in text fields, alongside the [`magnifier`](crate::magnifier) lens.
//!
//! - Press and hold on the focused `TextEdit` selects the word under the finger; sliding on without
//!   lifting grows the selection a whole word at a time.
//! - A tap, then a press and hold on the same spot, moves a bare caret with the finger and selects
//!   nothing. The press must follow the tap within egui's double-click delay.
//!
//! A hold is [`HOLD_SECS`] without moving past egui's click distance, the moment the lens appears.
//! A press that slides before then is left to egui, which drag-selects characters, and a quick double
//! tap still selects a word through egui's own double click. An unfocused field is left alone too:
//! on a touch screen egui focuses it only on a tap.
//!
//! The iOS and Android runtimes install it for every app; [`set_enabled`] turns it off.

use std::ops::Range;
use std::sync::Arc;

use egui::emath::TSTransform;
use egui::epaint::text::PlacedRow;
use egui::text::{CCursor, CCursorRange, Galley};
use egui::text_edit::TextEditState;
use egui::text_selection::text_cursor_state::{ccursor_next_word, ccursor_previous_word, is_word_char};
use egui::{Context, Event, IMEPurpose, Id, PointerButton, Pos2, RawInput, Rect, Shape, Ui, Vec2};

use crate::magnifier::HOLD_SECS;

/// How far a press may land from the tap before it and still move the caret.
const DOUBLE_TAP_SLOP: f32 = 40.0;

/// Install the text gestures on `ctx`. Installing twice is harmless.
pub fn install(ctx: &Context) {
    ctx.add_plugin(TextGestures::default());
}

/// Turn the text gestures on or off for this context.
pub fn set_enabled(ctx: &Context, enabled: bool) {
    ctx.with_plugin::<TextGestures, _>(|g| g.enabled = enabled);
}

struct TextGestures {
    enabled: bool,
    /// The focused field as the last pass painted it; kept only while a finger is down.
    field: Option<Field>,
    gesture: Gesture,
    /// When and where the last quick tap lifted.
    last_tap: Option<(f64, Pos2)>,
}

impl Default for TextGestures {
    fn default() -> Self {
        Self { enabled: true, field: None, gesture: Gesture::Idle, last_tap: None }
    }
}

enum Gesture {
    Idle,
    /// Down, not yet held or moved. `field` is the field under the press with the range a hold
    /// there selects; `caret` when the press follows a tap.
    Pressed { origin: Pos2, start: f64, caret: bool, field: Option<(Id, Range<usize>)> },
    /// Held: the selection spans `anchor` and, once the finger leaves `grip`, the word under it.
    Words { id: Id, anchor: Range<usize>, grip: Pos2, sliding: bool },
    /// Held after a tap: the caret follows the finger.
    Caret { id: Id },
    /// Left to egui until the finger lifts.
    Passive,
}

impl egui::Plugin for TextGestures {
    fn debug_name(&self) -> &'static str {
        "text-gestures"
    }

    fn input_hook(&mut self, _ctx: &Context, input: &mut RawInput) {
        let in_field = match &self.gesture {
            Gesture::Pressed { field, .. } => field.is_some(),
            Gesture::Words { .. } | Gesture::Caret { .. } => true,
            Gesture::Idle | Gesture::Passive => false,
        };
        // The iOS runtime turns a held touch into a secondary click, whose press would move the caret.
        if self.enabled && in_field {
            input
                .events
                .retain(|e| !matches!(e, Event::PointerButton { button, .. } if *button != PointerButton::Primary));
        }
    }

    fn on_begin_pass(&mut self, ui: &mut Ui) {
        if self.enabled {
            self.track(ui.ctx());
        } else {
            self.gesture = Gesture::Idle;
        }
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx();
        let down = !matches!(self.gesture, Gesture::Idle | Gesture::Passive);
        self.field = if down { Field::focused(ctx) } else { None };
        let time = ctx.input(|i| i.time);
        match &mut self.gesture {
            // Read on the press pass only: the word the finger went down on.
            Gesture::Pressed { origin, start, field, .. } if *start == time => {
                *field = self.field.as_ref().filter(|f| f.contains(*origin)).map(|f| (f.id, f.word(*origin)));
            }
            Gesture::Caret { id } => collapse(ctx, *id),
            _ => {}
        }
    }
}

impl TextGestures {
    /// Advance the gesture by this pass's input and apply it to the field.
    fn track(&mut self, ctx: &Context) {
        let (buttons, down, pos, time) = ctx.input(|i| {
            let buttons: Vec<(Pos2, bool)> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    Event::PointerButton { pos, button: PointerButton::Primary, pressed, .. } => Some((*pos, *pressed)),
                    _ => None,
                })
                .collect();
            (buttons, i.pointer.primary_down(), i.pointer.latest_pos(), i.time)
        });
        let (slop, double_delay) =
            ctx.options(|o| (o.input_options.max_click_dist, o.input_options.max_double_click_delay));
        for (at, pressed) in buttons {
            if pressed {
                let caret = self
                    .last_tap
                    .take()
                    .is_some_and(|(t, p)| time - t <= double_delay && p.distance(at) <= DOUBLE_TAP_SLOP);
                self.gesture = Gesture::Pressed { origin: at, start: time, caret, field: None };
            } else {
                if let Gesture::Pressed { origin, start, .. } = self.gesture
                    && time - start < HOLD_SECS
                    && origin.distance(at) <= slop
                {
                    self.last_tap = Some((time, at));
                }
                self.gesture = Gesture::Idle;
            }
        }
        // A cancelled touch leaves the button down but takes the pointer away.
        let Some(pos) = pos.filter(|_| down) else {
            self.gesture = Gesture::Idle;
            return;
        };
        if let Gesture::Pressed { origin, start, caret, field } = &mut self.gesture {
            let moved = origin.distance(pos) > slop;
            if !moved && time - *start < HOLD_SECS {
                ctx.request_repaint_after_secs((*start + HOLD_SECS - time) as f32);
                return;
            }
            self.gesture = match (field.take(), *caret) {
                (Some((id, _)), true) => Gesture::Caret { id },
                (Some((id, anchor)), false) if !moved => Gesture::Words { id, anchor, grip: pos, sliding: false },
                _ => Gesture::Passive,
            };
        }
        let field = self.field.as_ref();
        match &mut self.gesture {
            Gesture::Words { id, anchor, grip, sliding } => {
                // egui would drag-select characters from the press point.
                ctx.stop_dragging();
                if let Some(field) = field.filter(|f| f.id == *id) {
                    *sliding |= grip.distance(pos) > slop;
                    let range = if *sliding { extend(anchor, &field.word(pos)) } else { span(anchor) };
                    select(ctx, *id, range);
                }
            }
            Gesture::Caret { id } => {
                if let Some(field) = field.filter(|f| f.id == *id) {
                    select(ctx, *id, CCursorRange::one(field.caret(pos)));
                    // egui's drag places the caret against this pass's layout and keeps it from blinking.
                    ctx.set_dragged_id(*id);
                }
            }
            _ => {}
        }
    }
}

/// A focused `TextEdit` and the galley it painted.
struct Field {
    id: Id,
    /// The field's box, in screen coordinates; wider than the text it holds.
    rect: Rect,
    password: bool,
    to_global: TSTransform,
    /// Layer position and galley of the painted text; `None` while the field is empty.
    text: Option<(Pos2, Arc<Galley>)>,
}

impl Field {
    fn focused(ctx: &Context) -> Option<Self> {
        let ime = ctx.output(|o| o.ime)?;
        let id = ctx.memory(|m| m.focused())?;
        TextEditState::load(ctx, id)?;
        let (layer, rect) = ctx.viewport(|v| v.this_pass.widgets.get(id).map(|w| (w.layer_id, w.interact_rect)))?;
        let to_global = ctx.layer_transform_to_global(layer).unwrap_or_default();
        // The TextEdit clips its galley to the text area; hint, prefix and suffix text use the parent clip.
        let area = (to_global.inverse() * ime.rect).expand(1.5);
        let text = ctx.graphics(|g| {
            g.get(layer)?.all_entries().find_map(|c| match &c.shape {
                Shape::Text(t) if area.contains_rect(c.clip_rect) => Some((t.pos, Arc::clone(&t.galley))),
                _ => None,
            })
        });
        Some(Self { id, rect: to_global * rect, password: ime.purpose == IMEPurpose::Password, to_global, text })
    }

    fn contains(&self, p: Pos2) -> bool {
        self.rect.expand(4.0).contains(p)
    }

    /// The galley and `p` in its coordinates.
    fn local(&self, p: Pos2) -> Option<(&Galley, Vec2)> {
        let (origin, galley) = self.text.as_ref()?;
        Some((galley, self.to_global.inverse() * p - *origin))
    }

    /// The caret position nearest `p`.
    fn caret(&self, p: Pos2) -> CCursor {
        self.local(p).map_or_else(CCursor::default, |(galley, at)| galley.cursor_from_pos(at))
    }

    /// The range a hold at `p` selects: the word there, else the bare caret; all of a password.
    fn word(&self, p: Pos2) -> Range<usize> {
        let Some((galley, at)) = self.local(p) else { return 0..0 };
        if self.password {
            return 0..galley.text().chars().count();
        }
        word_at(galley, at).unwrap_or_else(|| {
            let c = galley.cursor_from_pos(at).index.0;
            c..c
        })
    }
}

/// The word at `at` (galley coordinates): right of the nearest caret position on that row, else left
/// of it. `None` when neither neighbour is a word character.
fn word_at(galley: &Galley, at: Vec2) -> Option<Range<usize>> {
    let (first, row) = row_at(galley, at.y)?;
    let col = row.char_at(at.x - row.pos.x).0;
    let word = |c: usize| row.glyphs.get(c).is_some_and(|g| is_word_char(g.chr)).then_some(first + c);
    let c = word(col).or_else(|| col.checked_sub(1).and_then(word))?;
    Some(word_around(galley.text(), c))
}

/// The row nearest `y` and the index of its first char.
fn row_at(galley: &Galley, y: f32) -> Option<(usize, &PlacedRow)> {
    let mut first = 0;
    let mut best: Option<(f32, usize, &PlacedRow)> = None;
    for row in &galley.rows {
        let dist = (row.min_y() - y).max(y - row.max_y());
        if best.is_none_or(|(d, ..)| dist < d) {
            best = Some((dist, first, row));
        }
        first += row.char_count_including_newline().0;
    }
    best.map(|(_, first, row)| (first, row))
}

/// The word around char `c`, a word character, by egui's word boundaries.
fn word_around(text: &str, c: usize) -> Range<usize> {
    // egui's word scans walk the whole string; c's paragraph bounds them.
    let (mut lo, mut lo_byte, mut hi_byte) = (0, 0, text.len());
    for (i, (b, ch)) in text.char_indices().enumerate() {
        if ch == '\n' {
            if i < c {
                (lo, lo_byte) = (i + 1, b + 1);
            } else {
                hi_byte = b;
                break;
            }
        }
    }
    let paragraph = &text[lo_byte..hi_byte];
    let start = ccursor_previous_word(paragraph, CCursor::new(c - lo + 1)).index.0;
    let end = ccursor_next_word(paragraph, CCursor::new(c - lo)).index.0;
    lo + start..lo + end
}

/// `r` selected, the caret at its end.
fn span(r: &Range<usize>) -> CCursorRange {
    CCursorRange::two(row_start(r.start), CCursor::new(r.end))
}

/// From `anchor` out to `finger`, whole words at both ends, the caret on the finger's side.
fn extend(anchor: &Range<usize>, finger: &Range<usize>) -> CCursorRange {
    if finger.start < anchor.start {
        CCursorRange::two(CCursor::new(anchor.end), row_start(finger.start))
    } else if finger.end > anchor.end {
        CCursorRange::two(row_start(anchor.start), CCursor::new(finger.end))
    } else {
        span(anchor)
    }
}

/// A cursor before char `i`, on the row that starts there when a wrap falls at `i`.
fn row_start(i: usize) -> CCursor {
    CCursor { index: i.into(), prefer_next_row: true }
}

fn select(ctx: &Context, id: Id, range: CCursorRange) {
    let Some(mut state) = TextEditState::load(ctx, id) else { return };
    if state.cursor.char_range() != Some(range) {
        state.cursor.set_char_range(Some(range));
        state.store(ctx, id);
    }
}

/// Collapse the field's selection onto its primary end.
fn collapse(ctx: &Context, id: Id) {
    let Some(mut state) = TextEditState::load(ctx, id) else { return };
    let Some(range) = state.cursor.char_range() else { return };
    if range.primary.index != range.secondary.index {
        state.cursor.set_char_range(Some(CCursorRange::one(range.primary)));
        state.store(ctx, id);
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{TextEdit, TouchDeviceId, TouchId, TouchPhase, vec2};

    const TEXT: &str = "hello magnified world";

    /// One focused `TextEdit` driven by touch events, with a clock.
    struct Rig {
        ctx: Context,
        text: String,
        password: bool,
        multiline: bool,
        time: f64,
        /// Screen position and galley of the painted text.
        galley: Option<(Pos2, Arc<Galley>)>,
    }

    impl Rig {
        fn new(text: &str) -> Self {
            let ctx = Context::default();
            install(&ctx);
            ctx.memory_mut(|m| m.request_focus(Id::new("field")));
            Self { ctx, text: text.to_owned(), password: false, multiline: false, time: 0.0, galley: None }
        }

        fn start(mut self) -> Self {
            self.frame(0.0, Vec::new());
            self
        }

        /// Advance the clock by `dt` and run a frame with `events`.
        fn frame(&mut self, dt: f64, events: Vec<Event>) {
            self.time += dt;
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 800.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let mut galley = None;
            let mut out = self.ctx.run_ui(input, |ui| {
                ui.add_space(300.0);
                let edit = if self.multiline { TextEdit::multiline(&mut self.text) } else { TextEdit::singleline(&mut self.text) };
                let o = edit.password(self.password).id(Id::new("field")).show(ui);
                galley = Some((o.galley_pos - vec2(o.galley.rect.left(), 0.0), o.galley));
            });
            out.textures_delta.clear();
            self.galley = galley;
        }

        /// Run frames without input for `secs`, as repaint wake-ups do.
        fn wait(&mut self, secs: f64) {
            let end = self.time + secs;
            while self.time < end {
                self.frame(0.05, Vec::new());
            }
        }

        /// Just right of the caret position before char `i`.
        fn pos(&self, i: usize) -> Pos2 {
            let (origin, galley) = self.galley.as_ref().expect("a painted galley");
            let caret = galley.pos_from_cursor(CCursor { index: i.into(), prefer_next_row: true });
            *origin + vec2(caret.min.x + 1.0, caret.center().y)
        }

        /// The selection as (secondary, primary).
        fn selection(&self) -> (usize, usize) {
            let range = TextEditState::load(&self.ctx, Id::new("field")).unwrap().cursor.char_range().unwrap();
            (range.secondary.index.0, range.primary.index.0)
        }

        fn down(&mut self, dt: f64, at: Pos2) {
            self.frame(dt, touch(TouchPhase::Start, at));
        }

        fn slide(&mut self, dt: f64, at: Pos2) {
            self.frame(dt, touch(TouchPhase::Move, at));
        }

        fn up(&mut self, dt: f64, at: Pos2) {
            self.frame(dt, touch(TouchPhase::End, at));
        }
    }

    /// egui-winit's events for a touch: the touch itself, then the emulated pointer.
    fn touch(phase: TouchPhase, pos: Pos2) -> Vec<Event> {
        let button = |pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Default::default() };
        let mut events = vec![Event::Touch { device_id: TouchDeviceId(0), id: TouchId(1), phase, pos, force: None }];
        match phase {
            TouchPhase::Start => events.extend([Event::PointerMoved(pos), button(true)]),
            TouchPhase::Move => events.push(Event::PointerMoved(pos)),
            TouchPhase::End | TouchPhase::Cancel => events.extend([button(false), Event::PointerGone]),
        }
        events
    }

    #[test]
    fn a_hold_selects_the_word_under_the_finger() {
        let mut rig = Rig::new(TEXT).start();
        let at = rig.pos(8);
        rig.down(0.1, at);
        rig.wait(HOLD_SECS - 0.1);
        let (a, b) = rig.selection();
        assert_eq!(a, b, "nothing selected before the hold");
        rig.wait(0.15);
        assert_eq!(rig.selection(), (6, 15));
        // Past egui's own long press, and after lifting.
        rig.wait(0.6);
        assert_eq!(rig.selection(), (6, 15));
        rig.up(0.05, at);
        assert_eq!(rig.selection(), (6, 15));
    }

    #[test]
    fn sliding_after_a_hold_grows_the_selection_a_word_at_a_time() {
        for wait in [HOLD_SECS + 0.05, 1.0] {
            let mut rig = Rig::new(TEXT).start();
            let at = rig.pos(8);
            rig.down(0.1, at);
            rig.wait(wait);
            rig.slide(0.05, rig.pos(18));
            assert_eq!(rig.selection(), (6, 21), "into the next word, held {wait}s");
            rig.slide(0.05, rig.pos(2));
            assert_eq!(rig.selection(), (15, 0), "back past the held word, held {wait}s");
            rig.slide(0.05, rig.pos(10));
            assert_eq!(rig.selection(), (6, 15), "back onto the held word, held {wait}s");
        }
    }

    #[test]
    fn a_small_wobble_after_the_hold_keeps_the_word() {
        let mut rig = Rig::new(TEXT).start();
        let at = rig.pos(14);
        rig.down(0.1, at);
        rig.wait(HOLD_SECS + 0.05);
        rig.slide(0.05, at + vec2(4.0, 0.0));
        assert_eq!(rig.selection(), (6, 15));
    }

    #[test]
    fn a_hold_beside_a_word_selects_that_word() {
        let mut rig = Rig::new(TEXT).start();
        // Left half of the space after "hello".
        let at = rig.pos(5);
        rig.down(0.1, at);
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (0, 5));
        rig.up(0.05, at);
        // Past the end of the text, inside the field.
        let end = rig.pos(21) + vec2(30.0, 0.0);
        rig.down(1.0, end);
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (16, 21));
    }

    #[test]
    fn a_hold_between_spaces_leaves_the_caret() {
        let mut rig = Rig::new("one   two").start();
        let at = rig.pos(4);
        rig.down(0.1, at);
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (4, 4));
    }

    #[test]
    fn a_hold_in_a_multiline_field_selects_on_its_own_row() {
        let mut rig = Rig::new("one two\nthree four");
        rig.multiline = true;
        let mut rig = rig.start();
        let at = rig.pos(10);
        rig.down(0.1, at);
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (8, 13));
        rig.up(0.05, at);
        // Past the end of the first row.
        let row_end = rig.pos(7) + vec2(30.0, 0.0);
        rig.down(1.0, row_end);
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (4, 7));
    }

    #[test]
    fn a_hold_in_a_password_selects_all_of_it() {
        let mut rig = Rig::new("hunter two");
        rig.password = true;
        let mut rig = rig.start();
        let at = rig.pos(3);
        rig.down(0.1, at);
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (0, 10));
    }

    #[test]
    fn a_tap_then_a_hold_moves_a_bare_caret() {
        let mut rig = Rig::new(TEXT).start();
        let at = rig.pos(8);
        rig.down(0.1, at);
        rig.up(0.08, at);
        rig.down(0.1, at);
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (8, 8));
        assert_eq!(rig.ctx.dragged_id(), Some(Id::new("field")), "egui's drag keeps a still caret lit");
        // Clear of the "fi" ligature, whose second char has no width of its own.
        for i in [7, 17, 3] {
            rig.slide(0.05, rig.pos(i));
            assert_eq!(rig.selection(), (i, i));
        }
        // Past egui's long press, which ends egui's own drag.
        rig.wait(0.6);
        rig.slide(0.05, rig.pos(10));
        assert_eq!(rig.selection(), (10, 10));
        rig.up(0.05, rig.pos(10));
        assert_eq!(rig.selection(), (10, 10));
    }

    #[test]
    fn a_tap_then_a_slide_moves_the_caret_at_once() {
        let mut rig = Rig::new(TEXT).start();
        let at = rig.pos(8);
        rig.down(0.1, at);
        rig.up(0.08, at);
        rig.down(0.1, at);
        rig.slide(0.05, rig.pos(18));
        assert_eq!(rig.selection(), (18, 18));
    }

    #[test]
    fn a_press_far_from_the_tap_is_a_hold_again() {
        let mut rig = Rig::new(TEXT).start();
        rig.down(0.1, rig.pos(1));
        rig.up(0.08, rig.pos(1));
        rig.down(0.1, rig.pos(18));
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (16, 21));
    }

    #[test]
    fn a_slide_before_the_hold_is_left_to_egui() {
        let mut rig = Rig::new(TEXT).start();
        rig.down(0.1, rig.pos(8));
        rig.slide(0.05, rig.pos(18));
        assert_eq!(rig.selection(), (8, 18));
    }

    #[test]
    fn a_quick_double_tap_still_selects_the_word() {
        let mut rig = Rig::new(TEXT).start();
        let at = rig.pos(8);
        rig.down(0.1, at);
        rig.up(0.05, at);
        rig.down(0.05, at);
        rig.up(0.05, at);
        assert_eq!(rig.selection(), (6, 15));
    }

    #[test]
    fn a_hold_on_an_unfocused_field_is_left_to_egui() {
        let ctx = Context::default();
        install(&ctx);
        ctx.memory_mut(|m| m.request_focus(Id::new("a")));
        let (mut a, mut b) = (String::from("focused"), String::from("other words"));
        // Runs a frame and returns the second field's rect.
        let mut frame = |time: f64, events: Vec<Event>| {
            let input = RawInput { time: Some(time), events, ..Default::default() };
            let mut rect = Rect::NOTHING;
            let mut out = ctx.run_ui(input, |ui| {
                ui.add(TextEdit::singleline(&mut a).id(Id::new("a")));
                rect = ui.add(TextEdit::singleline(&mut b).id(Id::new("b"))).rect;
            });
            out.textures_delta.clear();
            rect
        };
        let at = frame(0.0, Vec::new()).left_center() + vec2(12.0, 0.0);
        frame(0.1, touch(TouchPhase::Start, at));
        frame(0.1 + HOLD_SECS + 0.05, Vec::new());
        frame(1.0, Vec::new());
        assert_eq!(ctx.memory(|m| m.focused()), Some(Id::new("a")));
        let b = TextEditState::load(&ctx, Id::new("b")).and_then(|s| s.cursor.char_range());
        assert!(b.is_none_or(|r| r.primary == r.secondary), "{b:?}");
    }

    #[test]
    fn a_disabled_plugin_leaves_the_hold_to_egui() {
        let rig = Rig::new(TEXT);
        set_enabled(&rig.ctx, false);
        let mut rig = rig.start();
        rig.down(0.1, rig.pos(8));
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (8, 8));
    }

    #[test]
    fn a_synthesized_long_press_click_keeps_the_word() {
        // The iOS runtime sends no touch events and adds a secondary click to a held press.
        let mut rig = Rig::new(TEXT).start();
        let at = rig.pos(8);
        let button = |button, pressed| Event::PointerButton { pos: at, button, pressed, modifiers: Default::default() };
        rig.frame(0.1, vec![Event::PointerMoved(at), button(PointerButton::Primary, true)]);
        rig.wait(HOLD_SECS + 0.05);
        assert_eq!(rig.selection(), (6, 15));
        rig.frame(0.1, vec![button(PointerButton::Secondary, true), button(PointerButton::Secondary, false)]);
        assert_eq!(rig.selection(), (6, 15));
        rig.frame(0.05, vec![Event::PointerMoved(rig.pos(18))]);
        assert_eq!(rig.selection(), (6, 21));
    }
}
