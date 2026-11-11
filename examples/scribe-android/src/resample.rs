//! Streaming sample-rate conversion down to Whisper's 16 kHz.
//!
//! The recording service asks Android for 16 kHz and almost always gets it. A device that
//! refuses records at 48 or 44.1 kHz instead, and feeding that to the model unconverted would
//! transcribe three-times-slow audio as noise — so capture at any other rate passes through
//! here: a windowed-sinc low-pass evaluated at each output instant, which handles the integer
//! 3:1 and the awkward 441:160 alike.

/// Input samples the kernel reaches on each side of an output instant, at the output rate's
/// scale; the reach in input samples grows with the decimation ratio.
const LOBES: f64 = 12.0;

pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Low-pass cutoff as a fraction of the input rate's Nyquist.
    cutoff: f64,
    half: usize,
    /// Tail of the input not yet fully consumed, oldest first.
    history: Vec<f32>,
    /// Position of the next output instant, in samples from the start of `history`.
    next: f64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let step = from as f64 / to as f64;
        // Just under the output Nyquist so the transition band stays out of the passband.
        let cutoff = (0.92 / step).min(1.0);
        let half = (LOBES / cutoff).ceil() as usize;
        Self { step, cutoff, half, history: vec![0.0; half], next: half as f64 }
    }

    /// Whether `from` needs converting at all.
    pub fn needed(from: u32, to: u32) -> bool {
        from != to
    }

    fn kernel(&self, distance: f64) -> f64 {
        let reach = self.half as f64;
        if distance.abs() >= reach {
            return 0.0;
        }
        let x = std::f64::consts::PI * distance * self.cutoff;
        let sinc = if x.abs() < 1e-9 { 1.0 } else { x.sin() / x };
        // Hann taper over the kernel's reach.
        let window = 0.5 + 0.5 * (std::f64::consts::PI * distance / reach).cos();
        sinc * window * self.cutoff
    }

    /// Convert `input`, appending whatever output instants it completes to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        self.history.extend(input.iter().map(|&s| s as f32));
        let half = self.half as f64;
        while self.next + half < self.history.len() as f64 {
            let centre = self.next;
            let first = (centre - half).ceil().max(0.0) as usize;
            let last = ((centre + half).floor() as usize).min(self.history.len() - 1);
            let mut acc = 0f64;
            for i in first..=last {
                acc += self.history[i] as f64 * self.kernel(i as f64 - centre);
            }
            out.push(acc.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            self.next += self.step;
        }
        // Drop input no future output can reach.
        let keep_from = ((self.next - half).floor().max(0.0) as usize).min(self.history.len());
        self.history.drain(..keep_from);
        self.next -= keep_from as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, seconds: f64, amplitude: f64) -> Vec<i16> {
        (0..(rate as f64 * seconds) as usize).map(|i| ((i as f64 * hz * std::f64::consts::TAU / rate as f64).sin() * amplitude) as i16).collect()
    }

    fn convert(from: u32, input: &[i16], chunk: usize) -> Vec<i16> {
        let mut resampler = Resampler::new(from, 16_000);
        let mut out = Vec::new();
        for piece in input.chunks(chunk) {
            resampler.process(piece, &mut out);
        }
        out
    }

    fn rms(samples: &[i16]) -> f64 {
        (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len().max(1) as f64).sqrt()
    }

    #[test]
    fn speech_band_passes_at_full_level_from_both_fallback_rates() {
        for from in [48_000, 44_100] {
            let out = convert(from, &tone(from, 1_000.0, 1.0, 10_000.0), 4_800);
            // A second in, a second out, give or take the filter's delay.
            assert!((out.len() as i64 - 16_000).abs() < 80, "{from}: {} samples", out.len());
            let level = rms(&out[2_000..14_000]);
            assert!((level - 10_000.0 / 2f64.sqrt()).abs() < 150.0, "{from}: rms {level}");
        }
    }

    #[test]
    fn the_output_is_the_same_tone_not_a_slowed_one() {
        let out = convert(48_000, &tone(48_000, 1_000.0, 1.0, 10_000.0), 4_800);
        // 1 kHz at 16 kHz crosses zero upward once every 16 samples.
        let body = &out[2_000..14_000];
        let crossings = body.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        assert!((crossings as i64 - 750).abs() <= 2, "{crossings} crossings");
    }

    #[test]
    fn content_above_the_new_nyquist_is_removed_not_folded() {
        // 10 kHz would alias to 6 kHz at 16 kHz if it got through.
        let out = convert(48_000, &tone(48_000, 10_000.0, 1.0, 10_000.0), 4_800);
        assert!(rms(&out[2_000..14_000]) < 100.0, "rms {}", rms(&out[2_000..14_000]));
    }

    #[test]
    fn chunk_boundaries_leave_no_seam() {
        let input = tone(44_100, 440.0, 1.0, 8_000.0);
        assert_eq!(convert(44_100, &input, 441), convert(44_100, &input, 4_410));
        assert_eq!(convert(44_100, &input, 7), convert(44_100, &input, 44_100));
    }

    #[test]
    fn steady_input_keeps_its_level() {
        let out = convert(48_000, &vec![5_000i16; 48_000], 4_800);
        assert!(out[2_000..14_000].iter().all(|&s| (s - 5_000).abs() <= 25), "dc drifted");
        assert!(!Resampler::needed(16_000, 16_000));
        assert!(Resampler::needed(48_000, 16_000));
    }
}
