//! Decoding one 30 s window: the prompt, OpenAI's logit rules, and the temperature fallback.
//!
//! This is `whisper/decoding.py` on a one-token-per-run decoder. The rules matter more than
//! they look: without the timestamp pairing rules the long-form seek in [`crate::longform`]
//! has nothing to cut on, and without the fallback a window that falls into a repetition loop
//! is transcribed as that loop.

use crate::engine::Engine;
use crate::error::{Error, Result};
use crate::vocab::Vocab;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Task {
    Transcribe,
    /// Whisper's built-in X-to-English translation.
    Translate,
}

#[derive(Clone, Debug)]
pub struct DecodeOptions {
    /// Language code to force, or `None` to detect it from the audio.
    pub language: Option<String>,
    pub task: Task,
    /// Tried in order until a window passes the quality checks. Zero is greedy.
    pub temperatures: Vec<f32>,
    /// Above this the text is compressing like a loop, and the window is retried.
    pub compression_ratio_threshold: f32,
    /// Below this average token log-probability the window is retried.
    pub logprob_threshold: f32,
    /// Above this no-speech probability a low-confidence window is treated as silence.
    pub no_speech_threshold: f32,
    /// Latest first timestamp allowed, in 20 ms steps; stops a window opening far from zero.
    pub max_initial_timestamp_index: u32,
    /// With no language forced, how sure the model must be that a window is in a different
    /// language than the recording's to decode that window in it.
    pub language_switch_threshold: f32,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            language: None,
            task: Task::Transcribe,
            temperatures: vec![0.0, 0.2, 0.4, 0.6, 0.8, 1.0],
            compression_ratio_threshold: 2.4,
            logprob_threshold: -1.0,
            no_speech_threshold: 0.6,
            max_initial_timestamp_index: 50,
            language_switch_threshold: 0.9,
        }
    }
}

/// Where a window's time went.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    pub encode: Duration,
    pub decode: Duration,
    /// Decoder runs, every fallback attempt included.
    pub steps: u32,
}

impl Timings {
    pub fn add(&mut self, other: &Timings) {
        self.encode += other.encode;
        self.decode += other.decode;
        self.steps += other.steps;
    }

    pub fn ms_per_step(&self) -> f32 {
        if self.steps == 0 { 0.0 } else { self.decode.as_secs_f32() * 1000.0 / self.steps as f32 }
    }
}

/// One decoded window.
#[derive(Clone, Debug)]
pub struct Decoded {
    /// Sampled tokens after the prompt, timestamps included, end-of-text excluded.
    pub tokens: Vec<u32>,
    /// The language token the window was decoded under.
    pub language: u32,
    pub avg_logprob: f32,
    pub no_speech_prob: f32,
    pub compression_ratio: f32,
    pub temperature: f32,
}

fn log_softmax(logits: &[f32], out: &mut Vec<f32>) {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let sum: f64 = logits.iter().map(|&l| ((l - max) as f64).exp()).sum();
    let log_z = max as f64 + sum.ln();
    out.clear();
    out.extend(logits.iter().map(|&l| (l as f64 - log_z) as f32));
}

fn argmax(values: &[f32]) -> usize {
    let mut best = 0;
    for (i, &v) in values.iter().enumerate() {
        if v > values[best] {
            best = i;
        }
    }
    best
}

/// Text bytes over their zlib size; repetition compresses, speech does not.
pub fn compression_ratio(text: &str) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    text.len() as f32 / miniz_oxide::deflate::compress_to_vec_zlib(text.as_bytes(), 6).len() as f32
}

/// xorshift64*: sampling at a fallback temperature needs to be different per attempt, not good.
struct Rng(u64);

