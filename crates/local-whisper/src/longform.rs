//! Long-form transcription: a 30 s window slid through audio of any length, cut at the model's
//! own timestamps.
//!
//! This is the seek loop of `whisper/transcribe.py`. Each window is decoded with timestamps;
//! where the model closes a segment and opens the next, the audio up to that point is final and
//! the next window starts there — so speech is never split mid-sentence at a 30 s boundary, and
//! a window whose decode ran out of positions simply resumes from its last closed segment.
//!
//! The loop is resumable one window at a time and reads audio through [`AudioSource`], so the
//! same code transcribes a recording while it is still being written and one made last month,
//! without either ever being held in memory whole.

use crate::decode::{DecodeOptions, Decoded, Decoder, Timings};
use crate::engine::{Engine, ModelDims};
use crate::error::Result;
use crate::mel::{MelFrontEnd, CHUNK_SAMPLES, SAMPLE_RATE};
use crate::vocab::Vocab;
use std::sync::atomic::AtomicBool;

/// Samples one timestamp step (20 ms) spans.
const TIMESTAMP_SAMPLES: u64 = SAMPLE_RATE as u64 / 50;
/// A window whose loudest sample is under this (about -56 dBFS) is not run through the model:
/// Whisper invents text for dead air, and a recorder left running produces a lot of it.
const QUIET_PEAK: f32 = 0.0015;
/// A final fragment shorter than this is dropped rather than decoded; the model was never
/// shown clips this short and answers them with a stock phrase.
const MIN_TAIL_SAMPLES: u64 = SAMPLE_RATE as u64 * 3 / 10;
/// Least audio past the seek point worth a preview decode.
const MIN_PREVIEW_SAMPLES: u64 = SAMPLE_RATE as u64;
/// When the model stops early and what is left of the window holds at least this many 100 ms
/// stretches peaking above [`TAIL_PEAK`], the remainder gets a window of its own instead of
/// being skipped. Two seconds' worth: the last syllable of a sentence followed by silence is
/// not a reason to decode silence, which Whisper answers with a stock phrase.
const TAIL_LOUD_BLOCKS: usize = 20;
/// About -40 dBFS: speech at arm's length, well clear of a quiet room.
const TAIL_PEAK: f32 = 0.01;

/// One timestamped stretch of transcript. Times are seconds from the start of the audio.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub avg_logprob: f32,
    pub no_speech_prob: f32,
    pub temperature: f32,
}

/// Random access to mono 16 kHz audio that may still be growing.
pub trait AudioSource {
    /// Copy samples from `start` into `out`, returning how many existed.
    fn read(&mut self, start: u64, out: &mut [f32]) -> Result<usize>;
}

impl AudioSource for &[f32] {
    fn read(&mut self, start: u64, out: &mut [f32]) -> Result<usize> {
        let start = (start as usize).min(self.len());
        let n = out.len().min(self.len() - start);
        out[..n].copy_from_slice(&self[start..start + n]);
        Ok(n)
    }
}

/// What a window turned out to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Speech,
    /// Decoded, and judged silence by the model's own no-speech probability.
    Silence,
    /// Too quiet to be worth decoding.
    Quiet,
}

/// The result of one [`LongForm::advance`] that consumed audio.
#[derive(Clone, Debug)]
pub struct Window {
    pub segments: Vec<Segment>,
    /// Sample range this step made final.
    pub from: u64,
    pub to: u64,
    pub outcome: Outcome,
    /// The language this window was decoded in, when it was decoded.
    pub language: Option<&'static str>,
    pub timings: Timings,
}

#[derive(Clone, Debug)]
pub enum Advance {
    /// Less than a full window is recorded so far and more is coming.
    Waiting,
    Window(Window),
    /// Everything up to the end of the audio is transcribed.
    Finished,
}

/// Transcription state for one recording.
pub struct LongForm {
    front: MelFrontEnd,
    vocab: Vocab,
    opts: DecodeOptions,
    /// First sample not yet covered by a final segment.
    seek: u64,
    /// The language token in force: forced by the options, or detected on the first window
    /// that produced text and kept, so one noisy stretch cannot flip a recording's language.
    language: Option<u32>,
    /// The window now at the seek point is a second look at a tail; it is not given a third.
    rechecking: bool,
    buf: Vec<f32>,
}

