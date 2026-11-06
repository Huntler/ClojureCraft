//! The curated catalog of coding models and hardware-fit estimation.

use clojurecraft_hardware::{GIB, MIB, MemoryBudget};
use serde::Deserialize;

use crate::manifest::Artifact;

pub const BUNDLED: &str = include_str!("../../../resources/model-catalog.toml");

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("model catalog is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("model catalog schema version {0} is not supported")]
    Schema(u32),
    #[error("model catalog entry {0}: {1}")]
    Invalid(String, &'static str),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    SmokeTested,
    Unverified,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Quant {
    pub name: String,
    /// Overrides the model's repository for this quantization.
    pub repo: Option<String>,
    pub revision: Option<String>,
    pub files: Vec<ModelFile>,
}

impl Quant {
    pub fn weights_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub family: String,
    pub repo: String,
    pub revision: String,
    pub license: String,
    pub license_note: Option<String>,
    pub architecture: String,
    pub parameters_b: f64,
    pub active_parameters_b: f64,
    pub max_context: u32,
    /// Transformer block count; the unit of partial GPU offload.
    pub layers: u32,
    pub kv_bytes_per_token: u64,
    pub quality_rank: u32,
    pub status: Status,
    pub summary: String,
    pub quants: Vec<Quant>,
}

impl Model {
    /// Download descriptors for every file of `quant`, pinned to the resolved revision.
    pub fn artifacts(&self, quant: &Quant) -> Vec<(String, Artifact)> {
        let repo = quant.repo.as_deref().unwrap_or(&self.repo);
        let rev = quant.revision.as_deref().unwrap_or(&self.revision);
        quant
            .files
            .iter()
            .map(|f| {
                let url = format!("https://huggingface.co/{repo}/resolve/{rev}/{}", f.path);
                (
                    f.path.clone(),
                    Artifact {
                        url,
                        size: f.size,
                        sha256: f.sha256.clone(),
                    },
                )
            })
            .collect()
    }

    /// Estimated memory to run `quant` with a context of `ctx` tokens.
    pub fn estimate(&self, quant: &Quant, ctx: u32) -> MemoryEstimate {
        // Compute buffers grow with the active model width; this is a coarse allowance.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let overhead = 384 * MIB + (self.active_parameters_b * 48.0) as u64 * MIB;
        MemoryEstimate {
            weights: quant.weights_bytes(),
            kv_cache: self.kv_bytes_per_token * u64::from(ctx),
            overhead,
        }
    }
}

/// Estimate for a model known only from its files and GGUF metadata (no parameter counts), with
/// a compute allowance proportional to the weights.
pub fn estimate_from_metadata(weights: u64, kv_bytes_per_token: u64, ctx: u32) -> MemoryEstimate {
    MemoryEstimate {
        weights,
        kv_cache: kv_bytes_per_token * u64::from(ctx),
        overhead: 384 * MIB + weights / 20,
    }
}

/// How a model would be split between GPU and system memory. An estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffloadPlan {
    pub gpu_layers: u32,
    pub total_layers: u32,
    /// Estimated bytes on the GPU (or unified memory).
    pub device_bytes: u64,
    /// Estimated bytes of weights kept in system RAM.
    pub host_bytes: u64,
}

impl OffloadPlan {
    pub fn is_full(&self) -> bool {
        self.gpu_layers >= self.total_layers
    }
}

/// Splits whole layers between the device and system RAM so the device budget is respected. The
/// KV cache and compute buffers are assumed to stay on the device.
pub fn offload_plan(estimate: &MemoryEstimate, layers: u32, budget: &MemoryBudget) -> OffloadPlan {
    let layers = layers.max(1);
    if estimate.total() <= budget.device_bytes {
        return OffloadPlan {
            gpu_layers: layers,
            total_layers: layers,
            device_bytes: estimate.total(),
            host_bytes: 0,
        };
    }
    let per_layer = (estimate.weights / u64::from(layers)).max(1);
    let for_weights = budget
        .device_bytes
        .saturating_sub(estimate.kv_cache + estimate.overhead);
    let gpu_layers = u32::try_from(for_weights / per_layer)
        .unwrap_or(u32::MAX)
        .min(layers);
    let on_device = per_layer * u64::from(gpu_layers);
    OffloadPlan {
        gpu_layers,
        total_layers: layers,
        device_bytes: on_device + estimate.kv_cache + estimate.overhead,
        host_bytes: estimate.weights - on_device,
    }
}

/// Estimated runtime memory; never presented as a measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryEstimate {
    pub weights: u64,
    pub kv_cache: u64,
    pub overhead: u64,
}

