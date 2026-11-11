//! The two-graph contract every backend implements.
//!
//! Qualcomm's AI Hub export splits Whisper into an encoder that emits the decoder's
//! cross-attention key/value caches directly, and a decoder that advances exactly one token per
//! run over fixed-size self-attention caches. A backend owns those caches in whatever element
//! type its graphs use; the decoding logic above it only ever sees tokens in and logits out.

use crate::error::Result;

/// Shape of one model, read from its graphs rather than configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelDims {
    /// Mel bins the encoder takes: 80 up to large-v2, 128 for large-v3 and turbo.
    pub n_mels: usize,
    /// Decoder layers, each with one self and one cross cache pair.
    pub n_layers: usize,
    pub n_heads: usize,
    pub head_dim: usize,
    /// Encoder output positions per window (1500).
    pub n_audio_ctx: usize,
    /// Width of the decoder's attention mask. The self caches hold one fewer, so a window
    /// decodes at most `n_text_ctx - 1` tokens including the prompt.
    pub n_text_ctx: usize,
    pub n_vocab: usize,
}

impl ModelDims {
    /// Decoder runs available per window.
    pub fn max_steps(&self) -> usize {
        self.n_text_ctx - 1
    }

    /// Short description for logs and the debug panel.
    pub fn summary(&self) -> String {
        format!(
            "{} mels, {} decoder layers, {}x{} heads, {} tokens/window, vocab {}",
            self.n_mels, self.n_layers, self.n_heads, self.head_dim, self.max_steps(), self.n_vocab
        )
    }
}

/// Attention bias of a cache slot that holds no token yet. Qualcomm's export trains nothing on
/// this; it only has to drown the slot in the softmax, and -100 still fits fp16 with room.
pub const MASK_NEG: f32 = -100.0;

/// An encoder/decoder pair that can transcribe one 30 s window at a time.
pub trait Engine {
    fn dims(&self) -> &ModelDims;

    /// Run the encoder over one window of log-mel features (`n_mels * 3000`, mel-major) and
    /// keep its cross-attention caches for the decoder runs that follow.
    fn encode(&mut self, mel: &[f32]) -> Result<()>;

    /// Forget the token sequence so far; the encoded window stays.
    fn reset(&mut self);

    /// Tokens fed since the last [`Engine::reset`].
    fn position(&self) -> usize;

    /// Feed `token` at the next position and write the logits for the one after it into
    /// `logits` (`n_vocab` wide). Fails once [`ModelDims::max_steps`] tokens have been fed.
    fn step(&mut self, token: u32, logits: &mut [f32]) -> Result<()>;
}

/// Index of the attention-mask slot that step `position` opens. The self caches shift left by
/// one each run and the new entry lands at the end, so live slots grow from the tail.
pub fn mask_slot(dims: &ModelDims, position: usize) -> usize {
    dims.n_text_ctx - 1 - position
}
