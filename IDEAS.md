# EguiMobile AI / NPU ideas

Companion to `PLAN.md` (what to build now). This is the menu of what the on-device stack could do
beyond today's SD1.5 and Anima text2img, WD14 tags, CLIP search + aesthetic score, and the CPU
rewriter. Written 2026-09-04. Starred items are Logan's picks; everything else is wanted too.

## Quick wins with packs already on the phone

| Idea | Payoff | Needs |
|---|---|---|
| Zero-shot open-vocabulary tags | Embed the bundled danbooru tag list with the CLIP text tower once, then nearest tags per image. Catches characters and terms WD14 doesn't know. | Nothing new |
| Prompt adherence score | CLIP image-text cosine per output. Rank a batch, flag "asked for X, image scores low on X" beside the prompt lint. | Nothing new |
| Best-of-N local drafts | Four 512px SD1.5 drafts on the phone, keep the best by aesthetic and adherence. | Nothing new |
| Personal aesthetic head | Ridge regression on CLIP embeddings from favourites vs trashed. Sorts the gallery by the user's taste, not LAION's. | Closed-form math only |
| Auto-albums and "more like this" | k-means on the existing CLIP index. Nearest prompts in history feed the cooc suggestions. | Nothing new |
| Instant draft while the server renders | Local SD1.5 draft in seconds, the hi-res result replaces it when comfy-gate finishes. | Engine plumbing |
| Offline fallback | Queue when comfy-gate is unreachable, run locally, replay later. | Engine plumbing |
| Rewriter as an assistant | Same Qwen 0.5B: explain a ComfyUI error, describe a node, draft a negative prompt. | Prompt templates |

## New packs

Most of these have precompiled HTP exports on Qualcomm AI Hub, and older-arch exports run on the
S26 Ultra's V81 (see the arch-compat note in memory).

| Idea | Payoff | Needs |
|---|---|---|
| Depth (Depth Anything V2) | ringdesigner photo to relief, declared (`Kind::Depth`) but unbuilt. comfyui builds depth control images on the phone and sends those instead of photos. | Pack + `ImageGraph` runner |
| Segment Anything (MobileSAM) | Tap-to-mask for inpainting instead of hand-painted masks. Cut-outs and alphas from photos. | Encoder on HTP, tiny decoder on CPU |
| Background removal / matting | Subjects for img2img, alphas, stickers. | Pack |
| LaMa / AOT-GAN inpainting | Magic-eraser object removal in the gallery with no diffusion. Cleans photo-to-alpha scans. | Pack |
| Real-ESRGAN / QuickSRNet | Upscale outputs and textures on device. | Pack |
| OpenPose, MediaPipe hand and face | Pose control images from a phone photo. Hand landmarks for ring try-on and finger sizing against a reference card. | Packs, camera frames for live use |
| Face embedding (ArcFace) | Character consistency check across a LoRA's outputs, an "off-model" flag, face clustering. | Pack |
| TrOCR | Read prompts out of screenshots. Engraving text. | Pack |
| Whisper | Prompt dictation. | Pack, mic capture in the Android host |
| Captioner (Florence-2 base) | Prose captions beside WD14 tags. | Conversion pass |
| Text embeddings (bge-small, CPU) | Semantic search over prompt history, presets, node docs. LoRA matching by description. | candle only |
| ★ LLM on the NPU via Genie | See the Genie section below. | `genie-rs` wrapper |
| Scribble control + distilled SD (LCM/turbo) | Live sketch to image or relief in ringdesigner's Sketch tab at 1 to 4 steps. | Distilled UNet export |
| SD1.5 VAE encoder | img2img and inpaint on device. The pack is decoder-only today. | Small export |
| ★ LoRA on the NPU | See the LoRA section below. | Base UNet rebuilt once, per-LoRA adapter bins from the desktop |
| Frame interpolation / video super-res | Smooth and enlarge the server's Wan clips on device. Generation stays server-side. | Pack |
| ★ Radar and CSI classifiers | See the radar section below. | Training data surveyor and WireLab already collect |
| Gemma 4 via LiteRT-LM | See the Gemma 4 section below. | `litert-lm-rs` wrapper, 2.6 to 3.7 GB model download |

## Platform work the library would need

- **Background jobs while charging**: tag the whole gallery overnight. Needs a generic foreground
  service; only the VPN service pattern exists, and the pause/resume hooks are still unplumbed.
