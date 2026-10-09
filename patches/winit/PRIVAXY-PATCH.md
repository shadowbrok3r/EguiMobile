Based on the crates.io winit 0.30.13 package (Apache-2.0).

`src/event_loop.rs` scopes the one-event-loop-per-process guard to non-Android platforms.
Android starts each NativeActivity on a new `android_main` thread. Activity lifetimes may overlap
during replacement, and a foreground service can keep the process alive.
Rejecting the replacement loop leaves the relaunched Activity unable to render.

`src/platform_impl/android/mod.rs` exits the event loop on `MainEvent::Destroy`. NativeActivity
waits for `android_main` to return; ignoring Destroy leaves the Java UI thread waiting forever.
The normal LoopExiting callback runs before the loop returns, allowing backend state to be parked.

No other upstream source changes. Recheck these patches when updating winit; remove them when
upstream supports Activity recreation in a surviving Android process.
