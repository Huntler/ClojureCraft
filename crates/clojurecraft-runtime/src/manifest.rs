//! The runtime manifest: which llama.cpp, JRE, and Clojure artifacts are trusted.

use std::collections::BTreeSet;

use clojurecraft_hardware::Backend;
use serde::Deserialize;

/// The manifest shipped with this build of the application.
pub const BUNDLED: &str = include_str!("../../../resources/runtime-manifest.toml");

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("runtime manifest is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("runtime manifest schema version {0} is not supported")]
    Schema(u32),
    #[error("runtime manifest entry {0}: {1}")]
    Invalid(String, &'static str),
}

/// One downloadable file with the facts needed to verify it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LlamaAsset {
    pub platform: String,
    pub backend: Backend,
    #[serde(flatten)]
    pub artifact: Artifact,
    /// Additional archives merged into the installation (e.g. the CUDA runtime libraries).
    #[serde(default)]
    pub extra: Vec<ExtraArchive>,
}

/// An archive whose contents are merged into a directory of the main installation.
#[derive(Clone, Debug, Deserialize)]
pub struct ExtraArchive {
    #[serde(flatten)]
    pub artifact: Artifact,
    /// Directory (relative to the installation root) that receives the contents.
    pub extract_to: String,
    /// Leading path components removed from every entry.
    #[serde(default)]
    pub strip_components: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LlamaCpp {
    pub release: String,
    pub commit: String,
    pub homepage: String,
    pub license: String,
    pub server_path: String,
    pub assets: Vec<LlamaAsset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct JreAsset {
    pub platform: String,
    #[serde(flatten)]
    pub artifact: Artifact,
    pub java_path: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Jre {
    pub vendor: String,
    pub version: String,
    pub homepage: String,
    pub license: String,
    pub assets: Vec<JreAsset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Jar {
    pub name: String,
    #[serde(flatten)]
    pub artifact: Artifact,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Clojure {
    pub version: String,
    pub homepage: String,
    pub license: String,
    pub jars: Vec<Jar>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub llama_cpp: LlamaCpp,
    pub jre: Jre,
    pub clojure: Clojure,
}

impl Manifest {
    pub fn bundled() -> Result<Self, ManifestError> {
        Self::parse(BUNDLED)
    }

    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let m: Manifest = toml::from_str(text)?;
        m.validate()?;
        Ok(m)
    }

    pub fn llama_asset(&self, platform: &str, backend: Backend) -> Option<&LlamaAsset> {
        self.llama_cpp
            .assets
            .iter()
            .find(|a| a.platform == platform && a.backend == backend)
    }

    /// Backends with a pinned llama.cpp build for `platform`.
    pub fn llama_backends(&self, platform: &str) -> Vec<Backend> {
        self.llama_cpp
            .assets
            .iter()
            .filter(|a| a.platform == platform)
            .map(|a| a.backend)
            .collect()
    }

    pub fn jre_asset(&self, platform: &str) -> Option<&JreAsset> {
        self.jre.assets.iter().find(|a| a.platform == platform)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != 1 {
            return Err(ManifestError::Schema(self.schema_version));
        }
        let mut seen = BTreeSet::new();
        for a in &self.llama_cpp.assets {
            let key = format!("llama.cpp {}/{:?}", a.platform, a.backend);
            if !seen.insert(key.clone()) {
                return Err(ManifestError::Invalid(key, "duplicate platform/backend"));
            }
            for art in std::iter::once(&a.artifact).chain(a.extra.iter().map(|e| &e.artifact)) {
                check_artifact(&key, art)?;
            }
            for e in &a.extra {
                check_relative(&e.extract_to)?;
            }
        }
        for a in &self.jre.assets {
            check_artifact(&format!("jre {}", a.platform), &a.artifact)?;
        }
        for j in &self.clojure.jars {
            check_artifact(&j.name, &j.artifact)?;
        }
        check_relative(&self.llama_cpp.server_path)?;
        for a in &self.jre.assets {
            check_relative(&a.java_path)?;
        }
        Ok(())
    }
}

fn check_artifact(key: &str, a: &Artifact) -> Result<(), ManifestError> {
    if !a.url.starts_with("https://") {
        return Err(ManifestError::Invalid(key.to_owned(), "URL must use https"));
    }
    if a.sha256.len() != 64
        || !a
            .sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(ManifestError::Invalid(
            key.to_owned(),
            "sha256 must be 64 lowercase hex digits",
        ));
    }
    if a.size == 0 {
        return Err(ManifestError::Invalid(key.to_owned(), "size must be positive"));
    }
    Ok(())
}

fn check_relative(p: &str) -> Result<(), ManifestError> {
    let path = std::path::Path::new(p);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ManifestError::Invalid(
            p.to_owned(),
            "paths must stay inside the archive",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_manifest_is_valid_and_covers_supported_platforms() {
        let m = Manifest::bundled().expect("bundled manifest");
        assert!(m.llama_asset("linux-x86_64", Backend::Cpu).is_some());
        assert!(m.llama_asset("linux-x86_64", Backend::Cuda).is_some());
        assert!(m.llama_asset("linux-x86_64", Backend::Vulkan).is_some());
        assert!(m.llama_asset("macos-aarch64", Backend::Metal).is_some());
        assert!(m.jre_asset("linux-x86_64").is_some());
        assert!(m.jre_asset("macos-aarch64").is_some());
        assert!(!m.clojure.jars.is_empty());
    }

    fn with_llama_asset(asset: &str) -> String {
        let base = BUNDLED.split("[[llama_cpp.assets]]").next().unwrap_or_default();
        let rest = BUNDLED.split_once("[jre]").map(|(_, r)| r).unwrap_or_default();
        format!("{base}[[llama_cpp.assets]]\n{asset}\n[jre]{rest}")
    }

    #[test]
    fn rejects_plain_http() {
        let text = with_llama_asset(&format!(
            "platform = \"linux-x86_64\"\nbackend = \"cpu\"\nurl = \"http://x\"\nsize = 1\nsha256 = \"{}\"",
            "a".repeat(64)
        ));
        assert!(matches!(
            Manifest::parse(&text),
            Err(ManifestError::Invalid(_, _))
        ));
    }

    #[test]
    fn rejects_malformed_hash() {
        let text = with_llama_asset(
            "platform = \"linux-x86_64\"\nbackend = \"cpu\"\nurl = \"https://x\"\nsize = 1\nsha256 = \"ABC\"",
        );
        assert!(matches!(
            Manifest::parse(&text),
            Err(ManifestError::Invalid(_, _))
        ));
    }

    #[test]
    fn rejects_unknown_schema() {
        let text = BUNDLED.replace("schema_version = 1", "schema_version = 2");
        assert!(matches!(Manifest::parse(&text), Err(ManifestError::Schema(2))));
    }
}