impl Rng {
    /// Seeds one apart must not share a stream, which a plain `seed | 1` would give every
    /// other pair of them; splitmix64 spreads them first.
    fn seeded(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        // xorshift has one bad state, zero.
        Self((z ^ (z >> 31)) | 1)
    }

    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// OpenAI's SuppressTokens, SuppressBlank and ApplyTimestampRules, in that order, for the
/// position after `sampled`. `logprobs` is scratch.
fn apply_rules(logits: &mut [f32], logprobs: &mut Vec<f32>, vocab: &Vocab, suppressed: &[u32], sampled: &[u32], opts: &DecodeOptions) {
    const OFF: f32 = f32::NEG_INFINITY;
    let (eot, begin) = (vocab.eot as usize, vocab.timestamp_begin as usize);
    for &id in suppressed {
        logits[id as usize] = OFF;
    }
    if sampled.is_empty() {
        for id in vocab.suppressed_at_start() {
            logits[id as usize] = OFF;
        }
    }
    logits[vocab.no_timestamps as usize] = OFF;

    // Timestamps come in pairs — one closing a segment, one opening the next — except
    // directly before end-of-text.
    let last_was_timestamp = sampled.last().is_some_and(|&t| vocab.is_timestamp(t));
    let penultimate_was_timestamp = sampled.len() < 2 || vocab.is_timestamp(sampled[sampled.len() - 2]);
    if last_was_timestamp {
        if penultimate_was_timestamp {
            logits[begin..].fill(OFF);
        } else {
            logits[..eot].fill(OFF);
        }
    }
    // And never run backwards. A segment may close on the timestamp that opened it only when
    // that opening is not itself the close of the previous one.
    if let Some(&last) = sampled.iter().rev().find(|&&t| vocab.is_timestamp(t)) {
        let floor = if last_was_timestamp && !penultimate_was_timestamp { last as usize } else { last as usize + 1 };
        let floor = floor.min(logits.len());
        logits[begin..floor].fill(OFF);
    }
    if sampled.is_empty() {
        logits[..begin].fill(OFF);
        let latest = begin + opts.max_initial_timestamp_index as usize + 1;
        if latest < logits.len() {
            logits[latest..].fill(OFF);
        }
    }

    // When the timestamps together outweigh the likeliest text token, it is a timestamp.
    log_softmax(logits, logprobs);
    let peak = logprobs[begin..].iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if peak > f32::NEG_INFINITY {
        let mass = peak as f64 + logprobs[begin..].iter().map(|&l| ((l - peak) as f64).exp()).sum::<f64>().ln();
        let best_text = logprobs[..begin].iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if mass as f32 > best_text {
            logits[..begin].fill(OFF);
        }
    }
}

/// The per-recording decoding state: vocabulary, options and the cancel flag every run checks.
pub struct Decoder<'a> {
    vocab: &'a Vocab,
    opts: &'a DecodeOptions,
    cancel: &'a AtomicBool,
    suppressed: Vec<u32>,
}

impl<'a> Decoder<'a> {
    pub fn new(vocab: &'a Vocab, opts: &'a DecodeOptions, cancel: &'a AtomicBool) -> Self {
        Self { vocab, opts, cancel, suppressed: vocab.suppressed() }
    }