- **Camera frames into Rust**: the host offers a preview layer only. Gates live depth,
  segmentation, try-on, and sizing.
- **Mic capture on Android**: gates Whisper and Gemma 4 audio input.
- **Thermal and memory awareness**: read thermal headroom through JNI to back off burst mode, and
  evict caches on the low-memory event.
- **Pack downloads**: the appstore app as a pack store with resume and checksums, instead of
  sideloading zips. Gemma-class models are 2.6 to 3.7 GB each.
- **Resident contexts**: pre-warm the UNet and DiT at idle and use the spill-fill groups already in
  `ContextOpts` for faster model switching.
- **Shared NPU bench**: generalize ringdesigner's bench module to per-graph timings and a thermal
  curve for any pack.

## ★ LoRA on the NPU, and how much of ComfyUI that unlocks

### What QAIRT 2.48 already ships

The SDK at `~/Documents/Ai/QNN/qairt/2.48.40.260702` has a first-class LoRA path, so this is
tooling work rather than research:

| Tool (`bin/x86_64-linux-clang`) | Role |
|---|---|
| `qairt-lora-mapper` | Maps the adapter's PyTorch module names to ONNX tensors (`attach_point_onnx_mapping`) from a LoRA YAML config. |
| `qairt-lora-importer` | Base DLC (float or quantized) + source ONNX + LoRA YAML. Needs the same `--input_list` calibration or float fallback the base used. |
| `qairt-lora-model-creator` | Builds the max-rank-concatenated LoRA graph (`create_lora_graph`), with `quant_updatable_mode` adapter_only or all, and per use-case adapter weights + encodings. |
| `qairt-lora-adapter-bin-updater` | New or edited adapter safetensors + the LoRA-metadata DLC + old adapter binaries + backend lib produce updated adapter binaries. A new LoRA never touches the base context. |
| Optimizer pass `extract_lora_alpha` | Exposes the LoRA alpha as an additional graph input, so strength can be a runtime input rather than baked. |

The YAML describes adapters (name, rank, alpha, target modules) and use cases (adapter names,
per-adapter alphas, quant overrides), so several adapters can be combined at build time and
swapped at run time as long as their ranks fit the graph's max rank.

On the runtime side `qnn-rs` already binds everything needed: `contextApplyBinarySection`,
`contextGetBinarySection(Size)`, `contextGetBinarySectionUpdate` in the v2.37 interface table,
`QNN_GRAPH_CONFIG_OPTION_ENABLE_BINARY_SECTION_WEIGHTS_UPDATES`, and the
`QNN_TENSOR_TYPE_UPDATEABLE_*` tensor types. `QnnHtpContext.h` adds `loraWeightSharingEnabled`
(one replaceable weight blob shared by all graphs), a RAM-preload variant, and
`skipValidationOnBinarySection` for super adapters. None of it is wrapped yet.

The text-encoder half of a LoRA needs none of this: CLIP runs on the CPU through candle, so the
app can patch `W += alpha * B * A` at load time for any LoRA at any strength with no desktop step.
Textual inversion is the same story (extra rows in the token table).

### A local executor for the app's own workflow vocabulary

comfyui-android already builds and edits ComfyUI API-format graphs. The node types its own
workflow builder emits (counts from the pre-move tree, `git show 0f33158:examples/comfyui-android/src`)
map onto local ops like this:

