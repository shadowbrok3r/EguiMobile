//! The HTP backend: both graphs on the Qualcomm NPU through [`qnn_rs`].
//!
//! Everything about the model — mel bins, layers, heads, vocabulary, context length — is read
//! from the graphs' tensor metadata, so any size of Qualcomm's Whisper export loads without a
//! config file, and a binary with some other layout is refused with the tensor it broke on.
//!
//! The caches never leave the graphs' own element type. A decoder run takes every cross cache
//! the encoder produced (30 MB for turbo) and returns every self cache, per token; passing those
//! through f32 would cost several times what the NPU spends on the token. They live here as raw
//! buffers: the encoder writes the cross caches straight into the decoder's inputs, and the
//! self caches ping-pong between an input and an output set.

use crate::engine::{mask_slot, Engine, ModelDims, MASK_NEG};
use crate::error::{Error, Result};
use crate::mel::N_FRAMES;
use crate::pack::{WhisperPack, DECODER, ENCODER};
use memmap2::Mmap;
use qnn_rs::{quant, Context, ContextOpts, DataType, GraphInfo, QnnSystem, Session, TensorInfo};

/// Element type of a graph's float tensors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Float {
    F16,
    F32,
}

impl Float {
    fn of(part: &'static str, t: &TensorInfo) -> Result<Self> {
        match t.dtype {
            DataType::Float16 => Ok(Float::F16),
            DataType::Float32 => Ok(Float::F32),
            other => Err(Error::contract(part, format!("tensor {} is {other:?}, expected a float type", t.name))),
        }
    }

    fn width(self) -> usize {
        match self {
            Float::F16 => 2,
            Float::F32 => 4,
        }
    }

    fn put(self, bytes: &mut [u8], index: usize, value: f32) {
        match self {
            Float::F16 => bytes[index * 2..index * 2 + 2].copy_from_slice(&quant::f32_to_f16(value).to_le_bytes()),
            Float::F32 => bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes()),
        }
    }

    fn fill(self, bytes: &mut [u8], value: f32) {
        for index in 0..bytes.len() / self.width() {
            self.put(bytes, index, value);
        }
    }

    fn encode(self, values: &[f32], bytes: &mut [u8]) {
        match self {
            Float::F16 => {
                for (v, b) in values.iter().zip(bytes.chunks_exact_mut(2)) {
                    b.copy_from_slice(&quant::f32_to_f16(*v).to_le_bytes());
                }
            }
            Float::F32 => {
                for (v, b) in values.iter().zip(bytes.chunks_exact_mut(4)) {
                    b.copy_from_slice(&v.to_le_bytes());
                }
            }
        }
    }

    fn decode(self, bytes: &[u8], values: &mut [f32]) {
        match self {
            Float::F16 => {
                for (v, b) in values.iter_mut().zip(bytes.chunks_exact(2)) {
                    *v = quant::f16_to_f32(u16::from_le_bytes([b[0], b[1]]));
                }
            }
            Float::F32 => {
                for (v, b) in values.iter_mut().zip(bytes.chunks_exact(4)) {
                    *v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                }
            }
        }
    }
}

fn tensor<'g>(part: &'static str, list: &'g [TensorInfo], name: &str) -> Result<&'g TensorInfo> {
    list.iter().find(|t| t.name == name).ok_or_else(|| Error::contract(part, format!("no tensor named {name}")))
}

fn expect_dims(part: &'static str, t: &TensorInfo, dims: &[usize]) -> Result<()> {
    if t.dims.iter().map(|&d| d as usize).eq(dims.iter().copied()) {
        Ok(())
    } else {
        Err(Error::contract(part, format!("tensor {} is {:?}, expected {dims:?}", t.name, t.dims)))
    }
}

fn expect_int32(part: &'static str, t: &TensorInfo, dims: &[usize]) -> Result<()> {
    if t.dtype != DataType::Int32 {
        return Err(Error::contract(part, format!("tensor {} is {:?}, expected Int32", t.name, t.dtype)));
    }
    expect_dims(part, t, dims)
}

