//! The whole local-speaker pipeline against real weights and real meetings, on the host.
//!
//! Needs the speaker network's ONNX — `SPEAKER_ONNX`, or `model_static.onnx` under
//! `~/Documents/Ai/speaker/build`, where `scripts/speaker-export/export.py` leaves it — and for
//! the meeting an AMI recording with its reference RTTM under `SPEAKER_AUDIO_DIR` (default
//! `~/Documents/Ai/speaker/data/ami`). Without them the tests pass vacuously and say so.

use local_speaker::{cluster, fill, plan, Embedder, Prints, Span, DIM, WINDOW_SAMPLES};
use std::path::PathBuf;
use whisper_host::OnnxEmbedder;

const RATE: f64 = 16_000.0;

fn home(rest: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
}

fn embedder() -> Option<OnnxEmbedder> {
    let model = std::env::var_os("SPEAKER_ONNX").map(PathBuf::from).unwrap_or_else(|| home("Documents/Ai/speaker/build/model_static.onnx"));
    if !model.exists() {
        eprintln!("skipping: no speaker ONNX at {}", model.display());
        return None;
    }
    Some(OnnxEmbedder::load(&model).unwrap())
}

fn jfk() -> Vec<f32> {
    include_bytes!("../../../crates/local-whisper/tests/fixtures/jfk.s16").chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect()
}

fn likeness(a: &[f32], b: &[f32]) -> f32 {
    let dot = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>();
    dot(a, b) / (dot(a, a) * dot(b, b)).sqrt()
}

#[test]
fn the_crates_features_give_the_reference_print() {
    let Some(mut embedder) = embedder() else { return };
    let reference: Vec<f32> = include_bytes!("../../../crates/local-speaker/tests/fixtures/jfk_print.f32").chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let mut print = [0f32; DIM];
    embedder.embed(&jfk()[..WINDOW_SAMPLES], &mut print).unwrap();
    // The reference came through numpy's features; these are the crate's own.
    assert!(likeness(&print, &reference) > 0.9999, "{}", likeness(&print, &reference));
    let length = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((length(&print) / length(&reference) - 1.0).abs() < 1e-3);

    // The same man a few seconds later is far more like himself than like noise.
    let mut later = [0f32; DIM];
    embedder.embed(&jfk()[5 * 16_000..5 * 16_000 + WINDOW_SAMPLES], &mut later).unwrap();
    let hiss: Vec<f32> = (0..WINDOW_SAMPLES).scan(1u32, |state, _| { *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); Some((*state >> 8) as f32 / (1u32 << 24) as f32 * 0.2 - 0.1) }).collect();
    let mut noise = [0f32; DIM];
    embedder.embed(&hiss, &mut noise).unwrap();
    assert!(likeness(&print, &later) > 0.5 && likeness(&print, &noise) < 0.3, "same {} noise {}", likeness(&print, &later), likeness(&print, &noise));
    // And the network is surer of speech than of hiss: the length the grouping leans on.
    assert!(length(&noise) < length(&print), "noise {} speech {}", length(&noise), length(&print));
}

/// A meeting's audio and who spoke when, from its reference RTTM.
fn meeting(name: &str) -> Option<(Vec<f32>, Vec<(f64, f64, String)>)> {
    let dir = std::env::var_os("SPEAKER_AUDIO_DIR").map(PathBuf::from).unwrap_or_else(|| home("Documents/Ai/speaker/data/ami"));
    let (wav, rttm) = (dir.join(format!("{name}.wav")), dir.join(format!("{}.rttm", name.split('.').next().unwrap_or(name))));
    let (Ok(bytes), Ok(text)) = (std::fs::read(&wav), std::fs::read_to_string(&rttm)) else {
        eprintln!("skipping: no {} with its RTTM", wav.display());
        return None;
    };
    // The data chunk follows whatever else the header carries.
    let data = bytes.windows(4).position(|w| w == b"data").map(|at| at + 8)?;
    let audio = bytes[data..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
    let turns = text.lines().filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.first() == Some(&"SPEAKER")).then(|| (fields[3].parse::<f64>().unwrap(), fields[3].parse::<f64>().unwrap() + fields[4].parse::<f64>().unwrap(), fields[7].to_string()))
    });
    let mut turns: Vec<_> = turns.collect();
    turns.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some((audio, turns))
}

