//! A scripted stand-in for the NPU, for devices that have none.
//!
//! An emulator has no HTP, so without this the whole path from "a window of audio is ready"
//! to "its text is in the note" could only ever run on the phone. This engine answers every
//! window with canned sentences, through the same trait and the same decoding rules as the
//! real one. It is off unless switched on in the debug panel, and says what it is in every
//! sentence it produces.

use local_whisper::{Engine, Error, ModelDims, Result, Vocab};
use std::collections::HashMap;

const N_VOCAB: usize = 51865;
const SENTENCES: [&str; 6] = [
    " This is a simulated transcript.",
    " No speech model heard this recording.",
    " The simulated engine is switched on in the debug panel.",
    " Switch it off to transcribe on the NPU.",
    " These words stand in for the next few seconds of audio.",
    " The audio itself is saved exactly as it was recorded.",
];
/// Seconds of audio each scripted sentence claims.
const SENTENCE_SECONDS: f64 = 6.0;

pub struct SimEngine {
    dims: ModelDims,
    vocab: Vocab,
    sentences: Vec<Vec<u32>>,
    script: Vec<u32>,
    encoded: usize,
    position: usize,
    /// Added to every decoder run, to stand in for a slow model.
    pub step_delay: std::time::Duration,
}

impl SimEngine {
    pub fn new() -> Self {
        let vocab = Vocab::multilingual(N_VOCAB).expect("a known vocabulary size");
        // Greedy longest match: not the tokenization Whisper would pick, but one that decodes
        // back to the same text, which is all a script needs.
        let by_bytes: HashMap<&[u8], u32> = (0..vocab.eot).map(|id| (vocab.bytes(id), id)).collect();
        let longest = by_bytes.keys().map(|b| b.len()).max().unwrap_or(1);
        let tokenize = |text: &str| {
            let (bytes, mut at, mut ids) = (text.as_bytes(), 0, Vec::new());
            while at < bytes.len() {
                let len = (1..=longest.min(bytes.len() - at)).rev().find(|&len| by_bytes.contains_key(&bytes[at..at + len])).unwrap_or(1);
                ids.extend(by_bytes.get(&bytes[at..at + len]));
                at += len;
            }
            ids
        };
        Self {
            dims: ModelDims { n_mels: 80, n_layers: 1, n_heads: 1, head_dim: 1, n_audio_ctx: 1500, n_text_ctx: 200, n_vocab: N_VOCAB },
            sentences: SENTENCES.iter().map(|s| tokenize(s)).collect(),
            vocab,
            script: Vec::new(),
            encoded: 0,
            position: 0,
            step_delay: std::time::Duration::ZERO,
        }
    }

    fn timestamp(&self, seconds: f64) -> u32 {
        self.vocab.timestamp_begin + (seconds / 0.02).round() as u32
    }
}

impl Default for SimEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for SimEngine {
    fn dims(&self) -> &ModelDims {
        &self.dims
    }

    fn encode(&mut self, _mel: &[f32]) -> Result<()> {
        // Five sentences fill a window; each closes on the timestamp the next opens with, and
        // the last closes alone so the window is consumed whole.
        self.script.clear();
        for slot in 0..5 {
            let sentence = &self.sentences[(self.encoded * 5 + slot) % self.sentences.len()];
            self.script.push(self.timestamp(slot as f64 * SENTENCE_SECONDS));
            self.script.extend(sentence);
            self.script.push(self.timestamp((slot + 1) as f64 * SENTENCE_SECONDS - 0.5));
        }
        self.encoded += 1;
        self.position = 0;
        // Roughly what the real encoder takes, so progress on screen moves believably.
        std::thread::sleep(std::time::Duration::from_millis(150));
        Ok(())
    }

    fn reset(&mut self) {
        self.position = 0;
    }

    fn position(&self) -> usize {
        self.position
    }

