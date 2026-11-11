#!/usr/bin/env python3
"""Export the WeSpeaker ResNet34 speaker-embedding ONNX to a Qualcomm HTP "pack" for local-speaker.

Pipeline (each stage skippable with --from):
  0. download   the ONNX, checked against a pinned SHA-256
  1. shape      force the input to static 1 x 198 x 80 (one 2 s window of fbank frames)
  2. convert    qairt-converter                ONNX -> DLC  (fp16)
  3. context    qnn-context-binary-generator   DLC + HTP -> context .bin
  4. pack       assemble <out>/ : SPEAKER marker, model.bin

Run it on the machine that has the QAIRT/QNN SDK; the converters are x86-64 host tools.

The model is WeSpeaker's VoxCeleb ResNet34 (large-margin fine-tune), the embedding half of
pyannote's diarization pipeline: 6.6 M weights, a 256-number voice print per window. The
ONNX is the one sherpa-onnx republishes from WeSpeaker's own release. CC-BY-4.0.

Contract the runtime (crates/local-speaker) implements and this graph must match:
  input  `feats`  [1, 198, 80] float: Kaldi fbank of 2 s at 16 kHz on int16-scaled samples
                  (25 ms Hamming frames every 10 ms, 80 mel bins from 20 Hz, no dither),
                  each bin's mean over the window subtracted
  output `embs`   [1, 256] float: the embedding, NOT normalised. Its length is kept: it is
                  how sure the model is of the window.

Example:
  <qairt-venv>/bin/python scripts/speaker-export/export.py --out ~/Documents/Ai/speaker/pack \\
      --work ~/Documents/Ai/speaker/build --sdk ~/Documents/Ai/QNN/qairt/<ver>
"""

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

URL = "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx"
SHA256 = "e9848563da86f263117134dfd7ad63c92355b37de492b55e325400c9d9c39012"
FRAMES, MELS, DIM = 198, 80, 256
NAME = "speaker"
STAGES = ["download", "shape", "convert", "context", "pack"]


def die(msg: str) -> "NoReturn":  # type: ignore[name-defined]
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def note(msg: str) -> None:
    print(f"==> {msg}", flush=True)


def stage_ge(name: str, start: str) -> bool:
    return STAGES.index(name) >= STAGES.index(start)


def find_sdk(arg: str | None) -> Path:
    root = arg or os.environ.get("QNN_SDK_ROOT") or os.environ.get("QAIRT_ROOT")
    if not root:
        die("no --sdk and no QNN_SDK_ROOT/QAIRT_ROOT in env (source scripts/qnn-env.sh)")
    p = Path(root)
    if not (p / "bin" / "x86_64-linux-clang").is_dir():
        die(f"{p}/bin/x86_64-linux-clang missing — extract the full QAIRT zip")
    return p


def sdk_tool(sdk: Path, name: str) -> Path:
    p = sdk / "bin" / "x86_64-linux-clang" / name
    if not p.exists():
        die(f"SDK tool not found: {p}")
    return p


def sdk_env(sdk: Path, python: str | None = None) -> dict:
    # SDK tools need libc++ (NDK host runtime) + the SDK's host libs on the loader path,
    # python wrappers additionally qti.* and libpython (mirrors qnn-env.sh).
    env = os.environ.copy()
    env["PYTHONPATH"] = f"{sdk}/lib/python" + os.pathsep + env.get("PYTHONPATH", "")
    host_rt = os.environ.get("QAIRT_HOST_RT", str(Path.home() / "Documents/Ai/QNN/host-runtime"))
    libs = [host_rt, f"{sdk}/lib/x86_64-linux-clang"]
    if python is not None:
        libs.append(str(Path(os.path.realpath(python)).parent.parent / "lib"))
    libs = [d for d in libs if Path(d).is_dir()]
    if "LD_LIBRARY_PATH" in env:
        libs.append(env["LD_LIBRARY_PATH"])
    env["LD_LIBRARY_PATH"] = os.pathsep.join(libs)
    return env