/// Speech cut where everyone pauses, and by length: what a transcriber blind to speakers gives.
fn stretches(turns: &[(f64, f64, String)]) -> Vec<(f64, f64)> {
    let mut joined: Vec<(f64, f64)> = Vec::new();
    for (start, end, _) in turns {
        match joined.last_mut() {
            Some(last) if start - last.1 <= 0.4 => last.1 = last.1.max(*end),
            _ => joined.push((*start, *end)),
        }
    }
    joined.iter().flat_map(|(start, end)| {
        let pieces = ((end - start) / 8.0).ceil().max(1.0) as usize;
        let step = (end - start) / pieces as f64;
        (0..pieces).map(move |k| (start + k as f64 * step, start + (k + 1) as f64 * step))
    }).collect()
}

/// Share of the stretches' time given to the right person, under the best naming of the
/// speakers found.
fn agreement(spans: &[(f64, f64)], labels: &[Option<u8>], turns: &[(f64, f64, String)], found: usize) -> f64 {
    let mut people: Vec<&str> = turns.iter().map(|t| t.2.as_str()).collect();
    people.sort_unstable();
    people.dedup();
    let mut table = vec![vec![0f64; people.len()]; found.max(1)];
    let mut total = 0.0;
    for ((start, end), label) in spans.iter().zip(labels) {
        let mut heard = vec![0f64; people.len()];
        for (t_start, t_end, who) in turns {
            let shared = end.min(*t_end) - start.max(*t_start);
            if shared > 0.0 {
                heard[people.iter().position(|p| p == who).unwrap()] += shared;
            }
        }
        let most = (0..people.len()).max_by(|a, b| heard[*a].total_cmp(&heard[*b])).unwrap();
        if let Some(label) = label.filter(|_| heard[most] > 0.0) {
            table[label as usize][most] += end - start;
            total += end - start;
        }
    }
    // Few enough speakers to try every naming.
    fn best(table: &[Vec<f64>], row: usize, used: &mut Vec<bool>) -> f64 {
        if row == table.len() {
            return 0.0;
        }
        let mut top = best(table, row + 1, used);
        for person in 0..used.len() {
            if !used[person] {
                used[person] = true;
                top = top.max(table[row][person] + best(table, row + 1, used));
                used[person] = false;
            }
        }
        top
    }
    best(&table, 0, &mut vec![false; people.len()]) / total.max(1e-9)
}

fn label(name: &str, embedder: &mut OnnxEmbedder) -> Option<(usize, f64, f64)> {
    let (audio, turns) = meeting(name)?;
    let spans = stretches(&turns);
    let windows = plan(&spans.iter().map(|(s, e)| Span { start: (s * RATE).round() as u64, end: (e * RATE).round() as u64 }).collect::<Vec<_>>(), audio.len() as u64);
    let mut prints = Prints::default();
    let (mut window, mut print) = (vec![0f32; WINDOW_SAMPLES], [0f32; DIM]);
    for planned in &windows {
        let from = planned.start as usize;
        fill(&audio[from..from + planned.len as usize], &mut window);
        embedder.embed(&window, &mut print).unwrap();
        prints.push(*planned, &print);
    }
    let found = cluster(&prints, spans.len(), None);
    let known = cluster(&prints, spans.len(), Some(4));
    Some((found.speakers, agreement(&spans, &found.spans, &turns, found.speakers), agreement(&spans, &known.spans, &turns, known.speakers)))
}

#[test]
fn a_four_person_meeting_is_given_to_its_four_speakers() {
    let Some(mut embedder) = embedder() else { return };
    // The headset mix, and the same meeting from one microphone across the table.
    for (name, least) in [("ES2004a.Mix-Headset", 0.88), ("ES2004a.Array1-01", 0.84)] {
        let Some((speakers, found, known)) = label(name, &mut embedder) else { return };
        eprintln!("{name}: {speakers} speakers found, {:.1} % of speech with the right one; {:.1} % told there are four", found * 100.0, known * 100.0);
        assert_eq!(speakers, 4, "{name}");
        assert!(found > least && known > least, "{name}: {found:.3} found, {known:.3} told");
    }
}
