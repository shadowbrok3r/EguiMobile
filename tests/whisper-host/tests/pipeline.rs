//! The whole local-whisper pipeline against real weights, on the host.
//!
//! Needs the ONNX export from `scripts/whisper-export/reference.py export`; set
//! `WHISPER_ONNX_DIR` to a model directory, or leave it unset to use whisper-base under
//! `~/Documents/Ai/whisper/onnx`. Without one the tests pass vacuously and say so.

use local_whisper::{join_text, Advance, DecodeOptions, Engine, LongForm, Segment, Vocab, SAMPLE_RATE};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use whisper_host::OnnxEngine;

const JFK: &str = "And so my fellow Americans, ask not what your country can do for you, ask what you can do for your country.";

fn model_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("WHISPER_ONNX_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Documents/Ai/whisper/onnx/whisper-base")
    });
    if dir.join("encoder.onnx").exists() {
        Some(dir)
    } else {
        eprintln!("skipping: no ONNX export at {}", dir.display());
        None
    }
}

fn jfk() -> Vec<f32> {
    include_bytes!("../../../crates/local-whisper/tests/fixtures/jfk.s16").chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect()
}

fn transcribe(engine: &mut OnnxEngine, audio: &[f32], opts: DecodeOptions) -> (Vec<Segment>, Option<&'static str>, usize) {
    let mut long = LongForm::new(engine.dims(), opts).unwrap();
    let cancel = AtomicBool::new(false);
    let (mut source, mut segments, mut windows) = (audio, Vec::new(), 0);
    while let Advance::Window(window) = long.advance(engine, &mut source, audio.len() as u64, true, &cancel).unwrap() {
        segments.extend(window.segments);
        windows += 1;
    }
    assert_eq!(long.seek(), audio.len() as u64);
    (segments, long.language(), windows)
}

/// Lowercase letters and spaces only, so punctuation and casing choices do not fail a match.
fn plain(text: &str) -> String {
    text.to_lowercase().chars().filter(|c| c.is_alphabetic() || c.is_whitespace()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_engine_reproduces_qualcomms_own_greedy_loop() {
    let Some(dir) = model_dir() else { return };
    let mut engine = OnnxEngine::load(&dir).unwrap();
    let dims = engine.dims().clone();
    let vocab = Vocab::multilingual(dims.n_vocab).unwrap();
    let mel = local_whisper::MelFrontEnd::new(dims.n_mels).compute(&jfk());
    engine.encode(&mel).unwrap();

    // Start-of-transcript only, argmax every step: `reference.py greedy`, token for token.
    let mut logits = vec![0f32; dims.n_vocab];
    let mut ids = vec![vocab.sot];
    while ids.len() < dims.max_steps() {
        engine.step(*ids.last().unwrap(), &mut logits).unwrap();
        let next = logits.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0 as u32;
        ids.push(next);
        if next == vocab.eot {
            break;
        }
    }
    if dir.ends_with("whisper-base") {
        let reference = [
            50258, 50259, 50359, 50363, 400, 370, 452, 7177, 6280, 11, 1029, 406, 437, 428, 1941, 393, 360, 337, 291, 11, 1029, 437, 291,
            393, 360, 337, 428, 1941, 13, 50257,
        ];
        assert_eq!(ids, reference);
    }
    // Larger models punctuate differently; the words are the same.
    assert_eq!(plain(&vocab.decode(&ids)), plain(JFK));
}

#[test]
fn a_short_clip_is_transcribed_with_timestamps_and_its_language() {
    let Some(dir) = model_dir() else { return };
    let mut engine = OnnxEngine::load(&dir).unwrap();
    let audio = jfk();
    let (segments, language, windows) = transcribe(&mut engine, &audio, DecodeOptions::default());
    assert_eq!(windows, 1);
    assert_eq!(language, Some("en"));
    assert_eq!(plain(&join_text(&segments)), plain(JFK));
    let seconds = audio.len() as f64 / SAMPLE_RATE as f64;
    for pair in segments.windows(2) {
        assert!(pair[0].end <= pair[1].start + 0.01, "segments overlap: {pair:?}");
    }
    assert!(segments.iter().all(|s| s.start < s.end && s.end <= seconds + 0.01), "{segments:?}");
    // The speech starts within the first second and runs to near the end of the clip.
    assert!(segments[0].start < 1.0 && segments.last().unwrap().end > seconds - 3.0, "{segments:?}");
    assert!(segments.iter().all(|s| s.temperature == 0.0 && s.avg_logprob > -1.0), "{segments:?}");
}

/// Distinct utterances with pauses between them, in order, and a phrase only each one contains.
/// The same clip repeated would not do: a transcript that repeats itself is exactly what the
/// compression-ratio check exists to reject.
const PHRASES: [&str; 4] = ["ask not what your country can do for you", "stew for dinner", "after early nightfall", "stuff it into you"];

/// About 50 s of varied speech: the JFK clip, three English utterances and a Spanish one, from
/// `WHISPER_SPEECH_DIR` (`reference.py speech` fetches them). `None` when they are not there.
fn varied_speech() -> Option<(Vec<f32>, Vec<f64>)> {
    let dir = std::env::var_os("WHISPER_SPEECH_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Documents/Ai/whisper/ref/libri")
    });
    let mut clips = vec![jfk()];
    for name in ["1.s16", "3.s16", "2.s16", "4.s16"] {
        let Ok(bytes) = std::fs::read(dir.join(name)) else {
            eprintln!("skipping: no {name} in {}", dir.display());
            return None;
        };
        clips.push(bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect());
    }
    let (mut audio, mut starts) = (Vec::new(), Vec::new());
    for clip in clips {
        starts.push(audio.len() as f64 / SAMPLE_RATE as f64);
        audio.extend(clip);
        audio.extend(std::iter::repeat_n(0.0, 5 * SAMPLE_RATE / 2));
    }
    Some((audio, starts))
}