impl LongForm {
    pub fn new(dims: &ModelDims, opts: DecodeOptions) -> Result<Self> {
        let vocab = Vocab::multilingual(dims.n_vocab)?;
        let language = opts.language.as_deref().map(|code| vocab.language_token(code)).transpose()?;
        Ok(Self { front: MelFrontEnd::new(dims.n_mels), vocab, opts, seek: 0, language, rechecking: false, buf: vec![0.0; CHUNK_SAMPLES] })
    }

    /// First sample not yet transcribed.
    pub fn seek(&self) -> u64 {
        self.seek
    }

    /// Resume from a seek point a previous run reached.
    pub fn set_seek(&mut self, seek: u64) {
        self.seek = seek;
        self.rechecking = false;
    }

    /// Carry on in the language an earlier pass over this recording settled on. Unlike a
    /// language forced through the options, a pinned one still gives way to a window that is
    /// plainly in another — and where one is forced, that stands and this changes nothing.
    pub fn pin_language(&mut self, code: &str) -> Result<()> {
        let token = self.vocab.language_token(code)?;
        if self.opts.language.is_none() {
            self.language = Some(token);
        }
        Ok(())
    }

    /// The language being transcribed, once forced or detected.
    pub fn language(&self) -> Option<&'static str> {
        self.language.and_then(|token| self.vocab.language_code(token))
    }

    pub fn vocab(&self) -> &Vocab {
        &self.vocab
    }

    /// Transcribe the next window if one is ready. `total` is the number of samples recorded
    /// so far; while `finished` is false only full 30 s windows are taken, since a partial
    /// one would be cut wherever the recording happens to be.
    pub fn advance(
        &mut self,
        engine: &mut dyn Engine,
        audio: &mut dyn AudioSource,
        total: u64,
        finished: bool,
        cancel: &AtomicBool,
    ) -> Result<Advance> {
        let available = total.saturating_sub(self.seek);
        if !finished && available < CHUNK_SAMPLES as u64 {
            return Ok(Advance::Waiting);
        }
        if available < MIN_TAIL_SAMPLES {
            self.seek = self.seek.max(total);
            return Ok(Advance::Finished);
        }
        let from = self.seek;
        let wanted = available.min(CHUNK_SAMPLES as u64) as usize;
        let take = audio.read(from, &mut self.buf[..wanted])?;
        if take == 0 {
            self.seek = total;
            return Ok(Advance::Finished);
        }
        let mut window = Window { segments: Vec::new(), from, to: from + take as u64, outcome: Outcome::Quiet, language: None, timings: Timings::default() };
        let rechecking = std::mem::take(&mut self.rechecking);
        if peak(&self.buf[..take]) < QUIET_PEAK {
            self.seek = window.to;
            return Ok(Advance::Window(window));
        }

        let mel = self.front.compute(&self.buf[..take]);
        let decoded = Decoder::new(&self.vocab, &self.opts, cancel).window(engine, &mel, self.language, from, &mut window.timings)?;
        if is_silence(&decoded, &self.opts) {
            window.outcome = Outcome::Silence;
            self.seek = window.to;
            return Ok(Advance::Window(window));
        }
        window.outcome = Outcome::Speech;
        window.language = self.vocab.language_code(decoded.language);
        let (segments, resume) = cut(&self.vocab, &decoded, from, take, false);
        // Noise the model answered with nothing has no language worth holding the rest of the
        // recording to.
        if !segments.is_empty() {
            self.language.get_or_insert(decoded.language);
        }
        window.segments = segments;
        window.to = match resume {
            // A window that closes no segment past its start would be decoded again unchanged.
            Resume::At(samples) if samples > 0 => (from + samples).min(from + take as u64),
            // The model stopped early. OpenAI's loop takes its word that nothing follows; a
            // recorder would rather look once more than lose speech it went quiet on — after
            // a long pause, a change of speaker, or a change of language.
            Resume::Stopped(samples) if !rechecking && samples > 0 && self.tail_is_loud(samples as usize, take) => {
                self.rechecking = true;
                from + samples
            }
            _ => from + take as u64,
        };
        self.seek = window.to;
        Ok(Advance::Window(window))
    }

    /// Whether the audio from `start` to the end of the window holds enough sound to be worth
    /// decoding on its own.
    fn tail_is_loud(&self, start: usize, take: usize) -> bool {
        let tail = &self.buf[start.min(take)..take];
        tail.chunks(SAMPLE_RATE / 10).filter(|block| peak(block) >= TAIL_PEAK).count() >= TAIL_LOUD_BLOCKS
    }

    /// Decode the audio past the seek point once, greedily, without consuming it: text to show
    /// while a recording has not yet filled the window that will transcribe it for real.
    pub fn preview(&mut self, engine: &mut dyn Engine, audio: &mut dyn AudioSource, total: u64, cancel: &AtomicBool) -> Result<(Vec<Segment>, Timings)> {
        let mut timings = Timings::default();
        let available = total.saturating_sub(self.seek);
        if available < MIN_PREVIEW_SAMPLES {
            return Ok((Vec::new(), timings));
        }
        let wanted = available.min(CHUNK_SAMPLES as u64) as usize;
        let take = audio.read(self.seek, &mut self.buf[..wanted])?;
        if take == 0 || peak(&self.buf[..take]) < QUIET_PEAK {
            return Ok((Vec::new(), timings));
        }
        let mel = self.front.compute(&self.buf[..take]);
        let decoded = Decoder::new(&self.vocab, &self.opts, cancel).window_greedy(engine, &mel, self.language, &mut timings)?;
        if is_silence(&decoded, &self.opts) {
            return Ok((Vec::new(), timings));
        }
        Ok((cut(&self.vocab, &decoded, self.seek, take, true).0, timings))
    }
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0f32, |m, &s| m.max(s.abs()))
}

