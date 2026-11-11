//! Host reference backend for local-whisper: the same two graphs as fp32 ONNX, run by ONNX
//! Runtime on the CPU.
//!
//! `scripts/whisper-export/reference.py export` rebuilds Qualcomm's HfWhisper encoder and
//! KV-cache decoder — the modules the HTP context binaries were compiled from, with the same
//! tensor names and shapes — as ONNX. Running them here puts the whole pipeline (features,
//! prompt, rules, seek, vocabulary) under `cargo test` on a machine with no NPU. Only the
//! tensor plumbing differs from `local_whisper::QnnEngine`, and that half is small.
//!
//! [`OnnxEmbedder`] does the same for local-speaker: the speaker network's own fp32 ONNX
//! behind that crate's `Embedder` trait, fed by that crate's features.
//!
//! A crate of its own, outside the workspace, so ONNX Runtime and the TLS stack its build
//! script downloads it with never enter the workspace lockfile or an Android build.

use local_whisper::{mask_slot, Engine, Error, ModelDims, Result, MASK_NEG, N_FRAMES};
use ort::session::{Session, SessionInputValue};
use ort::value::TensorRef;
use std::borrow::Cow;
use std::path::Path;

fn ort_err(e: ort::Error) -> Error {
    Error::Msg(format!("onnxruntime: {e}"))
}

/// One numeric field of the flat `dims.json` the export writes.
fn field(json: &str, key: &str) -> Result<usize> {
    let at = json.find(&format!("\"{key}\"")).ok_or_else(|| Error::Msg(format!("dims.json has no {key}")))?;
    let digits: String = json[at + key.len() + 2..].chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
    digits.parse().map_err(|_| Error::Msg(format!("dims.json: {key} is not a number")))
}

struct Cache {
    name: String,
    /// Name of the decoder output that replaces this cache after a run; empty for cross caches.
    fed_by: String,
    shape: [i64; 4],
    data: Vec<f32>,
}

/// An exported model directory (`encoder.onnx`, `decoder.onnx`, `dims.json`) loaded for CPU.
pub struct OnnxEngine {
    dims: ModelDims,
    encoder: Session,
    decoder: Session,
    cross: Vec<Cache>,
    selfs: Vec<Cache>,
    mask: Vec<f32>,
    position: usize,
}

impl OnnxEngine {
    pub fn load(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let json = std::fs::read_to_string(dir.join("dims.json"))?;
        let (n_layers, n_heads, d_model) = (field(&json, "n_layers")?, field(&json, "n_heads")?, field(&json, "d_model")?);
        let dims = ModelDims {
            n_mels: field(&json, "n_mels")?,
            n_layers,
            n_heads,
            head_dim: d_model / n_heads,
            n_audio_ctx: N_FRAMES / 2,
            n_text_ctx: 200,
            n_vocab: field(&json, "n_vocab")?,
        };
        let session = |name: &str| Session::builder().and_then(|b| b.commit_from_file(dir.join(name))).map_err(ort_err);
        let (heads, head_dim) = (dims.n_heads as i64, dims.head_dim as i64);
        let cache = |name: String, fed_by: String, len: usize, keys: bool| {
            let len = len as i64;
            let shape = if keys { [heads, 1, head_dim, len] } else { [heads, 1, len, head_dim] };
            Cache { name, fed_by, shape, data: vec![0.0; (heads * head_dim * len) as usize] }
        };
        let (mut cross, mut selfs) = (Vec::new(), Vec::new());
        for i in 0..n_layers {
            for kv in ["k", "v"] {
                cross.push(cache(format!("{kv}_cache_cross_{i}"), String::new(), dims.n_audio_ctx, kv == "k"));
                selfs.push(cache(format!("{kv}_cache_self_{i}_in"), format!("{kv}_cache_self_{i}_out"), dims.n_text_ctx - 1, kv == "k"));
            }
        }
        let mut engine = Self { encoder: session("encoder.onnx")?, decoder: session("decoder.onnx")?, mask: vec![MASK_NEG; dims.n_text_ctx], dims, cross, selfs, position: 0 };
        engine.reset();
        Ok(engine)
    }
}

fn float_input<'a>(shape: &[i64], data: &'a [f32]) -> Result<SessionInputValue<'a>> {
    Ok(TensorRef::from_array_view((shape.to_vec(), data)).map_err(ort_err)?.into())
}

impl Engine for OnnxEngine {
    fn dims(&self) -> &ModelDims {
        &self.dims
    }

