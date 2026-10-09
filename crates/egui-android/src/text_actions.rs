//! The text toolbar owns a layout strip; it must never cover the app's editable content.
//! Kept free of Android/JNI dependencies so the real egui layout and buttons can be tested on host.

use egui::{Align2, Area, Button, Context, Frame, Id, Order, Rect, RichText, Vec2, pos2, vec2};

const GAP: f32 = 8.0;
const BUTTON_PADDING: Vec2 = vec2(10.0, 8.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Paste,
    Copy,
    Cut,
    SelectAll,
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, PointerButton, RawInput, ScrollArea, TextEdit};

    struct Harness {
        ctx: Context,
        dock: Dock,
        size: Vec2,
        keyboard: f32,
        text: String,
        visible_text: Rect,
        content: Rect,
        icons: Vec<(String, egui::Pos2)>,
    }

    impl Harness {
        fn new(size: Vec2, keyboard: f32) -> Self {
            Self {
                ctx: Context::default(),
                dock: Dock::default(),
                size,
                keyboard,
                text: (0..40).map(|n| format!("Line {n}\n")).collect(),
                visible_text: Rect::NOTHING,
                content: Rect::NOTHING,
                icons: vec![],
            }
        }

        fn frame(
            &mut self,
            visible: bool,
            events: Vec<Event>,
            anchor: Option<Rect>,
        ) -> Option<Action> {
            // The app gets the same inset viewport as Adapter::raw_input_hook.
            let safe = Rect::from_min_max(
                pos2(12.0, 24.0),
                pos2(self.size.x - 12.0, self.size.y - self.keyboard - 24.0),
            );
            let inset = self.dock.reserve(&self.ctx, safe);
            let screen = Rect::from_min_size(
                egui::Pos2::ZERO,
                vec2(self.size.x, self.size.y - self.keyboard - inset),
            );
            self.content = Rect::from_min_max(safe.min, safe.max - vec2(0.0, inset));
            let mut action = None;
            let mut output = self.ctx.run_ui(
                RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ui| {
                    ui.scope_builder(egui::UiBuilder::new().max_rect(self.content), |ui| {
                        ui.set_clip_rect(self.content);
                        ScrollArea::vertical().show(ui, |ui| {
                            let field = ui.add(
                                TextEdit::multiline(&mut self.text).desired_width(f32::INFINITY),
                            );
                            self.visible_text = field.rect.intersect(ui.clip_rect());
                        });
                    });
                    self.dock.set_visible(&self.ctx, visible);
                    action = self.dock.show(&self.ctx, true, anchor);
                },
            );
            output.textures_delta.clear(); // This harness inspects shapes without a GPU.
            self.icons.clear();
            for shape in output.shapes {
                if let egui::Shape::Text(t) = shape.shape
                    && ["📋", "📄", "✂", "Aa"].contains(&t.galley.text())
                {
                    let rect =
                        Rect::from_min_size(t.pos, t.galley.size()).intersect(shape.clip_rect);
                    if rect.is_positive() {
                        self.icons.push((t.galley.text().to_owned(), rect.center()));
                    }
                }
            }
            action
        }

        fn settle(&mut self) {
            for _ in 0..8 {
                self.frame(true, vec![], None);
            }
        }

        fn assert_separate(&self) {
            let bar = self.dock.rect.expect("toolbar laid out");
            assert!(
                bar.top() >= self.content.bottom(),
                "bar {bar:?}, content {:?}",
                self.content
            );
            assert!(
                !bar.intersects(self.visible_text),
                "toolbar covers editable text"
            );
            assert!(bar.bottom() <= self.size.y - self.keyboard - 24.0);
            assert_eq!(self.icons.len(), 4, "all actions must remain visible");
        }
    }

    #[test]
    fn toolbar_never_overlaps_multiline_content_or_keyboard() {
        for (size, keyboard) in [
            (vec2(411.0, 891.0), 330.0),
            (vec2(891.0, 411.0), 190.0),
            (vec2(230.0, 640.0), 240.0),
            (vec2(411.0, 891.0), 0.0),
        ] {
            let mut h = Harness::new(size, keyboard);
            h.settle();
            h.assert_separate();
        }
    }

    #[test]
    fn resizing_keyboard_and_style_remeasures_without_painting_over_text() {
        let mut h = Harness::new(vec2(411.0, 891.0), 330.0);
        h.settle();
        for keyboard in [420.0, 220.0, 0.0, 350.0] {
            h.keyboard = keyboard;
            h.settle();
            h.assert_separate();
        }
        h.ctx.global_style_mut(|s| s.spacing.interact_size.y = 76.0);
        h.settle();
        h.assert_separate();
        let wide_height = h.dock.rect.unwrap().height();
        h.size = vec2(170.0, 891.0); // Buttons wrap and the reserved height grows.
        h.settle();
        h.assert_separate();
        assert!(h.dock.rect.unwrap().height() > wide_height);
        h.size = vec2(411.0, 891.0);
        h.settle();
        h.assert_separate();
        assert!((h.dock.rect.unwrap().height() - wide_height).abs() < 1.0);
    }

    #[test]
    fn dismissal_returns_the_entire_content_area() {
        let mut h = Harness::new(vec2(411.0, 891.0), 330.0);
        h.settle();
        let reduced = h.content.height();
        h.frame(false, vec![], None);
        h.frame(false, vec![], None);
        assert!(h.dock.rect.is_none());
        assert!(h.content.height() > reduced + 40.0);
    }

    #[test]
    fn old_app_anchors_cannot_move_the_bar_back_over_the_field() {
        let mut h = Harness::new(vec2(411.0, 891.0), 330.0);
        h.settle();
        let anchor = Rect::from_min_size(pos2(-100.0, 300.0), vec2(200.0, 100.0));
        for _ in 0..4 {
            h.frame(true, vec![], Some(anchor));
        }
        h.assert_separate();
    }

    #[test]
    fn buttons_outside_the_app_viewport_still_receive_taps() {
        let mut h = Harness::new(vec2(411.0, 891.0), 330.0);
        h.settle();
        for (label, expected) in [
            ("📋", Action::Paste),
            ("📄", Action::Copy),
            ("✂", Action::Cut),
            ("Aa", Action::SelectAll),
        ] {
            let pos = h.icons.iter().find(|(text, _)| text == label).unwrap().1;
            assert!(pos.y > h.content.bottom());
            h.frame(
                true,
                vec![
                    Event::PointerMoved(pos),
                    Event::PointerButton {
                        pos,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                None,
            );
            let action = h.frame(
                true,
                vec![Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                }],
                None,
            );
            assert_eq!(action, Some(expected));
            h.settle();
        }
    }
}