| Node | Local equivalent | Status |
|---|---|---|
| CheckpointLoaderSimple | SD1.5 pack: UNet + VAE contexts, CLIP safetensors | Exists. Another checkpoint is another UNet export, GBs each |
| CLIPTextEncode | CPU CLIP (candle) | Exists. Prompt weighting syntax to port from local-anima's `text.rs` |
| CLIPSetLastLayer | CPU | Small: needs the penultimate hidden state out of candle's CLIP |
| LoraLoader / LoraLoaderModelOnly | UNet half: HTP adapter section. CLIP half: CPU weight patch | New. The CLIP half needs no desktop step |
| EmptyLatentImage | CPU | Exists, 64x64 latent (512px) only |
| KSampler | CPU scheduler + HTP UNet | Exists: euler_a, dpmpp_2m_karras. More samplers are CPU code |
| KSamplerAdvanced | Same plus start/end step and add-noise | Small: expose start step and denoise in the loop |
| VAEDecode | HTP | Exists |
| VAEEncode | HTP | Needs a VAE encoder export; the SD pack is decoder-only |
| SetLatentNoiseMask | CPU per-step blend | Small |
| LatentUpscaleBy + second KSampler (hires fix) | CPU resize, then a UNet at the larger size | Needs a 768 or 1024 UNet export, or tiled sampling at 512 |
| UpscaleModelLoader + ImageUpscaleWithModel | ESRGAN pack on HTP, CPU tiling | New pack |
| ImageScale | CPU | Trivial |
| LoadImage / SaveImage | Device picker / gallery | Exists in the app |
| UltralyticsDetectorProvider + FaceDetailer | Detector pack on HTP, crop, masked inpaint at 512, paste back | Composable from the rows above |
| WD14Tagger (custom node) | local-wd14 | Exists |
| ControlNet (not in the app's vocabulary yet) | ControlNet pack + UNet exported with residual inputs | AI Hub ships SD1.5 + ControlNet |

What does not map: arbitrary Python custom nodes; SDXL and Flux-class checkpoints (the Anima DiT
is about the ceiling); IP-Adapter and InstantID (a UNet export with extra cross-attention inputs);
dynamic resolution or batch (HTP graphs are fixed-shape, every size is another export); LoRAs
whose rank exceeds the graph's max rank or that touch modules outside the attach points (base
rebuild).

### Shape of the work

1. Rebuild the SD1.5 UNet once as a LoRA-enabled context: attention q/k/v/out in every block as
   attach points, max rank 64, alpha as a graph input. Drive it from `scripts/qnn-convert.sh` plus
   the four tools above.
2. A desktop adapter builder: kohya / A1111 key names to diffusers module names (mapper), then one
   adapter bin per LoRA (updater). Ship as a `lora/<name>/` pack folder: `adapter.bin`, the CLIP
   half as safetensors, and a json with rank, alpha and trigger words.
3. `qnn-rs`: `Context::apply_binary_section`, the HTP LoRA config arms, the graph config at
   generation time. `local-sd`: `apply_lora(adapter, strength)`, the alpha input, the CPU CLIP
   patch.
4. A local executor: `class_type` to op table, a `LocalPreflight` mirroring `preflight.rs` that
   says which nodes cannot run on the phone, consuming the same API-format JSON the uiwf converter
   already produces. The phone's graph editor stays the UI.
5. Order: 1 to 3 first (LoRA in the existing Create tab), the executor after.

## ★ Genie: LLMs on the HTP

Genie is QAIRT's LLM runtime (`libGenie.so`, a C dialog API driven by a JSON config that points
at QNN context binaries for the transformer plus a tokenizer). Qualcomm AI Hub publishes Llama 3.2
and Qwen bundles per SoC; the SDK's `lib/python/qti/aisw/genai` even carries LoRA support for that
path (`qnn_genai_transformer_lora.py`).

- **Payoff**: a 1 to 3B chat model at usable speed on the NPU, against the 0.5B CPU rewriter today.
  Workflow edits from plain language constrained by the schemas in `schema.rs`, error repair
  suggestions, the planned tag LM, node and LoRA descriptions.
- **Wrapper**: `genie-rs`, a dlopen wrapper shaped like `qnn-rs`. Genie links the same QAIRT, so
  one QAIRT version serves diffusion and the LLM in one process. That is an advantage over the
  LiteRT-LM NPU path, which pins its own (see below).
- **Caveats**: compiled graphs bake a fixed context length; binaries are per SoC; 2 to 4 GB of RAM
  while resident, so the diffusion caches must evict first; export of anything not on AI Hub goes
  through the SDK's genai tooling, which is unverified here.

## ★ Radar and CSI classifiers (surveyor, WireLab)

- **Data**: surveyor's 60 GHz micro-motion instrument and WireLab's Wi-Fi CSI, with the LD2450
  radar as ground truth (position and velocity per target gives free labels for presence and
  motion; breathing rate needs a reference).
- **Model**: a small 1D CNN or tiny transformer over 2 to 4 s windows of amplitude, phase or
  micro-motion spectra, producing presence, breathing rate, gesture class, fall. Train on the
  desktop, export through `scripts/qnn-convert.sh` (a trivial op set), run through the same
  generic tensor-in tensor-out runner as the image models. candle on the CPU is a fine first
  target; the NPU is for battery.
- **Library deliverable**: a `local-signal` crate with windowing, normalization, the classifier
  runner and a pack format for 1D models, so surveyor and WireLab share it.

