//! Typed failures of loading a model and of generating with it, and the concrete remedies the
//! application can offer for them.

use std::path::PathBuf;
use std::time::Duration;

/// Which memory a check or failure concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    /// GPU memory (dedicated VRAM).
    Device,
    /// System RAM (also Apple Silicon unified memory).
    Host,
}

impl MemoryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Device => "GPU memory",
            Self::Host => "system memory",
        }
    }
}

/// Why a model could not be loaded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    #[error("the llama.cpp server program is missing: {}", .0.display())]
    RuntimeMissing(PathBuf),
    #[error("the model file is missing: {}", .0.display())]
    ModelMissing(PathBuf),
    #[error("the llama.cpp server could not be started: {0}")]
    Spawn(String),
    #[error("the {backend} backend could not be initialised: {detail}")]
    BackendUnavailable { backend: String, detail: String },
    #[error("not enough {} to load the model: {detail}", kind.map_or("memory", MemoryKind::label))]
    OutOfMemory {
        kind: Option<MemoryKind>,
        detail: String,
    },
    #[error(
        "the model needs an estimated {} MiB of {} but only {} MiB are free",
        needed / (1024 * 1024),
        kind.label(),
        available / (1024 * 1024)
    )]
    InsufficientMemory {
        kind: MemoryKind,
        needed: u64,
        available: u64,
    },
    #[error("this llama.cpp build does not support the model: {0}")]
    UnsupportedModel(String),
    #[error("the model's chat template is missing or not supported: {0}")]
    ChatTemplate(String),
    #[error("the model file could not be read: {0}")]
    ModelFile(String),
    #[error("the llama.cpp server stopped while loading ({status})")]
    Exited { status: String },
    #[error("the model did not finish loading within {0:?}")]
    Timeout(Duration),
    #[error("loading was cancelled")]
    Cancelled,
    #[error("could not talk to the llama.cpp server: {0}")]
    Transport(String),
}

impl LoadError {
    pub fn is_memory(&self) -> bool {
        matches!(self, Self::OutOfMemory { .. } | Self::InsufficientMemory { .. })
    }
}

/// Why a generation request produced no usable result.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GenerationError {
    #[error("no model is loaded")]
    NotReady,
    #[error("cancelled")]
    Cancelled,
    #[error("the request did not finish within {0:?}")]
    Timeout(Duration),
    #[error("the server sent nothing for {0:?}")]
    Idle(Duration),
    #[error(
        "the request needs {needed} tokens but the context holds {context} \
         ({reserved_output} of them reserved for the answer)"
    )]
    ContextExceeded {
        needed: u32,
        context: u32,
        reserved_output: u32,
        /// The largest parts that cannot be left out, largest first, with their token counts.
        largest: Vec<(String, u32)>,
        remedies: Vec<Remedy>,
    },
    #[error("the model ran out of memory while generating: {0}")]
    OutOfMemory(String),
    #[error("the llama.cpp server stopped unexpectedly ({0})")]
    ServerCrashed(String),
    #[error("the server rejected the request (HTTP {status}): {message}")]
    Rejected { status: u16, message: String },
    #[error("could not talk to the llama.cpp server: {0}")]
    Transport(String),
    #[error("the model's answer is not usable: {problem}")]
    Malformed {
        problem: String,
        /// The answer as received, for inspection and for repair prompts.
        raw: String,
    },
    #[error("the model kept calling tools after {0} rounds")]
    ToolRounds(u32),
    /// A structured answer reached the token limit before it was complete. Reasoning models can
    /// spend the whole budget thinking before they answer.
    #[error(
        "the answer was cut off at the limit of {limit} tokens{}",
        if *reasoning_chars > 0 { " after the model reasoned at length" } else { "" }
    )]
    CutOff {
        limit: u32,
        raw: String,
        /// Characters of reasoning produced before the answer.
        reasoning_chars: usize,
    },
}

/// A concrete change the user can make to recover from a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remedy {
    /// Place fewer layers on the GPU (the rest run from system memory, more slowly).
    FewerGpuLayers(u32),
    /// Use a smaller context window.
    SmallerContext(u32),
    /// Use a larger context window (the model supports it).
    LargerContext(u32),
    /// Install or select a smaller quantization or model.
    SmallerModel,
    /// Run on the CPU backend instead.
    CpuBackend,
    /// Load even though the memory check failed.
    LoadAnyway,
    /// Shorten the named parts of the specification.
    ShortenInput(Vec<String>),
    /// Start the server again.
    Restart,
}