fn is_silence(decoded: &Decoded, opts: &DecodeOptions) -> bool {
    decoded.no_speech_prob > opts.no_speech_threshold && decoded.avg_logprob <= opts.logprob_threshold
}

/// Where the next window starts, in samples from the start of this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resume {
    /// At the last closed segment: an unfinished one follows it.
    At(u64),
    /// The model closed its last segment here and said nothing more.
    Stopped(u64),
    /// Past the whole window.
    Whole,
}

/// Split one window's tokens into segments and say where the next window should start.
/// `keep_tail` also returns text after the last closed segment, which a final pass leaves for
/// the next window to decode with its ending in view.
fn cut(vocab: &Vocab, decoded: &Decoded, from: u64, take: usize, keep_tail: bool) -> (Vec<Segment>, Resume) {
    let tokens = &decoded.tokens;
    let origin = from as f64 / SAMPLE_RATE as f64;
    let duration = take as f64 / SAMPLE_RATE as f64;
    let is_ts = |i: usize| vocab.is_timestamp(tokens[i]);
    let mut segments = Vec::new();
    let mut push = |start: f64, end: f64, piece: &[u32]| {
        let text = vocab.decode(piece).trim().to_string();
        if text.is_empty() {
            return;
        }
        let start = start.clamp(0.0, duration);
        segments.push(Segment {
            start: origin + start,
            end: origin + end.clamp(start, duration),
            text,
            avg_logprob: decoded.avg_logprob,
            no_speech_prob: decoded.no_speech_prob,
            temperature: decoded.temperature,
        });
    };

    // A closing timestamp immediately followed by an opening one is a segment boundary.
    let boundaries: Vec<usize> = (1..tokens.len()).filter(|&i| is_ts(i - 1) && is_ts(i)).collect();
    let ends_on_timestamp = tokens.len() >= 2 && !is_ts(tokens.len() - 2) && is_ts(tokens.len() - 1);
    if boundaries.is_empty() {
        // One segment, or none closed: the whole window is consumed. A timestamp after the
        // first token closes it; the opening one alone says nothing about where it ends.
        let closing = (1..tokens.len()).rev().find(|&i| is_ts(i));
        let end = closing.map_or(duration, |i| vocab.timestamp_seconds(tokens[i]));
        let start = tokens.first().filter(|&&t| vocab.is_timestamp(t)).map_or(0.0, |&t| vocab.timestamp_seconds(t));
        push(start.min(end), end, tokens);
        let resume = match closing {
            Some(i) if i == tokens.len() - 1 => Resume::Stopped(samples_at(vocab, tokens[i])),
            _ => Resume::Whole,
        };
        return (segments, resume);
    }

    let mut cuts = boundaries;
    if ends_on_timestamp {
        cuts.push(tokens.len());
    }
    let mut last = 0;
    for &at in &cuts {
        let piece = &tokens[last..at];
        push(vocab.timestamp_seconds(piece[0]), vocab.timestamp_seconds(piece[piece.len() - 1]), piece);
        last = at;
    }
    if ends_on_timestamp {
        return (segments, Resume::Stopped(samples_at(vocab, tokens[tokens.len() - 1])));
    }
    if keep_tail && last < tokens.len() {
        push(vocab.timestamp_seconds(tokens[last]), duration, &tokens[last..]);
    }
    (segments, Resume::At(samples_at(vocab, tokens[last - 1])))
}

