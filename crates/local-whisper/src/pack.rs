//! Whisper pack on disk: the encoder and decoder HTP context binaries, keyed by the `WHISPER`
//! marker file.
//!
//! The two graphs are exactly the `encoder.bin` and `decoder.bin` of a Qualcomm AI Hub
//! `qnn_context_binary` release zip, unmodified. The marker is written last, by whatever put
//! the files there ([`WhisperPack::seal`]), so a download or extraction that died halfway is
//! never mistaken for a pack.

use crate::error::{Error, Result};
use memmap2::Mmap;
use std::path::{Path, PathBuf};

/// The marker file that identifies a complete Whisper pack.
pub const MARKER: &str = "WHISPER";
pub const ENCODER: &str = "encoder.bin";
pub const DECODER: &str = "decoder.bin";

/// Files every pack must have.
pub const REQUIRED: [&str; 3] = [MARKER, ENCODER, DECODER];

/// A validated pack directory. The context binaries are mmapped per load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhisperPack {
    dir: PathBuf,
}

impl WhisperPack {
    /// Validate `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.join(MARKER).exists() {
            return Err(Error::NotWhisperPack(dir));
        }
        for name in REQUIRED {
            let p = dir.join(name);
            if !p.exists() {
                return Err(Error::MissingFile(p));
            }
        }
        Ok(Self { dir })
    }

    /// True when `dir` carries the `WHISPER` marker.
    pub fn is_whisper_pack(dir: impl AsRef<Path>) -> bool {
        dir.as_ref().join(MARKER).exists()
    }

    /// Mark `dir` complete once both graphs are in place.
    pub fn seal(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        for name in [ENCODER, DECODER] {
            let p = dir.join(name);
            if !p.exists() {
                return Err(Error::MissingFile(p));
            }
        }
        std::fs::write(dir.join(MARKER), b"")?;
        Self::open(dir)
    }

    /// The pack directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The directory's own name, which is how a pack is referred to.
    pub fn name(&self) -> String {
        self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Path of `name` inside the pack.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Bytes the two graphs occupy on disk.
    pub fn size_bytes(&self) -> u64 {
        [ENCODER, DECODER].iter().filter_map(|name| std::fs::metadata(self.path(name)).ok()).map(|m| m.len()).sum()
    }

    /// mmap a context binary.
    pub fn map(&self, name: &str) -> Result<Mmap> {
        let f = std::fs::File::open(self.path(name))?;
        Ok(unsafe { Mmap::map(&f)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("local-whisper-pack-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_marker_is_rejected() {
        let dir = scratch("empty");
        std::fs::write(dir.join(ENCODER), b"e").unwrap();
        std::fs::write(dir.join(DECODER), b"d").unwrap();
        // Both graphs present but unsealed: an interrupted install, not a pack.
        assert!(matches!(WhisperPack::open(&dir), Err(Error::NotWhisperPack(_))));
        assert!(!WhisperPack::is_whisper_pack(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_without_files_reports_the_missing_one() {
        let dir = scratch("marker");
        std::fs::write(dir.join(MARKER), b"").unwrap();
        std::fs::write(dir.join(ENCODER), b"e").unwrap();
        match WhisperPack::open(&dir) {
            Err(Error::MissingFile(p)) => assert!(p.ends_with(DECODER)),
            other => panic!("expected MissingFile, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seal_refuses_half_a_pack_then_marks_a_whole_one() {
        let dir = scratch("seal");
        std::fs::write(dir.join(ENCODER), b"enc").unwrap();
        assert!(matches!(WhisperPack::seal(&dir), Err(Error::MissingFile(_))));
        assert!(!WhisperPack::is_whisper_pack(&dir));
        std::fs::write(dir.join(DECODER), b"decoder").unwrap();
        let pack = WhisperPack::seal(&dir).unwrap();
        assert_eq!(pack.size_bytes(), 10);
        assert_eq!(&pack.map(DECODER).unwrap()[..], b"decoder");
        assert!(pack.name().starts_with("local-whisper-pack-seal"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
