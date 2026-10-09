Based on the crates.io eframe 0.36.2 package (MIT OR Apache-2.0).

`src/native/glow_integration.rs` allows shutdown after Android Suspend has dropped the window
and released the current GL context. Save uses the optional window, as the existing suspend-save
path already does. App cleanup receives `None` when GL is no longer current; the owning EGL
context reclaims GPU resources on destruction instead of issuing commands without a context.
The upstream painter can log its generic missing-destroy warning on this path.

This complements the winit Destroy-event patch. Wgpu already handles the optional window.
No other upstream source changes. Recheck when updating eframe and remove when fixed upstream.
