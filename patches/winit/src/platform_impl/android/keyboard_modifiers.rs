//! Android's AMETA flags are snapshots on both key and motion events. Keep the
//! last forwarded snapshot so modifiers precede the associated input, without
//! flooding applications with unchanged ModifiersChanged events.
use crate::keyboard::ModifiersState;

#[derive(Default)]
pub(crate) struct AndroidModifiers {
    state: ModifiersState,
}

impl AndroidModifiers {
    pub(crate) fn update(&mut self, meta: u32) -> Option<ModifiersState> {
        let state = from_meta(meta);
        if self.state == state {
            return None;
        }
        self.state = state;
        Some(state)
    }
}

fn from_meta(meta: u32) -> ModifiersState {
    // Stable AMETA_* values from <android/input.h>. Side-specific bits count
    // even if an input device omitted the corresponding aggregate *_ON flag.
    const SHIFT: u32 = 0x01 | 0x40 | 0x80;
    const ALT: u32 = 0x02 | 0x10 | 0x20;
    const CONTROL: u32 = 0x1000 | 0x2000 | 0x4000;
    const SUPER: u32 = 0x10000 | 0x20000 | 0x40000;
    let mut state = ModifiersState::empty();
    state.set(ModifiersState::SHIFT, meta & SHIFT != 0);
    state.set(ModifiersState::ALT, meta & ALT != 0);
    state.set(ModifiersState::CONTROL, meta & CONTROL != 0);
    state.set(ModifiersState::SUPER, meta & SUPER != 0);
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_and_either_side_set_the_same_modifier() {
        for (bits, expected) in [
            ([0x01, 0x40, 0x80], ModifiersState::SHIFT),
            ([0x02, 0x10, 0x20], ModifiersState::ALT),
            ([0x1000, 0x2000, 0x4000], ModifiersState::CONTROL),
            ([0x10000, 0x20000, 0x40000], ModifiersState::SUPER),
        ] {
            for bit in bits {
                assert_eq!(from_meta(bit), expected, "AMETA {bit:#x}");
            }
        }
    }

    #[test]
    fn combined_modifiers_preserve_control_and_super_independently() {
        assert_eq!(
            from_meta(0x2000 | 0x40 | 0x20 | 0x40000),
            ModifiersState::all()
        );
        assert_eq!(from_meta(0x1000), ModifiersState::CONTROL);
        assert_eq!(from_meta(0x10000), ModifiersState::SUPER);
        // Sym, Function, Caps/Num/Scroll Lock are not shortcut modifiers.
        assert_eq!(
            from_meta(0x04 | 0x08 | 0x100000 | 0x200000 | 0x400000),
            ModifiersState::empty()
        );
    }

    #[test]
    fn control_chord_repeat_and_key_up_keep_modifiers_until_control_releases() {
        let mut modifiers = AndroidModifiers::default();
        // Ctrl Down -> K Down -> repeated K -> K Up -> Ctrl Up.
        assert_eq!(
            modifiers.update(0x1000 | 0x2000),
            Some(ModifiersState::CONTROL)
        );
        assert_eq!(modifiers.update(0x1000 | 0x2000), None);
        assert_eq!(modifiers.update(0x1000 | 0x2000), None);
        assert_eq!(modifiers.update(0x1000 | 0x2000), None);
        assert_eq!(modifiers.update(0), Some(ModifiersState::empty()));
        assert_eq!(modifiers.update(0), None);
    }

    #[test]
    fn motion_snapshots_update_shift_for_a_pen_without_an_extra_key_event() {
        let mut modifiers = AndroidModifiers::default();
        assert_eq!(modifiers.update(0), None);
        assert_eq!(modifiers.update(0x40), Some(ModifiersState::SHIFT));
        assert_eq!(modifiers.update(0x01 | 0x40), None);
        assert_eq!(modifiers.update(0x02), Some(ModifiersState::ALT));
        assert_eq!(modifiers.update(0), Some(ModifiersState::empty()));
    }

    #[test]
    fn focus_pause_and_surface_loss_reset_without_duplicate_notifications() {
        let mut modifiers = AndroidModifiers::default();
        assert_eq!(
            modifiers.update(0x1000 | 0x01),
            Some(ModifiersState::CONTROL | ModifiersState::SHIFT)
        );
        assert_eq!(modifiers.update(0), Some(ModifiersState::empty()));
        assert_eq!(modifiers.update(0), None);
        // A new focused event can start a fresh chord after resuming.
        assert_eq!(modifiers.update(0x10000), Some(ModifiersState::SUPER));
    }
}
