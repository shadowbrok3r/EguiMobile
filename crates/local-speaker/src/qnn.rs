//! The HTP backend: the embedding graph on the Qualcomm NPU through [`qnn_rs`].
//!
//! The graph is the one `scripts/speaker-export/export.py` compiles: features in, voice print
//! out, both in the graph's own float type. Its shape is checked against what this crate's
//! front end produces, so a binary exported for some other window is refused by name rather
//! than fed the wrong numbers.

use crate::error::{Error, Result};
use crate::fbank::{Fbank, FRAMES, MELS};
use crate::pack::SpeakerPack;
use crate::window::WINDOW_SAMPLES;
use crate::{Embedder, DIM};
use memmap2::Mmap;
use qnn_rs::{quant, Context, ContextOpts, DataType, GraphInfo, QnnSystem, Session, TensorInfo};

/// Element type of the graph's tensors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Float {
    F16,
    F32,
}

impl Float {
    fn of(tensor: &TensorInfo) -> Result<Self> {
        match tensor.dtype {
            DataType::Float16 => Ok(Float::F16),
            DataType::Float32 => Ok(Float::F32),
            other => Err(Error::Contract(format!("tensor {} is {other:?}, expected a float type", tensor.name))),
        }
    }

    fn width(self) -> usize {
        match self {
            Float::F16 => 2,
            Float::F32 => 4,
        }
    }

    fn encode(self, values: &[f32], bytes: &mut [u8]) {
        match self {
            Float::F16 => values.iter().zip(bytes.chunks_exact_mut(2)).for_each(|(v, b)| b.copy_from_slice(&quant::f32_to_f16(*v).to_le_bytes())),
            Float::F32 => values.iter().zip(bytes.chunks_exact_mut(4)).for_each(|(v, b)| b.copy_from_slice(&v.to_le_bytes())),
        }
    }

    fn decode(self, bytes: &[u8], values: &mut [f32]) {
        match self {
            Float::F16 => values.iter_mut().zip(bytes.chunks_exact(2)).for_each(|(v, b)| *v = quant::f16_to_f32(u16::from_le_bytes([b[0], b[1]]))),
            Float::F32 => values.iter_mut().zip(bytes.chunks_exact(4)).for_each(|(v, b)| *v = f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        }
    }
}

/// Check the graph takes one window of this crate's features and gives one voice print.
fn read_contract(graph: &GraphInfo) -> Result<Float> {
    let ([input], [output]) = (graph.inputs.as_slice(), graph.outputs.as_slice()) else {
        return Err(Error::Contract(format!("{} inputs and {} outputs, expected one of each", graph.inputs.len(), graph.outputs.len())));
    };
    let float = Float::of(input)?;
    if Float::of(output)? != float {
        return Err(Error::Contract(format!("{} is {:?} but {} is {:?}", input.name, input.dtype, output.name, output.dtype)));
    }
    for (tensor, dims) in [(input, &[1, FRAMES, MELS][..]), (output, &[1, DIM][..])] {
        if !tensor.dims.iter().map(|d| *d as usize).eq(dims.iter().copied()) {
            return Err(Error::Contract(format!("tensor {} is {:?}, expected {dims:?}", tensor.name, tensor.dims)));
        }
    }
    Ok(float)
}

/// The speaker network resident on the NPU.
pub struct QnnEmbedder<'s> {
    context: Context<'s>,
    graph: String,
    input: String,
    output: String,
    float: Float,
    fbank: Fbank,
    features: Vec<f32>,
    fed: Vec<u8>,
    read: Vec<u8>,
    build_id: Option<String>,
    // The binary stays mapped for as long as its context lives.
    _map: Mmap,
}

