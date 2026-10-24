//! Screens and the shared widgets they are built from.

pub mod chips;
pub mod dashboard;
pub mod filters;
pub mod frost;
pub mod icons;
pub mod inspect;
pub mod requests;
pub mod settings;

use egui::{Color32, CornerRadius, Frame, Margin, Response, RichText, Stroke, Ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Requests,
    Filters,
    Settings,
    Pairing,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Dashboard,
        Tab::Requests,
        Tab::Filters,
        Tab::Settings,
        Tab::Pairing,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Requests => "Requests",
            Tab::Filters => "Filters",
            Tab::Settings => "Settings",
            Tab::Pairing => "Pairing",
        }
    }
}

// ── Palette ──────────────────────────────────────────────────────────────────
//
// One violet hue at three lightnesses, because the neon has to live in the glow and the strokes
// rather than in the text: a bright accent *on* an accent fill is the classic neon-UI mistake and
// lands around 2:1 contrast. `ACCENT` is for text and hairlines on glass, `ACCENT_FILL` is the only
// thing a button is ever filled with, and `ACCENT_GLOW` never touches a glyph.
//
// Translucent constants need `_const`: the plain `from_rgba_unmultiplied` goes through a lookup
// table and is not a const fn.
const fn glass(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(r, g, b, a)
}

/// Backdrop: true black at the bottom so an OLED panel actually switches those pixels off, with
/// just enough violet at the very top for the glass to have something to tint.
pub const BG_TOP: Color32 = Color32::from_rgb(17, 8, 30);
pub const BG_BOTTOM: Color32 = Color32::from_rgb(0, 0, 0);

/// Layered glass, tinted hot pink. Lower alpha than before: on a black backdrop a pane reads as
/// glass at a much lighter touch, and anything heavier just looks like grey plastic.
pub const GLASS: Color32 = glass(255, 130, 220, 14);
pub const GLASS_RAISED: Color32 = glass(255, 130, 220, 26);
/// Recessed: text fields, the body pane, anything the eye should read as "inside". Pure black on
/// an OLED, so the wells genuinely go dark.
pub const WELL: Color32 = glass(0, 0, 0, 140);
pub const HAIRLINE: Color32 = glass(255, 150, 225, 34);
/// Brighter top edge — the catch-light that sells a pane as glass rather than a rectangle.
pub const HAIRLINE_LIT: Color32 = glass(255, 200, 240, 70);

/// Text and hairlines. Violet, and the calmest of the neons — it carries the most text.
pub const ACCENT: Color32 = Color32::from_rgb(198, 164, 255);
/// Button fills only — a pink-leaning violet. White on this is 6.6:1.
pub const ACCENT_FILL: Color32 = Color32::from_rgb(146, 42, 199);
/// Bloom behind a primary control. Hot pink, and never a text colour.
pub const ACCENT_GLOW: Color32 = glass(255, 60, 180, 116);

/// Hot pink — the loud one. Highlights, the second bloom, JSON strings.
pub const PINK: Color32 = Color32::from_rgb(255, 105, 180);
pub const PINK_FILL: Color32 = Color32::from_rgb(190, 24, 93);
/// Aqua green — the cool counterweight. JSON keys, "everything is fine".
pub const AQUA: Color32 = Color32::from_rgb(64, 240, 208);

pub const TEXT: Color32 = Color32::from_rgb(234, 228, 246);
pub const MUTED: Color32 = Color32::from_rgb(168, 160, 196);
pub const ON_ACCENT: Color32 = Color32::from_rgb(255, 255, 255);

/// Aqua rather than leaf green, so "running" belongs to the same galaxy as everything else.
pub const GOOD: Color32 = Color32::from_rgb(52, 245, 197);
pub const WARN: Color32 = Color32::from_rgb(255, 199, 89);
/// Coral, deliberately kept clear of the hot pink so "bad" never reads as "accent".
pub const BAD: Color32 = Color32::from_rgb(255, 107, 107);
/// Destructive fill. Translucent rose glass rather than a solid red slab: Stop is a normal thing
/// to press, and a saturated red button shouted louder than everything around it. Over the dark
/// backdrop this settles near rgb(60,12,32), so white still reads about 13:1.
pub const DANGER_FILL: Color32 = glass(214, 40, 100, 76);

/// Comfortable thumb target; egui's default button height is built for a mouse.
pub const TOUCH_HEIGHT: f32 = 46.0;
pub const RADIUS: u8 = 14;

/// Block `target` and everything under it, keeping the Filters screen's text buffer in step.
///
/// That resync is load-bearing: the custom-rules TextEdit overwrites the config from its buffer on
/// `lost_focus`, so a rule added without it would be silently deleted the next time that field was
/// touched.
pub fn apply_block(app: &mut crate::app::PrivaxyApp, target: &str, host: &egui_mobile::Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };

    match loaded.block(target) {
        Some(rule) => {
            app.notice = Some(if loaded.proxy.is_some() {
                format!("Blocking {rule} — the engine rebuilds in a moment.")
            } else {
                format!("Added {rule}. It takes effect when the proxy starts.")
            });
            host.haptic(egui_mobile::Haptic::Success);
        }
        None => {
            app.notice = Some(format!("{target} is already blocked."));
            host.haptic(egui_mobile::Haptic::Light);
        }
    }
}

/// Removes a previously added `||host^` rule and tells the user which one went away.
pub fn apply_unblock(app: &mut crate::app::PrivaxyApp, target: &str, host: &egui_mobile::Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };
    let bare = target.split(':').next().unwrap_or_default().trim();
    let rule = format!("||{bare}^");
    if loaded.unblock(&rule) {
        app.notice = Some(format!("Unblocked {bare}."));
        host.haptic(egui_mobile::Haptic::Success);
    }
}