fn sole_graph<'c>(part: &'static str, ctx: &'c Context<'_>) -> Result<&'c GraphInfo> {
    match ctx.info().graphs.as_slice() {
        [graph] => Ok(graph),
        graphs => Err(Error::contract(part, format!("context holds {} graphs, expected 1", graphs.len()))),
    }
}

/// Read a model's shape off its two graphs and check every tensor the engine will bind.
fn read_contract(encoder: &GraphInfo, decoder: &GraphInfo) -> Result<(ModelDims, Float)> {
    let [features] = encoder.inputs.as_slice() else {
        return Err(Error::contract("encoder", format!("{} inputs, expected 1", encoder.inputs.len())));
    };
    let float = Float::of("encoder", features)?;
    let [1, n_mels, frames] = features.dims.as_slice() else {
        return Err(Error::contract("encoder", format!("input {} is {:?}, expected [1, mels, {N_FRAMES}]", features.name, features.dims)));
    };
    if *frames as usize != N_FRAMES {
        return Err(Error::contract("encoder", format!("input holds {frames} frames, expected {N_FRAMES}")));
    }

    let n_layers = encoder.outputs.iter().filter(|t| t.name.starts_with("k_cache_cross_")).count();
    if n_layers == 0 || encoder.outputs.len() != n_layers * 2 {
        return Err(Error::contract("encoder", format!("{} outputs do not pair into cross caches", encoder.outputs.len())));
    }
    let k0 = tensor("encoder", &encoder.outputs, "k_cache_cross_0")?;
    let [n_heads, 1, head_dim, n_audio_ctx] = k0.dims.as_slice() else {
        return Err(Error::contract("encoder", format!("k_cache_cross_0 is {:?}, expected [heads, 1, head_dim, frames]", k0.dims)));
    };
    let (n_heads, head_dim, n_audio_ctx) = (*n_heads as usize, *head_dim as usize, *n_audio_ctx as usize);

    let mask = tensor("decoder", &decoder.inputs, "attention_mask")?;
    let [1, 1, 1, n_text_ctx] = mask.dims.as_slice() else {
        return Err(Error::contract("decoder", format!("attention_mask is {:?}, expected [1, 1, 1, context]", mask.dims)));
    };
    let n_text_ctx = *n_text_ctx as usize;
    if n_text_ctx < 8 {
        return Err(Error::contract("decoder", format!("a context of {n_text_ctx} tokens cannot hold a prompt")));
    }
    let logits = tensor("decoder", &decoder.outputs, "logits")?;
    let n_vocab = logits.elem_count() as usize;

    expect_int32("decoder", tensor("decoder", &decoder.inputs, "input_ids")?, &[1, 1])?;
    expect_int32("decoder", tensor("decoder", &decoder.inputs, "position_ids")?, &[1])?;
    let mut floats = vec![mask, logits];
    for i in 0..n_layers {
        for (list, part, name, dims) in [
            (&encoder.outputs, "encoder", format!("k_cache_cross_{i}"), [n_heads, 1, head_dim, n_audio_ctx]),
            (&encoder.outputs, "encoder", format!("v_cache_cross_{i}"), [n_heads, 1, n_audio_ctx, head_dim]),
            (&decoder.inputs, "decoder", format!("k_cache_cross_{i}"), [n_heads, 1, head_dim, n_audio_ctx]),
            (&decoder.inputs, "decoder", format!("v_cache_cross_{i}"), [n_heads, 1, n_audio_ctx, head_dim]),
            (&decoder.inputs, "decoder", format!("k_cache_self_{i}_in"), [n_heads, 1, head_dim, n_text_ctx - 1]),
            (&decoder.inputs, "decoder", format!("v_cache_self_{i}_in"), [n_heads, 1, n_text_ctx - 1, head_dim]),
            (&decoder.outputs, "decoder", format!("k_cache_self_{i}_out"), [n_heads, 1, head_dim, n_text_ctx - 1]),
            (&decoder.outputs, "decoder", format!("v_cache_self_{i}_out"), [n_heads, 1, n_text_ctx - 1, head_dim]),
        ] {
            let t = tensor(part, list, &name)?;
            expect_dims(part, t, &dims)?;
            floats.push(t);
        }
    }
    // One element type across both graphs is what lets the caches pass between them untouched.
    for t in floats {
        if Float::of("decoder", t)? != float {
            return Err(Error::contract("decoder", format!("tensor {} is {:?} but the encoder input is {float:?}", t.name, t.dtype)));
        }
    }
    if decoder.inputs.len() != 3 + n_layers * 4 || decoder.outputs.len() != 1 + n_layers * 2 {
        return Err(Error::contract("decoder", format!("{} inputs and {} outputs for {n_layers} layers", decoder.inputs.len(), decoder.outputs.len())));
    }

    Ok((ModelDims { n_mels: *n_mels as usize, n_layers, n_heads, head_dim, n_audio_ctx, n_text_ctx, n_vocab }, float))
}

