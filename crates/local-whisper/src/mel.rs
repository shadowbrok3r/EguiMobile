//! Whisper's log-mel front end: 16 kHz mono in, `n_mels x 3000` normalized log-mel frames out.
//!
//! Numerically the same as OpenAI's `log_mel_spectrogram` and Hugging Face's
//! `WhisperFeatureExtractor`: a 30 s window zero-padded in the sample domain, a centred STFT
//! (periodic Hann, 400-point FFT, hop 160, reflect padding), the Slaney mel filterbank, then
//! `log10`, an 8-decade floor under the window's own peak, and `(x + 4) / 4`. The encoder was
//! trained on exactly these numbers, so "close" is not good enough — the tests pin this against
//! features dumped from the reference implementation.

use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

pub const SAMPLE_RATE: usize = 16_000;
pub const N_FFT: usize = 400;
pub const HOP: usize = 160;
/// Samples in one model window (30 s).
pub const CHUNK_SAMPLES: usize = 30 * SAMPLE_RATE;
/// Mel frames in one model window.
pub const N_FRAMES: usize = CHUNK_SAMPLES / HOP;
/// Seconds of audio one mel frame advances.
pub const FRAME_SECONDS: f64 = HOP as f64 / SAMPLE_RATE as f64;

const N_BINS: usize = N_FFT / 2 + 1;
const PAD: usize = N_FFT / 2;

/// One triangular mel filter, stored as its non-zero run of FFT bins.
struct Filter {
    first_bin: usize,
    weights: Vec<f64>,
}

fn hertz_to_mel(hz: f64) -> f64 {
    if hz >= 1000.0 { 15.0 + (hz / 1000.0).ln() * (27.0 / 6.4f64.ln()) } else { 3.0 * hz / 200.0 }
}

fn mel_to_hertz(mel: f64) -> f64 {
    if mel >= 15.0 { 1000.0 * ((6.4f64.ln() / 27.0) * (mel - 15.0)).exp() } else { 200.0 * mel / 3.0 }
}

/// The Slaney-scale, Slaney-normalized filterbank (`librosa.filters.mel(sr=16000, n_fft=400)`),
/// dense, row-major `[n_mels][201]`.
pub fn mel_filters(n_mels: usize) -> Vec<f64> {
    let (lo, hi) = (hertz_to_mel(0.0), hertz_to_mel(SAMPLE_RATE as f64 / 2.0));
    let edges: Vec<f64> = (0..n_mels + 2).map(|i| mel_to_hertz(lo + (hi - lo) * i as f64 / (n_mels + 1) as f64)).collect();
    let mut out = vec![0.0; n_mels * N_BINS];
    for m in 0..n_mels {
        let (left, centre, right) = (edges[m], edges[m + 1], edges[m + 2]);
        let norm = 2.0 / (right - left);
        for bin in 0..N_BINS {
            let hz = (SAMPLE_RATE as f64 / 2.0) * bin as f64 / (N_BINS - 1) as f64;
            let weight = ((hz - left) / (centre - left)).min((right - hz) / (right - centre)).max(0.0);
            out[m * N_BINS + bin] = weight * norm;
        }
    }
    out
}

/// Reusable log-mel extractor for one mel count (80 up to large-v2, 128 for large-v3 and turbo).
pub struct MelFrontEnd {
    n_mels: usize,
    filters: Vec<Filter>,
    window: Vec<f64>,
    fft: Arc<dyn RealToComplex<f64>>,
}

