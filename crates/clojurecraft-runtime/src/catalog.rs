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
        let weights = quant.weights_bytes();
        let kv = self.kv_bytes_per_token * u64::from(ctx);
        // Compute buffers grow with the active model width; this is a coarse allowance.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let overhead = 384 * MIB + (self.active_parameters_b * 48.0) as u64 * MIB;
        MemoryEstimate {
            weights,
            kv_cache: kv,
            overhead,
        }
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
}

#[derive(Clone, Debug, Deserialize)]
pub struct Catalog {
    pub schema_version: u32,
    pub default_context: u32,
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

    /// Every model/quantization with its estimate and fit, sorted best-first: full fits before
    /// partial offload before too large, then by quality, then by larger quantization.
    pub fn ranked(&self, budget: &MemoryBudget, ctx: u32) -> Vec<Candidate<'_>> {
        let mut out: Vec<Candidate<'_>> = self
            .models
            .iter()
            .flat_map(|m| {
                m.quants.iter().map(move |q| {
                    let estimate = m.estimate(q, ctx);
                    Candidate {
                        model: m,
                        quant: q,
                        estimate,
                        fit: fit(&estimate, budget),
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
    use clojurecraft_hardware::BudgetBasis;

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
