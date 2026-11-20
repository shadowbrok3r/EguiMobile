//! Speaker pack on disk: the embedding network's HTP context binary, keyed by the `SPEAKER`
//! marker file. `scripts/speaker-export/export.py` makes one.

use crate::error::{Error, Result};
use memmap2::Mmap;
use std::path::{Path, PathBuf};

/// The marker file that identifies a complete speaker pack.
pub const MARKER: &str = "SPEAKER";
pub const MODEL: &str = "model.bin";

/// A validated pack directory. The context binary is mmapped per load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakerPack {
    dir: PathBuf,
}

impl SpeakerPack {
    /// Validate `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.join(MARKER).exists() {
            return Err(Error::NotSpeakerPack(dir));
        }
        if !dir.join(MODEL).exists() {
            return Err(Error::MissingFile(dir.join(MODEL)));
        }
        Ok(Self { dir })
    }

    /// Mark `dir` complete once the model is in place. Written last by whatever put the file
    /// there, so a copy that died halfway is never mistaken for a pack.
    pub fn seal(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        if !dir.join(MODEL).exists() {
            return Err(Error::MissingFile(dir.join(MODEL)));
        }
        std::fs::write(dir.join(MARKER), b"")?;
        Self::open(dir)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn size_bytes(&self) -> u64 {
        std::fs::metadata(self.dir.join(MODEL)).map_or(0, |m| m.len())
    }

    /// mmap the context binary.
    pub fn map(&self) -> Result<Mmap> {
        let f = std::fs::File::open(self.dir.join(MODEL))?;
        Ok(unsafe { Mmap::map(&f)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pack_needs_its_model_and_its_marker() {
        let dir = std::env::temp_dir().join(format!("local-speaker-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(SpeakerPack::open(&dir), Err(Error::NotSpeakerPack(_))));
        assert!(matches!(SpeakerPack::seal(&dir), Err(Error::MissingFile(_))));
        std::fs::write(dir.join(MODEL), b"graph").unwrap();
        // The model without the marker is an interrupted copy.
        assert!(SpeakerPack::open(&dir).is_err());
        let pack = SpeakerPack::seal(&dir).unwrap();
        assert_eq!((pack.size_bytes(), &pack.map().unwrap()[..]), (5, &b"graph"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