def run_py_tool(python: str, tool: Path, args: list[str], sdk: Path) -> None:
    # QAIRT converters are Python wrappers; launch via ld-linux so a rewritten argv0
    # (e.g. Cursor's AppImage on paths containing a version string) can't break them.
    ld = "/lib64/ld-linux-x86-64.so.2"
    cmd = [ld, python, str(tool), *args] if Path(ld).exists() else [python, str(tool), *args]
    note(" ".join(cmd))
    subprocess.run(cmd, check=True, env=sdk_env(sdk, python))


def run_native_tool(tool: Path, args: list[str], sdk: Path) -> None:
    note(f"{tool} {' '.join(args)}")
    subprocess.run([str(tool), *args], check=True, env=sdk_env(sdk))


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def download(work: Path, given: Path | None) -> Path:
    onnx = work / "model.onnx"
    if given is not None:
        shutil.copyfile(given, onnx)
    elif not onnx.exists():
        note(f"download {URL}")
        try:
            urllib.request.urlretrieve(URL, onnx)
        except Exception as e:  # noqa: BLE001
            die(f"could not fetch the model: {e} (download it by hand and pass --onnx)")
    got = sha256(onnx)
    if got != SHA256:
        die(f"{onnx} has SHA-256 {got}, expected {SHA256}: not the model this export was written against")
    note(f"have model.onnx ({onnx.stat().st_size} bytes, checksum ok)")
    return onnx


def fix_shape(onnx_in: Path, onnx_out: Path) -> None:
    try:
        import onnx  # type: ignore
    except ImportError:
        die("python 'onnx' package required for the shape stage")
    model = onnx.load(str(onnx_in))
    if [i.name for i in model.graph.input] != ["feats"] or [o.name for o in model.graph.output] != ["embs"]:
        die("unexpected graph IO (expected one input `feats`, one output `embs`)")
    for value, shape in ((model.graph.input[0], [1, FRAMES, MELS]), (model.graph.output[0], [1, DIM])):
        dims = value.type.tensor_type.shape.dim
        if len(dims) != len(shape):
            die(f"{value.name} has rank {len(dims)}, expected {len(shape)}")
        for dim, size in zip(dims, shape):
            dim.ClearField("dim_param")
            dim.dim_value = size
    note(f"input `feats` fixed to [1, {FRAMES}, {MELS}], output `embs` to [1, {DIM}]")
    onnx.save(model, str(onnx_out))


