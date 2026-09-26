# EguiMobile library plan

Written 2026-09-04. The two apps have already left this repo (commit `8e589f0`):
comfyui-android lives at `~/Documents/Rust/Mobile/comfyui-android`
(github.com/shadowbrok3r/comfyui-android, private) and ringdesigner-android at
`~/Documents/Rust/JewelryProjects/RingDesigner/crates/ringdesigner-android`. Both consume this
repo as git dependencies (`egui-mobile`, `local-*`), so every library change here lands as one PR
plus a revision bump in each app repo. File references below point at those checkouts; the line
numbers are from the pre-move tree (`git show 0f33158:examples/...`).

## 1. NPU: lift the layer both apps hand-roll

The six crates (`qnn-rs`, `local-sd`, `local-anima`, `local-wd14`, `local-clip`, `local-rewrite`)
are already shared. Both apps then build the same layer on top of them, differently:

| Concern | comfyui `src/local_engine.rs` | ringdesigner `src/npu.rs` |
|---|---|---|
| Pack scan / classify / status line | own `PackEntry` + `find_wd14/clip/rewrite_pack_many` | own `Kind` + `scan_many` + `status()` |
| QNN stack bring-up (env, System, Backend, Session, perf vote) | warm `UtilCache` singleton | reloaded on every call in `stack()`: a dlopen per embed |
| One HTP job at a time, cache eviction | `run_lock` + `drop_cache` | none |
| Worker thread + progress + cancel + repaint | `run(.., tx, ctx, cancel)` | `generate_tile(.., progress)` |
| APK metadata (`runtime_libs`, `extract_native_libs`, `uses_native_library libcdsprpc`) | present | missing (still, in the RingDesigner checkout): a `--features local-npu` build ships no QNN libs, and its own status line reports "No QNN runtime" |
| Consistent qnn re-exports | n/a | borrows `Session` / `ContextOpts` from local-clip because local-sd does not re-export them |

### Work items, in order

1. **`crates/npu-runtime`** (or `qnn_rs::runtime`). `Npu::open(lib_dir)` holds the warm
   `QnnSystem` + `Backend` process-wide; a job lock serializes HTP use; the perf vote happens
   after `backendCreate` + `deviceCreate` (burst-mode ordering); `evict()` for `on_pause` and low
   memory; a `Status` enum (`NotBuilt`, `NoRuntime`, `NoPacks`, `Ready`) replaces both apps'
   status strings. Both engines shrink to model glue.