/// A Whisper pack resident on the NPU.
pub struct QnnEngine<'s> {
    dims: ModelDims,
    float: Float,
    encoder: Context<'s>,
    decoder: Context<'s>,
    encoder_graph: String,
    decoder_graph: String,
    features_name: String,
    features: Vec<u8>,
    /// Encoder outputs, bound unchanged as decoder inputs under the same names.
    cross: Vec<(String, Vec<u8>)>,
    /// `k/v_cache_self_i_in` and the matching `_out`, index-aligned; swapped after each run.
    self_in: Vec<(String, Vec<u8>)>,
    self_out: Vec<(String, Vec<u8>)>,
    mask: Vec<u8>,
    logits: Vec<u8>,
    position: usize,
    build_id: Option<String>,
    soc_model: Option<u32>,
    // The binaries stay mapped for as long as their contexts live.
    _maps: [Mmap; 2],
}

impl<'s> QnnEngine<'s> {
    /// Create both contexts on `session` and size the cache buffers. Seconds for turbo.
    pub fn load(session: &'s Session<'_>, system: &QnnSystem, pack: &WhisperPack) -> Result<Self> {
        let maps = [pack.map(ENCODER)?, pack.map(DECODER)?];
        let encoder = session.load_context(system, &maps[0], &ContextOpts::default())?;
        let decoder = session.load_context(system, &maps[1], &ContextOpts::default())?;
        let (encoder_info, decoder_info) = (sole_graph("encoder", &encoder)?.clone(), sole_graph("decoder", &decoder)?.clone());
        let (dims, float) = read_contract(&encoder_info, &decoder_info)?;

        let width = float.width();
        let (heads, head_dim) = (dims.n_heads, dims.head_dim);
        let cache = |len: usize| vec![0u8; heads * head_dim * len * width];
        let mut cross = Vec::with_capacity(dims.n_layers * 2);
        let (mut self_in, mut self_out) = (Vec::with_capacity(dims.n_layers * 2), Vec::with_capacity(dims.n_layers * 2));
        for i in 0..dims.n_layers {
            for kv in ["k", "v"] {
                cross.push((format!("{kv}_cache_cross_{i}"), cache(dims.n_audio_ctx)));
                self_in.push((format!("{kv}_cache_self_{i}_in"), cache(dims.n_text_ctx - 1)));
                self_out.push((format!("{kv}_cache_self_{i}_out"), cache(dims.n_text_ctx - 1)));
            }
        }
        let info = encoder.info();
        let (build_id, soc_model) = (info.build_id.clone(), info.soc_model);
        let mut engine = Self {
            features_name: encoder_info.inputs[0].name.clone(),
            features: vec![0u8; dims.n_mels * N_FRAMES * width],
            mask: vec![0u8; dims.n_text_ctx * width],
            logits: vec![0u8; dims.n_vocab * width],
            encoder_graph: encoder_info.name,
            decoder_graph: decoder_info.name,
            dims,
            float,
            encoder,
            decoder,
            cross,
            self_in,
            self_out,
            position: 0,
            build_id,
            soc_model,
            _maps: maps,
        };
        engine.reset();
        Ok(engine)
    }

    /// The QAIRT build that compiled the pack and the SoC it targets, for diagnostics.
    pub fn provenance(&self) -> String {
        format!(
            "{} for soc {} ({:?} tensors)",
            self.build_id.as_deref().unwrap_or("unknown build"),
            self.soc_model.map_or("?".into(), |m| m.to_string()),
            self.float
        )
    }

