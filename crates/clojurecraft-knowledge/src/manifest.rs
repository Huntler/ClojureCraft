//! The knowledge manifest: which documentation sources and which embedding model are trusted.

use serde::Deserialize;

use clojurecraft_runtime::manifest::Artifact;
use clojurecraft_runtime::models::{ModelFile, ModelRecord, ModelSource};

/// The manifest shipped with this build of the application.
pub const BUNDLED: &str = include_str!("../../../resources/knowledge-manifest.toml");

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("knowledge manifest is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("knowledge manifest schema version {0} is not supported")]
    Schema(u32),
    #[error("knowledge manifest entry {0}: {1}")]
    Invalid(String, &'static str),
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApiSource {
    pub name: String,
    pub url_template: String,
    pub special_form_url: String,
    pub license: String,
    pub attribution: String,
}

impl ApiSource {
    pub fn url_for(&self, namespace: &str, name: &str) -> String {
        self.url_template
            .replace("{ns}", namespace)
            .replace("{name}", name)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PageKind {
    Reference,
    Guide,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Page {
    /// Path inside the source repository.
    pub path: String,
    /// The published page.
    pub url: String,
    pub kind: PageKind,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SiteSource {
    pub name: String,
    pub repository: String,
    pub commit: String,
    pub license: String,
    pub attribution: String,
    pub pages: Vec<Page>,
}

impl SiteSource {
    /// Download descriptor of a page's source file at the pinned commit.
    pub fn artifact(&self, page: &Page) -> Artifact {
        Artifact {
            url: format!(
                "https://raw.githubusercontent.com/{}/{}/{}",
                self.repository, self.commit, page.path
            ),
            size: page.size,
            sha256: page.sha256.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingStatus {
    SmokeTested,
    Unverified,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EmbeddingFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EmbeddingModel {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub revision: String,
    pub license: String,
    pub architecture: String,
    pub quant: String,
    pub dimensions: usize,
    pub pooling: String,
    pub max_tokens: u32,
    pub query_instruction: String,
    pub status: EmbeddingStatus,
    pub file: EmbeddingFile,
}

impl EmbeddingModel {
    /// The installation plan for the model store.
    pub fn record(&self) -> ModelRecord {
        ModelRecord {
            format: 1,
            key: format!("{}/{}", self.id, self.quant),
            display_name: self.name.clone(),
            source: ModelSource::Catalog { id: self.id.clone() },
            repo: self.repo.clone(),
            revision: self.revision.clone(),
            quant: self.quant.clone(),
            architecture: self.architecture.clone(),
            license: Some(self.license.clone()),
            files: vec![ModelFile {
                repo_path: self.file.path.clone(),
                name: self
                    .file
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&self.file.path)
                    .to_owned(),
                size: self.file.size,
                sha256: self.file.sha256.to_lowercase(),
            }],
            context_length: Some(self.max_tokens),
            layers: None,
            kv_bytes_per_token: None,
            installed_at: 0,
        }
    }

    /// The text embedded for a search query (the model expects an instruction with queries,
    /// and none with documents).
    pub fn query_text(&self, query: &str) -> String {
        format!("Instruct: {}\nQuery:{query}", self.query_instruction)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct KnowledgeManifest {
    pub schema_version: u32,
    pub revision: String,
    pub api: ApiSource,
    pub site: SiteSource,
    pub embedding: EmbeddingModel,
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl KnowledgeManifest {
    pub fn bundled() -> Result<Self, ManifestError> {
        Self::parse(BUNDLED)
    }

    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let m: Self = toml::from_str(text)?;
        if m.schema_version != 1 {
            return Err(ManifestError::Schema(m.schema_version));
        }
        let safe_id = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
                && !s.starts_with('.')
        };
        if !safe_id(&m.revision) {
            return Err(ManifestError::Invalid(
                "revision".into(),
                "must be a plain identifier",
            ));
        }
        if m.site.commit.len() != 40 || !m.site.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ManifestError::Invalid(
                "site".into(),
                "commit must be a full hash",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for p in &m.site.pages {
            let path_ok = p.path.ends_with(".adoc")
                && !p.path.contains("..")
                && !p.path.starts_with('/')
                && p.url.starts_with("https://");
            if !path_ok || !is_sha256(&p.sha256) || p.size == 0 {
                return Err(ManifestError::Invalid(p.path.clone(), "bad page entry"));
            }
            if !seen.insert(&p.path) {
                return Err(ManifestError::Invalid(p.path.clone(), "duplicate page"));
            }
        }
        let e = &m.embedding;
        if !safe_id(&e.id) || !safe_id(&e.quant) {
            return Err(ManifestError::Invalid(
                "embedding".into(),
                "id and quant must be plain identifiers",
            ));
        }
        if e.revision.len() != 40 || !is_sha256(&e.file.sha256) || !e.file.path.ends_with(".gguf") {
            return Err(ManifestError::Invalid("embedding".into(), "bad model pin"));
        }
        if e.dimensions == 0 || e.max_tokens < 256 {
            return Err(ManifestError::Invalid(
                "embedding".into(),
                "bad dimensions or token limit",
            ));
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_manifest_is_valid_and_pins_every_source() {
        let m = KnowledgeManifest::bundled().expect("bundled manifest");
        assert!(m.site.pages.len() >= 10);
        let a = m.site.artifact(&m.site.pages[0]);
        assert!(a.url.contains(&m.site.commit), "{}", a.url);
        assert_eq!(
            m.api.url_for("clojure.core", "map"),
            "https://clojure.github.io/clojure/clojure.core-api.html#clojure.core/map"
        );
        let record = m.embedding.record();
        assert_eq!(record.files[0].sha256, m.embedding.file.sha256);
        assert_eq!(record.revision.len(), 40);
    }

    #[test]
    fn unpinned_or_escaping_pages_are_refused() {
        let base = BUNDLED.to_owned();
        let first_hash = KnowledgeManifest::bundled().expect("manifest").site.pages[0]
            .sha256
            .clone();
        let bad_hash = base.replacen(&first_hash, "abc", 1);
        assert!(KnowledgeManifest::parse(&bad_hash).is_err());
        let escaping = base.replacen("content/reference/", "../reference/", 1);
        assert!(KnowledgeManifest::parse(&escaping).is_err());
    }
}