2. **`crates/npu-packs`**, dependency-light. Marker registry (`CLIPV`, `WD14`, `ANIMA`, `RWTR`,
   `unet.bin`, `DEPTH`), `scan` / `scan_many` / `pick`, a `pack.json` manifest, zip import and
   move-with-progress (from comfyui's `spawn_import` / `spawn_move`), `dir_newest_mtime`. Light so
   a non-NPU build can still list packs, which ringdesigner's module already requires.
3. **`Job<T>` in `egui-mobile-core`**: thread + channel + `AtomicBool` cancel + `request_repaint`.
   Not NPU-specific; every long task in both apps re-implements it.
4. **Umbrella `crates/egui-mobile-npu`** with features `sd15`, `anima`, `wd14`, `clip`, `rewrite`,
   `depth`, and one qnn re-export surface. The apps get one git dependency instead of five, and
   the re-export drift goes away.
5. **Generic image-graph runner** (`ImageGraph`, in qnn-rs or a new `local-vision`).
   `local_wd14::infer` and `local_clip::embed` are the same eight lines; read shape, layout and
   dtype from `ContextBinaryInfo`, normalization from the pack config. Depth (ringdesigner already
   declares `Kind::Depth` with no crate behind it), background removal, super-resolution and OCR
   then become pack + config rather than a crate each.
6. **`local-llm`** generalizing local-rewrite: streaming tokens, chat template, GGUF loader;
   rewrite becomes one template. Whisper via candle for prompt dictation fits the same slot.
7. **`cargo egui-mobile build --npu`**: `scripts/qnn-stage-libs.sh` already takes the app dir as an
   argument since the move; still to do: validate the three metadata entries, run the
   `strings | grep local-` check from CLAUDE.md after the build so a feature-less build cannot
   ship silently, and bundle several HTP skels with a runtime arch probe.
8. **Plugin host ops** `npu.embed`, `npu.tag`, `npu.generate` behind the manifest `permissions`
   model in `plugin-abi`, so WASM plugins get AI features without linking QNN.
9. **Non-Qualcomm path** (later): a backend trait with a candle CPU / Metal fallback so the same
   `local-clip` API runs on iOS and on Tensor / MediaTek phones.
10. **`docs/packs.md`**: markers, required files, config schema, export scripts. Needed now that
    the apps live in other repos.

## 2. UI: keep widgets on the screen

Two root causes, both visible in ringdesigner:

- **egui's default in a plain `ui.horizontal` row is `Extend`** (`egui/src/ui.rs`, `wrap_mode`),
  not wrap or truncate. A label or button grows to its text and nothing clips it. Instances: the
  probe readout row (`src/app.rs:634`), the layer title row (`src/layers.rs:251`), every
  `ui.horizontal` holding `ui.button("...")`.
- **Fixed minimum widths whose sum exceeds the phone.** The nav bar (`src/app.rs:895`) floors
  each labeled tab at 58pt: four labeled tabs + the 182pt icon cluster + spacing needs 438pt
  before the bar's own margin, and a portrait phone is roughly 360 to 430pt wide. comfyui carries
  the same code with a 72pt floor.
- Popups are already bounded by `menu_width_cap` / `menu_height_cap`, but only popups, and the
  code is duplicated in both themes.

### Restraints, cheapest first

1. **egui's own tripwire.** `style.debug.show_expand_width = true` paints every widget that
   widens its parent. Debug-build toggle in Settings or a three-finger long-press. No new layout
   code.
2. **End-of-frame overflow lint in the runtime.** After `app.update` in
   `crates/egui-android/src/lib.rs:191`, read `ctx.viewport(|v| &v.prev_pass.widgets)` (public in
   egui 0.36) and flag any `WidgetRect` whose `rect` passes the content rect's right edge while
   its `interact_rect` stops exactly at the screen edge; a scroll-area child is clipped by an
   inner rect instead, so the two are distinguishable. Paint a red edge, log id + layer once per
   widget, allow an opt-out tag. Catches anything on the real device regardless of how it was
   written. Behind `cfg(debug_assertions)` or a `HostExt` flag.
3. **A paved road: a `layout` module in `egui-mobile-core`.** Both themes ship this as private
   code today.
   - `row(ui, ..)`: horizontal scope with `wrap_mode = Truncate` and `set_max_width(available)`.
   - `fit_columns(ui, n, ..)`: equal split of `available_width` with no floor.
   - `NavBar`: the labeled-tabs-plus-icon-squares bar both apps hand-roll, with a demotion policy
     (shrink text, truncate, demote a labeled tab to an icon, overflow menu).
   - `Chips`: `horizontal_wrapped` + truncate.
   - `bounded_popup`: the `menu_width_cap` / `menu_height_cap` pair moved out of the two themes.
   Each helper runs the check from item 2 internally in debug builds.
4. **Host-side layout tests.** `egui_kittest` 0.36 is already in the registry (backdrop-blur and
   RingDesigner's graph-ui use it). An `egui-mobile-test` crate with a phone size matrix (360x640,
   412x915, landscape, keyboard-up rect) and `assert_nothing_past_edge`. Prerequisite: `mod app`
   is Android-only in both apps, so screens must compile on the host first.
5. **A written rule** for CLAUDE.md: no `ui.horizontal` with unbounded text, no `.max(px)` floors
   in bars; use the helpers or `horizontal_wrapped`. The lint enforces what the rule states.

## Order and coordination

- First: npu-runtime + npu-packs. Batch the two so each app repo revs its git dependency once and
  swaps its engine file in the same change.
- Then: the frame lint + `show_expand_width` toggle (an afternoon), then the layout module, then
  replace both nav bars with the shared `NavBar`.
- The other agent's worktree (`claude/objective-greider-5cb52c`) has no unmerged commits; its only
  pending change is uncommitted work in comfyui `app.rs`. Nothing under `crates/` is contended.
- `IDEAS.md` holds the feature menu and the starred picks (LoRA on the NPU, Genie, radar and CSI
  classifiers, Gemma 4 via LiteRT-LM).
