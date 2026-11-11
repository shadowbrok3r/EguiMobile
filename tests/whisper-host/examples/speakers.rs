//! Transcribe a recording and label its speakers, on the host: what the app writes for it.
//!
//!   cargo run --release --example speakers -- <whisper onnx dir> <speaker onnx> <audio.wav> [speakers]
//!
//! The audio is 16 kHz mono 16-bit. A fourth argument fixes the number of speakers.

use local_speaker::{cluster, fill, plan, Embedder, Prints, Span, DIM, WINDOW_SAMPLES};
use local_whisper::{Advance, DecodeOptions, Engine, LongForm, SAMPLE_RATE};
use std::sync::atomic::AtomicBool;
use whisper_host::{OnnxEmbedder, OnnxEngine};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: speakers <whisper onnx dir> <speaker onnx> <audio.wav> [speakers]");
        std::process::exit(2);
    }
    let bytes = std::fs::read(&args[3]).expect("audio file");
    let data = bytes.windows(4).position(|w| w == b"data").map_or(0, |at| at + 8);
    let audio: Vec<f32> = bytes[data..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();

    let mut whisper = OnnxEngine::load(&args[1]).expect("whisper model");
    let mut long = LongForm::new(whisper.dims(), DecodeOptions::default()).expect("options");
    let cancel = AtomicBool::new(false);
    let (mut source, mut segments) = (&audio[..], Vec::new());
    let started = std::time::Instant::now();
    while let Advance::Window(window) = long.advance(&mut whisper, &mut source, audio.len() as u64, true, &cancel).expect("transcription") {
        segments.extend(window.segments);
    }
    let transcribed = started.elapsed();

    let mut embedder = OnnxEmbedder::load(&args[2]).expect("speaker model");
    let spans: Vec<Span> = segments.iter().map(|s| Span { start: (s.start * SAMPLE_RATE as f64).round() as u64, end: (s.end * SAMPLE_RATE as f64).round() as u64 }).collect();
    let windows = plan(&spans, audio.len() as u64);
    let mut prints = Prints::default();
    let (mut window, mut print) = (vec![0f32; WINDOW_SAMPLES], [0f32; DIM]);
    let started = std::time::Instant::now();
    for planned in &windows {
        fill(&audio[planned.start as usize..planned.start as usize + planned.len as usize], &mut window);
        embedder.embed(&window, &mut print).expect("speaker network");
        prints.push(*planned, &print);
    }
    let labels = cluster(&prints, spans.len(), args.get(4).and_then(|n| n.parse().ok()));

    let mut last = None;
    for (segment, speaker) in segments.iter().zip(&labels.spans) {
        if labels.speakers > 1 && *speaker != last && speaker.is_some() {
            print!("\n\nSpeaker {}: ", speaker.unwrap_or(0) + 1);
            last = *speaker;
        } else {
            print!(" ");
        }
        print!("[{:.1}] {}", segment.start, segment.text);
    }
    println!(
        "\n\n{:.0} s of audio, {} segments: transcribed in {:.1} s, {} windows told apart in {:.1} s, {} speaker{}",
        audio.len() as f64 / SAMPLE_RATE as f64,
        segments.len(),
        transcribed.as_secs_f32(),
        windows.len(),
        started.elapsed().as_secs_f32(),
        labels.speakers,
        if labels.speakers == 1 { "" } else { "s" }
    );
}