/// Samples into the window a timestamp token marks.
fn samples_at(vocab: &Vocab, token: u32) -> u64 {
    token.saturating_sub(vocab.timestamp_begin) as u64 * TIMESTAMP_SAMPLES
}

/// Join segments into running text.
pub fn join_text(segments: &[Segment]) -> String {
    let mut out = String::new();
    for segment in segments {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&segment.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::sync::atomic::Ordering;

    const N_VOCAB: usize = 51865;

    fn dims() -> ModelDims {
        ModelDims { n_mels: 80, n_layers: 1, n_heads: 1, head_dim: 1, n_audio_ctx: 1500, n_text_ctx: 200, n_vocab: N_VOCAB }
    }

    /// Replays a fixed token script per encoded window, as logits that survive the rules.
    struct Scripted {
        dims: ModelDims,
        windows: Vec<Vec<u32>>,
        encoded: usize,
        position: usize,
        no_speech: f32,
        /// Per window: the language the audio "is in" and how far its logit leads English's.
        languages: Vec<(&'static str, f32)>,
    }

    impl Scripted {
        fn new(windows: Vec<Vec<u32>>) -> Self {
            Self { dims: dims(), windows, encoded: 0, position: 0, no_speech: -20.0, languages: Vec::new() }
        }
    }

    impl Engine for Scripted {
        fn dims(&self) -> &ModelDims {
            &self.dims
        }

        fn encode(&mut self, _mel: &[f32]) -> Result<()> {
            self.encoded += 1;
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
                return Err(Error::Msg("out of positions".into()));
            }
            let v = Vocab::multilingual(N_VOCAB).unwrap();
            logits.fill(-30.0);
            // Position 0 answers start-of-transcript: language and no-speech are read here.
            logits[v.language_token("en").unwrap() as usize] = 5.0;
            if let Some((code, lead)) = self.languages.get(self.encoded - 1) {
                logits[v.language_token(code).unwrap() as usize] = 5.0 + lead;
            }
            logits[v.no_speech as usize] = self.no_speech;
            let script = &self.windows[(self.encoded - 1).min(self.windows.len() - 1)];
            // The prompt is three tokens; the script starts with the answer to the third.
            if self.position >= 2 {
                let next = script.get(self.position - 2).copied().unwrap_or(v.eot);
                logits[next as usize] = 30.0;
            }
            self.position += 1;
            Ok(())
        }
    }

    fn ts(seconds: f64) -> u32 {
        Vocab::multilingual(N_VOCAB).unwrap().timestamp_begin + (seconds / 0.02).round() as u32
    }

    fn tone(samples: usize) -> Vec<f32> {
        (0..samples).map(|i| (i as f32 * 0.05).sin() * 0.3).collect()
    }

    fn run(engine: &mut Scripted, audio: &[f32]) -> (Vec<Segment>, Vec<(u64, u64)>) {
        let mut long = LongForm::new(&dims(), DecodeOptions::default()).unwrap();
        let cancel = AtomicBool::new(false);
        let (mut segments, mut ranges) = (Vec::new(), Vec::new());
        let mut source = audio;
        loop {
            match long.advance(engine, &mut source, audio.len() as u64, true, &cancel).unwrap() {
                Advance::Window(w) => {
                    ranges.push((w.from, w.to));
                    segments.extend(w.segments);
                }
                Advance::Finished => break,
                Advance::Waiting => panic!("finished audio never waits"),
            }
        }
        (segments, ranges)
    }

    // " And so my fellow Americans" / " ask not" in whisper-base ids.
    const AND_SO: [u32; 5] = [400, 370, 452, 7177, 6280];
    const ASK_NOT: [u32; 2] = [1029, 406];

    #[test]
    fn seeks_to_the_last_closed_segment() {
        // Two closed segments, then one left open at the window's end.
        let mut first = vec![ts(0.0)];
        first.extend(AND_SO);
        first.extend([ts(4.0), ts(4.0)]);
        first.extend(ASK_NOT);
        first.extend([ts(21.5), ts(21.5)]);
        first.extend(AND_SO);
        let mut second = vec![ts(0.0)];
        second.extend(AND_SO);
        second.push(ts(18.0));
        let mut engine = Scripted::new(vec![first, second]);

        let audio = tone(40 * SAMPLE_RATE);
        let (segments, ranges) = run(&mut engine, &audio);
        // The open third segment is dropped and re-read from 21.5 s.
        assert_eq!(ranges, vec![(0, 344_000), (344_000, 640_000)]);
        let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["And so my fellow Americans", "ask not", "And so my fellow Americans"]);
        assert_eq!((segments[0].start, segments[0].end), (0.0, 4.0));
        assert_eq!((segments[1].start, segments[1].end), (4.0, 21.5));
        // The second window's times are offset by where it started.
        assert_eq!((segments[2].start, segments[2].end), (21.5, 39.5));
    }

    /// The scripted model says two things and stops at 5 s, whatever the audio holds.
    fn stops_at_five_seconds() -> Vec<u32> {
        let mut script = vec![ts(0.0)];
        script.extend(AND_SO);
        script.extend([ts(3.0), ts(3.0)]);
        script.extend(ASK_NOT);
        script.push(ts(5.0));
        script
    }

    #[test]
    fn a_window_the_model_finishes_early_over_quiet_is_consumed_whole() {
        let mut engine = Scripted::new(vec![stops_at_five_seconds()]);
        // Speech-level sound for five seconds, then a quiet room.
        let mut audio = tone(5 * SAMPLE_RATE);
        audio.extend(std::iter::repeat_n(0.003, 7 * SAMPLE_RATE));
        let (segments, ranges) = run(&mut engine, &audio);
        assert_eq!(ranges, vec![(0, 192_000)]);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1].end, 5.0);
        assert_eq!(join_text(&segments), "And so my fellow Americans ask not");
        assert_eq!(engine.encoded, 1);
    }

    #[test]
    fn a_loud_tail_the_model_skipped_gets_one_more_look() {
        // Twelve seconds of sound; the model stops at five on the first pass, and at five
        // again when shown the rest.
        let mut engine = Scripted::new(vec![stops_at_five_seconds(), stops_at_five_seconds()]);
        let audio = tone(12 * SAMPLE_RATE);
        let (segments, ranges) = run(&mut engine, &audio);
        // The second look starts where the model stopped — and is not followed by a third.
        assert_eq!(ranges, vec![(0, 80_000), (80_000, 192_000)]);
        assert_eq!(engine.encoded, 2);
        assert_eq!(segments.len(), 4);
        assert_eq!((segments[2].start, segments[2].end), (5.0, 8.0));
        assert_eq!((segments[3].start, segments[3].end), (8.0, 10.0));
    }

    #[test]
    fn a_trailing_syllable_before_silence_is_not_worth_a_second_pass() {
        let mut engine = Scripted::new(vec![stops_at_five_seconds()]);
        // Sound runs a second past where the model stopped, then four seconds of quiet.
        let mut audio = tone(6 * SAMPLE_RATE);
        audio.extend(std::iter::repeat_n(0.002, 4 * SAMPLE_RATE));
        let (_, ranges) = run(&mut engine, &audio);
        assert_eq!(ranges, vec![(0, 160_000)]);
        assert_eq!(engine.encoded, 1);
    }

    #[test]
    fn an_unclosed_window_becomes_one_segment_to_its_end() {
        let mut script = vec![ts(0.5)];
        script.extend(AND_SO);
        let mut engine = Scripted::new(vec![script]);
        let audio = tone(8 * SAMPLE_RATE);
        let (segments, ranges) = run(&mut engine, &audio);
        assert_eq!(ranges, vec![(0, 128_000)]);
        assert_eq!((segments[0].start, segments[0].end), (0.5, 8.0));
    }

    #[test]
    fn timestamps_past_the_audio_are_clamped() {
        let mut script = vec![ts(0.0)];
        script.extend(AND_SO);
        script.push(ts(29.0));
        let mut engine = Scripted::new(vec![script]);
        let audio = tone(5 * SAMPLE_RATE);
        let (segments, _) = run(&mut engine, &audio);
        assert_eq!(segments[0].end, 5.0);
    }

    #[test]
    fn dead_air_never_reaches_the_model() {
        let mut engine = Scripted::new(vec![vec![]]);
        let audio = vec![0.0004f32; 70 * SAMPLE_RATE];
        let (segments, ranges) = run(&mut engine, &audio);
        assert!(segments.is_empty());
        assert_eq!(engine.encoded, 0);
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[2].1, audio.len() as u64);
    }

    #[test]
    fn a_window_the_model_calls_silent_is_skipped() {
        let mut script = vec![ts(0.0)];
        script.extend(AND_SO);
        script.push(ts(2.0));
        let mut engine = Scripted::new(vec![script]);
        // No-speech dominates the first position, and the text is scored as a guess.
        engine.no_speech = 30.0;
        let mut long = LongForm::new(&dims(), DecodeOptions { logprob_threshold: 1.0, temperatures: vec![0.0], ..Default::default() }).unwrap();
        let audio = tone(10 * SAMPLE_RATE);
        let mut source = &audio[..];
        let cancel = AtomicBool::new(false);
        match long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).unwrap() {
            Advance::Window(w) => {
                assert_eq!(w.outcome, Outcome::Silence);
                assert!(w.segments.is_empty());
                assert_eq!(w.to, audio.len() as u64);
            }
            other => panic!("expected a window, got {other:?}"),
        }
    }

    #[test]
    fn a_live_recording_waits_for_a_full_window() {
        let mut script = vec![ts(0.0)];
        script.extend(AND_SO);
        script.extend([ts(20.0), ts(20.0)]);
        script.extend(ASK_NOT);
        let mut engine = Scripted::new(vec![script]);
        let mut long = LongForm::new(&dims(), DecodeOptions::default()).unwrap();
        let audio = tone(45 * SAMPLE_RATE);
        let mut source = &audio[..];
        let cancel = AtomicBool::new(false);
        // 29 s recorded: not yet.
        assert!(matches!(long.advance(&mut engine, &mut source, 29 * SAMPLE_RATE as u64, false, &cancel).unwrap(), Advance::Waiting));
        // 31 s: one window, final up to 20 s.
        match long.advance(&mut engine, &mut source, 31 * SAMPLE_RATE as u64, false, &cancel).unwrap() {
            Advance::Window(w) => assert_eq!((w.from, w.to), (0, 320_000)),
            other => panic!("expected a window, got {other:?}"),
        }
        // 45 s recorded, seek at 20 s: 25 s is not a full window while recording continues.
        assert!(matches!(long.advance(&mut engine, &mut source, audio.len() as u64, false, &cancel).unwrap(), Advance::Waiting));
        assert_eq!(long.seek(), 320_000);
        assert_eq!(long.language(), Some("en"));
    }

    #[test]
    fn preview_shows_unclosed_text_without_consuming_it() {
        let mut script = vec![ts(0.0)];
        script.extend(AND_SO);
        script.extend([ts(3.0), ts(3.0)]);
        script.extend(ASK_NOT);
        let mut engine = Scripted::new(vec![script]);
        let mut long = LongForm::new(&dims(), DecodeOptions::default()).unwrap();
        let audio = tone(6 * SAMPLE_RATE);
        let mut source = &audio[..];
        let cancel = AtomicBool::new(false);
        let (segments, _) = long.preview(&mut engine, &mut source, audio.len() as u64, &cancel).unwrap();
        assert_eq!(join_text(&segments), "And so my fellow Americans ask not");
        assert_eq!(segments[1].end, 6.0);
        assert_eq!(long.seek(), 0);
    }

    #[test]
    fn a_short_tail_is_dropped_and_cancel_stops_a_decode() {
        let mut engine = Scripted::new(vec![vec![]]);
        let mut long = LongForm::new(&dims(), DecodeOptions::default()).unwrap();
        let audio = tone(SAMPLE_RATE / 5);
        let mut source = &audio[..];
        let cancel = AtomicBool::new(false);
        assert!(matches!(long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).unwrap(), Advance::Finished));
        assert_eq!(engine.encoded, 0);

        let audio = tone(5 * SAMPLE_RATE);
        let mut source = &audio[..];
        let mut long = LongForm::new(&dims(), DecodeOptions::default()).unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel), Err(Error::Cancelled)));
    }

    /// Decode `languages.len()` windows of sound and report the language each was decoded in.
    fn window_languages(languages: Vec<(&'static str, f32)>, opts: DecodeOptions) -> Vec<Option<&'static str>> {
        let mut script = vec![ts(0.0)];
        script.extend(AND_SO);
        script.push(ts(29.98));
        let mut engine = Scripted::new(vec![script]);
        let audio = tone(30 * SAMPLE_RATE * languages.len());
        engine.languages = languages;
        let mut long = LongForm::new(&dims(), opts).unwrap();
        let cancel = AtomicBool::new(false);
        let (mut source, mut seen) = (&audio[..], Vec::new());
        while let Advance::Window(w) = long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).unwrap() {
            seen.push(w.language);
        }
        assert_eq!(long.language(), seen[0], "the recording's language is its first window's");
        seen
    }

    #[test]
    fn the_language_follows_speech_that_plainly_switches_and_ignores_a_doubtful_window() {
        // English, then a window the model is sure is Spanish, then one that only leans German.
        let seen = window_languages(vec![("en", 0.0), ("es", 9.0), ("de", 1.0)], DecodeOptions::default());
        assert_eq!(seen, [Some("en"), Some("es"), Some("en")]);
    }

    #[test]
    fn a_resumed_recording_keeps_its_language_but_can_still_switch() {
        let mut long = LongForm::new(&dims(), DecodeOptions::default()).unwrap();
        long.pin_language("de").unwrap();
        assert_eq!(long.language(), Some("de"));
        assert!(matches!(long.pin_language("xx"), Err(Error::UnknownLanguage(_))));

        // Pinned German: a window that only leans French keeps it, one that is surely Spanish
        // does not.
        let mut script = vec![ts(0.0)];
        script.extend(AND_SO);
        script.push(ts(29.98));
        let mut engine = Scripted::new(vec![script]);
        engine.languages = vec![("fr", 1.0), ("es", 9.0)];
        let audio = tone(60 * SAMPLE_RATE);
        let cancel = AtomicBool::new(false);
        let (mut source, mut seen) = (&audio[..], Vec::new());
        while let Advance::Window(w) = long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).unwrap() {
            seen.push(w.language);
        }
        assert_eq!(seen, [Some("de"), Some("es")]);
        assert_eq!(long.language(), Some("de"), "one window in another language does not re-pin the recording");
    }

    #[test]
    fn a_window_without_text_does_not_settle_the_language() {
        // Half a minute of noise the model scores as Welsh and answers with nothing, then
        // English speech.
        let mut speech = vec![ts(0.0)];
        speech.extend(AND_SO);
        speech.push(ts(29.98));
        let mut engine = Scripted::new(vec![vec![ts(0.0)], speech]);
        engine.languages = vec![("cy", 2.0), ("en", 0.0)];
        let audio = tone(60 * SAMPLE_RATE);
        let mut long = LongForm::new(&dims(), DecodeOptions::default()).unwrap();
        let cancel = AtomicBool::new(false);
        let (mut source, mut seen) = (&audio[..], Vec::new());
        while let Advance::Window(w) = long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).unwrap() {
            seen.push((w.segments.len(), w.language));
        }
        assert_eq!(seen, [(0, Some("cy")), (1, Some("en"))]);
        assert_eq!(long.language(), Some("en"));
    }

    #[test]
    fn a_forced_language_never_switches() {
        let opts = DecodeOptions { language: Some("en".into()), ..Default::default() };
        let seen = window_languages(vec![("en", 0.0), ("es", 9.0)], opts);
        assert_eq!(seen, [Some("en"), Some("en")]);
    }

    #[test]
    fn a_forced_language_is_validated_up_front() {
        let opts = DecodeOptions { language: Some("xx".into()), ..Default::default() };
        assert!(matches!(LongForm::new(&dims(), opts), Err(Error::UnknownLanguage(_))));
        let opts = DecodeOptions { language: Some("de".into()), ..Default::default() };
        let mut long = LongForm::new(&dims(), opts).unwrap();
        assert_eq!(long.language(), Some("de"));
        // What an earlier pass detected does not displace it.
        long.pin_language("cy").unwrap();
        assert_eq!(long.language(), Some("de"));
    }
}
