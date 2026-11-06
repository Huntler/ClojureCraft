//! Hugging Face Hub access: repository metadata, GGUF file discovery, and header inspection
//! before download.
//!
//! Every lookup resolves the requested branch or tag to a commit, and later downloads use that
//! commit, so a repository changing upstream cannot change what is installed. File hashes come
//! from the Hub's LFS metadata and are verified after download.
//!
//! Access tokens are sent only to the Hub host (redirects to storage hosts never carry them) and
//! never appear in errors or logs. The Hub's license and access agreements are never accepted on
//! the user's behalf; a gated repository reports what the user has to do on the website.

use std::collections::BTreeMap;
use std::io::Read as _;

use serde::Deserialize;

use crate::catalog::{Catalog, MemoryEstimate, estimate_from_metadata};
use crate::download::agent;
use crate::gguf::{self, GgufError, ModelFacts};

pub const HUGGING_FACE: &str = "https://huggingface.co";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HubError {
    #[error(
        "“{0}” is not a repository id; use the form owner/name, for example Qwen/Qwen2.5-Coder-7B-Instruct-GGUF"
    )]
    InvalidRepo(String),
    #[error("“{0}” is not a valid revision name")]
    InvalidRevision(String),
    #[error("{0} was not found on Hugging Face")]
    NotFound(String),
    #[error(
        "{0} was not found, or it is private or gated: add a Hugging Face access token in settings and try again"
    )]
    SignInRequired(String),
    #[error(
        "your access token cannot read {0}: if the repository is gated, open it on huggingface.co, review and accept its conditions with your account, then try again"
    )]
    AccessDenied(String),
    #[error("Hugging Face is limiting requests; wait a moment and try again")]
    RateLimited,
    #[error("could not reach Hugging Face: {0}")]
    Network(String),
    #[error("unexpected answer from Hugging Face: {0}")]
    Malformed(String),
}

/// A repository at one resolved commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    pub repo: String,
    /// The commit every download is pinned to.
    pub revision: String,
    /// The Hub requires users to accept conditions before downloading.
    pub gated: bool,
    pub private: bool,
    pub files: Vec<RepoFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoFile {
    pub path: String,
    pub size: u64,
    /// SHA-256 recorded by the Hub's LFS storage; `None` for files stored without LFS.
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
struct ApiModel {
    sha: Option<String>,
    #[serde(default)]
    gated: serde_json::Value,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    siblings: Vec<ApiSibling>,
}

#[derive(Deserialize)]
struct ApiSibling {
    rfilename: String,
    size: Option<u64>,
    lfs: Option<ApiLfs>,
}

#[derive(Deserialize)]
struct ApiLfs {
    sha256: String,
    size: u64,
}

/// A Hub client. The token is held only in memory.
#[derive(Clone)]
pub struct Hub {
    base: String,
    token: Option<String>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "[set]"))
            .finish()
    }
}

pub fn validate_repo_id(repo: &str) -> Result<(), HubError> {
    let part_ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 96
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !p.starts_with(['.', '-'])
            && !p.contains("..")
    };
    match repo.split_once('/') {
        Some((owner, name)) if part_ok(owner) && part_ok(name) => Ok(()),
        _ => Err(HubError::InvalidRepo(repo.to_owned())),
    }
}

fn validate_revision(rev: &str) -> Result<(), HubError> {
    let ok = !rev.is_empty()
        && rev.len() <= 128
        && rev
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !rev.contains("..");
    if ok {
        Ok(())
    } else {
        Err(HubError::InvalidRevision(rev.to_owned()))
    }
}

