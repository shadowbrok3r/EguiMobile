//! Soft-keyboard hints for text fields; only the Android runtime acts on them.
//!
//! ```ignore
//! let r = ui.add(egui::DragValue::new(&mut width_mm));
//! egui_mobile::keyboard::number(&r);
//! ```

use egui::{Context, IMEPurpose, Id, Response};

/// Which soft keyboard the focused text field asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyboardKind {
    /// The full text keyboard.
    #[default]
    Text,
    /// Digits, a decimal point, a minus sign and a Done key that arrives as Enter.
    Number,
}

/// Number keypad for `response`'s field while it has focus; call it every frame the field is drawn.
pub fn number(response: &Response) {
    mark(&response.ctx, Some(response.id));
}

/// Show a number keypad for whichever text field has focus this frame.
pub fn request_number(ctx: &Context) {
    mark(ctx, None);
}

/// Number marks made during one pass.
#[derive(Clone, Default)]
struct Marks {
    pass: u64,
    focused: bool,
    ids: Vec<Id>,
}

fn marks_id() -> Id {
    Id::new("egui_mobile_core::keyboard::marks")
}

fn mark(ctx: &Context, id: Option<Id>) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| {
        let marks = d.get_temp_mut_or_default::<Marks>(marks_id());
        if marks.pass != pass {
            *marks = Marks { pass, ..Marks::default() };
        }
        match id {
            Some(id) => marks.ids.push(id),
            None => marks.focused = true,
        }
    });
}

/// The keyboard this pass's marks ask for the focused field; read after the app's `update`.
#[doc(hidden)]
pub fn requested(ctx: &Context) -> KeyboardKind {
    let pass = ctx.cumulative_pass_nr();
    let focused = ctx.memory(|m| m.focused());
    let number = ctx.data(|d| {
        d.get_temp::<Marks>(marks_id()).is_some_and(|m| {
            m.pass == pass && (m.focused || focused.is_some_and(|f| m.ids.contains(&f)))
        })
    });
    if number { KeyboardKind::Number } else { KeyboardKind::Text }
}

/// The keyboard kind a runtime last handed the soft keyboard.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KindLatch {
    sent: KeyboardKind,
}

impl KindLatch {
    /// The kind to send, or `None` to keep it: no focused field, a password field, or no change.
    pub fn update(
        &mut self,
        purpose: Option<IMEPurpose>,
        requested: KeyboardKind,
    ) -> Option<KeyboardKind> {
        match purpose {
            None | Some(IMEPurpose::Password) => None,
            Some(_) if requested == self.sent => None,
            Some(_) => {
                self.sent = requested;
                Some(requested)
            }
        }
    }

    /// The kind last returned by [`KindLatch::update`], [`KeyboardKind::Text`] before any.
    pub fn current(&self) -> KeyboardKind {
        self.sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{DragValue, RawInput, TextEdit, Ui};

    fn pass(ctx: &Context, mut f: impl FnMut(&mut Ui)) {
        ctx.run_ui(RawInput::default(), |ui| f(ui)).textures_delta.clear();
    }

    #[test]
    fn a_marked_focused_text_edit_asks_for_numbers() {
        let ctx = Context::default();
        let mut text = String::from("12.5");
        let mut kind = None;
        pass(&ctx, |ui| {
            let r = ui.add(TextEdit::singleline(&mut text));
            r.request_focus();
            number(&r);
            kind = Some(requested(ui.ctx()));
        });
        assert_eq!(kind, Some(KeyboardKind::Number));
    }

    #[test]
    fn a_marked_drag_value_is_matched_by_its_own_id() {
        let ctx = Context::default();
        let mut value = 4.2_f64;
        let mut kind = None;
        pass(&ctx, |ui| {
            let r = ui.add(DragValue::new(&mut value));
            r.request_focus();
            number(&r);
            kind = Some(requested(ui.ctx()));
        });
        assert_eq!(kind, Some(KeyboardKind::Number));
    }

    #[test]
    fn a_mark_on_another_field_leaves_the_focused_one_on_text() {
        let ctx = Context::default();
        let (mut a, mut b) = (String::new(), String::new());
        let mut kind = None;
        pass(&ctx, |ui| {
            let ra = ui.add(TextEdit::singleline(&mut a));
            let rb = ui.add(TextEdit::singleline(&mut b));
            ra.request_focus();
            number(&rb);
            kind = Some(requested(ui.ctx()));
        });
        assert_eq!(kind, Some(KeyboardKind::Text));
    }

    #[test]
    fn a_context_mark_covers_whichever_field_is_focused() {
        let ctx = Context::default();
        let mut text = String::new();
        let mut kind = None;
        pass(&ctx, |ui| {
            ui.add(TextEdit::singleline(&mut text)).request_focus();
            request_number(ui.ctx());
            kind = Some(requested(ui.ctx()));
        });
        assert_eq!(kind, Some(KeyboardKind::Number));
    }

    #[test]
    fn marks_last_one_pass() {
        let ctx = Context::default();
        let mut text = String::new();
        pass(&ctx, |ui| {
            let r = ui.add(TextEdit::singleline(&mut text));
            r.request_focus();
            number(&r);
        });
        let mut kind = None;
        pass(&ctx, |ui| {
            let r = ui.add(TextEdit::singleline(&mut text));
            assert!(r.has_focus());
            kind = Some(requested(ui.ctx()));
        });
        assert_eq!(kind, Some(KeyboardKind::Text));
    }

    #[test]
    fn the_latch_sends_a_kind_once_per_change() {
        let mut latch = KindLatch::default();
        assert_eq!(latch.update(Some(IMEPurpose::Normal), KeyboardKind::Text), None);
        assert_eq!(latch.update(Some(IMEPurpose::Normal), KeyboardKind::Number), Some(KeyboardKind::Number));
        assert_eq!(latch.update(Some(IMEPurpose::Normal), KeyboardKind::Number), None);
        assert_eq!(latch.update(Some(IMEPurpose::Terminal), KeyboardKind::Text), Some(KeyboardKind::Text));
        assert_eq!(latch.current(), KeyboardKind::Text);
    }

    #[test]
    fn a_pass_without_a_focused_field_keeps_the_keypad() {
        let mut latch = KindLatch::default();
        latch.update(Some(IMEPurpose::Normal), KeyboardKind::Number);
        assert_eq!(latch.update(None, KeyboardKind::Text), None);
        assert_eq!(latch.current(), KeyboardKind::Number);
        assert_eq!(latch.update(Some(IMEPurpose::Normal), KeyboardKind::Number), None);
    }

    #[test]
    fn a_password_field_keeps_the_kind_until_it_loses_focus() {
        let mut latch = KindLatch::default();
        latch.update(Some(IMEPurpose::Normal), KeyboardKind::Number);
        assert_eq!(latch.update(Some(IMEPurpose::Password), KeyboardKind::Text), None);
        assert_eq!(latch.update(Some(IMEPurpose::Password), KeyboardKind::Number), None);
        assert_eq!(latch.current(), KeyboardKind::Number);
        assert_eq!(latch.update(Some(IMEPurpose::Normal), KeyboardKind::Text), Some(KeyboardKind::Text));
    }
}
