//! Crate error and result types.

use std::path::PathBuf;
use std::result::Result as StdResult;

pub type Result<T> = StdResult<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("qnn: {0}")]
    Qnn(#[from] qnn_rs::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0} is not a Whisper pack (no WHISPER marker)")]
    NotWhisperPack(PathBuf),

    #[error("Whisper pack is missing {0}")]
    MissingFile(PathBuf),

    /// The graph does not have the tensor layout of a Qualcomm AI Hub Whisper export.
    #[error("{part} graph: {what}")]
    Contract { part: &'static str, what: String },

    #[error("vocabulary of {0} tokens is not a multilingual Whisper size (51865 or 51866)")]
    UnknownVocab(usize),

    #[error("unknown language code {0:?}")]
    UnknownLanguage(String),

    #[error("transcription cancelled")]
    Cancelled,

    #[error("{0}")]
    Msg(String),
}

impl Error {
    pub(crate) fn contract(part: &'static str, what: impl Into<String>) -> Self {
        Error::Contract { part, what: what.into() }
    }
}
