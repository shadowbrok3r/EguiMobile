#!/usr/bin/env python3
"""The speaker pass in numpy: what `crates/local-speaker` is checked against, and the
measurements its numbers come from.

It is the same procedure as the crate, written the slow obvious way: Kaldi fbank features,
one voice print per two-second window from the fp32 ONNX, and the grouping that decides how
many voices there are. Nothing here ships; it exists so that every constant in the crate can
be re-derived, and so a change to either side can be checked against the other.

  --fixtures DIR         write the crate's test fixtures (kaldi-native-fbank frames and the
                         reference print for the Whisper tests' clip)
  --features LIBRISPEECH  which feature settings do the weights expect? Same/different-speaker
                         error on two-second clips, per setting
  --ranges WAV...        largest activation anywhere in the network: is fp16 safe?
  --meeting WAV RTTM     label one recording whose speakers are known (an AMI meeting and its
                         reference RTTM), cut blind at pauses, and score it
  --prints FILE...       group prints saved by the crate (`Prints::save`, key 0), one JSON line
                         per file in the format of the crate's `cluster_prints` example, so
                         the two can be diffed

All of them need --onnx (the checked model from export.py's work dir) except --prints.
Needs numpy, onnxruntime, soundfile; scipy for --meeting; kaldi-native-fbank for --fixtures.

Measured with this script (QAIRT 2.48.40, model SHA-256 in export.py):
  features   Hamming + mean normalisation: 1.6 % equal error on 2 s LibriSpeech clips.
             Without the mean normalisation: 13 %. Povey window: 1.7 %.
  ranges     largest activation: 36 over 180 windows of meeting speech, 66 on a studio
             clip. fp16 holds 65504.
  grouping   306 recordings cut from four AMI meetings (headset mix and one far microphone)
             and LibriSpeech, one to six speakers, one to forty minutes: 96.8 % of speech
             given to the right speaker; the count exact on 99 % of whole recordings and on
             every single-speaker one but one in 104, wrong towards fewer otherwise. Told
             the count: 97.4 %.
             Whole four-person meetings, overlaps included, cut blind at pauses (--meeting,
             four meetings, each from both microphones): 86-96 %, four speakers found in all.
"""

import argparse
import json
import struct
import sys

import numpy as np

RATE, WINDOW, FRAME, SHIFT, NFFT, MELS, DIM = 16000, 32000, 400, 160, 512, 80, 256

# ── Features ─────────────────────────────────────────────────────────────────


def mel(hz):
    return 1127.0 * np.log(1.0 + hz / 700.0)


