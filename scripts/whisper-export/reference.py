#!/usr/bin/env python3
"""Host reference for crates/local-whisper: the Qualcomm AI Hub Whisper graph as fp32 ONNX.

The on-device packs are Qualcomm's precompiled HTP context binaries (see README.md here), which
only execute on the phone. This script rebuilds the *same* graph — `qai_hub_models`' HfWhisper
encoder and single-step KV-cache decoder, identical tensor names and shapes — as fp32 ONNX so the
Rust pipeline (mel, decode loop, timestamp seeking, tokenizer) can be exercised on the host
through local-whisper's `host-onnx` feature.

Stages:
  export    encoder.onnx + decoder.onnx for --model into <out>/onnx/<name>/
  fixtures  mel features and filterbanks for <audio> into <out>/ref/ (raw little-endian f32)
  greedy    transcribe <audio> with Qualcomm's own loop (SOT only, greedy) and print the ids
  speech    fetch the four short utterances tests/whisper-host stitches into a long recording

Needs the venv from README.md (torch, transformers==4.56.2, qai-hub-models==0.62.2, onnxruntime).
"""

import argparse
import json
import sys
from pathlib import Path

import numpy as np

MEAN_DECODE_LEN = 200
MASK_NEG = -100.0


def note(msg: str) -> None:
    print(f"==> {msg}", flush=True)


def load_audio(path: Path) -> np.ndarray:
    import soundfile as sf

    audio, rate = sf.read(str(path), dtype="float32", always_2d=True)
    audio = audio.mean(axis=1)
    if rate != 16000:
        from scipy.signal import resample_poly

        audio = resample_poly(audio, 16000, rate).astype(np.float32)
    return audio


def build(model_id: str):
    from qai_hub_models.models.templates.hf_whisper.model import HfWhisper

    class Model(HfWhisper):
        @classmethod
        def get_hf_whisper_version(cls) -> str:
            return model_id

    return Model.from_pretrained()


def export(model_id: str, out: Path) -> None:
    import torch

    whisper = build(model_id)
    out.mkdir(parents=True, exist_ok=True)
    for part, module in (("encoder", whisper.encoder), ("decoder", whisper.decoder)):
        spec = module.get_input_spec()
        args = tuple(
            torch.zeros(shape, dtype=torch.int32 if dtype == "int32" else torch.float32)
            for shape, dtype in spec.values()
        )
        path = out / f"{part}.onnx"
        note(f"{part}: {len(spec)} inputs -> {path}")
        torch.onnx.export(
            module,
            args,
            str(path),
            input_names=list(spec.keys()),
            output_names=list(module.get_output_spec().keys()),
            opset_version=17,
            dynamo=False,
            do_constant_folding=True,
        )
    config = whisper.config
    (out / "dims.json").write_text(
        json.dumps(
            {
                "model": model_id,
                "n_mels": config.num_mel_bins,
                "n_layers": config.decoder_layers,
                "n_heads": config.decoder_attention_heads,
                "d_model": config.d_model,
                "n_vocab": config.vocab_size,
            },
            indent=1,
        )
    )


def features(model_id: str, audio: np.ndarray) -> np.ndarray:
    from transformers import WhisperFeatureExtractor

    extractor = WhisperFeatureExtractor.from_pretrained(model_id)
    return extractor(audio, sampling_rate=16000, return_tensors="np")["input_features"].astype(np.float32)


def fixtures(model_id: str, audio_path: Path, out: Path) -> None:
    from transformers import WhisperFeatureExtractor

    out.mkdir(parents=True, exist_ok=True)
    # Quantize first: the Rust tests read the 16-bit PCM, so the features have to be computed
    # from those same samples, not from the float audio they were rounded from.
    pcm = np.round(load_audio(audio_path) * 32768.0).clip(-32768, 32767).astype("<i2")
    audio = pcm.astype(np.float32) / 32768.0
    extractor = WhisperFeatureExtractor.from_pretrained(model_id)
    n_mels = extractor.feature_size
    # The float64 numpy path, not the float32 torch one the extractor picks when torch is
    # installed: its rounding noise in near-silent bins is larger than the tolerance a test of
    # the Rust front end should need.
    padded = np.zeros(480000, np.float32)
    padded[: len(audio)] = audio[:480000]
    mel = extractor._np_extract_fbank_features(padded[None, :], "cpu")[0].astype(np.float32)
    torch_mel = features(model_id, audio)[0]
    note(f"numpy vs torch extractor: max |diff| {np.abs(mel - torch_mel).max():.5f}")
    stem = f"{audio_path.stem}.mel{n_mels}"
    mel.tofile(out / f"{stem}.f32")
    np.asarray(extractor.mel_filters, dtype=np.float32).T.copy().tofile(out / f"filters{n_mels}.f32")
    pcm.tofile(out / f"{audio_path.stem}.s16")
    note(f"mel {mel.shape} -> {out / (stem + '.f32')}; filters {extractor.mel_filters.T.shape}; pcm {pcm.shape}")