fn is_commit(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Hub {
    pub fn new(token: Option<String>) -> Self {
        Self::with_base(HUGGING_FACE, token)
    }

    pub fn with_base(base: &str, token: Option<String>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.filter(|t| !t.trim().is_empty()),
        }
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn file_url(&self, repo: &str, revision: &str, path: &str) -> String {
        format!("{}/{repo}/resolve/{revision}/{path}", self.base)
    }

    fn get(&self, url: &str, range: Option<(u64, u64)>) -> Result<ureq::http::Response<ureq::Body>, u16> {
        let mut req = agent().get(url);
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        if let Some((start, end)) = range {
            req = req.header("Range", format!("bytes={start}-{end}"));
        }
        req.call().map_err(|e| match e {
            ureq::Error::StatusCode(code) => code,
            other => {
                tracing::debug!(error = %other, "hub request failed");
                0
            }
        })
    }

    fn status_error(&self, code: u16, repo: &str, err: impl FnOnce() -> String) -> HubError {
        match code {
            401 if self.token.is_none() => HubError::SignInRequired(repo.to_owned()),
            401 | 403 => HubError::AccessDenied(repo.to_owned()),
            404 => HubError::NotFound(repo.to_owned()),
            429 => HubError::RateLimited,
            0 => HubError::Network(err()),
            other => HubError::Malformed(format!("HTTP {other}")),
        }
    }

    /// Lists the repository's files at `revision` (default branch when `None`), resolving it to
    /// a commit.
    pub fn repo_info(&self, repo: &str, revision: Option<&str>) -> Result<RepoInfo, HubError> {
        let repo = repo.trim();
        validate_repo_id(repo)?;
        let rev = revision
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("main");
        validate_revision(rev)?;
        let url = format!("{}/api/models/{repo}/revision/{rev}?blobs=true", self.base);
        let resp = self
            .get(&url, None)
            .map_err(|code| self.status_error(code, repo, || "no connection".into()))?;
        let api: ApiModel = resp
            .into_body()
            .read_json()
            .map_err(|e| HubError::Malformed(e.to_string()))?;
        let sha = api.sha.filter(|s| is_commit(s)).ok_or_else(|| {
            HubError::Malformed("the repository revision did not resolve to a commit".into())
        })?;
        Ok(RepoInfo {
            repo: repo.to_owned(),
            revision: sha.to_lowercase(),
            gated: !matches!(
                api.gated,
                serde_json::Value::Bool(false) | serde_json::Value::Null
            ),
            private: api.private,
            files: api
                .siblings
                .into_iter()
                .map(|s| RepoFile {
                    size: s.lfs.as_ref().map(|l| l.size).or(s.size).unwrap_or(0),
                    sha256: s.lfs.map(|l| l.sha256.to_lowercase()),
                    path: s.rfilename,
                })
                .collect(),
        })
    }

    /// Reads the GGUF header of a file through range requests, fetching only as much as needed.
    pub fn read_gguf_header(&self, info: &RepoInfo, file: &RepoFile) -> Result<ModelFacts, InspectError> {
        let url = self.file_url(&info.repo, &info.revision, &file.path);
        let mut want: u64 = 2 << 20;
        loop {
            let end = want.min(file.size).saturating_sub(1);
            let resp = self.get(&url, Some((0, end))).map_err(|code| {
                InspectError::Hub(self.status_error(code, &info.repo, || "no connection".into()))
            })?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut resp.into_body().into_reader().take(want), &mut bytes)
                .map_err(|e| InspectError::Hub(HubError::Network(e.to_string())))?;
            match gguf::parse_header(&bytes) {
                Ok(h) => return Ok(h.facts()),
                Err(GgufError::Truncated { .. })
                    if want < file.size && want < gguf::MAX_HEADER_BYTES as u64 =>
                {
                    want = (want * 4).min(gguf::MAX_HEADER_BYTES as u64);
                }
                Err(e) => return Err(InspectError::Gguf(e)),
            }
        }
    }
}

/// A downloadable model variant: one GGUF file, or every part of a split model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GgufOption {
    /// Quantization label such as `Q4_K_M`, or the file name when none is recognizable.
    pub quant: String,
    /// Files in load order; the first is what llama.cpp is given.
    pub files: Vec<RepoFile>,
    /// For split models: the number of parts the file names announce.
    pub expected_parts: usize,
}

impl GgufOption {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn is_split(&self) -> bool {
        self.expected_parts > 1
    }

    pub fn is_complete(&self) -> bool {
        self.files.len() == self.expected_parts
    }
}

