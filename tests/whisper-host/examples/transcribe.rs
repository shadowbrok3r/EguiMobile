//! Transcribe a 16 kHz mono 16-bit recording with an exported model, on the host.
//!
//!   cargo run --release --example transcribe -- <onnx model dir> <audio.s16 | audio.wav> [language]

use local_whisper::{Advance, DecodeOptions, Engine, LongForm, Timings, SAMPLE_RATE};
use std::sync::atomic::AtomicBool;
use whisper_host::OnnxEngine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: transcribe <onnx model dir> <audio.s16 | audio.wav> [language]");
        std::process::exit(2);
    }
    let bytes = std::fs::read(&args[2]).expect("audio file");
    // A canonical WAV is its 44-byte header and then exactly what a .s16 file holds.
    let pcm = if bytes.starts_with(b"RIFF") { &bytes[44..] } else { &bytes[..] };
    let audio: Vec<f32> = pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();

    let mut engine = OnnxEngine::load(&args[1]).expect("model");
    println!("{}", engine.dims().summary());
    let opts = DecodeOptions { language: args.get(3).cloned(), ..Default::default() };
    let mut long = LongForm::new(engine.dims(), opts).expect("options");
    let cancel = AtomicBool::new(false);
    let (mut source, mut timings) = (&audio[..], Timings::default());
    let started = std::time::Instant::now();
    while let Advance::Window(window) = long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).expect("transcription") {
        timings.add(&window.timings);
        println!("-- window {:.2}..{:.2} s: {:?}", window.from as f64 / SAMPLE_RATE as f64, window.to as f64 / SAMPLE_RATE as f64, window.outcome);
        for s in &window.segments {
            println!("[{:7.2} -> {:7.2}] {}  (logprob {:.2}, t={})", s.start, s.end, s.text, s.avg_logprob, s.temperature);
        }
    }
    println!(
        "{:.1} s of audio in {:.1} s; encoder {:.0} ms total, {:.1} ms/token over {} tokens; language {:?}",
        audio.len() as f64 / SAMPLE_RATE as f64,
        started.elapsed().as_secs_f32(),
        timings.encode.as_secs_f32() * 1000.0,
        timings.ms_per_step(),
        timings.steps,
        long.language()
    );
}
