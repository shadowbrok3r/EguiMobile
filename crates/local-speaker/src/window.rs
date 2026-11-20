//! Cutting the stretches to label into the two-second windows the network takes.

/// Samples in one window: two seconds. Long enough for a voice print that tells speakers
/// apart (1.6 % error on two-second clips), short enough that most windows hold one voice.
pub const WINDOW_SAMPLES: usize = 32_000;
/// Stretches shorter than this get no window: there is no voice in 50 ms.
const MIN_SAMPLES: u64 = 800;

/// A stretch of audio one label is wanted for, in samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

/// One input to the network: up to [`WINDOW_SAMPLES`] of one span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// Index of the span it was cut from.
    pub span: u32,
    pub start: u64,
    /// Samples of real audio; a shorter one is repeated to fill the window ([`fill`]).
    pub len: u32,
}

impl Window {
    pub fn seconds(&self) -> f32 {
        self.len as f32 / crate::SAMPLE_RATE as f32
    }
}

/// The windows covering `spans` of a recording `total` samples long. A span longer than a
/// window gets as many as it takes to cover it, spread evenly and overlapping a little; a
/// shorter one gets a single window of what it has. Never audio from outside the span: the
/// next span is as likely as not someone else.
pub fn plan(spans: &[Span], total: u64) -> Vec<Window> {
    let size = WINDOW_SAMPLES as u64;
    let mut windows = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        let (start, end) = (span.start.min(total), span.end.min(total));
        if end < start + MIN_SAMPLES {
            continue;
        }
        let length = end - start;
        if length <= size {
            windows.push(Window { span: index as u32, start, len: length as u32 });
            continue;
        }
        let count = length.div_ceil(size);
        let step = (length - size) as f64 / (count - 1) as f64;
        windows.extend((0..count).map(|k| Window { span: index as u32, start: start + (k as f64 * step).round() as u64, len: size as u32 }));
    }
    windows
}

/// Fill `window` with `audio`, repeated as often as it takes. Repeating a short utterance is
/// how the network was trained to see one; padding it with silence, or with its neighbours,
/// is not.
pub fn fill(audio: &[f32], window: &mut [f32]) {
    if audio.is_empty() {
        window.fill(0.0);
        return;
    }
    for (slot, sample) in window.iter_mut().zip(audio.iter().cycle()) {
        *slot = *sample;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_covered_by_windows_of_their_own_audio() {
        let spans = [
            Span { start: 0, end: 16_000 },        // one second: one short window
            Span { start: 20_000, end: 52_000 },   // exactly two: one full window
            Span { start: 60_000, end: 140_000 },  // five seconds: three, overlapping
            Span { start: 150_000, end: 150_100 }, // a click: none
            Span { start: 190_000, end: 230_000 }, // runs past the end of the audio
        ];
        let windows = plan(&spans, 200_000);
        assert_eq!(windows[0], Window { span: 0, start: 0, len: 16_000 });
        assert_eq!(windows[1], Window { span: 1, start: 20_000, len: 32_000 });
        assert_eq!(windows[2..5], [Window { span: 2, start: 60_000, len: 32_000 }, Window { span: 2, start: 84_000, len: 32_000 }, Window { span: 2, start: 108_000, len: 32_000 }]);
        assert_eq!(windows[5], Window { span: 4, start: 190_000, len: 10_000 });
        assert_eq!(windows.len(), 6);
        for window in &windows {
            let span = spans[window.span as usize];
            assert!(window.start >= span.start && window.start + window.len as u64 <= span.end.min(200_000));
        }
        assert_eq!(windows[0].seconds(), 1.0);
    }

    #[test]
    fn short_audio_is_repeated_to_fill_a_window() {
        let mut window = [9.0f32; 8];
        fill(&[1.0, 2.0, 3.0], &mut window);
        assert_eq!(window, [1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 1.0, 2.0]);
        fill(&[5.0; 20], &mut window);
        assert_eq!(window, [5.0; 8]);
        fill(&[], &mut window);
        assert_eq!(window, [0.0; 8]);
    }
}
