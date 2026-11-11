//! Group saved voice prints from the command line: one line of JSON per file, for checking
//! this crate's grouping against the Python reference over recordings with known speakers.
//!
//! `cargo run --release -p local-speaker --example cluster_prints -- <file.prints>... [--fixed N]`
//!
//! A file `<name>.k` beside a prints file, holding a number, adds a second line for that file
//! with the count fixed to it.

use local_speaker::{cluster, Prints};
use std::path::Path;

fn line(path: &Path, prints: &Prints, spans: usize, fixed: Option<usize>) {
    let labels = cluster(prints, spans, fixed);
    let spans: Vec<String> = labels.spans.iter().map(|s| s.map_or("null".into(), |n| n.to_string())).collect();
    println!(r#"{{"file": "{}", "fixed": {}, "speakers": {}, "spans": [{}]}}"#, path.display(), fixed.map_or("null".into(), |k| k.to_string()), labels.speakers, spans.join(", "));
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let fixed = args.iter().position(|a| a == "--fixed").map(|at| {
        let count = args.get(at + 1).and_then(|n| n.parse().ok()).expect("--fixed takes a number");
        args.drain(at..at + 2);
        count
    });
    for arg in &args {
        let path = Path::new(arg);
        let Some(prints) = Prints::load(path, 0) else {
            eprintln!("{arg}: not a prints file saved under key 0");
            std::process::exit(1);
        };
        let spans = prints.windows().iter().map(|w| w.span as usize + 1).max().unwrap_or(0);
        line(path, &prints, spans, fixed);
        if let Some(known) = std::fs::read_to_string(path.with_extension("k")).ok().and_then(|text| text.trim().parse().ok()) {
            line(path, &prints, spans, Some(known));
        }
    }
}