/// `name-00002-of-00004.gguf` -> `("name", 2, 4)`.
fn split_parts(path: &str) -> Option<(&str, usize, usize)> {
    let stem = path.strip_suffix(".gguf")?;
    let (rest, total) = stem.rsplit_once("-of-")?;
    let (prefix, index) = rest.rsplit_once('-')?;
    let ok = |s: &str| s.len() == 5 && s.bytes().all(|b| b.is_ascii_digit());
    if !ok(index) || !ok(total) {
        return None;
    }
    Some((prefix, index.parse().ok()?, total.parse().ok()?))
}

/// The quantization named in a GGUF file name, such as `Q4_K_M`, `IQ4_XS`, `UD-Q4_K_XL`, `BF16`.
pub fn quant_label(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.strip_suffix(".gguf").unwrap_or(name);
    let stem = split_parts(name).map_or(stem, |(prefix, _, _)| prefix);
    let pieces: Vec<&str> = stem.split(['-', '.']).collect();
    let is_quant = |p: &str| {
        let u = p.to_ascii_uppercase();
        let q = u.strip_prefix('I').unwrap_or(&u);
        let digits_after_q = q
            .strip_prefix('Q')
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
        let tq = q
            .strip_prefix("TQ")
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
        (digits_after_q || tq) && u.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || matches!(u.as_str(), "F16" | "BF16" | "F32" | "FP16" | "FP32")
            || u.starts_with("MXFP4") && u.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    for (i, p) in pieces.iter().enumerate() {
        if is_quant(p) {
            let label = p.to_ascii_uppercase();
            return match i.checked_sub(1).map(|j| pieces[j].to_ascii_uppercase()) {
                Some(prev) if prev == "UD" => format!("UD-{label}"),
                _ => label,
            };
        }
    }
    stem.to_owned()
}

/// Groups a repository's GGUF files into downloadable options. Vision projector files
/// (`mmproj`) are not generators and are left out.
pub fn gguf_options(info: &RepoInfo) -> Vec<GgufOption> {
    let mut singles = Vec::new();
    let mut splits: BTreeMap<(String, usize), Vec<(usize, RepoFile)>> = BTreeMap::new();
    for f in &info.files {
        let lower = f.path.to_ascii_lowercase();
        let base = lower.rsplit('/').next().unwrap_or(&lower);
        if !lower.ends_with(".gguf") || base.contains("mmproj") {
            continue;
        }
        match split_parts(&f.path) {
            Some((prefix, index, total)) => splits
                .entry((prefix.to_owned(), total))
                .or_default()
                .push((index, f.clone())),
            None => singles.push(GgufOption {
                quant: quant_label(&f.path),
                files: vec![f.clone()],
                expected_parts: 1,
            }),
        }
    }
    for ((prefix, total), mut parts) in splits {
        parts.sort_by_key(|(i, _)| *i);
        parts.dedup_by_key(|(i, _)| *i);
        singles.push(GgufOption {
            quant: quant_label(&prefix),
            files: parts.into_iter().map(|(_, f)| f).collect(),
            expected_parts: total,
        });
    }
    singles.sort_by_key(GgufOption::total_size);
    singles
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InspectError {
    #[error(transparent)]
    Hub(#[from] HubError),
    #[error("the file is not a readable GGUF model: {0}")]
    Gguf(#[from] GgufError),
}

/// Why an option cannot be installed. Explained before any large download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incompatibility {
    NoArchitecture,
    UnsupportedArchitecture {
        found: String,
        supported: Vec<String>,
    },
    MissingParts {
        expected: usize,
        found: usize,
    },
    /// The split layout recorded inside the file disagrees with the file names.
    SplitMismatch {
        header: u16,
        files: usize,
    },
    /// A file has no hash the Hub vouches for, so the download could not be verified.
    NoTrustedHash(String),
    /// The file holds far fewer tensors than its layer count requires, so it is only part of a
    /// model (for example a speculative-decoding head published next to the model).
    PartialModel {
        tensors: u64,
        layers: u32,
    },
}

impl std::fmt::Display for Incompatibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoArchitecture => write!(f, "the file does not state its model architecture"),
            Self::UnsupportedArchitecture { found, supported } => write!(
                f,
                "architecture “{found}” has not been validated with the bundled llama.cpp (supported: {})",
                supported.join(", ")
            ),
            Self::MissingParts { expected, found } => write!(
                f,
                "this split model has {found} of {expected} parts in the repository"
            ),
            Self::SplitMismatch { header, files } => write!(
                f,
                "the file says the model has {header} parts, but {files} were found"
            ),
            Self::PartialModel { tensors, layers } => write!(
                f,
                "this file is not a complete model ({tensors} tensors for {layers} layers); it is probably an add-on such as a speculative-decoding head"
            ),
            Self::NoTrustedHash(path) => {
                write!(
                    f,
                    "{path} has no published checksum, so its download could not be verified"
                )
            }
        }
    }
}

