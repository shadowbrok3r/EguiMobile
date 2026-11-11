# Speaker embeddings on the HTP: the pack, and the host reference for `local-speaker`

`local-speaker` tells the voices in a recording apart. Its network is WeSpeaker's VoxCeleb
ResNet34 (large-margin fine-tune) — 6.6 M weights, a 256-number voice print per two seconds of
audio, the embedding half of pyannote's diarization pipeline. CC-BY-4.0. Nobody publishes it
compiled for this chip, so unlike the Whisper packs it is converted here.

## Export

```sh
<qairt-venv>/bin/python scripts/speaker-export/export.py \
    --out ~/Documents/Ai/speaker/pack --work ~/Documents/Ai/speaker/build \
    --sdk ~/Documents/Ai/QNN/qairt/<ver>
```

Downloads the ONNX (the copy sherpa-onnx republishes from WeSpeaker's release, checked against
a pinned SHA-256), fixes its input to one window (`feats` `[1, 198, 80]`), converts it to an
fp16 DLC with `qairt-converter` and compiles that to an HTP V81 context binary for SM8850.
`--from` skips stages already done; `--dsp-arch`/`--soc` target another chip. The prerequisites
are those of `scripts/wd14-export` (QAIRT with `qairt-converter`, a Python with `onnx < 1.19`).

The pack is two files:

```
<out>/
├── SPEAKER      marker (empty; identifies the pack)
└── model.bin    the HTP context binary, 13 MB
```

Everything is fp16, tensors in and out included; `local-speaker` reads the graph's own element
type. Activations stay in the tens on real speech (`reference.py --ranges`), so nothing needs
pinning to fp32.

**Checking it without a phone.** The SDK's x86 HTP backend runs the compiled graph in
simulation:

```sh
source scripts/qnn-env.sh
L=$QAIRT_ROOT/lib/x86_64-linux-clang
qnn-net-run --backend $L/libQnnHtp.so --model $L/libQnnModelDlc.so --dlc_path <work>/speaker.dlc \
    --input_list inputs.txt --output_dir out --config_file <work>/backend_extensions.json
```

with `inputs.txt` lines of `feats:=<file of 198*80 float32>`. Against ONNX Runtime on the same
four windows the simulated prints came out at cosine 0.999994 or better, lengths within 0.05 %.
(The CPU backend cannot load an fp16 DLC.) On the phone, Scribe's self-test repeats the
comparison for one window and says so on screen.

## Shipping it

Scribe bundles the pack in its APK as an asset and unpacks it on first run:

```sh
scripts/speaker-stage.sh examples/scribe-android     # copies model.bin to assets/speaker/
```

The binary is not committed. An app built without it runs, with speaker labels unavailable.

## The reference

`reference.py` is the whole speaker pass in numpy — features, windows, and the grouping that
decides how many voices there are — and the measurements the crate's constants come from. Its
docstring lists the modes and the numbers. The two that matter when changing anything:

```sh
# The crate's fixtures: Kaldi's own frames and the fp32 print for the test clip.
python scripts/speaker-export/reference.py --onnx <work>/model_static.onnx --fixtures crates/local-speaker/tests/fixtures

# Same prints, both implementations, one JSON line per file: diff them.
cargo run --release -p local-speaker --example cluster_prints -- *.prints
python scripts/speaker-export/reference.py --prints *.prints
```

`tests/whisper-host` (`cargo test --test speakers`) runs the crate's own features and grouping
against the fp32 ONNX on an AMI meeting with its reference labels, and
`cargo run --release --example speakers` there prints a real recording's transcript with its
speakers.

## What to expect of it

Measured on four AMI meetings (headset mix and a single far microphone) and LibriSpeech, cut
into 306 recordings of one to six speakers:

| | |
|---|---|
| Speech given to the right speaker | 96.8 % |
| Speaker count, whole recordings | exact in 99 % |
| One person speaking | one speaker in 103 of 104 |
| Whole four-person meetings, overlaps included, cut blind at pauses | 86–96 %, all four speakers found in each |

It errs towards fewer speakers: a short recording where someone says a few sentences usually
comes out as one voice fewer. A transcript segment gets one speaker, so a quick exchange inside
one segment goes to whoever spoke more of it, and people talking over each other are not
separated. The count can always be set by hand; with the right count given, 97.4 %.