    fn step(&mut self, _token: u32, logits: &mut [f32]) -> Result<()> {
        if self.position >= self.dims.max_steps() {
            return Err(Error::Msg("simulated decoder context is full".into()));
        }
        if !self.step_delay.is_zero() {
            std::thread::sleep(self.step_delay);
        }
        logits.fill(-30.0);
        logits[self.vocab.language_tokens().start as usize] = 5.0;
        logits[self.vocab.no_speech as usize] = -20.0;
        // The prompt is three tokens; the script answers from the third on.
        if self.position >= 2 {
            let next = self.script.get(self.position - 2).copied().unwrap_or(self.vocab.eot);
            logits[next as usize] = 30.0;
        }
        self.position += 1;
        Ok(())
    }
}

/// A scripted stand-in for the speaker network. It takes the pitch of a window — how often
/// the signal crosses zero — for the voice, so tones of different pitch are different
/// speakers and everything else about the speaker pass runs for real: the windows, the
/// grouping, the labels.
#[derive(Default)]
pub struct SimEmbedder {
    /// Added to every window, to stand in for a slow network.
    pub delay: std::time::Duration,
}

impl local_speaker::Embedder for SimEmbedder {
    fn embed(&mut self, pcm: &[f32], print: &mut [f32]) -> local_speaker::Result<()> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let crossings = pcm.windows(2).filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0)).count();
        let voice = (crossings / 600).min(7);
        // Scatter that depends on the audio, so no two windows of a voice are the same print,
        // sized to leave them about as alike as real ones are.
        let mut state = pcm.iter().take(256).fold(crossings as u64 | 1, |hash, sample| (hash ^ sample.to_bits() as u64).wrapping_mul(0x0000_0100_0000_01b3));
        for slot in print.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *slot = ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.52;
        }
        print[voice] += 3.0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_whisper::{join_text, Advance, DecodeOptions, LongForm};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn scripted_sentences_come_out_as_timed_segments() {
        let mut engine = SimEngine::new();
        let audio: Vec<f32> = (0..16_000 * 14).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        let mut long = LongForm::new(engine.dims(), DecodeOptions::default()).unwrap();
        let cancel = AtomicBool::new(false);
        let mut source = &audio[..];
        let mut segments = Vec::new();
        while let Advance::Window(window) = long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).unwrap() {
            segments.extend(window.segments);
        }
        // 14 s of audio: the script's later sentences fall past the end and are clamped to it.
        assert_eq!(segments[0].text, "This is a simulated transcript.");
        assert_eq!((segments[0].start, segments[0].end), (0.0, 5.5));
        assert!(join_text(&segments).contains("No speech model heard this recording."));
        assert!(segments.iter().all(|s| s.end <= 14.0));
        assert_eq!(long.seek(), audio.len() as u64);
    }

    #[test]
    fn tones_of_different_pitch_are_different_scripted_voices() {
        use local_speaker::{cluster, Embedder, Prints, Window, DIM, WINDOW_SAMPLES};
        let tone = |step: f32, phase: usize| -> Vec<f32> { (0..WINDOW_SAMPLES).map(|i| ((i + phase) as f32 * step).sin() * 0.3).collect() };
        let mut prints = Prints::default();
        let mut print = [0f32; DIM];
        // Low, high, low, high: twelve spans of two windows each.
        for span in 0..12u32 {
            for window in 0..2 {
                SimEmbedder::default().embed(&tone(if span % 2 == 0 { 0.05 } else { 0.2 }, (span as usize * 2 + window) * 977), &mut print).unwrap();
                prints.push(Window { span, start: 0, len: WINDOW_SAMPLES as u32 }, &print);
            }
        }
        let labels = cluster(&prints, 12, None);
        assert_eq!(labels.speakers, 2);
        assert_eq!(labels.spans, (0..12).map(|span| Some((span % 2) as u8)).collect::<Vec<_>>());
    }
}