    /// Decode the window the engine last encoded, once, at `temperature`.
    fn once(&self, engine: &mut dyn Engine, language: Option<u32>, temperature: f32, seed: u64, timings: &mut Timings) -> Result<Decoded> {
        let (vocab, opts, cancel, suppressed) = (self.vocab, self.opts, self.cancel, &self.suppressed);
        let dims = engine.dims().clone();
        let mut logits = vec![0f32; dims.n_vocab];
        let mut logprobs = Vec::with_capacity(dims.n_vocab);
        let mut rng = Rng::seeded(seed);
        let started = Instant::now();
        let mut steps = 0u32;
        let mut step = |engine: &mut dyn Engine, token: u32, logits: &mut [f32]| -> Result<()> {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            steps += 1;
            engine.step(token, logits)
        };

        engine.reset();
        step(engine, vocab.sot, &mut logits)?;
        // Both are read off the position after start-of-transcript, before any rule touches it.
        log_softmax(&logits, &mut logprobs);
        let no_speech_prob = logprobs[vocab.no_speech as usize].exp();
        let range = vocab.language_tokens();
        let languages = &logits[range.start as usize..range.end as usize];
        let detected = range.start + argmax(languages) as u32;
        let language = match language {
            None => detected,
            // A recording's language is settled by its first words, so a mumbled window cannot
            // flip it — but speech that is plainly in another language is decoded as that, or
            // the model, told it is hearing the first one, skips it.
            Some(pinned) if opts.language.is_none() && detected != pinned => {
                let peak = languages.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let confidence = 1.0 / languages.iter().map(|&l| ((l - peak) as f64).exp()).sum::<f64>();
                if confidence as f32 >= opts.language_switch_threshold { detected } else { pinned }
            }
            Some(pinned) => pinned,
        };
        let task = if opts.task == Task::Translate { vocab.translate } else { vocab.transcribe };
        step(engine, language, &mut logits)?;
        step(engine, task, &mut logits)?;

        let mut tokens: Vec<u32> = Vec::new();
        let mut sum_logprob = 0f64;
        loop {
            apply_rules(&mut logits, &mut logprobs, vocab, suppressed, &tokens, opts);
            log_softmax(&logits, &mut logprobs);
            let next = if temperature > 0.0 {
                // logprobs / T, renormalized, is the tempered distribution; sample its CDF.
                let peak = logprobs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let weight = |l: f32| (((l - peak) / temperature) as f64).exp();
                let total: f64 = logprobs.iter().map(|&l| weight(l)).sum();
                let mut target = rng.unit() * total;
                let mut pick = argmax(&logprobs);
                for (i, &l) in logprobs.iter().enumerate() {
                    target -= weight(l);
                    if target <= 0.0 {
                        pick = i;
                        break;
                    }
                }
                pick as u32
            } else {
                argmax(&logprobs) as u32
            };
            sum_logprob += logprobs[next as usize] as f64;
            if next == vocab.eot {
                break;
            }
            tokens.push(next);
            // Out of positions: the window ends mid-thought and the seek re-reads the rest.
            if engine.position() >= dims.max_steps() {
                break;
            }
            step(engine, next, &mut logits)?;
        }
        timings.decode += started.elapsed();
        timings.steps += steps;

        let text = vocab.decode(&tokens);
        Ok(Decoded {
            avg_logprob: (sum_logprob / (tokens.len() + 1) as f64) as f32,
            compression_ratio: compression_ratio(&text),
            tokens,
            language,
            no_speech_prob,
            temperature,
        })
    }

    /// Encode `mel` and decode it, retrying at rising temperatures while the result looks like
    /// a repetition loop or a guess. `language` pins the language token; `None` detects it.
    pub fn window(&self, engine: &mut dyn Engine, mel: &[f32], language: Option<u32>, seed: u64, timings: &mut Timings) -> Result<Decoded> {
        let opts = self.opts;
        self.not_cancelled()?;
        let started = Instant::now();
        engine.encode(mel)?;
        timings.encode += started.elapsed();

        let temperatures: &[f32] = if opts.temperatures.is_empty() { &[0.0] } else { &opts.temperatures };
        let mut result = None;
        for (attempt, &temperature) in temperatures.iter().enumerate() {
            let decoded = self.once(engine, language, temperature, seed.wrapping_add(attempt as u64), timings)?;
            let repetitive = decoded.compression_ratio > opts.compression_ratio_threshold;
            let unsure = decoded.avg_logprob < opts.logprob_threshold;
            // Unsure about a window the model also calls silent is silence, not a bad decode.
            let silent = decoded.no_speech_prob > opts.no_speech_threshold && unsure;
            let acceptable = silent || !(repetitive || unsure);
            result = Some(decoded);
            if acceptable {
                break;
            }
        }
        // The ladder is never empty, so the loop ran at least once.
        result.ok_or_else(|| Error::Msg("no decode attempt ran".into()))
    }

    /// Encode `mel` and decode it greedily, once, with no quality retries: a preview of audio
    /// that will be decoded again properly once its window is complete.
    pub fn window_greedy(&self, engine: &mut dyn Engine, mel: &[f32], language: Option<u32>, timings: &mut Timings) -> Result<Decoded> {
        self.not_cancelled()?;
        let started = Instant::now();
        engine.encode(mel)?;
        timings.encode += started.elapsed();
        self.once(engine, language, 0.0, 0, timings)
    }