def mel_bank():
    low, high = mel(20.0), mel(RATE / 2)
    step = (high - low) / (MELS + 1)
    at = mel(np.arange(NFFT // 2) * RATE / NFFT)
    bank = np.zeros((MELS, NFFT // 2 + 1))
    for m in range(MELS):
        left, centre, right = low + m * step, low + (m + 1) * step, low + (m + 2) * step
        bank[m, : NFFT // 2] = np.maximum(0.0, np.minimum((at - left) / (centre - left), (right - at) / (right - centre)))
    return bank.astype(np.float32)


BANK = mel_bank()


def fbank(pcm, window="hamming", normalise=True):
    """Kaldi fbank of samples in -1..1: [frames, 80], each bin's mean over the clip removed."""
    x = pcm.astype(np.float32) * np.float32(32768.0)
    frames = x[np.arange(FRAME)[None, :] + SHIFT * np.arange(1 + (len(x) - FRAME) // SHIFT)[:, None]]
    frames = frames - frames.mean(axis=1, keepdims=True)
    frames = np.concatenate([frames[:, :1] * (1 - 0.97), frames[:, 1:] - 0.97 * frames[:, :-1]], axis=1)
    n = np.arange(FRAME)
    shape = 0.54 - 0.46 * np.cos(2 * np.pi * n / (FRAME - 1)) if window == "hamming" else (0.5 - 0.5 * np.cos(2 * np.pi * n / (FRAME - 1))) ** 0.85
    spectrum = np.fft.rfft(frames * shape.astype(np.float32)[None, :], NFFT, axis=1)
    energies = (spectrum.real**2 + spectrum.imag**2).astype(np.float32) @ BANK.T
    out = np.log(np.maximum(energies, np.finfo(np.float32).eps)).astype(np.float32)
    return out - out.mean(axis=0, keepdims=True) if normalise else out


def tile(pcm, size=WINDOW):
    return np.tile(pcm, -(-size // max(len(pcm), 1)))[:size] if len(pcm) else np.zeros(size, np.float32)


class Network:
    def __init__(self, path):
        import onnxruntime as ort

        self.session = ort.InferenceSession(path, providers=["CPUExecutionProvider"])
        # The exported model takes one window at a time; the original takes any number.
        batch = self.session.get_inputs()[0].shape[0]
        self.batch = batch if isinstance(batch, int) else 64

    def prints(self, clips, **features):
        """Raw (not normalised) prints of equal-length clips."""
        out = [self.session.run(None, {"feats": np.stack([fbank(c, **features) for c in clips[i : i + self.batch]])})[0] for i in range(0, len(clips), self.batch)]
        return np.concatenate(out) if out else np.zeros((0, DIM), np.float32)


# ── Windows ──────────────────────────────────────────────────────────────────


def plan(spans, total):
    """(span index, start sample, samples) per window; `spans` are (start, end) in samples."""
    out = []
    for index, (start, end) in enumerate(spans):
        start, end = min(start, total), min(end, total)
        if end < start + 800:
            continue
        if end - start <= WINDOW:
            out.append((index, start, end - start))
            continue
        count = -(-(end - start) // WINDOW)
        step = (end - start - WINDOW) / (count - 1)
        out += [(index, start + int(round(k * step)), WINDOW) for k in range(count)]
    return out


# ── Grouping: crates/local-speaker/src/cluster.rs, constant for constant ─────

STRONG_SECONDS, WEAK_SHARE, MIN_STRONG, PER_CELL, MIN_CELL, MAX_SPEAKERS = 1.0, 0.2, 6, 8, 3, 24
SAME_VOICE, STANDS_SMALL, STANDS_BIG, SMALL_SECONDS, BIG_SECONDS, COHERENT, SHRINK, MIN_SECONDS = 0.65, 0.5, 0.6, 20.0, 60.0, 0.75, 20.0, 6.0


class Rng:
    """splitmix64, as in the crate: the seeding must not depend on a library's generator."""

    def __init__(self, seed):
        self.state = seed

    def unit(self):
        mask = (1 << 64) - 1
        self.state = (self.state + 0x9E3779B97F4A7C15) & mask
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & mask
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & mask
        return ((z ^ (z >> 31)) >> 11) / float(1 << 53)


def centre(points):
    c = points.sum(axis=0)
    return c / max(np.linalg.norm(c), 1e-9)


def cells(x, k):
    centres = [x[int(np.argmax(x @ centre(x)))]]
    apart = 1 - x @ centres[0]
    rng = Rng(0x5EED)
    while len(centres) < k:
        odds = np.maximum(apart, 0).astype(np.float64) ** 2
        if odds.sum() <= 0:
            break
        pick = min(int(np.searchsorted(np.cumsum(odds), rng.unit() * odds.sum(), side="right")), len(x) - 1)
        centres.append(x[pick])
        apart = np.minimum(apart, 1 - x @ centres[-1])
    centres = np.stack(centres)
    cell = np.zeros(len(x), int)
    for round_ in range(25):
        nearest = np.argmax(x @ centres.T, axis=1)
        if round_ > 0 and (nearest == cell).all():
            break
        cell = nearest
        for i in range(len(centres)):
            if (cell == i).any():
                centres[i] = centre(x[cell == i])
    return cell


def within(total, count):
    return (float(total @ total) - count) / (count * (count - 1)) if count > 1 else None


def group(unit, norm, seconds, fixed=None):
    """A voice for every window."""
    n = len(unit)
    if fixed == 1:
        return np.zeros(n, int)
    strong = (seconds >= STRONG_SECONDS) & (norm > 0)
    if strong.sum() >= MIN_STRONG:
        strong &= norm >= np.quantile(norm[strong], WEAK_SHARE)
    wanted = fixed or 0
    if fixed is not None and strong.sum() < max(MIN_STRONG, wanted):
        strong = norm > 0
    if strong.sum() < (2 if fixed is not None else MIN_STRONG):
        return np.zeros(n, int)
    x, x_seconds = unit[strong], seconds[strong]
    room = max(min(2 * wanted, len(x) // 2), wanted)
    cell = cells(x, min(max(int(np.clip(len(x) // PER_CELL, 2, MAX_SPEAKERS)), room), len(x)))
    ids = sorted(set(cell))
    sound = [i for i in ids if (cell == i).sum() >= MIN_CELL]
    if len(sound) < wanted:
        sound = ids
    sound = sound or [max(ids, key=lambda i: (cell == i).sum())]
    centres = np.stack([centre(x[cell == i]) for i in sound])
    for j in np.where(~np.isin(cell, sound))[0]:
        cell[j] = sound[int(np.argmax(centres @ x[j]))]
    groups = {i: [x[cell == i].sum(axis=0), int((cell == i).sum()), float(x_seconds[cell == i].sum())] for i in sound}

    def overall():
        pairs = sum(c * (c - 1) / 2 for _, c, _ in groups.values())
        return sum(within(t, c) * c * (c - 1) / 2 for t, c, _ in groups.values() if c > 1) / max(pairs, 1.0)

    def own(i, ov):
        total, count, _ = groups[i]
        pairs = count * (count - 1) / 2
        return max(((within(total, count) or 0.0) * pairs + ov * SHRINK) / (pairs + SHRINK), 1e-6)

    def ratio(a, b, ov):
        return float(groups[a][0] @ groups[b][0]) / (groups[a][1] * groups[b][1]) / np.sqrt(own(a, ov) * own(b, ov))

    def merge(into, gone):
        total, count, secs = groups.pop(gone)
        groups[into] = [groups[into][0] + total, groups[into][1] + count, groups[into][2] + secs]
        cell[cell == gone] = into

    def closest():
        ov, keys = overall(), list(groups)
        best = None
        for i, a in enumerate(keys):
            for b in keys[i + 1 :]:
                r = ratio(a, b, ov)
                if best is None or r > best[0]:
                    best = (r, a, b)
        return best

    if fixed is not None:
        while len(groups) > fixed:
            _, a, b = closest()
            merge(a, b)
    else:
        while len(groups) > 1:
            r, a, b = closest()
            if r < SAME_VOICE:
                break
            merge(a, b)
        while len(groups) > 1:
            ov = overall()
            order = sorted(groups, key=lambda i: groups[i][2])
            moved = False
            for rank, i in enumerate(order[:-1]):
                total, count, secs = groups[i]
                needed = STANDS_SMALL + (STANDS_BIG - STANDS_SMALL) * float(np.clip((secs - SMALL_SECONDS) / (BIG_SECONDS - SMALL_SECONDS), 0, 1))
                if count > 1 and within(total, count) < COHERENT * ov:
                    needed = STANDS_SMALL
                best = None
                for other in order[rank + 1 :]:
                    r = ratio(i, other, ov)
                    if best is None or r > best[0]:
                        best = (r, other)
                if best[0] >= needed:
                    merge(best[1], i)
                    moved = True
                    break
            if not moved:
                break

    ids = list(groups)
    centres = np.stack([groups[i][0] / max(np.linalg.norm(groups[i][0]), 1e-9) for i in ids])
    labels = np.zeros(n, int)
    labels[strong] = cell
    if (~strong).any():
        labels[~strong] = np.array(ids)[np.argmax(unit[~strong] @ centres.T, axis=1)]
    if fixed is None:
        heard = [i for i in ids if seconds[labels == i].sum() >= MIN_SECONDS] or [max(ids, key=lambda i: seconds[labels == i].sum())]
        if len(heard) < len(ids):
            centres = np.stack([centre(unit[labels == i]) for i in heard])
            for j in np.where(~np.isin(labels, heard))[0]:
                labels[j] = heard[int(np.argmax(centres @ unit[j]))]
    before = labels.copy()
    for _ in range(2):
        ids = sorted(set(labels))
        centres = np.stack([centre(unit[labels == i]) for i in ids])
        nearest = np.array(ids)[np.argmax(unit @ centres.T, axis=1)]
        if (nearest == labels).all():
            break
        labels = nearest
    # Asked for by number, the number stands even where tidying would empty a voice.
    return before if fixed is not None and len(set(before)) > len(set(labels)) else labels


def cluster(raw, windows, spans, fixed=None):
    """Speaker per span (None where it had no window), numbered as first heard, and how many."""
    if not len(raw):
        return [None] * spans, 0
    norm = np.linalg.norm(raw, axis=1).astype(np.float32)
    # A print that is not a number shapes nothing: no length, no direction.
    sound = np.isfinite(norm)
    norm = np.where(sound, norm, 0).astype(np.float32)
    unit = np.where(sound[:, None], raw / np.maximum(norm[:, None], 1e-9), 0).astype(np.float32)
    seconds = np.array([w[2] / RATE for w in windows], np.float32)
    voices = group(unit, norm, seconds, None if fixed is None else int(np.clip(fixed, 1, MAX_SPEAKERS)))
    tally = [dict() for _ in range(spans)]
    for (span, _, _), voice, secs in zip(windows, voices, seconds):
        if span < spans:
            tally[span][voice] = tally[span].get(voice, 0.0) + float(secs)
    order, out = [], []
    for votes in tally:
        if not votes:
            out.append(None)
            continue
        voice = max(votes, key=votes.get)
        if voice not in order:
            order.append(voice)
        out.append(order.index(voice))
    return out, len(order)


# ── Modes ────────────────────────────────────────────────────────────────────


def read_prints(path):
    data = open(path, "rb").read()
    assert data[:8] == b"VPRINTS1", f"{path}: not a prints file"
    count = struct.unpack_from("<I", data, 16)[0]
    windows = [struct.unpack_from("<IQI", data, 20 + 16 * i) for i in range(count)]
    return np.frombuffer(data, "<f4", count * DIM, 20 + 16 * count).reshape(count, DIM), windows


def fixtures(network, out):
    import kaldi_native_fbank as knf

    pcm = np.frombuffer(open(f"{out}/../../../local-whisper/tests/fixtures/jfk.s16", "rb").read(), "<i2")[:WINDOW].astype(np.float32) / 32768.0
    options = knf.FbankOptions()
    options.frame_opts.dither, options.frame_opts.samp_freq, options.frame_opts.window_type, options.mel_opts.num_bins = 0, RATE, "hamming", MELS
    kaldi = knf.OnlineFbank(options)
    kaldi.accept_waveform(RATE, (pcm * 32768.0).tolist())
    kaldi.input_finished()
    frames = np.stack([kaldi.get_frame(i) for i in range(kaldi.num_frames_ready)])
    frames = (frames - frames.mean(axis=0, keepdims=True)).astype("<f4")
    frames.tofile(f"{out}/jfk_fbank.f32")
    network.prints([pcm])[0].astype("<f4").tofile(f"{out}/jfk_print.f32")
    print(f"wrote jfk_fbank.f32 {frames.shape} (this script's own features differ by at most {np.abs(fbank(pcm) - frames).max():.5f}) and jfk_print.f32")


def features(network, root):
    import glob, itertools, os
    import soundfile as sf

    clips, who = [], []
    speakers = {}
    for path in sorted(glob.glob(os.path.join(root, "**/*.flac"), recursive=True)):
        speakers.setdefault(os.path.basename(path).split("-")[0], []).append(path)
    for speaker, files in sorted(speakers.items()):
        for path in files[:: max(1, len(files) // 6)][:6]:
            pcm, rate = sf.read(path, dtype="float32")
            assert rate == RATE
            clips.append(tile(pcm[max(0, len(pcm) // 2 - WINDOW // 2) : len(pcm) // 2 + WINDOW // 2]))
            who.append(speaker)
    who = np.array(who)
    pairs = np.triu_indices(len(clips), 1)
    same = who[pairs[0]] == who[pairs[1]]
    print(f"{len(speakers)} speakers, {len(clips)} two-second clips")
    for window, normalise in itertools.product(("hamming", "povey"), (True, False)):
        raw = network.prints(clips, window=window, normalise=normalise)
        unit = raw / np.linalg.norm(raw, axis=1, keepdims=True)
        alike = (unit @ unit.T)[pairs]
        error = min((abs((alike[same] < t).mean() - (alike[~same] >= t).mean()), ((alike[same] < t).mean() + (alike[~same] >= t).mean()) / 2) for t in np.linspace(-0.2, 1, 601))[1]
        print(f"  {window:8} mean normalisation {'on ' if normalise else 'off'}  equal error {error * 100:5.2f} %   same voice {alike[same].mean():.2f}, different {alike[~same].mean():.2f}")


def ranges(onnx_path, wavs):
    import onnx, onnxruntime as ort
    import soundfile as sf

    model = onnx.shape_inference.infer_shapes(onnx.load(onnx_path))
    have = {o.name for o in model.graph.output}
    model.graph.output.extend(v for v in model.graph.value_info if v.name not in have and v.type.tensor_type.elem_type == onnx.TensorProto.FLOAT)
    session = ort.InferenceSession(model.SerializeToString(), providers=["CPUExecutionProvider"])
    names = [o.name for o in session.get_outputs()]
    worst, count = 0.0, 0
    for path in wavs:
        pcm, rate = sf.read(path, dtype="float32")
        assert rate == RATE
        for start in range(0, min(len(pcm), RATE * 600) - WINDOW, RATE * 7):
            worst = max([worst] + [float(np.abs(v).max()) for v in session.run(names, {"feats": fbank(pcm[start : start + WINDOW])[None]}) if v.size])
            count += 1
    print(f"largest activation over {count} windows and {len(names)} tensors: {worst:.1f} (fp16 holds 65504)")


def meeting(network, wav, rttm, fixed):
    import soundfile as sf
    from scipy.optimize import linear_sum_assignment

    turns = sorted((float(p[3]), float(p[3]) + float(p[4]), p[7]) for p in (line.split() for line in open(rttm)) if p and p[0] == "SPEAKER")
    # Cut where everyone pauses and by length, blind to who speaks: what a transcriber gives.
    stretches = []
    for start, end, _ in turns:
        if stretches and start - stretches[-1][1] <= 0.4:
            stretches[-1][1] = max(stretches[-1][1], end)
        else:
            stretches.append([start, end])
    spans = []
    for start, end in stretches:
        pieces = max(1, int(np.ceil((end - start) / 8.0)))
        spans += [(start + k * (end - start) / pieces, start + (k + 1) * (end - start) / pieces) for k in range(pieces)]
    pcm, rate = sf.read(wav, dtype="float32")
    assert rate == RATE
    windows = plan([(int(round(s * RATE)), int(round(e * RATE))) for s, e in spans], len(pcm))
    raw = network.prints([tile(pcm[start : start + length]) for _, start, length in windows])
    labels, found = cluster(raw, windows, len(spans), fixed)
    people = sorted({who for _, _, who in turns})
    table = np.zeros((max(found, 1), len(people)))
    for (start, end), label in zip(spans, labels):
        overlap = {}
        for t_start, t_end, who in turns:
            shared = min(end, t_end) - max(start, t_start)
            if shared > 0:
                overlap[who] = overlap.get(who, 0.0) + shared
        if label is not None and overlap:
            table[label, people.index(max(overlap, key=overlap.get))] += end - start
    rows, columns = linear_sum_assignment(-table)
    print(f"{wav}: {len(spans)} stretches, {len(windows)} windows, {found} speakers found of {len(people)}; {table[rows, columns].sum() / table.sum() * 100:.1f} % of speech with the right speaker")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--onnx", help="the fp32 model (export.py's work dir: model_static.onnx)")
    ap.add_argument("--fixtures", metavar="DIR", help="crates/local-speaker/tests/fixtures")
    ap.add_argument("--features", metavar="LIBRISPEECH")
    ap.add_argument("--ranges", nargs="+", metavar="WAV")
    ap.add_argument("--meeting", nargs=2, metavar=("WAV", "RTTM"))
    ap.add_argument("--prints", nargs="+", metavar="FILE")
    ap.add_argument("--fixed", type=int, help="with --meeting or --prints: the number of voices, instead of finding it")
    args = ap.parse_args()
    if args.prints:
        for path in args.prints:
            raw, windows = read_prints(path)
            labels, found = cluster(raw, windows, max((w[0] for w in windows), default=-1) + 1, args.fixed)
            print(json.dumps({"file": path, "fixed": args.fixed, "speakers": found, "spans": labels}))
        return
    if not args.onnx:
        sys.exit("--onnx is needed for everything but --prints")
    if args.ranges:
        ranges(args.onnx, args.ranges)
    network = Network(args.onnx)
    if args.fixtures:
        fixtures(network, args.fixtures)
    if args.features:
        features(network, args.features)
    if args.meeting:
        meeting(network, args.meeting[0], args.meeting[1], args.fixed)


if __name__ == "__main__":
    main()
