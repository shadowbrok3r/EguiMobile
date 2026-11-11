//! Scribe — voice notes transcribed on the phone's own NPU.
//!
//! Recordings are saved as plain WAV and transcribed by Whisper running on the Qualcomm HTP
//! through `local-whisper`; nothing leaves the device. The audio is kept, so any note can be
//! transcribed again later — with another model, in another language, or because the first
//! attempt never ran.
//!
//! Everything but the screens is portable and host-tested (`cargo test -p scribe_android`).

pub mod app;
pub mod clock;
pub mod engine;
pub mod export;
mod guard;
pub mod library;
pub mod models;
pub mod npu;
pub mod platform;
pub mod resample;
pub mod settings;
pub mod sim;
pub mod ui;
pub mod wav;

// By hand rather than through `app!`, for a logcat tag of the app's own.
//
// Glow rather than the default wgpu: every app in this repo runs on it, and it is the path the
// framework's blur and video surfaces need should this app grow either.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_tag("Scribe").with_max_level(log::LevelFilter::Info));
    platform::set_app(&app);
    egui_mobile::run_with(app, egui_mobile::Backend::Glow, |cc| Box::new(app::ScribeApp::new(cc)));
}