    /// The encoder is the one long run that cannot be interrupted; do not start it for a
    /// window nobody is waiting for any more.
    fn not_cancelled(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) { Err(Error::Cancelled) } else { Ok(()) }
    }

    pub fn vocab(&self) -> &'a Vocab {
        self.vocab
    }

    pub fn options(&self) -> &'a DecodeOptions {
        self.opts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vocab {
        Vocab::multilingual(51865).unwrap()
    }

    /// Run the rules on flat logits and return what is still allowed.
    fn allowed(v: &Vocab, sampled: &[u32]) -> Vec<f32> {
        let mut logits = vec![0f32; v.n_vocab()];
        // End-of-text well ahead, so the timestamp-mass rule leaves the pairing rules visible.
        logits[v.eot as usize] = 20.0;
        let mut scratch = Vec::new();
        apply_rules(&mut logits, &mut scratch, v, &v.suppressed(), sampled, &DecodeOptions::default());
        logits
    }

    #[test]
    fn a_window_opens_on_an_early_timestamp() {
        let v = vocab();
        let logits = allowed(&v, &[]);
        let begin = v.timestamp_begin as usize;
        assert!(logits[..begin].iter().all(|l| l.is_infinite()));
        // 0.00 through 1.00 s, nothing later.
        assert!(logits[begin..=begin + 50].iter().all(|l| l.is_finite()));
        assert!(logits[begin + 51..].iter().all(|l| l.is_infinite()));
    }

    #[test]
    fn text_follows_an_opening_timestamp() {
        let v = vocab();
        let begin = v.timestamp_begin;
        let logits = allowed(&v, &[begin]);
        // Flat logits put the timestamp mass above any one text token, but every timestamp is
        // barred after a lone opening one, so text survives.
        assert!(logits[begin as usize..].iter().all(|l| l.is_infinite()));
        assert!(logits[400].is_finite());
        assert!(logits[v.no_timestamps as usize].is_infinite());
        assert!(logits[v.sot as usize].is_infinite());
    }

    #[test]
    fn after_text_a_timestamp_cannot_go_back() {
        let v = vocab();
        let begin = v.timestamp_begin;
        let logits = allowed(&v, &[begin + 10, 400, 370]);
        assert!(logits[(begin + 10) as usize].is_infinite());
        assert!(logits[(begin + 11) as usize].is_finite());
    }

    #[test]
    fn a_closing_timestamp_is_followed_by_a_timestamp_or_the_end() {
        let v = vocab();
        let begin = v.timestamp_begin;
        let logits = allowed(&v, &[begin, 400, begin + 100]);
        assert!(logits[..v.eot as usize].iter().all(|l| l.is_infinite()));
        assert!(logits[v.eot as usize].is_finite());
        // The next segment may open exactly where this one closed.
        assert!(logits[(begin + 100) as usize].is_finite());
        assert!(logits[(begin + 99) as usize].is_infinite());
    }

    #[test]
    fn timestamp_mass_beats_a_slightly_likelier_word() {
        let v = vocab();
        let begin = v.timestamp_begin as usize;
        let mut logits = vec![-20f32; v.n_vocab()];
        logits[400] = 1.0;
        // Each timestamp is individually less likely than the word; together they are not.
        logits[begin + 100..begin + 120].fill(0.5);
        let mut scratch = Vec::new();
        apply_rules(&mut logits, &mut scratch, &v, &v.suppressed(), &[v.timestamp_begin + 10, 370], &DecodeOptions::default());
        assert!(logits[400].is_infinite());
        assert!(logits[begin + 100].is_finite());
    }

    #[test]
    fn compression_ratio_separates_loops_from_speech() {
        let speech = "And so my fellow Americans, ask not what your country can do for you, ask what you can do for your country.";
        let looped = "Thank you. ".repeat(40);
        assert!(compression_ratio(speech) < 2.4);
        assert!(compression_ratio(&looped) > 2.4);
        assert_eq!(compression_ratio(""), 0.0);
    }

    #[test]
    fn rng_is_uniform_enough_and_seed_dependent() {
        let mut a = Rng::seeded(1);
        let mean: f64 = (0..10_000).map(|_| a.unit()).sum::<f64>() / 10_000.0;
        assert!((mean - 0.5).abs() < 0.02, "mean {mean}");
        // A window's fallback attempts are seeded one apart from an even sample offset; each
        // has to draw differently, or a retry replays the one before it.
        let firsts: Vec<u64> = (0..64u64).map(|attempt| Rng::seeded(480_000 + attempt).unit().to_bits()).collect();
        let distinct: std::collections::HashSet<&u64> = firsts.iter().collect();
        assert_eq!(distinct.len(), firsts.len());
    }
}