impl Remedy {
    pub fn describe(&self) -> String {
        match self {
            Self::FewerGpuLayers(n) => format!("Place {n} layers on the GPU and the rest in system memory"),
            Self::SmallerContext(n) => format!("Reduce the context to {n} tokens"),
            Self::LargerContext(n) => format!("Increase the context to {n} tokens"),
            Self::SmallerModel => "Choose a smaller quantization or model".into(),
            Self::CpuBackend => "Run on the CPU instead".into(),
            Self::LoadAnyway => "Load anyway".into(),
            Self::ShortenInput(parts) => format!("Shorten {}", parts.join(", ")),
            Self::Restart => "Start the model again".into(),
        }
    }
}

/// The settings a load used, as far as remedies depend on them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadShape {
    pub context: u32,
    /// Layers on the GPU; `None` when the backend is the CPU.
    pub gpu_layers: Option<u32>,
    pub total_layers: Option<u32>,
}

/// Smallest context worth suggesting.
const MIN_CONTEXT: u32 = 4096;

/// Remedies for a load failure, most targeted first.
pub fn remedies_for_load(error: &LoadError, shape: LoadShape) -> Vec<Remedy> {
    let mut out = Vec::new();
    match error {
        LoadError::OutOfMemory { kind, .. } => {
            let device = *kind != Some(MemoryKind::Host);
            if device && let Some(layers) = shape.gpu_layers.filter(|n| *n > 0) {
                let current = shape.total_layers.map_or(layers, |t| layers.min(t));
                out.push(Remedy::FewerGpuLayers(current.saturating_mul(3) / 4));
            }
            if shape.context > MIN_CONTEXT {
                out.push(Remedy::SmallerContext((shape.context / 2).max(MIN_CONTEXT)));
            }
            out.push(Remedy::SmallerModel);
        }
        LoadError::InsufficientMemory { kind, .. } => {
            if *kind == MemoryKind::Device
                && let Some(layers) = shape.gpu_layers.filter(|n| *n > 0)
            {
                out.push(Remedy::FewerGpuLayers(layers.saturating_mul(3) / 4));
            }
            if shape.context > MIN_CONTEXT {
                out.push(Remedy::SmallerContext((shape.context / 2).max(MIN_CONTEXT)));
            }
            out.push(Remedy::SmallerModel);
            out.push(Remedy::LoadAnyway);
        }
        LoadError::BackendUnavailable { .. } => out.push(Remedy::CpuBackend),
        LoadError::UnsupportedModel(_) | LoadError::ChatTemplate(_) => out.push(Remedy::SmallerModel),
        LoadError::Exited { .. } | LoadError::Timeout(_) | LoadError::Transport(_) => {
            out.push(Remedy::Restart)
        }
        LoadError::RuntimeMissing(_)
        | LoadError::ModelMissing(_)
        | LoadError::Spawn(_)
        | LoadError::ModelFile(_)
        | LoadError::Cancelled => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(context: u32, gpu_layers: Option<u32>) -> LoadShape {
        LoadShape {
            context,
            gpu_layers,
            total_layers: Some(28),
        }
    }

    #[test]
    fn gpu_out_of_memory_offers_less_offload_smaller_context_and_model() {
        let e = LoadError::OutOfMemory {
            kind: Some(MemoryKind::Device),
            detail: "cudaMalloc failed".into(),
        };
        assert_eq!(
            remedies_for_load(&e, shape(16384, Some(28))),
            vec![
                Remedy::FewerGpuLayers(21),
                Remedy::SmallerContext(8192),
                Remedy::SmallerModel
            ]
        );
        // "All layers" is requested as a large number; suggestions start from the real count.
        assert_eq!(
            remedies_for_load(&e, shape(4096, Some(999)))[0],
            Remedy::FewerGpuLayers(21)
        );
    }

    #[test]
    fn host_out_of_memory_does_not_suggest_moving_layers_off_the_gpu() {
        let e = LoadError::OutOfMemory {
            kind: Some(MemoryKind::Host),
            detail: String::new(),
        };
        assert_eq!(
            remedies_for_load(&e, shape(4096, Some(10))),
            vec![Remedy::SmallerModel]
        );
    }

    #[test]
    fn failed_memory_check_can_be_overridden() {
        let e = LoadError::InsufficientMemory {
            kind: MemoryKind::Host,
            needed: 1,
            available: 0,
        };
        assert_eq!(
            remedies_for_load(&e, shape(8192, None)).last(),
            Some(&Remedy::LoadAnyway)
        );
    }
}