impl<'s> QnnEmbedder<'s> {
    /// Create the context on `session`. A fraction of a second: the graph is 13 MB.
    pub fn load(session: &'s Session<'_>, system: &QnnSystem, pack: &SpeakerPack) -> Result<Self> {
        let map = pack.map()?;
        let context = session.load_context(system, &map, &ContextOpts::default())?;
        let [graph] = context.info().graphs.as_slice() else {
            return Err(Error::Contract(format!("context holds {} graphs, expected 1", context.info().graphs.len())));
        };
        let float = read_contract(graph)?;
        let (name, input, output) = (graph.name.clone(), graph.inputs[0].name.clone(), graph.outputs[0].name.clone());
        let build_id = context.info().build_id.clone();
        Ok(Self {
            context,
            graph: name,
            input,
            output,
            float,
            fbank: Fbank::new(),
            features: vec![0f32; FRAMES * MELS],
            fed: vec![0u8; FRAMES * MELS * float.width()],
            read: vec![0u8; DIM * float.width()],
            build_id,
            _map: map,
        })
    }

    /// The QAIRT build that compiled the graph, for diagnostics.
    pub fn provenance(&self) -> String {
        format!("{} ({:?} tensors)", self.build_id.as_deref().unwrap_or("unknown build"), self.float)
    }
}

impl Embedder for QnnEmbedder<'_> {
    fn embed(&mut self, pcm: &[f32], print: &mut [f32]) -> Result<()> {
        if pcm.len() != WINDOW_SAMPLES || print.len() != DIM {
            return Err(Error::Msg(format!("{} samples into {} numbers, expected {WINDOW_SAMPLES} into {DIM}", pcm.len(), print.len())));
        }
        self.fbank.window(pcm, &mut self.features);
        self.float.encode(&self.features, &mut self.fed);
        let inputs = [(self.input.as_str(), &self.fed[..])];
        let mut outputs = [(self.output.as_str(), &mut self.read[..])];
        self.context.execute_raw(&self.graph, &inputs, &mut outputs)?;
        self.float.decode(&self.read, print);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, dims: &[u32], dtype: DataType) -> TensorInfo {
        TensorInfo { name: name.into(), id: 0, dims: dims.to_vec(), dtype, quant: None }
    }

    fn graph(float: DataType) -> GraphInfo {
        GraphInfo { name: "speaker".into(), inputs: vec![t("feats", &[1, 198, 80], float)], outputs: vec![t("embs", &[1, 256], float)] }
    }

    #[test]
    fn the_exported_layout_is_accepted_in_either_precision() {
        assert_eq!(read_contract(&graph(DataType::Float16)).unwrap(), Float::F16);
        assert_eq!(read_contract(&graph(DataType::Float32)).unwrap(), Float::F32);
    }

    #[test]
    fn a_graph_for_another_window_is_refused_by_name() {
        let mut other = graph(DataType::Float16);
        other.inputs[0].dims = vec![1, 298, 80];
        let err = read_contract(&other).unwrap_err().to_string();
        assert!(err.contains("feats") && err.contains("298"), "{err}");

        let mut wide = graph(DataType::Float16);
        wide.outputs[0].dims = vec![1, 512];
        assert!(read_contract(&wide).unwrap_err().to_string().contains("embs"));

        let mut mixed = graph(DataType::Float16);
        mixed.outputs[0].dtype = DataType::Float32;
        assert!(read_contract(&mixed).is_err());
        // A quantized export has fixed-point tensors this path cannot feed.
        assert!(read_contract(&graph(DataType::UFixedPoint8)).unwrap_err().to_string().contains("expected a float type"));
        let mut two = graph(DataType::Float16);
        two.inputs.push(t("extra", &[1], DataType::Float16));
        assert!(read_contract(&two).unwrap_err().to_string().contains("2 inputs"));
    }

    #[test]
    fn floats_round_trip_in_both_widths() {
        for float in [Float::F16, Float::F32] {
            let values = [0.0f32, -35.25, 1.5, 8.625, -0.125];
            let mut bytes = vec![0u8; values.len() * float.width()];
            float.encode(&values, &mut bytes);
            let mut back = [0f32; 5];
            float.decode(&bytes, &mut back);
            assert_eq!(back, values);
        }
    }
}