impl MelFrontEnd {
    pub fn new(n_mels: usize) -> Self {
        let dense = mel_filters(n_mels);
        let filters = (0..n_mels)
            .map(|m| {
                let row = &dense[m * N_BINS..(m + 1) * N_BINS];
                let first_bin = row.iter().position(|&w| w > 0.0).unwrap_or(0);
                let last_bin = row.iter().rposition(|&w| w > 0.0).map_or(first_bin, |b| b + 1);
                Filter { first_bin, weights: row[first_bin..last_bin].to_vec() }
            })
            .collect();
        let window = (0..N_FFT).map(|n| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / N_FFT as f64).cos()).collect();
        Self { n_mels, filters, window, fft: RealFftPlanner::<f64>::new().plan_fft_forward(N_FFT) }
    }

    pub fn n_mels(&self) -> usize {
        self.n_mels
    }

    /// Features for one window. `samples` is mono 16 kHz in -1..1, at most 30 s; anything
    /// shorter is zero-padded, anything longer is cut. Returns `n_mels * 3000` values, mel-major,
    /// which is the encoder's `[1, n_mels, 3000]` input laid out flat.
    pub fn compute(&self, samples: &[f32]) -> Vec<f32> {
        let samples = &samples[..samples.len().min(CHUNK_SAMPLES)];
        // The window is always 30 s, so the reflection at its end mirrors padding zeros unless
        // the audio fills it.
        let at = |i: usize| -> f64 { samples.get(i).copied().unwrap_or(0.0) as f64 };
        let padded = |i: usize| -> f64 {
            if i < PAD {
                at(PAD - i)
            } else if i < PAD + CHUNK_SAMPLES {
                at(i - PAD)
            } else {
                at(2 * CHUNK_SAMPLES + PAD - 2 - i)
            }
        };

        let mut log_mel = vec![0f64; self.n_mels * N_FRAMES];
        let mut frame = self.fft.make_input_vec();
        let mut spectrum = self.fft.make_output_vec();
        let mut scratch = self.fft.make_scratch_vec();
        let mut power = [0f64; N_BINS];
        // Frames wholly past the audio are silence; their value is the floor, with no FFT.
        let floor = 1e-10f64.log10();
        let mut peak = f64::NEG_INFINITY;
        for t in 0..N_FRAMES {
            let start = t * HOP;
            let silent = start >= samples.len() + PAD && start + N_FFT <= CHUNK_SAMPLES + PAD;
            if silent {
                for m in 0..self.n_mels {
                    log_mel[m * N_FRAMES + t] = floor;
                }
                peak = peak.max(floor);
                continue;
            }
            for (n, slot) in frame.iter_mut().enumerate() {
                *slot = padded(start + n) * self.window[n];
            }
            // The planner sized every buffer, so the only error path is unreachable.
            let _ = self.fft.process_with_scratch(&mut frame, &mut spectrum, &mut scratch);
            for (p, c) in power.iter_mut().zip(&spectrum) {
                *p = c.re * c.re + c.im * c.im;
            }
            for (m, filter) in self.filters.iter().enumerate() {
                let energy: f64 = filter.weights.iter().zip(&power[filter.first_bin..]).map(|(w, p)| w * p).sum();
                let value = energy.max(1e-10).log10();
                log_mel[m * N_FRAMES + t] = value;
                peak = peak.max(value);
            }
        }
        log_mel.iter().map(|&v| ((v.max(peak - 8.0) + 4.0) / 4.0) as f32).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32s(bytes: &[u8]) -> Vec<f32> {
        bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    fn jfk() -> Vec<f32> {
        include_bytes!("../tests/fixtures/jfk.s16").chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect()
    }

    #[test]
    fn filterbank_matches_the_reference() {
        for (n_mels, bytes) in [
            (80, &include_bytes!("../tests/fixtures/filters80.f32")[..]),
            (128, &include_bytes!("../tests/fixtures/filters128.f32")[..]),
        ] {
            let ours = mel_filters(n_mels);
            let theirs = f32s(bytes);
            assert_eq!(ours.len(), theirs.len());
            let worst = ours.iter().zip(&theirs).map(|(a, b)| (*a as f32 - b).abs()).fold(0f32, f32::max);
            assert!(worst < 1e-7, "{n_mels} mels: filter weights differ by {worst}");
        }
    }

    #[test]
    fn features_match_the_reference_extractor() {
        let audio = jfk();
        for (n_mels, bytes) in [
            (80, &include_bytes!("../tests/fixtures/jfk.mel80.every25.f32")[..]),
            (128, &include_bytes!("../tests/fixtures/jfk.mel128.every25.f32")[..]),
        ] {
            let ours = MelFrontEnd::new(n_mels).compute(&audio);
            assert_eq!(ours.len(), n_mels * N_FRAMES);
            let theirs = f32s(bytes);
            let kept = N_FRAMES / 25;
            assert_eq!(theirs.len(), n_mels * kept);
            let mut worst = 0f32;
            for m in 0..n_mels {
                for k in 0..kept {
                    worst = worst.max((ours[m * N_FRAMES + k * 25] - theirs[m * kept + k]).abs());
                }
            }
            assert!(worst < 2e-4, "{n_mels} mels: features differ by {worst}");
        }
    }

    #[test]
    fn silence_is_a_flat_floor() {
        let mel = MelFrontEnd::new(80).compute(&[]);
        // log10(1e-10) = -10 is its own peak, so the floor is (-10 + 4) / 4.
        assert!(mel.iter().all(|&v| (v + 1.5).abs() < 1e-6));
    }

    #[test]
    fn overlong_input_is_cut_to_one_window() {
        let long = vec![0.25f32; CHUNK_SAMPLES + 5000];
        let front = MelFrontEnd::new(80);
        assert_eq!(front.compute(&long), front.compute(&long[..CHUNK_SAMPLES]));
    }
}
