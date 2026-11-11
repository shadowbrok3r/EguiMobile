//! The network's front end: Kaldi `fbank` features of one two-second window.
//!
//! WeSpeaker trains on `torchaudio.compliance.kaldi.fbank` over int16-scaled samples — 25 ms
//! Hamming frames every 10 ms, DC removed and pre-emphasised per frame, a 512-point power
//! spectrum, 80 triangular mel filters from 20 Hz to Nyquist on Kaldi's mel scale, natural
//! log — with each bin's mean over the utterance subtracted. The weights expect exactly that:
//! without the mean subtraction the same model's error rate goes from 1.6 % to 13 %
//! (`scripts/speaker-export/reference.py --features` measures it). The tests pin this against
//! frames dumped from kaldi-native-fbank.

use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

use crate::window::WINDOW_SAMPLES;

/// Samples per frame (25 ms) and between frames (10 ms).
const FRAME: usize = 400;
const SHIFT: usize = 160;
const N_FFT: usize = 512;
pub const MELS: usize = 80;
/// Frames in one window: whole frames only, as Kaldi's `snip_edges` has it.
pub const FRAMES: usize = 1 + (WINDOW_SAMPLES - FRAME) / SHIFT;

const PREEMPHASIS: f32 = 0.97;
const LOW_HZ: f64 = 20.0;
const RATE: f64 = 16_000.0;

fn mel(hz: f64) -> f64 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

/// One triangular filter as its non-zero run of FFT bins.
struct Filter {
    first_bin: usize,
    weights: Vec<f32>,
}

/// Kaldi's filterbank: triangles even on the mel scale, evaluated at each bin's own mel, and
/// never reaching the Nyquist bin.
fn filters() -> Vec<Filter> {
    let (low, high) = (mel(LOW_HZ), mel(RATE / 2.0));
    let step = (high - low) / (MELS + 1) as f64;
    (0..MELS)
        .map(|m| {
            let (left, centre, right) = (low + m as f64 * step, low + (m + 1) as f64 * step, low + (m + 2) as f64 * step);
            let weight = |bin: usize| {
                let at = mel(bin as f64 * RATE / N_FFT as f64);
                ((at - left) / (centre - left)).min((right - at) / (right - centre)).max(0.0) as f32
            };
            let first_bin = (0..N_FFT / 2).find(|&bin| weight(bin) > 0.0).unwrap_or(0);
            let end = (first_bin..N_FFT / 2).find(|&bin| weight(bin) <= 0.0).unwrap_or(N_FFT / 2);
            Filter { first_bin, weights: (first_bin..end).map(weight).collect() }
        })
        .collect()
}

/// Reusable feature extractor.
pub struct Fbank {
    filters: Vec<Filter>,
    window: Vec<f32>,
    fft: Arc<dyn RealToComplex<f32>>,
    frame: Vec<f32>,
    spectrum: Vec<realfft::num_complex::Complex<f32>>,
    scratch: Vec<realfft::num_complex::Complex<f32>>,
}

impl Default for Fbank {
    fn default() -> Self {
        Self::new()
    }
}

