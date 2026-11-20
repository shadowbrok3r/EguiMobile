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

    #[error("{0} is not a speaker pack (no SPEAKER marker)")]
    NotSpeakerPack(PathBuf),

    #[error("speaker pack is missing {0}")]
    MissingFile(PathBuf),

    /// The graph does not have the tensor layout of the export in `scripts/speaker-export`.
    #[error("speaker graph: {0}")]
    Contract(String),

    #[error("{0}")]
    Msg(String),
}