/// One Android back press this frame, taken so no other layer reacts to the same press.
///
/// Back arrives as an ordinary key: winit maps `AKEYCODE_BACK` to `NamedKey::BrowserBack` and
/// already reports it to the framework as handled, so an unconsumed press is a no-op rather than
/// an exit. Escape rides along for a desktop/keyboard build.
pub fn back_pressed(ctx: &egui::Context) -> bool {
    ctx.input_mut(|input| {
        input.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
            || input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
    })
}

/// Ask for `POST_NOTIFICATIONS`, without which the ongoing service notification — and its Stop
/// button — is never drawn, even though the foreground service itself runs.
pub fn request_notifications(host: &egui_mobile::Host) {
    #[cfg(target_os = "android")]
    {
        use egui_mobile::HostExt;
        host.request_notification_permission();
    }
    #[cfg(not(target_os = "android"))]
    let _ = host;
}

/// Open Android's Security settings, where a CA certificate has to be installed from.
///
/// The programmatic route (`KeyChain.createInstallIntent`) is refused for CA certificates on
/// Android 11 and later — the installer answers "Can't install CA certificates" — so the file plus
/// this deep link is the only path that works on a current device.
pub fn open_security_settings(host: &egui_mobile::Host) {
    #[cfg(target_os = "android")]
    {
        use egui_mobile::HostExt;
        host.open_settings("android.settings.SECURITY_SETTINGS");
    }
    #[cfg(not(target_os = "android"))]
    let _ = host;
}

/// Install the shared default before persisted appearance settings are loaded.
pub fn apply_theme(ctx: &egui::Context) {
    egui_mobile::theme::apply(ctx, egui_mobile::theme::Density::Touch);
}

/// Paint the selected theme underneath translucent panels.
pub fn paint_backdrop(ctx: &egui::Context) {
    let rect = ctx.input(|input| input.viewport_rect());
    ctx.layer_painter(egui::LayerId::background()).rect_filled(
        rect,
        0,
        ctx.style_of(ctx.theme()).visuals.panel_fill,
    );
}

/// A pane of glass — the app's one grouping primitive.
pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::group(ui.style())
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let inner = add_contents(ui);
            catch_light(ui);
            inner
        })
        .inner
}

pub fn glass_frame(fill: Color32) -> Frame {
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(14))
}

/// Brightens the pane's top edge. A uniform hairline reads as a border; a lit top edge reads as a
/// surface catching light from above, which is most of what makes flat translucency look like glass.
fn catch_light(ui: &Ui) {
    let rect = ui.min_rect().expand(14.0);
    let inset = f32::from(RADIUS) * 0.7;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + inset, rect.top()),
            egui::pos2(rect.right() - inset, rect.top()),
        ],
        Stroke::new(1.0, HAIRLINE_LIT),
    );
}

pub fn section_title(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(11.0)
            .color(ui.visuals().weak_text_color())
            .strong(),
    );
}

/// A full-width primary control, with a bloom behind it.
///
/// The glow is a blurred rect written into a slot reserved *before* the button, because a painter
/// call after the fact would land on top of it. `Frame`'s own shadow is no use here: its margin
/// maths excludes the shadow, so the halo would be clipped by the enclosing scroll area.
pub fn big_button(ui: &mut Ui, text: &str, fill: Color32) -> Response {
    let fill = if fill == ACCENT_FILL {
        ui.visuals().selection.bg_fill
    } else {
        fill
    };
    let slot = ui.painter().add(egui::Shape::Noop);
    let response = ui.add_sized(
        [ui.available_width(), ui.spacing().interact_size.y],
        egui::Button::new(RichText::new(text).size(16.0).strong().color(ON_ACCENT))
            .fill(fill)
            .corner_radius(CornerRadius::same(RADIUS)),
    );

    let glow = if fill == ACCENT_FILL {
        ACCENT_GLOW
    } else {
        fill.gamma_multiply(0.45)
    };
    ui.painter().set(
        slot,
        egui::epaint::RectShape::filled(
            response.rect.shrink(2.0),
            CornerRadius::same(RADIUS),
            glow,
        )
        // Clamped to the shorter side by the tessellator, so a 46pt-tall button tops out here.
        .with_blur_width(18.0),
    );
    response
}

/// Label and value on one line, value pushed to the right edge.
///
/// Both sides truncate. Text in a `horizontal` does not wrap, so a long value would otherwise
/// widen the page past the screen and push everything to its right out of reach.
pub fn detail_row(ui: &mut Ui, label: &str, value: RichText) {
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(RichText::new(label).color(ui.visuals().weak_text_color())).truncate(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(value).truncate());
        });
    });
}

pub fn stat(ui: &mut Ui, label: &str, value: u64, color: Color32) {
    ui.vertical(|ui| {
        ui.label(
            RichText::new(format_count(value))
                .size(22.0)
                .color(color)
                .strong(),
        );
        ui.label(
            RichText::new(label)
                .size(11.0)
                .color(ui.visuals().weak_text_color()),
        );
    });
}

/// Thousands separators, then k/M once the number stops fitting a phone column.
pub fn format_count(value: u64) -> String {
    if value >= 1_000_000 {
        return format!("{:.1}M", value as f64 / 1_000_000.0);
    }
    if value >= 100_000 {
        return format!("{:.0}k", value as f64 / 1_000.0);
    }

    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(character);
    }
    out
}

/// Byte counts at the size a request log produces them.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Keeps long URLs from forcing horizontal scroll, trimming the middle where the noise is.
pub fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let tail: String = text
        .chars()
        .skip(text.chars().count().saturating_sub(max / 4))
        .collect();
    format!("{head}…{tail}")
}