#[derive(Default)]
pub(crate) struct Dock {
    wanted: bool,
    measured: Vec2,
    strip: Option<Rect>,
    pub rect: Option<Rect>,
}

impl Dock {
    pub fn set_visible(&mut self, ctx: &Context, visible: bool) {
        if self.wanted != visible {
            self.wanted = visible;
            ctx.request_repaint(); // Relayout before showing, or reclaim space after dismissal.
        }
        if !visible {
            self.rect = None;
        }
    }

    /// Called before egui begins the frame, after subtracting the actual keyboard inset.
    /// Return the space to subtract from screen_rect, so windows, scroll areas and plugins
    /// all get the same usable bounds. The strip itself paints outside that reduced viewport.
    pub fn reserve(&mut self, ctx: &Context, safe: Rect) -> f32 {
        self.strip = None;
        if !self.wanted {
            return 0.0;
        }
        let style = ctx.global_style();
        let minimum = (20.0 + 2.0 * BUTTON_PADDING.y).max(style.spacing.interact_size.y)
            + Frame::popup(&style).total_margin().sum().y;
        let height = self.measured.y.max(minimum) + 2.0 * GAP;
        // With almost no content space (e.g. a resizing split-screen window), hide rather
        // than clamp the menu over the remaining text field or over the keyboard.
        if safe.height() < height + 24.0 || safe.width() < 1.0 {
            return 0.0;
        }
        self.strip = Some(Rect::from_min_max(
            pos2(safe.left(), safe.bottom() - height),
            safe.max,
        ));
        height
    }

    pub fn show(&mut self, ctx: &Context, has_clip: bool, anchor: Option<Rect>) -> Option<Action> {
        let Some(strip) = self.strip.filter(|_| self.wanted) else {
            self.rect = None;
            return None;
        };
        let mut action = None;
        let response = Area::new(Id::new("egui-android-text-actions"))
            .order(Order::Foreground)
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(pos2(
                anchor.map_or(strip.center().x, |a| a.center().x),
                strip.bottom() - GAP,
            ))
            // Also the paint/input clip: even a font/style change or a newly wrapped row
            // cannot cover content for one frame while the measured height catches up.
            .constrain_to(strip.shrink2(vec2(0.0, GAP)))
            .show(ctx, |ui| {
                // Area remembers its previous size. Reflow from the current safe width,
                // including when a split-screen window grows again.
                ui.set_max_width(strip.width());
                Frame::popup(ui.style()).show(ui, |ui| {
                    ui.spacing_mut().button_padding = BUTTON_PADDING;
                    ui.horizontal_wrapped(|ui| {
                        for (kind, icon, label, enabled) in [
                            (Action::Paste, "📋", "Paste", has_clip),
                            (Action::Copy, "📄", "Copy", true),
                            (Action::Cut, "✂", "Cut", true),
                            (Action::SelectAll, "Aa", "Select all", true),
                        ] {
                            if ui
                                .add_enabled(enabled, Button::new(RichText::new(icon).size(20.0)))
                                .on_hover_text(label)
                                .clicked()
                            {
                                action = Some(kind);
                            }
                        }
                    });
                });
            });
        let measured = response.response.rect.size();
        if (measured.y - self.measured.y).abs() > 0.5 {
            ctx.request_repaint();
        }
        self.measured = measured;
        self.rect = Some(response.response.rect.intersect(strip));
        action
    }
}