def greedy(model_id: str, onnx_dir: Path, audio_path: Path) -> None:
    import onnxruntime as ort
    from transformers import WhisperTokenizer

    dims = json.loads((onnx_dir / "dims.json").read_text())
    layers, heads = dims["n_layers"], dims["n_heads"]
    head_dim = dims["d_model"] // heads
    encoder = ort.InferenceSession(str(onnx_dir / "encoder.onnx"), providers=["CPUExecutionProvider"])
    decoder = ort.InferenceSession(str(onnx_dir / "decoder.onnx"), providers=["CPUExecutionProvider"])
    tokenizer = WhisperTokenizer.from_pretrained(model_id)

    audio = load_audio(audio_path)
    ids_all = []
    for start in range(0, len(audio), 480000):
        mel = features(model_id, audio[start : start + 480000])
        cross = encoder.run(None, {"input_features": mel})
        feed = {}
        for i in range(layers):
            feed[f"k_cache_cross_{i}"] = cross[2 * i]
            feed[f"v_cache_cross_{i}"] = cross[2 * i + 1]
            feed[f"k_cache_self_{i}_in"] = np.zeros((heads, 1, head_dim, MEAN_DECODE_LEN - 1), np.float32)
            feed[f"v_cache_self_{i}_in"] = np.zeros((heads, 1, MEAN_DECODE_LEN - 1, head_dim), np.float32)
        mask = np.full((1, 1, 1, MEAN_DECODE_LEN), MASK_NEG, np.float32)
        ids = [50258]
        for n in range(MEAN_DECODE_LEN - 1):
            mask[0, 0, 0, MEAN_DECODE_LEN - n - 1] = 0.0
            feed["input_ids"] = np.array([[ids[n]]], np.int32)
            feed["attention_mask"] = mask
            feed["position_ids"] = np.array([n], np.int32)
            outs = decoder.run(None, feed)
            for i in range(layers):
                feed[f"k_cache_self_{i}_in"] = outs[1 + 2 * i]
                feed[f"v_cache_self_{i}_in"] = outs[2 + 2 * i]
            token = int(outs[0].reshape(-1).argmax())
            ids.append(token)
            if token == 50257:
                break
        ids_all.append(ids)
        print("ids", ids)
        print("text", tokenizer.decode(ids, skip_special_tokens=True).strip())
    print(json.dumps(ids_all))


SPEECH = "https://huggingface.co/datasets/Narsil/asr_dummy/resolve/main"


def speech(out: Path) -> None:
    """Three English utterances and a Spanish one, as 16 kHz 16-bit PCM in <out>/ref/libri."""
    import io
    import urllib.request

    import soundfile as sf
    from scipy.signal import resample_poly

    dest = out / "ref" / "libri"
    dest.mkdir(parents=True, exist_ok=True)
    for n in (1, 2, 3, 4):
        with urllib.request.urlopen(f"{SPEECH}/{n}.flac") as response:
            audio, rate = sf.read(io.BytesIO(response.read()), dtype="float32", always_2d=True)
        audio = audio.mean(axis=1)
        if rate != 16000:
            audio = resample_poly(audio, 16000, rate).astype(np.float32)
        np.round(audio * 32768.0).clip(-32768, 32767).astype("<i2").tofile(dest / f"{n}.s16")
        note(f"{n}.s16: {len(audio) / 16000:.2f} s")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("stage", choices=["export", "fixtures", "greedy", "speech"])
    parser.add_argument("--model", default="openai/whisper-base")
    parser.add_argument("--out", type=Path, default=Path.home() / "Documents/Ai/whisper")
    parser.add_argument("--audio", type=Path)
    args = parser.parse_args()

    name = args.model.split("/")[-1]
    onnx_dir = args.out / "onnx" / name
    if args.stage == "speech":
        speech(args.out)
    elif args.stage == "export":
        export(args.model, onnx_dir)
    elif args.stage == "fixtures":
        if not args.audio:
            sys.exit("--audio required")
        fixtures(args.model, args.audio, args.out / "ref")
    else:
        if not args.audio:
            sys.exit("--audio required")
        greedy(args.model, onnx_dir, args.audio)


if __name__ == "__main__":
    main()
