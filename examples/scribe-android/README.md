# Scribe

Voice notes transcribed on the phone's own NPU. Record, and Whisper running on the Qualcomm
HTP (through `crates/local-whisper` and `qnn-rs`) turns the audio into text as it arrives.
Nothing leaves the device.

The audio is always kept, as a plain 16 kHz WAV beside the note. A note can be transcribed
again at any time — with another model, in another language, or because the first attempt
never ran — and a recording made with no model installed simply waits for one.

A finished note's text is also written as a `.txt` to the phone's `Documents/Scribe`, and
where more than one person speaks their turns are labelled (`crates/local-speaker`).

## Build and run

```sh
scripts/qnn-stage-libs.sh examples/scribe-android         # once per checkout: the QAIRT .so the APK bundles
rm examples/scribe-android/qnn-runtime-libs/arm64-v8a/libQnnHtpPrepare.so   # 88 MB, only for on-device graph prepare
scripts/speaker-stage.sh examples/scribe-android          # once per checkout: the speaker network (scripts/speaker-export)
cd examples/scribe-android
cargo egui-mobile build -a            # debug; --release for the store
cargo egui-mobile run -a              # install and launch on the attached device
```

The QNN libraries are proprietary and never committed, and neither is the speaker network
compiled with them, so this app is not built by CI and is published from a machine: bump `version`, build `--release`, then
`scripts/publish-appstore.sh --no-changelog scribe examples/scribe-android "notes"`.

## Models

Settings, Speech model downloads a pack (173 MB to 1.88 GB) from Qualcomm's public release
bucket, resumes if interrupted, and checks the unpacked sizes. Packs live in
`/sdcard/Android/data/com.kingsofalchemy.scribe/files/whisper/<name>/`;
`scripts/whisper-push-pack.sh` puts one there over adb instead. Where the packs come from and
why the release is pinned is in `scripts/whisper-export/README.md`.

## How it works

- **Capture** is `EguiMicService`, the framework's microphone foreground service
  (`egui_mobile::mic`). It keeps recording with the screen off and with the app swiped away,
  shows a running clock with a Stop button, and holds a wake lock. Samples go from its thread
  straight to disk, so a slow or failed transcription costs text, never audio.
- **Storage** is one folder per note under `.../files/notes/`: `audio.wav` and `note.json`.
  The WAV header is refreshed every five seconds, and the length is read from the file itself
  when the two disagree, so a recording survives the process dying mid-way. Whatever a dead
  process left behind is repaired and queued at the next start.
- **Transcription** runs on one worker thread that owns the NPU. While recording it decodes
  each 30 s window as it fills, with a rough preview of the unfinished one every few seconds;
  then it works through the queue of saved recordings. Windows are cut at the model's own
  timestamps, so a sentence is never split at a window edge. The model is released after
  three idle minutes, and the NPU's full-power vote is held only while it is running.
- **The engine outlives the screen.** Android rebuilds the activity whenever it likes while
  the recording service keeps the process alive, so recording, queue and model sit in one
  process-wide `Engine` and each activity only looks at it.
- **Recording never waits for the NPU.** A second thread watches the service, so Stop saves
  the recording at once even while a window is decoding or a model is loading, and a fault
  in the decode path costs text, not the recorder.
- **A crash guard** (`src/guard.rs`) writes down what the NPU is about to do and crosses it
  out after. If the process dies with that still written two launches running, transcription
  stops starting by itself — the header says Paused, recording carries on as audio, and
  Transcribe or the self-test tries again. A note that was being transcribed both times is
  marked failed instead of queued a third time.
- **Edits and transcript coexist.** Text typed or corrected in a note stands in for the
  segments it was made over; whatever is transcribed after that still follows it.
- **Text files** (`src/export.rs`). When a note is finished its text is written to
  `Documents/Scribe/<date> <time> <title>.txt` on shared storage, where any other app's file
  picker finds it. The file follows its note: an edit or a new transcript rewrites it when
  the note is left, a new title renames it. It is written by path, which Android allows an
  app for its own files there from Android 11, and handed to the media scanner, without
  which a file made that way stays hidden from other apps. A file is the note's own only
  while it holds what the note last wrote: one that was edited by hand, or whose name
  another note has taken, is left alone and the note writes a new one. Deleting a note
  leaves its file. Settings switches it off. **Share file** on a note hands that same file
  to the share sheet, by the address Android's media index has for it, so a long transcript
  goes as an attachment and nothing is copied; a note with no file of its own (unfinished,
  or with text files off) is shared as a copy by way of Downloads.
- **A note's page** (`src/ui/notes.rs`) is three folding sections, Speakers, Transcript and
  Timeline, over a bar that holds the player and the Share and More menus, so that everything
  a note can have done to it stays in reach however long its text runs. The text and the
  timeline each scroll in an area of their own, as tall as the rest of the page leaves them:
  at most seven tenths of it, an even split when both are open, a quarter each when the page
  is crowded, and only then does the page itself scroll. Both are given their height
  outright, because egui offers a scroll area inside a scrolling page no more than what is
  left of that page in view. Each speaker has a colour. The text is laid out with their name
  in it and a band of it behind their turn, by the field's own layouter, so the colours
  follow every edit; the timeline keeps a table of row heights and lays out only the rows in
  view, an hour of speech being a thousand of them.
- **Speakers** (`crates/local-speaker`). Once a note is transcribed, each segment is cut
  into two-second windows, a speaker-verification network on the NPU turns each into a voice
  print, and the prints are grouped; a change of speaker then starts a paragraph under
  "Speaker 1", "Speaker 2". One voice leaves the text as it was. The pass runs on the worker
  after every waiting transcript and never while a recording is running, so a fault in the
  newest thing on the NPU cannot cost a capture. The prints are kept beside the note
  (`voices.bin`), so setting the count by hand or going back to finding it is immediate and
  needs no NPU; names typed for the speakers replace the numbers, one name to a speaker. It has its own crash guard (`speaker-guard.json`), stricter than
  transcription's: a network that takes the process down once pauses speaker labels until
  they are asked for, and never transcription. The network ships inside
  the APK as an asset and is unpacked on first run. What it can and cannot tell apart, with
  the measurements, is in `scripts/speaker-export/README.md`.

## Testing

```sh
cargo test -p scribe_android          # storage, engine, and every screen laid out at phone widths
cargo test -p local-whisper           # features, decoding rules, long-form seek, vocabulary
cargo test -p local-speaker           # fbank against Kaldi, window planning, the grouping
(cd tests/whisper-host && cargo test) # both pipelines against real weights; needs the ONNX exports
```

Host tests drive the whole app through the scripted engine in `src/sim.rs`, which answers
every window with placeholder sentences, and its scripted speaker network, which takes the
pitch of a window for the voice: a recording of two tones taking turns of six seconds is a
two-person conversation to it. The same engine is a switch in Settings, Debug, for
exercising the app on an emulator, which has no NPU. Layout tests render every screen at 411
and 360 points with the overflow guard on.

Settings, Debug also has a self-test (a bundled 11 s clip through the loaded model, with
encoder and per-token timings, then the speaker network's print of it against the one its
fp32 weights give), manual load and unload, the NPU and QNN runtime as detected,
and the engine's log with a copy button. To see the crash guard on an emulator, seed it and
relaunch: `echo '{"busy":"loading","strikes":1}' > g.json && cargo egui-mobile emulator put
g.json engine-guard.json`.

`cargo egui-mobile emulator boot s26ultra --headless`, a debug build and
`cargo egui-mobile logcat --check-ui` cover the recorder, the service, lifecycle and layout.
Only a phone can run the NPU path or a release build.