def main() -> None:
    ap = argparse.ArgumentParser(description="Export the speaker-embedding ONNX to a local-speaker HTP pack.")
    ap.add_argument("--out", required=True, type=Path, help="pack output dir (gets SPEAKER, model.bin)")
    ap.add_argument("--work", type=Path, default=None, help="scratch dir (default <out>-build)")
    ap.add_argument("--onnx", type=Path, default=None, help="use this copy of the model instead of downloading it")
    ap.add_argument("--sdk", default=None, help="QAIRT/QNN SDK root (else QNN_SDK_ROOT / QAIRT_ROOT)")
    ap.add_argument("--python", default=os.environ.get("QAIRT_VENV", "") and f"{os.environ['QAIRT_VENV']}/bin/python" or sys.executable, help="python that has the converter deps")
    ap.add_argument("--dsp-arch", default="v81", help="HTP arch (default v81, the S26 Ultra's)")
    ap.add_argument("--soc", default="sm8850", help="offline-cache SoC model (default sm8850 = V81)")
    ap.add_argument("--vtcm-mb", default="8", help="VTCM budget MB (default 8)")
    ap.add_argument("--from", dest="from_stage", default="download", choices=STAGES, help="skip earlier stages")
    args = ap.parse_args()

    work = args.work or Path(f"{args.out}-build")
    work.mkdir(parents=True, exist_ok=True)
    onnx_raw, onnx_static = work / "model.onnx", work / "model_static.onnx"

    if stage_ge("download", args.from_stage):
        onnx_raw = download(work, args.onnx)
    if stage_ge("shape", args.from_stage):
        fix_shape(onnx_raw, onnx_static)

    need_sdk = any(stage_ge(s, args.from_stage) for s in ("convert", "context"))
    sdk = find_sdk(args.sdk) if need_sdk else None
    if need_sdk:
        # QAIRT 2.48 converters crash on onnx >= 1.19 (removed onnx.version); fail fast with the fix.
        chk = subprocess.run([args.python, "-c", "import onnx; print(onnx.__version__)"], capture_output=True, text=True)
        if chk.returncode != 0:
            die(f"{args.python} lacks the 'onnx' package the QAIRT converters need")
        ver = chk.stdout.strip()
        if tuple(int(x) for x in ver.split(".")[:2]) >= (1, 19):
            die(f"onnx {ver} in {args.python} is too new for the QAIRT converters (need < 1.19); "
                f"fix: uv pip install --python {args.python} 'onnx==1.18.0'")

    model_dlc = work / f"{NAME}.dlc"
    if stage_ge("convert", args.from_stage):
        # All fp16, IO included: local-speaker reads the tensors' own element type. Activations
        # in this network stay in the tens on real speech, three orders of magnitude inside
        # fp16's range (reference.py --ranges prints the largest), so nothing needs pinning
        # to fp32.
        run_py_tool(args.python, sdk_tool(sdk, "qairt-converter"),
                    ["--input_network", str(onnx_static), "--output_path", str(model_dlc), "--float_bitwidth", "16"], sdk)
        if not model_dlc.exists():
            die(f"converter did not write {model_dlc}")

    ctx_bin = work / f"{NAME}_ctx.bin"
    if stage_ge("context", args.from_stage):
        lib = sdk / "lib" / "x86_64-linux-clang"
        for needed in ("libQnnHtp.so", "libQnnModelDlc.so"):
            if not (lib / needed).exists():
                die(f"missing {lib / needed}")
        htp_cfg = work / f"htp_{args.dsp_arch}.json"
        htp_cfg.write_text(
            '{\n'
            f'  "graphs": [{{"graph_names": ["{NAME}"], "vtcm_mb": {args.vtcm_mb}, "fp16_relaxed_precision": 1, "O": 3.0}}],\n'
            f'  "devices": [{{"dsp_arch": "{args.dsp_arch}", "cores": [{{"core_id": 0, "perf_profile": "burst", "rpc_control_latency": 100}}]}}]\n'
            '}\n'
        )
        gen_args = ["--model", str(lib / "libQnnModelDlc.so"), "--dlc_path", str(model_dlc),
                    "--backend", str(lib / "libQnnHtp.so"), "--binary_file", ctx_bin.stem,
                    "--output_dir", str(work), "--htp_socs", args.soc, "--vtcm_override", args.vtcm_mb]
        ext_so = lib / "libQnnHtpNetRunExtensions.so"
        if ext_so.exists():
            backend_cfg = work / "backend_extensions.json"
            backend_cfg.write_text('{"backend_extensions": {"shared_library_path": "%s", "config_file_path": "%s"}}\n' % (ext_so, htp_cfg))
            gen_args += ["--config_file", str(backend_cfg)]
        else:
            print(f"warn: no {ext_so} — generating with --htp_socs only", file=sys.stderr)
        for stale in work.glob(f"{ctx_bin.stem}*.bin"):
            stale.unlink()
        run_native_tool(sdk_tool(sdk, "qnn-context-binary-generator"), gen_args, sdk)
        made = sorted(work.glob(f"{ctx_bin.stem}*.bin"))
        if not made:
            die(f"context binary not produced under {work}")
        ctx_bin = made[0]

    if stage_ge("pack", args.from_stage):
        made = sorted(work.glob(f"{NAME}_ctx*.bin"))
        if not made:
            die(f"no context binary under {work} — run through the context stage first")
        args.out.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(made[0], args.out / "model.bin")
        (args.out / "SPEAKER").write_text("")
        note(f"pack ready: {args.out}")
        for f in ("SPEAKER", "model.bin"):
            print(f"    {f:10} {(args.out / f).stat().st_size} bytes")


if __name__ == "__main__":
    main()