    /// Bytes of cache passed to and from the NPU on every decoder run.
    pub fn bytes_per_step(&self) -> usize {
        let sum = |list: &[(String, Vec<u8>)]| list.iter().map(|(_, b)| b.len()).sum::<usize>();
        sum(&self.cross) + sum(&self.self_in) + sum(&self.self_out) + self.mask.len() + self.logits.len()
    }
}

impl Engine for QnnEngine<'_> {
    fn dims(&self) -> &ModelDims {
        &self.dims
    }

    fn encode(&mut self, mel: &[f32]) -> Result<()> {
        if mel.len() != self.dims.n_mels * N_FRAMES {
            return Err(Error::Msg(format!("{} feature values, expected {}", mel.len(), self.dims.n_mels * N_FRAMES)));
        }
        self.float.encode(mel, &mut self.features);
        let inputs = [(self.features_name.as_str(), &self.features[..])];
        let mut outputs: Vec<(&str, &mut [u8])> = self.cross.iter_mut().map(|(name, buf)| (name.as_str(), &mut buf[..])).collect();
        self.encoder.execute_raw(&self.encoder_graph, &inputs, &mut outputs)?;
        self.reset();
        Ok(())
    }

    fn reset(&mut self) {
        // Masked slots are never attended to, but a stale non-finite value would still poison
        // the weighted sum, so each sequence starts from clean caches.
        for (_, buf) in &mut self.self_in {
            buf.fill(0);
        }
        self.float.fill(&mut self.mask, MASK_NEG);
        self.position = 0;
    }

    fn position(&self) -> usize {
        self.position
    }

    fn step(&mut self, token: u32, logits: &mut [f32]) -> Result<()> {
        if self.position >= self.dims.max_steps() {
            return Err(Error::Msg(format!("decoder context of {} tokens is full", self.dims.max_steps())));
        }
        self.float.put(&mut self.mask, mask_slot(&self.dims, self.position), 0.0);
        let (ids, position) = ((token as i32).to_le_bytes(), (self.position as i32).to_le_bytes());
        let mut inputs: Vec<(&str, &[u8])> = Vec::with_capacity(3 + self.cross.len() + self.self_in.len());
        inputs.extend([("input_ids", &ids[..]), ("position_ids", &position[..]), ("attention_mask", &self.mask[..])]);
        inputs.extend(self.cross.iter().chain(&self.self_in).map(|(name, buf)| (name.as_str(), &buf[..])));
        let mut outputs: Vec<(&str, &mut [u8])> = Vec::with_capacity(1 + self.self_out.len());
        outputs.push(("logits", &mut self.logits[..]));
        outputs.extend(self.self_out.iter_mut().map(|(name, buf)| (name.as_str(), &mut buf[..])));
        self.decoder.execute_raw(&self.decoder_graph, &inputs, &mut outputs)?;

        for ((_, fed), (_, produced)) in self.self_in.iter_mut().zip(&mut self.self_out) {
            std::mem::swap(fed, produced);
        }
        self.float.decode(&self.logits, logits);
        self.position += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, dims: &[u32], dtype: DataType) -> TensorInfo {
        TensorInfo { name: name.into(), id: 0, dims: dims.to_vec(), dtype, quant: None }
    }

    /// The tensor layout `dump_context` prints for whisper-base's Qualcomm export.
    fn base(float: DataType) -> (GraphInfo, GraphInfo) {
        let (heads, dim, layers) = (8, 64, 6);
        let mut enc_out = Vec::new();
        let mut dec_in = vec![t("input_ids", &[1, 1], DataType::Int32), t("position_ids", &[1], DataType::Int32)];
        let mut dec_out = Vec::new();
        for i in 0..layers {
            enc_out.push(t(&format!("k_cache_cross_{i}"), &[heads, 1, dim, 1500], float));
            enc_out.push(t(&format!("v_cache_cross_{i}"), &[heads, 1, 1500, dim], float));
            dec_in.push(t(&format!("k_cache_self_{i}_in"), &[heads, 1, dim, 199], float));
            dec_in.push(t(&format!("v_cache_self_{i}_in"), &[heads, 1, 199, dim], float));
            if i == 0 {
                dec_in.push(t("attention_mask", &[1, 1, 1, 200], float));
            }
            dec_in.push(t(&format!("k_cache_cross_{i}"), &[heads, 1, dim, 1500], float));
            dec_in.push(t(&format!("v_cache_cross_{i}"), &[heads, 1, 1500, dim], float));
            dec_out.push(t(&format!("k_cache_self_{i}_out"), &[heads, 1, dim, 199], float));
            dec_out.push(t(&format!("v_cache_self_{i}_out"), &[heads, 1, 199, dim], float));
        }
        dec_out.push(t("logits", &[1, 51865, 1, 1], float));
        (
            GraphInfo { name: "hf_whisper_encoder".into(), inputs: vec![t("input_features", &[1, 80, 3000], float)], outputs: enc_out },
            GraphInfo { name: "hf_whisper_decoder".into(), inputs: dec_in, outputs: dec_out },
        )
    }

    #[test]
    fn dims_come_from_the_graphs() {
        let (encoder, decoder) = base(DataType::Float16);
        let (dims, float) = read_contract(&encoder, &decoder).unwrap();
        assert_eq!(dims, ModelDims { n_mels: 80, n_layers: 6, n_heads: 8, head_dim: 64, n_audio_ctx: 1500, n_text_ctx: 200, n_vocab: 51865 });
        assert_eq!(float, Float::F16);
        assert_eq!(dims.max_steps(), 199);
        assert_eq!(read_contract(&base(DataType::Float32).0, &base(DataType::Float32).1).unwrap().1, Float::F32);
    }

    #[test]
    fn a_foreign_layout_names_the_tensor_it_broke_on() {
        let (encoder, mut decoder) = base(DataType::Float16);
        decoder.inputs.retain(|t| t.name != "v_cache_self_3_in");
        let err = read_contract(&encoder, &decoder).unwrap_err().to_string();
        assert!(err.contains("v_cache_self_3_in"), "{err}");

        let (mut encoder, decoder) = base(DataType::Float16);
        encoder.inputs[0].dims = vec![1, 80, 1500];
        assert!(read_contract(&encoder, &decoder).unwrap_err().to_string().contains("1500 frames"));

        let (encoder, mut decoder) = base(DataType::Float16);
        decoder.inputs[0].dtype = DataType::Int64;
        assert!(read_contract(&encoder, &decoder).unwrap_err().to_string().contains("input_ids"));

        // A quantized export has fixed-point caches the raw path cannot pass through.
        let (encoder, decoder) = base(DataType::UFixedPoint16);
        assert!(read_contract(&encoder, &decoder).unwrap_err().to_string().contains("expected a float type"));
    }

    #[test]
    fn mixed_precision_between_the_graphs_is_refused() {
        let (encoder, _) = base(DataType::Float16);
        let (_, decoder) = base(DataType::Float32);
        assert!(read_contract(&encoder, &decoder).is_err());
    }

    #[test]
    fn float_buffers_round_trip_in_both_widths() {
        for float in [Float::F16, Float::F32] {
            let values = [0.0f32, -100.0, 1.5, -0.25, 3.0];
            let mut bytes = vec![0u8; values.len() * float.width()];
            float.encode(&values, &mut bytes);
            let mut back = [0f32; 5];
            float.decode(&bytes, &mut back);
            assert_eq!(back, values);
            float.fill(&mut bytes, MASK_NEG);
            float.put(&mut bytes, 4, 0.0);
            float.decode(&bytes, &mut back);
            assert_eq!(back, [MASK_NEG, MASK_NEG, MASK_NEG, MASK_NEG, 0.0]);
        }
    }

    #[test]
    fn the_mask_opens_from_the_tail() {
        let dims = read_contract(&base(DataType::Float16).0, &base(DataType::Float16).1).unwrap().0;
        assert_eq!(mask_slot(&dims, 0), 199);
        assert_eq!(mask_slot(&dims, 198), 1);
    }
}
