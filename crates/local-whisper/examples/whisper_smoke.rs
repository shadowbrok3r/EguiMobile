//! Load a Whisper pack on the HTP and transcribe a recording.
//!
//! Usage:
//!   whisper_smoke <lib_dir> <pack_dir> <audio.s16 | audio.wav> [language]
//! `lib_dir` holds libQnnHtp.so/libQnnSystem.so and the v81 skel; `pack_dir` is a sealed
//! Whisper pack; the audio is 16 kHz mono 16-bit, raw or a canonical WAV.
//!
//! Device-only: on a host the HTP backend fails at session creation, which this reports.
//! `tests/whisper-host` runs the same pipeline on the host through ONNX Runtime instead.

use local_whisper::{prepare_htp_env, Advance, Backend, DecodeOptions, Engine, LongForm, QnnEngine, QnnSystem, Session, Timings, WhisperPack, SAMPLE_RATE};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok())))
        .map_or(0, |kb| kb / 1024)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let [_, lib_dir, pack_dir, audio_path, rest @ ..] = args.as_slice() else {
        return Err("usage: whisper_smoke <lib_dir> <pack_dir> <audio.s16 | audio.wav> [language]".into());
    };
    let lib_dir = PathBuf::from(lib_dir);
    let bytes = std::fs::read(audio_path).map_err(|e| format!("{audio_path}: {e}"))?;
    // A canonical WAV is its 44-byte header and then exactly what a .s16 file holds.
    let pcm = if bytes.starts_with(b"RIFF") && bytes.len() > 44 { &bytes[44..] } else { &bytes[..] };
    let audio: Vec<f32> = pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();

    prepare_htp_env(&lib_dir);
    let system = QnnSystem::load(lib_dir.join("libQnnSystem.so")).map_err(|e| format!("load libQnnSystem.so: {e}"))?;
    let backend = Backend::load(lib_dir.join("libQnnHtp.so")).map_err(|e| format!("load libQnnHtp.so: {e}"))?;
    let session = Session::new(&backend).map_err(|e| format!("session: {e}"))?;
    // Must run after the session exists; without the burst vote long graphs outrun FastRPC.
    match session.set_htp_performance_mode() {
        Ok(()) => println!("htp burst mode on"),
        Err(e) => println!("htp burst mode unavailable: {e}"),
    }

    let pack = WhisperPack::open(pack_dir).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut engine = QnnEngine::load(&session, &system, &pack).map_err(|e| format!("load {}: {e}", pack.name()))?;
    println!("{} loaded in {:.1} s, rss {} MB", pack.name(), started.elapsed().as_secs_f32(), rss_mb());
    println!("{}; {}; {} MB of cache per decoder run", engine.dims().summary(), engine.provenance(), engine.bytes_per_step() / 1_048_576);

    let opts = DecodeOptions { language: rest.first().cloned(), ..Default::default() };
    let mut long = LongForm::new(engine.dims(), opts).map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    let (mut source, mut timings) = (&audio[..], Timings::default());
    let started = Instant::now();
    loop {
        match long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel).map_err(|e| format!("transcribe: {e}"))? {
            Advance::Window(window) => {
                timings.add(&window.timings);
                println!(
                    "-- {:.2}..{:.2} s {:?} {}: encoder {:.0} ms, {:.1} ms/token x {}",
                    window.from as f64 / SAMPLE_RATE as f64,
                    window.to as f64 / SAMPLE_RATE as f64,
                    window.outcome,
                    window.language.unwrap_or("-"),
                    window.timings.encode.as_secs_f32() * 1000.0,
                    window.timings.ms_per_step(),
                    window.timings.steps
                );
                for s in &window.segments {
                    println!("[{:7.2} -> {:7.2}] {}", s.start, s.end, s.text);
                }
            }
            _ => break,
        }
    }
    let seconds = audio.len() as f64 / SAMPLE_RATE as f64;
    let took = started.elapsed().as_secs_f64();
    println!("{seconds:.1} s of audio in {took:.2} s ({:.0}x real time), {:.1} ms/token over {} tokens, rss {} MB", seconds / took.max(1e-6), timings.ms_per_step(), timings.steps, rss_mb());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
