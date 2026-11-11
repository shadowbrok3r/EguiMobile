//! On-device Whisper speech-to-text.
//!
//! The log-mel front end ([`mel`]), the vocabulary ([`vocab`]), OpenAI's decoding rules
//! ([`decode`]) and the timestamp-seeking long-form loop ([`longform`]) are pure and
//! host-testable. The two graphs — an encoder that emits cross-attention caches and a
//! one-token-per-run KV-cache decoder, as exported by Qualcomm AI Hub — run on the Qualcomm
//! HTP NPU through [`qnn_rs`] ([`QnnEngine`]). Device paths compile on host and
//! `aarch64-android` but need an NPU, the QNN libs and a pack at runtime; `tests/whisper-host`
//! runs the same graphs as fp32 ONNX behind the [`Engine`] trait, so the whole pipeline is
//! testable without one.
//!
//! ```no_run
//! use local_whisper::{Advance, Backend, DecodeOptions, LongForm, QnnEngine, QnnSystem, Session, WhisperPack, prepare_htp_env};
//! # fn f(audio: &[f32]) -> local_whisper::Result<()> {
//! let lib_dir = std::path::Path::new("/data/local/tmp/qnn");
//! prepare_htp_env(lib_dir);
//! let system = QnnSystem::load(lib_dir.join("libQnnSystem.so"))?;
//! let backend = Backend::load(lib_dir.join("libQnnHtp.so"))?;
//! let session = Session::new(&backend)?;
//! session.set_htp_performance_mode()?;
//! let pack = WhisperPack::open("/sdcard/.../whisper-large-v3-turbo")?;
//! let mut engine = QnnEngine::load(&session, &system, &pack)?;
//!
//! let cancel = std::sync::atomic::AtomicBool::new(false);
//! let mut long = LongForm::new(local_whisper::Engine::dims(&engine), DecodeOptions::default())?;
//! let mut source = audio; // mono 16 kHz
//! while let Advance::Window(window) = long.advance(&mut engine, &mut source, audio.len() as u64, true, &cancel)? {
//!     for s in &window.segments { println!("[{:.2} -> {:.2}] {}", s.start, s.end, s.text); }
//! }
//! # Ok(()) }
//! ```

pub mod decode;
pub mod engine;
mod error;
pub mod longform;
pub mod mel;
pub mod pack;
pub mod qnn;
pub mod vocab;

pub use decode::{compression_ratio, DecodeOptions, Decoded, Decoder, Task, Timings};
pub use engine::{mask_slot, Engine, ModelDims, MASK_NEG};
pub use error::{Error, Result};
pub use longform::{join_text, Advance, AudioSource, LongForm, Outcome, Segment, Window};
pub use mel::{MelFrontEnd, CHUNK_SAMPLES, N_FRAMES, SAMPLE_RATE};
pub use pack::{WhisperPack, DECODER, ENCODER, MARKER};
pub use qnn::QnnEngine;
pub use qnn_rs::{fastrpc_available, prepare_htp_env, Backend, BurstVote, Context as QnnContext, ContextOpts, QnnSystem, Session};
pub use vocab::{Vocab, LANGUAGES};