## Gemma 4 via LiteRT-LM (Logan, 2026-09-04)

What the two pages and the surrounding releases say, as of 2026-09-04:

| Fact | Value |
|---|---|
| Models | E2B 2.58 GB, E4B 3.65 GB (`.litertlm`), text + image + audio in, 128K context, per-layer embeddings |
| MTP | Drafters for E2B and E4B; LiteRT-LM v0.11 (May 2026) added it; up to 2.2x decode on mobile GPU, 1.5x on mobile CPU; enabled through the speculative-decoding flag |
| Runtime | LiteRT-LM v0.16.x (Aug 2026): first versioned C API prebuilts (`litert_lm_c_api-0.1.0.zip`). `engine.h` takes model path, backend (`cpu`, `gpu`, `npu`), separate vision and audio backends; session config carries speculative decoding, a text LoRA path and an audio LoRA path; input data is text, image or audio |
| Kotlin | `Backend.NPU(nativeLibraryDir)`; GPU needs `libOpenCL.so` and `libvndksupport.so` manifest entries |
| Memory | E2B peak CPU RAM on the S26 Ultra: 1733 MB (their table) |
| NPU builds on Hugging Face | E2B only: `_qualcomm_sm8750` (8 Elite, S25 Ultra), `_qualcomm_qcs8275`, Tensor G5/G6, Intel. No E4B NPU build, no SM8850 (S26 Ultra) build for either |
| NPU traps | AOT per SoC with a fixed max sequence length baked in; the artifact pins a QAIRT version (issue 2226: version mismatch on the S25 Ultra with 2.45). Our process bundles 2.48, so a second QNN copy or a version match is required |
| AI Edge Gallery | Kotlin app: chat with thinking, Ask Image, Audio Scribe, Prompt Lab, Agent Skills (tools), Mobile Actions via FunctionGemma 270M |

What it gives us that the current stack cannot: a multimodal assistant on the phone (ask about a
gallery image, dictate and translate, function calling into the app's own actions), which is the
Gallery app's feature list rebuilt in egui.

Integration route: a `litert-lm-rs` dlopen wrapper over the C API prebuilt, bundled as
`runtime_libs` the same way as QNN. The Android `.so` names inside the C API zip still need
confirming (161 MB, not downloaded yet).

On the S26 Ultra today that means E4B on the GPU with MTP, or E2B on GPU or CPU with MTP. The NPU
is wait-and-see until an SM8850 artifact appears, or an experiment: the E2B `sm8750` build run
through the `litert_lm_main` CLI over adb with matching QAIRT libs, in its own process, before any
in-app bundling.

Experiment plan:

1. Scratch app: sideload `gemma-4-E4B-it-gpu.litertlm` plus the C API prebuilt. Measure decode
   tokens/s with and without MTP on the GPU, plus peak RAM.
2. Same for E2B, GPU and CPU.
3. The E2B `sm8750` NPU build through the CLI, as above.
4. If GPU E4B lands at a usable decode rate, wire `local-llm` on it: workflow assistant, image Q&A
   on outputs, dictation through audio input. Genie stays the NPU route for text-only models.

Sources: [Gemma 4 for LiteRT-LM](https://developers.google.com/edge/litert-lm/models/gemma-4),
[AI Edge Gallery](https://github.com/google-ai-edge/gallery),
[LiteRT-LM](https://github.com/google-ai-edge/LiteRT-LM) and its
[releases](https://github.com/google-ai-edge/LiteRT-LM/releases),
[LiteRT-LM on Android](https://developers.google.com/edge/litert-lm/android),
[Gemma 4 MTP drafters](https://blog.google/innovation-and-ai/technology/developers-tools/multi-token-prediction-gemma-4/),
[LiteRT-LM issue 2226](https://github.com/google-ai-edge/LiteRT-LM/issues/2226),
[mamai issue 58](https://github.com/nmrenyi/mamai/issues/58),
[LiteRT on Qualcomm NPU](https://developers.googleblog.com/unlocking-peak-performance-on-qualcomm-npu-with-litert/),
[Gemma 4 model card](https://ai.google.dev/gemma/docs/core/model_card_4),
[litert-community E2B files](https://huggingface.co/litert-community/gemma-4-E2B-it-litert-lm),
[litert-community E4B files](https://huggingface.co/litert-community/gemma-4-E4B-it-litert-lm).