impl MemoryEstimate {
    pub fn total(&self) -> u64 {
        self.weights + self.kv_cache + self.overhead
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fit {
    /// Everything fits in the primary device budget.
    Full,
    /// Needs some layers in system RAM; works, but slower.
    PartialOffload,
    /// Exceeds the configured budget.
    TooLarge,
}

pub fn fit(estimate: &MemoryEstimate, budget: &MemoryBudget) -> Fit {
    if estimate.total() <= budget.device_bytes {
        Fit::Full
    } else if estimate.total() <= budget.total_bytes() {
        Fit::PartialOffload
    } else {
        Fit::TooLarge
    }
}

#[derive(Clone, Debug)]
pub struct Candidate<'a> {
    pub model: &'a Model,
    pub quant: &'a Quant,
    pub estimate: MemoryEstimate,
    pub fit: Fit,
    pub offload: OffloadPlan,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Compatibility {
    /// GGUF `general.architecture` values validated with the pinned llama.cpp release.
    pub architectures: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Catalog {
    pub schema_version: u32,
    pub default_context: u32,
    pub compatibility: Compatibility,
    pub models: Vec<Model>,
}

impl Catalog {
    pub fn bundled() -> Result<Self, CatalogError> {
        Self::parse(BUNDLED)
    }

    pub fn parse(text: &str) -> Result<Self, CatalogError> {
        let c: Catalog = toml::from_str(text)?;
        if c.schema_version != 1 {
            return Err(CatalogError::Schema(c.schema_version));
        }
        let mut ids = std::collections::BTreeSet::new();
        for m in &c.models {
            if !ids.insert(&m.id) {
                return Err(CatalogError::Invalid(m.id.clone(), "duplicate id"));
            }
            if !c.supports_architecture(&m.architecture) {
                return Err(CatalogError::Invalid(
                    m.id.clone(),
                    "architecture is not in the compatibility list",
                ));
            }
            if m.layers == 0 || m.max_context == 0 {
                return Err(CatalogError::Invalid(
                    m.id.clone(),
                    "layers and context must be positive",
                ));
            }
            if m.quants.is_empty() {
                return Err(CatalogError::Invalid(m.id.clone(), "no quantizations"));
            }
            if m.revision.len() != 40 {
                return Err(CatalogError::Invalid(
                    m.id.clone(),
                    "revision must be a full commit hash",
                ));
            }
            for q in &m.quants {
                for f in &q.files {
                    let ok_hash = f.sha256.len() == 64 && f.sha256.bytes().all(|b| b.is_ascii_hexdigit());
                    if !ok_hash || f.size == 0 || !f.path.ends_with(".gguf") || f.path.contains("..") {
                        return Err(CatalogError::Invalid(
                            format!("{}/{}", m.id, q.name),
                            "bad file entry",
                        ));
                    }
                }
            }
        }
        Ok(c)
    }

    pub fn supports_architecture(&self, architecture: &str) -> bool {
        self.compatibility.architectures.iter().any(|a| a == architecture)
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Every model/quantization with its estimate and fit, sorted best-first: full fits before
    /// partial offload before too large, then by quality, then by larger quantization.
    pub fn ranked(&self, budget: &MemoryBudget, ctx: u32) -> Vec<Candidate<'_>> {
        let mut out: Vec<Candidate<'_>> = self
            .models
            .iter()
            .flat_map(|m| {
                m.quants.iter().map(move |q| {
                    let estimate = m.estimate(q, ctx.min(m.max_context));
                    Candidate {
                        model: m,
                        quant: q,
                        estimate,
                        fit: fit(&estimate, budget),
                        offload: offload_plan(&estimate, m.layers, budget),
                    }
                })
            })
            .collect();
        out.sort_by(|a, b| {
            a.fit
                .cmp(&b.fit)
                .then(b.model.quality_rank.cmp(&a.model.quality_rank))
                .then(b.quant.weights_bytes().cmp(&a.quant.weights_bytes()))
        });
        out
    }

    /// The default suggestion: the best smoke-tested option that fits fully, else the best that
    /// fits with partial offload. `None` when nothing fits the budget.
    pub fn recommended(&self, budget: &MemoryBudget, ctx: u32) -> Option<Candidate<'_>> {
        let ranked = self.ranked(budget, ctx);
        let tested = |o: &&Candidate<'_>| o.model.status == Status::SmokeTested;
        ranked
            .iter()
            .filter(tested)
            .find(|o| o.fit == Fit::Full)
            .or_else(|| {
                ranked
                    .iter()
                    .filter(tested)
                    .find(|o| o.fit == Fit::PartialOffload)
            })
            .cloned()
    }
}

pub fn format_gib(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let g = bytes as f64 / GIB as f64;
    format!("{g:.1} GiB")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clojurecraft_hardware::{
        Backend, BudgetBasis, CpuInfo, DetectionSource, GpuDevice, GpuVendor, HardwareReport, default_budget,
    };

    fn budget(device_gib: u64, spill_gib: u64) -> MemoryBudget {
        MemoryBudget {
            device_bytes: device_gib * GIB,
            host_spill_bytes: spill_gib * GIB,
            basis: BudgetBasis::DedicatedVram,
        }
    }

    #[test]
    fn bundled_catalog_is_valid() {
        let c = Catalog::bundled().expect("catalog");
        assert!(c.models.len() >= 5);
        assert!(c.models.iter().any(|m| m.status == Status::SmokeTested));
    }

    #[test]
    fn artifacts_are_pinned_to_revision() {
        let c = Catalog::bundled().expect("catalog");
        let m = &c.models[0];
        let arts = m.artifacts(&m.quants[0]);
        assert!(arts[0].1.url.contains(&m.revision));
        assert!(arts[0].1.url.starts_with("https://huggingface.co/"));
    }

    #[test]
    fn eight_gb_gpu_gets_a_model_that_fits_fully() {
        let c = Catalog::bundled().expect("catalog");
        let b = budget(7, 16);
        let rec = c.recommended(&b, 8192).expect("recommendation");
        assert_eq!(rec.fit, Fit::Full);
        assert!(rec.estimate.total() <= b.device_bytes);
    }

    #[test]
    fn reference_mac_keeps_a_30b_class_model_eligible() {
        let c = Catalog::bundled().expect("catalog");
        let b = MemoryBudget {
            device_bytes: 18 * GIB,
            host_spill_bytes: 0,
            basis: BudgetBasis::UnifiedMemory,
        };
        let ranked = c.ranked(&b, 8192);
        assert!(
            ranked
                .iter()
                .any(|o| o.model.parameters_b >= 24.0 && o.fit == Fit::Full),
            "a ~30B model must fit an 18 GiB budget"
        );
    }

    fn report(os: &str, ram_gib: u64, gpus: Vec<GpuDevice>) -> HardwareReport {
        HardwareReport {
            os: os.into(),
            arch: if os == "macos" { "aarch64" } else { "x86_64" }.into(),
            total_ram_bytes: Some(ram_gib * GIB),
            available_ram_bytes: Some(ram_gib * GIB),
            cpu: CpuInfo::default(),
            gpus,
            notes: vec![],
        }
    }

    fn gpu(vendor: GpuVendor, vram_gib: u64) -> GpuDevice {
        GpuDevice {
            vendor,
            name: "test".into(),
            vram_total_bytes: Some(vram_gib * GIB),
            vram_used_bytes: None,
            unified_memory: vendor == GpuVendor::Apple,
            driver_version: None,
            source: DetectionSource::Manual,
        }
    }

    /// The recommendation a machine gets from detection alone.
    fn recommend_for(r: &HardwareReport) -> (Backend, String, Fit) {
        let c = Catalog::bundled().expect("catalog");
        let backend = r.recommended_backend();
        let budget = default_budget(r, backend);
        let rec = c
            .recommended(&budget, c.default_context)
            .expect("a recommendation");
        (backend, format!("{} {}", rec.model.id, rec.quant.name), rec.fit)
    }

    #[test]
    fn sensible_recommendations_for_reference_machines() {
        let mac = report("macos", 36, vec![gpu(GpuVendor::Apple, 36)]);
        let (backend, _, fit) = recommend_for(&mac);
        assert_eq!((backend, fit), (Backend::Metal, Fit::Full));

        let nvidia = report("linux", 32, vec![gpu(GpuVendor::Nvidia, 8)]);
        let (backend, model, fit) = recommend_for(&nvidia);
        assert_eq!((backend, fit), (Backend::Cuda, Fit::Full));
        assert!(model.starts_with("qwen2.5-coder-7b"), "{model}");

        let cpu_only = report("linux", 16, vec![]);
        let (backend, model, fit) = recommend_for(&cpu_only);
        assert_eq!(backend, Backend::Cpu);
        assert_eq!(fit, Fit::Full);
        // Half of 16 GiB: a 7B model fits, a 14B or larger does not.
        assert!(model.starts_with("qwen2.5-coder-7b"), "{model}");

        let small_cpu = report("linux", 6, vec![]);
        let (_, model, _) = recommend_for(&small_cpu);
        assert!(model.starts_with("qwen2.5-coder-1.5b"), "{model}");
    }

    #[test]
    fn partial_offload_keeps_the_device_within_budget() {
        let c = Catalog::bundled().expect("catalog");
        let m = c.model("qwen3-coder-30b-a3b-instruct").expect("model");
        let q = &m.quants[0];
        let b = budget(7, 16);
        let est = m.estimate(q, 8192);
        let plan = offload_plan(&est, m.layers, &b);
        assert!(!plan.is_full());
        assert!(plan.gpu_layers > 0 && plan.gpu_layers < m.layers);
        assert!(plan.device_bytes <= b.device_bytes);
        assert_eq!(fit(&est, &b), Fit::PartialOffload);
        // A model that fits gets every layer.
        let small = c.model("qwen2.5-coder-1.5b-instruct").expect("model");
        let full = offload_plan(&small.estimate(&small.quants[0], 8192), small.layers, &b);
        assert!(full.is_full());
        assert_eq!(full.host_bytes, 0);
    }

    #[test]
    fn catalog_architectures_must_be_validated() {
        let text = BUNDLED.replace("architectures = [\"qwen2\", ", "architectures = [");
        assert!(matches!(Catalog::parse(&text), Err(CatalogError::Invalid(_, _))));
    }

    #[test]
    fn tiny_budget_recommends_nothing() {
        let c = Catalog::bundled().expect("catalog");
        assert!(c.recommended(&budget(0, 0), 4096).is_none());
    }

    #[test]
    fn ranking_puts_full_fits_first() {
        let c = Catalog::bundled().expect("catalog");
        let ranked = c.ranked(&budget(7, 16), 8192);
        let fits: Vec<Fit> = ranked.iter().map(|o| o.fit).collect();
        let mut sorted = fits.clone();
        sorted.sort();
        assert_eq!(fits, sorted);
    }
}