    fn encode(&mut self, mel: &[f32]) -> Result<()> {
        if mel.len() != self.dims.n_mels * N_FRAMES {
            return Err(Error::Msg(format!("{} feature values, expected {}", mel.len(), self.dims.n_mels * N_FRAMES)));
        }
        let shape = [1, self.dims.n_mels as i64, N_FRAMES as i64];
        let inputs: Vec<(Cow<'_, str>, SessionInputValue<'_>)> = vec![("input_features".into(), float_input(&shape, mel)?)];
        let outputs = self.encoder.run(inputs).map_err(ort_err)?;
        for cache in &mut self.cross {
            let (_, data) = outputs[cache.name.as_str()].try_extract_tensor::<f32>().map_err(ort_err)?;
            cache.data.copy_from_slice(data);
        }
        drop(outputs);
        self.reset();
        Ok(())
    }

    fn reset(&mut self) {
        for cache in &mut self.selfs {
            cache.data.fill(0.0);
        }
        self.mask.fill(MASK_NEG);
        self.position = 0;
    }

    fn position(&self) -> usize {
        self.position
    }

    fn step(&mut self, token: u32, logits: &mut [f32]) -> Result<()> {
        if self.position >= self.dims.max_steps() {
            return Err(Error::Msg(format!("decoder context of {} tokens is full", self.dims.max_steps())));
        }
        self.mask[mask_slot(&self.dims, self.position)] = 0.0;
        let (ids, position) = ([token as i32], [self.position as i32]);
        let mask_shape = [1, 1, 1, self.dims.n_text_ctx as i64];
        let mut inputs: Vec<(Cow<'_, str>, SessionInputValue<'_>)> = vec![
            ("input_ids".into(), TensorRef::from_array_view((vec![1i64, 1], &ids[..])).map_err(ort_err)?.into()),
            ("position_ids".into(), TensorRef::from_array_view((vec![1i64], &position[..])).map_err(ort_err)?.into()),
            ("attention_mask".into(), float_input(&mask_shape, &self.mask)?),
        ];
        for cache in self.cross.iter().chain(&self.selfs) {
            inputs.push((cache.name.as_str().into(), float_input(&cache.shape, &cache.data)?));
        }
        let outputs = self.decoder.run(inputs).map_err(ort_err)?;
        let (_, data) = outputs["logits"].try_extract_tensor::<f32>().map_err(ort_err)?;
        logits.copy_from_slice(data);
        let mut next: Vec<Vec<f32>> = Vec::with_capacity(self.selfs.len());
        for cache in &self.selfs {
            let (_, data) = outputs[cache.fed_by.as_str()].try_extract_tensor::<f32>().map_err(ort_err)?;
            next.push(data.to_vec());
        }
        drop(outputs);
        for (cache, data) in self.selfs.iter_mut().zip(next) {
            cache.data = data;
        }
        self.position += 1;
        Ok(())
    }
}

/// The speaker-embedding network as fp32 ONNX: `model_static.onnx` from the work directory of
/// `scripts/speaker-export/export.py`, the file the HTP context binary was compiled from.
pub struct OnnxEmbedder {
    session: Session,
    fbank: local_speaker::Fbank,
    features: Vec<f32>,
}

impl OnnxEmbedder {
    pub fn load(model: impl AsRef<Path>) -> local_speaker::Result<Self> {
        let session = Session::builder().and_then(|b| b.commit_from_file(model.as_ref())).map_err(|e| local_speaker::Error::Msg(format!("onnxruntime: {e}")))?;
        Ok(Self { session, fbank: local_speaker::Fbank::new(), features: vec![0.0; local_speaker::FRAMES * local_speaker::MELS] })
    }
}

impl local_speaker::Embedder for OnnxEmbedder {
    fn embed(&mut self, pcm: &[f32], print: &mut [f32]) -> local_speaker::Result<()> {
        let fail = |e: ort::Error| local_speaker::Error::Msg(format!("onnxruntime: {e}"));
        self.fbank.window(pcm, &mut self.features);
        let shape = vec![1i64, local_speaker::FRAMES as i64, local_speaker::MELS as i64];
        let input: SessionInputValue<'_> = TensorRef::from_array_view((shape, &self.features[..])).map_err(fail)?.into();
        let inputs: Vec<(Cow<'_, str>, SessionInputValue<'_>)> = vec![("feats".into(), input)];
        let outputs = self.session.run(inputs).map_err(fail)?;
        let (_, data) = outputs["embs"].try_extract_tensor::<f32>().map_err(fail)?;
        print.copy_from_slice(data);
        Ok(())
    }
}
