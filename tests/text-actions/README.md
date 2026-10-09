# Android text toolbar regression

`cargo test --manifest-path tests/text-actions/Cargo.toml` renders the production
`egui-android/src/text_actions.rs` widget on the host without Android/JNI dependencies.
It checks viewport separation, portrait/landscape and hardware-keyboard bounds,
wrapping after window/font changes, space reclamation, legacy anchors, and taps on
all four buttons outside the app viewport.

For device checks, build/install `examples/android-hello` and open **Text actions**.
Edit the last line of the multiline field; use Select all, Copy, Cut and Paste;
keep typing; rotate and continue typing without retapping the field. Text and the
caret should stay above the toolbar. Back dismisses both keyboard and toolbar;
Enter dismisses the single-line field but adds a line in the multiline field.
No app-specific toolbar anchor or keyboard-height estimate is needed.

Luma's `scripts/smoke-text-menu.py` additionally checks these actions against the
Qwen prompt using OCR and pixel bounds on the existing s26ultra emulator.
`scripts/smoke-keyboard-focus.py` in Luma additionally exercises repeated open/Back/
reopen cycles in full and short viewports, and typing through both rotations without
retapping. Explicit widget IDs preserve app focus across layout parents; the shared
Java bridge confirms hidden insets after 300 ms so transient reattachment does not
cancel Android's pending keyboard show.
