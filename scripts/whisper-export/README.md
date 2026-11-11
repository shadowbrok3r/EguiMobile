# Whisper on the HTP: packs, and the host reference for `local-whisper`

Unlike the other `local-*` models, Whisper is not converted here. Qualcomm AI Hub publishes
HTP context binaries of it compiled for this exact chip (Snapdragon 8 Elite Gen 5 for Galaxy,
`sm8850-ad`, HTP V81), and `local-whisper` loads them unmodified. What lives in this directory
is everything around that: where the packs come from, and a host-side copy of the same graphs
so the Rust pipeline can be tested without a phone.

## Packs

A pack is a directory holding `encoder.bin`, `decoder.bin` and an empty `WHISPER` marker. The
two graphs are the contents of an AI Hub `qnn_context_binary` release zip:

```
https://qaihub-public-assets.s3.us-west-2.amazonaws.com/qai-hub-models/models/<id>/releases/v0.62.2/<id>-qnn_context_binary-float-qualcomm_snapdragon_8_elite_gen5_for_galaxy.zip
```

| `<id>` | pack name | zip | encoder (30 s) | decoder (per token) |
|---|---|---|---|---|
| `whisper_large_v3_turbo` | `whisper-large-v3-turbo` | 1.88 GB | 274 ms | 6.3 ms |
| `whisper_small` | `whisper-small` | 547 MB | 58 ms | 7.4 ms |
| `whisper_base` | `whisper-base` | 173 MB | 22 ms | 2.7 ms |

Timings are AI Hub's own, measured on a Galaxy S26.

**The release is pinned to v0.62.2 on purpose.** A context binary loads on a QNN runtime at
least as new as the compiler that built it. v0.62.2 is the last release compiled with QAIRT
2.45; from v0.63.0 on they are built with 2.50, which the 2.48.40 runtime staged by
`scripts/qnn-stage-libs.sh` refuses. Moving the pin means moving the bundled runtime first.

The Scribe app downloads and unpacks these itself (Settings, Speech model). To put one on a
phone over adb instead:

```sh
scripts/whisper-push-pack.sh whisper-large-v3-turbo ~/Documents/Ai/whisper/packs-src/whisper_large_v3_turbo/*/
```

`cargo run -p qnn-rs --example dump_context -- <libQnnSystem.so> <encoder.bin>` prints a
binary's tensors on the host. `local-whisper` reads the model's whole shape from them — mel
bins, layers, heads, vocabulary, context length — so any size of this export loads with no
config file, and anything with another layout is refused by name.

### The graphs

Qualcomm's export (`qai_hub_models.models.templates.hf_whisper`) is not stock Whisper:

- **Encoder** `hf_whisper_encoder`: `input_features [1, mels, 3000]` in, and the decoder's
  cross-attention caches out — `k_cache_cross_i [heads, 1, 64, 1500]` and
  `v_cache_cross_i [heads, 1, 1500, 64]` per decoder layer.
- **Decoder** `hf_whisper_decoder`: one token per run. `input_ids [1, 1]` and
  `position_ids [1]` (int32), `attention_mask [1, 1, 1, 200]`, every cross cache, and
  `k/v_cache_self_i_in [heads, 1, 64, 199]`; out come `logits [1, vocab, 1, 1]` and the
  `_out` self caches, which are the next run's `_in`. The self caches shift left each run with
  the new entry at the end, so the mask opens from the tail: step `n` clears slot `199 - n`.
- Everything float is fp16, and a window decodes at most 199 tokens including the prompt.

## Host reference

`reference.py` rebuilds those two modules as fp32 ONNX with the same tensor names, and
`tests/whisper-host` runs them through ONNX Runtime behind the same `Engine` trait the HTP
backend implements. That puts features, prompt, logit rules, timestamp seeking and the
vocabulary under `cargo test` with real weights; only the QNN tensor plumbing is left for
the device.

```sh
uv venv --python 3.12 ~/Documents/Ai/whisper/venv
VIRTUAL_ENV=~/Documents/Ai/whisper/venv uv pip install --index-strategy unsafe-best-match \
  --extra-index-url https://download.pytorch.org/whl/cpu \
  'torch==2.8.0+cpu' 'transformers==4.56.2' 'qai-hub-models==0.62.2' onnx onnxruntime soundfile scipy

PY=~/Documents/Ai/whisper/venv/bin/python
$PY scripts/whisper-export/reference.py export --model openai/whisper-base
$PY scripts/whisper-export/reference.py export --model openai/whisper-large-v3-turbo   # 3.4 GB
$PY scripts/whisper-export/reference.py speech                                         # long-form test audio

cargo test --release --manifest-path tests/whisper-host/Cargo.toml
WHISPER_ONNX_DIR=~/Documents/Ai/whisper/onnx/whisper-large-v3-turbo \
  cargo test --release --manifest-path tests/whisper-host/Cargo.toml
cargo run --release --manifest-path tests/whisper-host/Cargo.toml --example transcribe -- \
  ~/Documents/Ai/whisper/onnx/whisper-base recording.wav
```

Without the exports the tests pass vacuously and say so. `tests/whisper-host` is its own
workspace so ONNX Runtime never enters this repo's lockfile or an Android build.

Other stages:

- `fixtures --audio clip.flac` rewrites the mel and filterbank fixtures under `<out>/ref`;
  `crates/local-whisper/tests/fixtures` holds every 25th frame of them, which pins the STFT
  framing, the filterbank and the normalization.
- `greedy --audio clip.flac` transcribes with Qualcomm's own loop (start-of-transcript only,
  argmax). `tests/whisper-host` asserts the Rust engine reproduces its token ids exactly.
- `vocab.py` packs OpenAI's `multilingual.tiktoken` into
  `crates/local-whisper/assets/multilingual.vocab`.

## On the device

```sh
cargo ndk -t arm64-v8a build -p local-whisper --example whisper_smoke --release
adb push target/aarch64-linux-android/release/examples/whisper_smoke /data/local/tmp/
adb shell /data/local/tmp/whisper_smoke <lib_dir> <pack_dir> recording.wav
```

`<lib_dir>` holds `libQnnHtp.so`, `libQnnSystem.so`, the V81 stub and skel (what
`qnn-stage-libs.sh` stages). It prints load time, each window's encoder time and ms/token, and
the transcript. In the app, Settings, Debug, Self-test does the same with a bundled clip.