/// What is known about an option after reading its header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspection {
    pub option: GgufOption,
    pub facts: ModelFacts,
    pub problems: Vec<Incompatibility>,
}

impl Inspection {
    pub fn is_installable(&self) -> bool {
        self.problems.is_empty()
    }

    /// Estimated memory at `ctx` tokens; `None` when the header lacks the attention geometry.
    pub fn estimate(&self, ctx: u32) -> Option<MemoryEstimate> {
        let ctx = self.facts.context_length.map_or(ctx, |max| ctx.min(max));
        Some(estimate_from_metadata(
            self.option.total_size(),
            self.facts.kv_bytes_per_token()?,
            ctx,
        ))
    }
}

/// Checks an option before downloading it: file layout and hashes from the Hub metadata, then
/// architecture and split layout from the first file's header (a few megabytes at most).
pub fn inspect(
    hub: &Hub,
    info: &RepoInfo,
    option: &GgufOption,
    catalog: &Catalog,
) -> Result<Inspection, InspectError> {
    let mut problems = Vec::new();
    if !option.is_complete() {
        problems.push(Incompatibility::MissingParts {
            expected: option.expected_parts,
            found: option.files.len(),
        });
    }
    for f in &option.files {
        if f.sha256.is_none() {
            problems.push(Incompatibility::NoTrustedHash(f.path.clone()));
        }
    }
    let first = option
        .files
        .first()
        .ok_or_else(|| InspectError::Hub(HubError::Malformed("empty option".into())))?;
    let facts = hub.read_gguf_header(info, first)?;
    match &facts.architecture {
        None => problems.push(Incompatibility::NoArchitecture),
        Some(a) if !catalog.supports_architecture(a) => {
            problems.push(Incompatibility::UnsupportedArchitecture {
                found: a.clone(),
                supported: catalog.compatibility.architectures.clone(),
            })
        }
        Some(_) => {}
    }
    // Every transformer layer has several weight tensors; a single-file model with fewer than two
    // per layer cannot run on its own.
    if !option.is_split()
        && let Some(layers) = facts.block_count
        && facts.tensor_count < u64::from(layers) * 2
    {
        problems.push(Incompatibility::PartialModel {
            tensors: facts.tensor_count,
            layers,
        });
    }
    let header_parts = facts.split_count.unwrap_or(1);
    if usize::from(header_parts.max(1)) != option.expected_parts.max(1) {
        problems.push(Incompatibility::SplitMismatch {
            header: header_parts,
            files: option.expected_parts,
        });
    }
    Ok(Inspection {
        option: option.clone(),
        facts,
        problems,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::gguf::fixture;
    use crate::test_http::{Route, serve};

    pub(crate) const REV: &str = "0123456789abcdef0123456789abcdef01234567";

    fn api_json(files: &[(&str, &[u8])], gated: bool) -> String {
        let siblings: Vec<String> = files
            .iter()
            .map(|(p, body)| {
                use sha2::Digest;
                format!(
                    r#"{{"rfilename":"{p}","size":{},"lfs":{{"sha256":"{}","size":{},"pointerSize":134}}}}"#,
                    body.len(),
                    hex::encode(sha2::Sha256::digest(body)),
                    body.len()
                )
            })
            .collect();
        format!(
            r#"{{"id":"x","sha":"{REV}","gated":{},"private":false,"siblings":[{{"rfilename":"README.md","size":10}},{}]}}"#,
            if gated { "\"manual\"" } else { "false" },
            siblings.join(",")
        )
    }

    fn repo_server(
        repo: &str,
        files: &[(&str, Vec<u8>)],
        gated_token: Option<&str>,
    ) -> crate::test_http::Server {
        let borrowed: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
        let mut routes = HashMap::new();
        routes.insert(
            format!("/api/models/{repo}/revision/main"),
            Route::ok(api_json(&borrowed, gated_token.is_some())),
        );
        for (p, b) in files {
            let mut r = Route::ok(b.clone());
            if let Some(t) = gated_token {
                r = r.gated(t);
            }
            routes.insert(format!("/{repo}/resolve/{REV}/{p}"), r);
        }
        serve(routes)
    }

    /// A GGUF fixture padded with tensor-like bytes so it is larger than one header read.
    pub(crate) fn model_file(arch: &str, extra: usize) -> Vec<u8> {
        let mut b = fixture::model(arch);
        b.extend((0..extra).map(|i| (i % 251) as u8));
        b
    }

    fn catalog() -> Catalog {
        Catalog::bundled().expect("catalog")
    }

    #[test]
    fn resolves_files_and_pins_the_commit() {
        let server = repo_server("o/m-GGUF", &[("m-Q4_K_M.gguf", model_file("qwen2", 10))], None);
        let hub = Hub::with_base(&server.base, None);
        let info = hub.repo_info("o/m-GGUF", None).expect("info");
        assert_eq!(info.revision, REV);
        assert!(!info.gated);
        let opts = gguf_options(&info);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].quant, "Q4_K_M");
        assert!(opts[0].files[0].sha256.is_some());
    }

    #[test]
    fn architecture_is_checked_from_a_small_prefix_before_download() {
        let big = model_file("llama4", 20 << 20);
        let len = big.len() as u64;
        let server = repo_server("o/unsupported", &[("x-Q8_0.gguf", big)], None);
        let hub = Hub::with_base(&server.base, None);
        let info = hub.repo_info("o/unsupported", None).expect("info");
        let insp = inspect(&hub, &info, &gguf_options(&info)[0], &catalog()).expect("inspect");
        assert!(matches!(
            insp.problems.as_slice(),
            [Incompatibility::UnsupportedArchitecture { found, .. }] if found == "llama4"
        ));
        let fetched = server.bytes_requested(&format!("/o/unsupported/resolve/{REV}/x-Q8_0.gguf"), len);
        assert!(
            fetched <= 4 << 20,
            "only the header was fetched, not {fetched} bytes"
        );
    }

    #[test]
    fn supported_model_is_installable_with_an_estimate() {
        let server = repo_server("o/ok", &[("ok-IQ4_XS.gguf", model_file("qwen2", 1000))], None);
        let hub = Hub::with_base(&server.base, None);
        let info = hub.repo_info("o/ok", None).expect("info");
        let insp = inspect(&hub, &info, &gguf_options(&info)[0], &catalog()).expect("inspect");
        assert!(insp.is_installable(), "{:?}", insp.problems);
        let est = insp.estimate(8192).expect("estimate");
        assert_eq!(est.kv_cache, 28_672 * 8192);
    }

    #[test]
    fn add_on_files_are_not_offered_as_models() {
        // A speculative-decoding head: same architecture and layer count, only a few tensors.
        let mut head = fixture::model_with_tensors("qwen2", 19);
        head.extend(vec![0u8; 1000]);
        let server = repo_server("o/heads", &[("mtp-m-BF16.gguf", head)], None);
        let hub = Hub::with_base(&server.base, None);
        let info = hub.repo_info("o/heads", None).expect("info");
        let insp = inspect(&hub, &info, &gguf_options(&info)[0], &catalog()).expect("inspect");
        assert!(matches!(
            insp.problems.as_slice(),
            [Incompatibility::PartialModel {
                tensors: 19,
                layers: 28
            }]
        ));
        assert!(!insp.is_installable());
    }

    #[test]
    fn repositories_without_gguf_offer_nothing() {
        let server = repo_server("o/safetensors", &[("model.safetensors", vec![1, 2, 3])], None);
        let hub = Hub::with_base(&server.base, None);
        let info = hub.repo_info("o/safetensors", None).expect("info");
        assert!(gguf_options(&info).is_empty());
    }

    #[test]
    fn gated_repositories_explain_what_to_do() {
        let server = repo_server(
            "o/gated",
            &[("g-Q4_0.gguf", model_file("qwen2", 10))],
            Some("hf_right_token_0123456789"),
        );
        let info = Hub::with_base(&server.base, None)
            .repo_info("o/gated", None)
            .expect("info");
        assert!(info.gated);
        let opt = &gguf_options(&info)[0];

        let anonymous = Hub::with_base(&server.base, None);
        let err = inspect(&anonymous, &info, opt, &catalog()).expect_err("needs a token");
        assert!(matches!(err, InspectError::Hub(HubError::SignInRequired(_))));

        let wrong = Hub::with_base(&server.base, Some("hf_wrong_token_0123456789".into()));
        let err = inspect(&wrong, &info, opt, &catalog()).expect_err("not accepted");
        assert!(matches!(err, InspectError::Hub(HubError::AccessDenied(_))));
        assert!(!format!("{err} {err:?} {wrong:?}").contains("hf_wrong_token"));

        let right = Hub::with_base(&server.base, Some("hf_right_token_0123456789".into()));
        assert!(
            inspect(&right, &info, opt, &catalog())
                .expect("inspect")
                .is_installable()
        );
    }

    #[test]
    fn missing_repositories_and_bad_ids() {
        let server = repo_server("o/exists", &[], None);
        let hub = Hub::with_base(&server.base, None);
        assert_eq!(
            hub.repo_info("o/missing", None),
            Err(HubError::NotFound("o/missing".into()))
        );
        for bad in ["noslash", "a/b/c", "../x/y", "a/b?x=1", "a /b", ""] {
            assert!(
                matches!(hub.repo_info(bad, None), Err(HubError::InvalidRepo(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            hub.repo_info("o/exists", Some("../main")),
            Err(HubError::InvalidRevision(_))
        ));
    }

    #[test]
    fn split_models_are_grouped_and_incomplete_sets_rejected() {
        let mk = |n: usize, of: usize| format!("Big-Q4_K_M/Big-Q4_K_M-{n:05}-of-{of:05}.gguf");
        let files = vec![
            RepoFile {
                path: mk(2, 3),
                size: 5,
                sha256: Some("a".repeat(64)),
            },
            RepoFile {
                path: mk(1, 3),
                size: 5,
                sha256: Some("b".repeat(64)),
            },
            RepoFile {
                path: "mmproj-F16.gguf".into(),
                size: 5,
                sha256: Some("c".repeat(64)),
            },
            RepoFile {
                path: "Big-Q8_0.gguf".into(),
                size: 9,
                sha256: Some("d".repeat(64)),
            },
        ];
        let info = RepoInfo {
            repo: "o/b".into(),
            revision: REV.into(),
            gated: false,
            private: false,
            files,
        };
        let opts = gguf_options(&info);
        assert_eq!(opts.len(), 2, "mmproj is not offered");
        let split = opts.iter().find(|o| o.is_split()).expect("split");
        assert_eq!(split.quant, "Q4_K_M");
        assert!(split.files[0].path.contains("00001-of"));
        assert!(!split.is_complete());
    }

    #[test]
    fn quant_labels() {
        for (file, label) in [
            ("qwen2.5-coder-7b-instruct-q4_k_m.gguf", "Q4_K_M"),
            ("Qwen3-Coder-30B-A3B-Instruct-IQ4_XS.gguf", "IQ4_XS"),
            ("Devstral-Small-2-24B-Instruct-2512-UD-Q4_K_XL.gguf", "UD-Q4_K_XL"),
            ("model-bf16.gguf", "BF16"),
            ("qwen2.5-coder-0.5b-instruct-fp16.gguf", "FP16"),
            ("Mellum2.1-12B-A2.5B-Thinking-MXFP4_MOE.gguf", "MXFP4_MOE"),
            ("Big-Q4_K_M-00001-of-00004.gguf", "Q4_K_M"),
            ("plain.gguf", "plain"),
        ] {
            assert_eq!(quant_label(file), label, "{file}");
        }
    }
}
