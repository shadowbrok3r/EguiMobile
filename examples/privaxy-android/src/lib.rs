//! Privaxy for Android — an ad-blocking proxy with an egui front end.
//!
//! The proxy core is ported from the desktop privaxy server: same `adblock` engine and the same
//! CONNECT-interception shape, with OpenSSL swapped for rcgen, the home directory swapped for the
//! app's private storage, and a hostname-only mode added for the Android certificate trust rules.

pub mod app;
pub mod certificate;
pub mod proxy;
pub mod ui;
pub mod vpn;

// Invoked by path so the macro does not collide with this crate's `app` module. Gated on the
// mobile targets so the crate still builds — and its tests still run — on the host.
//
// Glow rather than the default wgpu: the frosted panels grab the live framebuffer from inside an
// egui paint callback, and only egui_glow dispatches those. This app uses no plugin viewports, so
// it gives up nothing by switching.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    let sink = android_logger::AndroidLogger::new(
        android_logger::Config::default()
            .with_tag("Privaxy")
            .with_max_level(log::LevelFilter::Trace),
    );
    let _ = privaxy_diagnostics::init(Some(Box::new(sink)));
    egui_mobile::run_with(app, egui_mobile::Backend::Glow, |cc| {
        Box::new(app::PrivaxyApp::new(cc))
    });
}

#[cfg(target_os = "ios")]
egui_mobile::app!(app::PrivaxyApp::new);
