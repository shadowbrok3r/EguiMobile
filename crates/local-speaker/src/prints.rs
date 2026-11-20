//! The voice prints of a recording's windows, kept so the grouping can be redone without the
//! audio or the network — with a different count, in a few milliseconds.

use crate::window::{Window, WINDOW_SAMPLES};
use crate::DIM;
use std::io::{self, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"VPRINTS1";
/// More windows than a week of speech has: a count above it is a damaged file.
const MAX_WINDOWS: usize = 1 << 20;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prints {
    windows: Vec<Window>,
    /// [`DIM`] numbers per window, as the network gave them.
    values: Vec<f32>,
}

impl Prints {
    pub fn push(&mut self, window: Window, print: &[f32]) {
        assert_eq!(print.len(), DIM);
        self.windows.push(window);
        self.values.extend_from_slice(print);
    }

    pub fn len(&self) -> usize {
        self.windows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    pub fn print(&self, index: usize) -> &[f32] {
        &self.values[index * DIM..(index + 1) * DIM]
    }

    /// Write to `path` through a temporary file. `key` says what the prints are of; a load
    /// asking for another key gets nothing.
    pub fn save(&self, path: &Path, key: u64) -> io::Result<()> {
        let temporary = path.with_extension("tmp");
        let mut out = io::BufWriter::new(std::fs::File::create(&temporary)?);
        out.write_all(MAGIC)?;
        out.write_all(&key.to_le_bytes())?;
        out.write_all(&(self.windows.len() as u32).to_le_bytes())?;
        for window in &self.windows {
            out.write_all(&window.span.to_le_bytes())?;
            out.write_all(&window.start.to_le_bytes())?;
            out.write_all(&window.len.to_le_bytes())?;
        }
        for value in &self.values {
            out.write_all(&value.to_le_bytes())?;
        }
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&temporary, path)
    }

    /// The prints saved at `path` under `key`; `None` if there are none, they are of
    /// something else, or the file is not whole.
    pub fn load(path: &Path, key: u64) -> Option<Self> {
        let mut file = io::BufReader::new(std::fs::File::open(path).ok()?);
        let mut head = [0u8; 20];
        file.read_exact(&mut head).ok()?;
        if &head[..8] != MAGIC || u64::from_le_bytes(head[8..16].try_into().ok()?) != key {
            return None;
        }
        let count = u32::from_le_bytes(head[16..20].try_into().ok()?) as usize;
        // A count the file cannot hold is a damaged file, not a reason to allocate for it.
        let expected = 20 + count as u64 * (16 + DIM as u64 * 4);
        if count > MAX_WINDOWS || std::fs::metadata(path).ok()?.len() != expected {
            return None;
        }
        let mut windows = Vec::with_capacity(count);
        let mut record = [0u8; 16];
        for _ in 0..count {
            file.read_exact(&mut record).ok()?;
            let window = Window {
                span: u32::from_le_bytes(record[..4].try_into().ok()?),
                start: u64::from_le_bytes(record[4..12].try_into().ok()?),
                len: u32::from_le_bytes(record[12..].try_into().ok()?),
            };
            // No window is longer than a window.
            if window.len as usize > WINDOW_SAMPLES {
                return None;
            }
            windows.push(window);
        }
        let mut bytes = vec![0u8; count * DIM * 4];
        file.read_exact(&mut bytes).ok()?;
        let values = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        Some(Self { windows, values })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_round_trip_and_answer_only_to_their_key() {
        let dir = std::env::temp_dir().join(format!("local-speaker-prints-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("voices.bin");
        let mut prints = Prints::default();
        for i in 0..3u32 {
            let print: Vec<f32> = (0..DIM).map(|d| (i as f32 + 1.0) * d as f32 * 0.01 - 1.0).collect();
            prints.push(Window { span: i, start: i as u64 * 40_000, len: 32_000 - i * 1_000 }, &print);
        }
        assert_eq!(Prints::load(&path, 7), None);
        prints.save(&path, 7).unwrap();
        assert_eq!(Prints::load(&path, 7).as_ref(), Some(&prints));
        assert_eq!(Prints::load(&path, 8), None, "prints of another transcript");
        assert_eq!((prints.len(), prints.print(2)[1]), (3, 3.0 * 0.01 - 1.0));

        // A file cut short is no prints at all.
        let whole = std::fs::read(&path).unwrap();
        std::fs::write(&path, &whole[..whole.len() - 5]).unwrap();
        assert_eq!(Prints::load(&path, 7), None);
        std::fs::write(&path, b"VPRINTS1").unwrap();
        assert_eq!(Prints::load(&path, 7), None);
        // Nor is one whose windows could not have been planned.
        let mut odd = whole.clone();
        odd[20 + 12..20 + 16].copy_from_slice(&40_000u32.to_le_bytes());
        std::fs::write(&path, &odd).unwrap();
        assert_eq!(Prints::load(&path, 7), None);
        std::fs::write(&path, &whole).unwrap();
        assert!(Prints::load(&path, 7).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
