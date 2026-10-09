//! Small vector controls with accessible names, independent of the installed emoji fonts.

use egui::{Response, Sense, Shape, Stroke, Ui, Vec2, pos2};

#[derive(Clone, Copy)]
pub enum Icon {
    Filter,
    Play,
    Pause,
    Trash,
    Download,
    Refresh,
}

pub fn button(ui: &mut Ui, icon: Icon, label: &str, selected: bool, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if ui.is_rect_visible(rect) {
        let fill = if selected {
            super::ACCENT_FILL
        } else if response.hovered() {
            super::GLASS_RAISED
        } else {
            super::GLASS
        };
        let painter = ui.painter();
        painter.rect_filled(rect, 8, fill);
        let color = if ui.is_enabled() {
            super::TEXT
        } else {
            super::MUTED
        };
        let stroke = Stroke::new(1.7, color);
        let r = egui::Rect::from_center_size(rect.center(), Vec2::splat(18.0));
        let p = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
        let line = |a, b| {
            painter.line_segment([a, b], stroke);
        };
        match icon {
            Icon::Filter => {
                painter.add(Shape::closed_line(
                    vec![
                        p(0.05, 0.15),
                        p(0.95, 0.15),
                        p(0.6, 0.55),
                        p(0.6, 0.9),
                        p(0.4, 0.75),
                        p(0.4, 0.55),
                    ],
                    stroke,
                ));
            }
            Icon::Play => {
                painter.add(Shape::convex_polygon(
                    vec![p(0.25, 0.1), p(0.85, 0.5), p(0.25, 0.9)],
                    color,
                    Stroke::NONE,
                ));
            }
            Icon::Pause => {
                for x in [0.23, 0.62] {
                    painter.rect_filled(
                        egui::Rect::from_min_max(p(x, 0.15), p(x + 0.15, 0.85)),
                        1,
                        color,
                    );
                }
            }
            Icon::Trash => {
                line(p(0.1, 0.25), p(0.9, 0.25));
                line(p(0.35, 0.1), p(0.65, 0.1));
                painter.add(Shape::line(
                    vec![p(0.22, 0.3), p(0.28, 0.9), p(0.72, 0.9), p(0.78, 0.3)],
                    stroke,
                ));
                line(p(0.42, 0.43), p(0.42, 0.73));
                line(p(0.58, 0.43), p(0.58, 0.73));
            }
            Icon::Download => {
                line(p(0.5, 0.05), p(0.5, 0.65));
                painter.add(Shape::line(
                    vec![p(0.25, 0.42), p(0.5, 0.67), p(0.75, 0.42)],
                    stroke,
                ));
                painter.add(Shape::line(
                    vec![p(0.1, 0.7), p(0.1, 0.9), p(0.9, 0.9), p(0.9, 0.7)],
                    stroke,
                ));
            }
            Icon::Refresh => {
                let points = (0..=24)
                    .map(|i| {
                        let angle = 0.5 + i as f32 / 24.0 * 5.0;
                        r.center() + Vec2::angled(angle) * 7.0
                    })
                    .collect();
                painter.add(Shape::line(points, stroke));
                painter.add(Shape::convex_polygon(
                    vec![p(0.98, 0.2), p(0.62, 0.18), p(0.9, 0.51)],
                    color,
                    Stroke::NONE,
                ));
            }
        }
    }
    response.on_hover_text(label)
}