/// Every phrase exactly once and in the order spoken.
fn assert_all_phrases_once(segments: &[Segment]) {
    let text = plain(&join_text(segments));
    let mut last = 0;
    for phrase in PHRASES {
        assert_eq!(text.matches(phrase).count(), 1, "{phrase:?} in {text:?}");
        let at = text.find(phrase).unwrap();
        assert!(at >= last, "{phrase:?} out of order in {text:?}");
        last = at;
    }
    for pair in segments.windows(2) {
        assert!(pair[0].end <= pair[1].start + 0.01, "segments overlap: {pair:?}");
    }
}

#[test]
fn a_recording_longer_than_one_window_is_stitched_at_segment_boundaries() {
    let Some(dir) = model_dir() else { return };
    let Some((audio, starts)) = varied_speech() else { return };
    let mut engine = OnnxEngine::load(&dir).unwrap();
    let (segments, _, windows) = transcribe(&mut engine, &audio, DecodeOptions { language: Some("en".into()), ..Default::default() });
    assert!(windows >= 2, "{windows} windows");
    assert_all_phrases_once(&segments);
    // Each utterance's text is timed to where the utterance is. The slack before it is the
    // pause it follows: a small model opens a segment at the window's start, not the speech's.
    for (phrase, start) in PHRASES.iter().zip(&starts) {
        let segment = segments.iter().find(|s| plain(&s.text).contains(phrase)).unwrap();
        assert!(segment.start > start - 3.0 && segment.start < start + 6.0, "{phrase:?} timed at {:.2}, spoken from {start:.2}", segment.start);
    }
    // Clean speech in the forced language decodes greedily; only the Spanish tail, forced
    // through as English here, may need a fallback temperature.
    let english = segments.iter().filter(|s| PHRASES.iter().any(|phrase| plain(&s.text).contains(phrase)));
    assert!(english.clone().count() >= 4 && english.clone().all(|s| s.temperature == 0.0), "{segments:?}");
}

#[test]
fn silence_and_noise_produce_no_text() {
    let Some(dir) = model_dir() else { return };
    let mut engine = OnnxEngine::load(&dir).unwrap();
    let silence = vec![0f32; 20 * SAMPLE_RATE];
    assert!(transcribe(&mut engine, &silence, DecodeOptions::default()).0.is_empty());
    // Hiss loud enough to pass the quiet gate; the model's own no-speech call has to hold.
    let mut seed = 0x2545f4914f6cdd1du64;
    let hiss: Vec<f32> = (0..20 * SAMPLE_RATE)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.02
        })
        .collect();
    let (segments, ..) = transcribe(&mut engine, &hiss, DecodeOptions { language: Some("en".into()), ..Default::default() });
    let words = join_text(&segments).split_whitespace().count();
    assert!(words <= 3, "hallucinated over hiss: {segments:?}");
}

#[test]
fn a_live_recording_is_transcribed_in_step_with_the_audio() {
    let Some(dir) = model_dir() else { return };
    let Some((audio, _)) = varied_speech() else { return };
    let mut engine = OnnxEngine::load(&dir).unwrap();
    let mut long = LongForm::new(engine.dims(), DecodeOptions::default()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut source = &audio[..];
    let mut segments = Vec::new();
    // Audio arrives five seconds at a time; windows are taken as they fill.
    let (mut recorded, mut previews) = (0, 0);
    while recorded < audio.len() {
        recorded = (recorded + 5 * SAMPLE_RATE).min(audio.len());
        if recorded == 10 * SAMPLE_RATE {
            // A preview of the unfinished first window shows text without consuming it.
            let (preview, _) = long.preview(&mut engine, &mut source, recorded as u64, &cancel).unwrap();
            assert!(plain(&join_text(&preview)).starts_with("and so my fellow americans"), "{preview:?}");
            assert_eq!(long.seek(), 0);
            previews += 1;
        }
        while let Advance::Window(window) = long.advance(&mut engine, &mut source, recorded as u64, false, &cancel).unwrap() {
            segments.extend(window.segments);
        }
    }
    assert_eq!(previews, 1);
    // Some of it was final before the recording ended.
    assert!(!segments.is_empty());
    while let Advance::Window(window) = long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).unwrap() {
        segments.extend(window.segments);
    }
    assert_eq!(long.language(), Some("en"));
    assert_all_phrases_once(&segments);
    // The last utterance is Spanish, straight after English in the same window. A model told
    // it is hearing English stops there; the large models get it back from the second look at
    // the window's tail, decoded in the language it is actually in.
    if engine.dims().n_mels == 128 {
        assert!(plain(&join_text(&segments)).contains("ramas medio sumergidas"), "{segments:?}");
    }
}
