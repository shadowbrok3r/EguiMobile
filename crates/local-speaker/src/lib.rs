//! Telling the voices in a recording apart.
//!
//! Given a recording and the stretches of it someone spoke in — a transcript's segments — this
//! says which of them are the same voice. It does not say whose: speakers come out numbered in
//! the order they are first heard.
//!
//! Each stretch is cut into windows of two seconds ([`plan`]), each window is turned into a
//! voice print by a speaker-verification network ([`Embedder`]), and the prints are grouped
//! ([`cluster`]). The network is WeSpeaker's VoxCeleb ResNet34, the embedding half of
//! pyannote's pipeline; its graph runs on the Qualcomm HTP NPU through [`qnn_rs`]
//! ([`QnnEmbedder`]), and `tests/whisper-host` runs the same weights as fp32 ONNX behind the
//! same trait. The features ([`Fbank`]), the planning and the grouping are pure and tested on
//! the host; `scripts/speaker-export` holds the export and the Python reference they are
//! checked against.
//!
//! What it cannot do: a stretch gets one speaker, so two people inside one transcript segment
//! are both given to whoever spoke more of it, and people talking over each other are not
//! separated. Voices that sound alike can be merged; the count can be set by hand instead of
//! found ([`cluster`]'s `fixed`), which is cheap because the prints are kept ([`Prints`]).

mod cluster;
mod error;
mod fbank;
pub mod pack;
mod prints;
pub mod qnn;
mod window;

pub use cluster::{cluster, Labels, MAX_SPEAKERS};
pub use error::{Error, Result};
pub use fbank::{Fbank, FRAMES, MELS};
pub use pack::{SpeakerPack, MARKER, MODEL};
pub use prints::Prints;
pub use qnn::QnnEmbedder;
pub use window::{fill, plan, Span, Window, WINDOW_SAMPLES};

pub const SAMPLE_RATE: u32 = 16_000;
/// Numbers in one voice print.
pub const DIM: usize = 256;

/// Something that turns two seconds of audio into a voice print.
pub trait Embedder {
    /// `pcm` is exactly [`WINDOW_SAMPLES`] of mono 16 kHz audio in -1..1; `print` receives
    /// [`DIM`] numbers. They are deliberately not normalised: how long the vector is says how
    /// sure the network is of the window, and [`cluster`] leans on that.
    fn embed(&mut self, pcm: &[f32], print: &mut [f32]) -> Result<()>;
}