impl Fbank {
    pub fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(N_FFT);
        Self {
            filters: filters(),
            // Symmetric Hamming, as Kaldi defines it.
            window: (0..FRAME).map(|n| (0.54 - 0.46 * (2.0 * std::f64::consts::PI * n as f64 / (FRAME - 1) as f64).cos()) as f32).collect(),
            frame: vec![0.0; N_FFT],
            spectrum: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
        }
    }

    /// Features of one window: `pcm` holds [`WINDOW_SAMPLES`] samples in -1..1, `out`
    /// receives [`FRAMES`] rows of [`MELS`] values, mean-normalised per mel bin.
    pub fn window(&mut self, pcm: &[f32], out: &mut [f32]) {
        assert_eq!(pcm.len(), WINDOW_SAMPLES, "one window of audio");
        assert_eq!(out.len(), FRAMES * MELS, "one window of features");
        for (index, row) in out.chunks_exact_mut(MELS).enumerate() {
            let samples = &pcm[index * SHIFT..index * SHIFT + FRAME];
            // The weights were trained on 16-bit sample values, not on -1..1.
            let mean = samples.iter().sum::<f32>() / FRAME as f32 * 32768.0;
            for (slot, sample) in self.frame.iter_mut().zip(samples) {
                *slot = sample * 32768.0 - mean;
            }
            for i in (1..FRAME).rev() {
                self.frame[i] -= PREEMPHASIS * self.frame[i - 1];
            }
            self.frame[0] -= PREEMPHASIS * self.frame[0];
            for (slot, weight) in self.frame.iter_mut().zip(&self.window) {
                *slot *= weight;
            }
            self.frame[FRAME..].fill(0.0);
            // The transform uses its input as scratch; the frame is rebuilt each time anyway.
            self.fft.process_with_scratch(&mut self.frame, &mut self.spectrum, &mut self.scratch).expect("buffers sized by the planner");
            for (value, filter) in row.iter_mut().zip(&self.filters) {
                let energy: f32 = filter.weights.iter().zip(&self.spectrum[filter.first_bin..]).map(|(weight, bin)| weight * bin.norm_sqr()).sum();
                *value = energy.max(f32::EPSILON).ln();
            }
        }
        // Cepstral mean normalisation: whatever the microphone and the room add to every
        // frame alike comes out here.
        for bin in 0..MELS {
            let mean = (0..FRAMES).map(|frame| out[frame * MELS + bin]).sum::<f32>() / FRAMES as f32;
            for frame in 0..FRAMES {
                out[frame * MELS + bin] -= mean;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_198_frames_and_the_filters_cover_the_band() {
        assert_eq!(FRAMES, 198);
        let bank = filters();
        assert_eq!(bank.len(), MELS);
        // 20 Hz sits below bin 1 (31.25 Hz): the first triangle starts there, and the last
        // ends just short of Nyquist.
        assert_eq!(bank[0].first_bin, 1);
        let last = bank.last().unwrap();
        assert_eq!(last.first_bin + last.weights.len(), N_FFT / 2);
        assert!(bank.iter().all(|f| !f.weights.is_empty() && f.weights.iter().all(|w| (0.0..=1.0).contains(w))));
    }

    #[test]
    fn features_have_no_mean_and_ignore_loudness() {
        let tone = |gain: f32| -> Vec<f32> { (0..WINDOW_SAMPLES).map(|i| gain * ((i as f32 * 0.07).sin() + 0.4 * (i as f32 * 0.31).sin() * (i as f32 * 0.0007).cos())).collect() };
        let mut fbank = Fbank::new();
        let (mut quiet, mut loud) = (vec![0f32; FRAMES * MELS], vec![0f32; FRAMES * MELS]);
        fbank.window(&tone(0.05), &mut quiet);
        fbank.window(&tone(0.5), &mut loud);
        for bin in 0..MELS {
            let mean: f32 = (0..FRAMES).map(|f| quiet[f * MELS + bin]).sum::<f32>() / FRAMES as f32;
            assert!(mean.abs() < 1e-3, "bin {bin} has mean {mean}");
        }
        // Ten times louder is a constant added to every log energy, which the mean takes out.
        let worst = quiet.iter().zip(&loud).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(worst < 2e-2, "loudness changed the features by {worst}");
        // Digital silence is finite and featureless: the floor under the log holds.
        fbank.window(&vec![0.0; WINDOW_SAMPLES], &mut quiet);
        assert!(quiet.iter().all(|v| v.abs() < 1e-4));
    }

    /// Frames dumped by `scripts/speaker-export/reference.py --fixtures` from
    /// kaldi-native-fbank for the first two seconds of the clip the Whisper tests use.
    #[test]
    fn features_match_kaldi() {
        let pcm: Vec<f32> = include_bytes!("../../local-whisper/tests/fixtures/jfk.s16").chunks_exact(2).take(WINDOW_SAMPLES).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
        let expected: Vec<f32> = include_bytes!("../tests/fixtures/jfk_fbank.f32").chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        assert_eq!(expected.len(), FRAMES * MELS);
        let mut got = vec![0f32; FRAMES * MELS];
        Fbank::new().window(&pcm, &mut got);
        let worst = got.iter().zip(&expected).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        let mean = got.iter().zip(&expected).map(|(a, b)| (a - b).abs()).sum::<f32>() / got.len() as f32;
        // Log energies of near-silent bins amplify rounding; everywhere else it is 1e-4.
        assert!(worst < 2e-2 && mean < 2e-4, "worst {worst}, mean {mean}");
    }
}
