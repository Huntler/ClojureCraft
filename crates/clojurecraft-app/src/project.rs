//! A Clojure project on disk. Each namespace under the project's source paths is one function
//! document: its specification lives in `ClojureCraft.json` at the project root, its code is the
//! namespace's source file. The library still keeps drafts, history, and results, keyed by the
//! identifier recorded in `ClojureCraft.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clojurecraft_domain::{
    Clarification, Contract, Example, FunctionDocument, FunctionId, Implementation, Step, TestCase,
};
use clojurecraft_edn::{Delim, NodeKind, TokenKind};
use clojurecraft_workflow::structure::{self, Expectations};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "ClojureCraft.json";
const CONFIG_VERSION: u32 = 1;
const DEFAULT_SOURCE_PATH: &str = "src";
const SOURCE_EXTENSIONS: [&str; 2] = ["clj", "cljc"];
/// Specification edits are written to `ClojureCraft.json` once typing pauses this long.
pub const SAVE_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub namespaces: BTreeMap<String, NamespaceSpec>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            namespaces: BTreeMap::new(),
        }
    }
}

/// What the centre pane edits for one namespace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamespaceSpec {
    pub id: FunctionId,
    /// The display name of the namespace's main function.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub contract: Option<Contract>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub examples: Vec<Example>,
    #[serde(default)]
    pub clarifications: Vec<Clarification>,
    #[serde(default)]
    pub tests: Vec<TestCase>,
}

impl NamespaceSpec {
    fn new(name: String) -> Self {
        Self::from_doc(&FunctionDocument::new(name))
    }

    fn from_doc(doc: &FunctionDocument) -> Self {
        Self {
            id: doc.id,
            name: doc.name.clone(),
            description: doc.description.clone(),
            contract: doc.contract.clone(),
            steps: doc.steps.clone(),
            examples: doc.examples.clone(),
            clarifications: doc.clarifications.clone(),
            tests: doc.tests.clone(),
        }
    }

    /// Makes `doc`'s specification this one; returns whether anything changed.
    fn apply(&self, doc: &mut FunctionDocument) -> bool {
        let before = Self::from_doc(doc);
        if before == *self {
            return false;
        }
        doc.name.clone_from(&self.name);
        doc.description.clone_from(&self.description);
        doc.contract.clone_from(&self.contract);
        doc.steps.clone_from(&self.steps);
        doc.examples.clone_from(&self.examples);
        doc.clarifications.clone_from(&self.clarifications);
        doc.tests.clone_from(&self.tests);
        true
    }
}

/// A namespace's source file and its content as last read or written.
#[derive(Clone, Debug)]
struct SourceFile {
    path: PathBuf,
    content: Option<String>,
}

/// One segment of a namespace name in the project tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeNode {
    pub segment: String,
    /// The full dotted name up to this segment.
    pub path: String,
    /// Whether `path` is a namespace (rather than only a prefix of deeper ones).
    pub namespace: bool,
    pub children: Vec<TreeNode>,
}

pub struct Project {
    root: PathBuf,
    /// Relative to the root; new namespaces go into the first.
    source_paths: Vec<String>,
    config: Config,
    files: BTreeMap<String, SourceFile>,
    /// Since when `config` holds changes not yet written.
    dirty_since: Option<Instant>,
}

impl Project {
    /// Reads the project at `root`: its namespaces and `ClojureCraft.json`, which is created (or
    /// completed with empty specifications for namespaces it does not list yet).
    pub fn open(root: &Path) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("{} could not be opened: {e}", root.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a folder", root.display()));
        }
        let source_paths = source_paths(&root);
        let mut files = BTreeMap::new();
        for path in &source_paths {
            let dir = root.join(path);
            scan(&dir, &dir, &mut files);
        }
        let config_path = root.join(CONFIG_FILE);
        let (mut config, existed) = match std::fs::read_to_string(&config_path) {
            Ok(text) => (
                serde_json::from_str::<Config>(&text)
                    .map_err(|e| format!("{CONFIG_FILE} could not be read: {e}"))?,
                true,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), false),
            Err(e) => return Err(format!("{CONFIG_FILE} could not be read: {e}")),
        };
        let mut added = false;
        for (ns, file) in &files {
            if !config.namespaces.contains_key(ns) {
                let name = default_name(ns, file.content.as_deref());
                config.namespaces.insert(ns.clone(), NamespaceSpec::new(name));
                added = true;
            }
        }
        let mut project = Self {
            root,
            source_paths,
            config,
            files,
            dirty_since: None,
        };
        if !existed || added {
            project.save()?;
        }
        Ok(project)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The project folder's name.
    pub fn title(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    pub fn contains(&self, namespace: &str) -> bool {
        self.config.namespaces.contains_key(namespace)
    }

    pub fn id_of(&self, namespace: &str) -> Option<FunctionId> {
        self.config.namespaces.get(namespace).map(|s| s.id)
    }

    pub fn namespace_of(&self, id: FunctionId) -> Option<&str> {
        self.config
            .namespaces
            .iter()
            .find(|(_, s)| s.id == id)
            .map(|(ns, _)| ns.as_str())
    }

    /// Whether the namespace has a source file yet.
    pub fn has_file(&self, namespace: &str) -> bool {
        self.files.get(namespace).is_some_and(|f| f.content.is_some())
    }

    /// Adds a namespace with an empty specification; its file is written once it has code.
    pub fn add_namespace(&mut self, namespace: &str) -> Result<FunctionId, String> {
        let namespace = namespace.trim();
        validate_namespace(namespace)?;
        if self.contains(namespace) {
            return Err(format!("{namespace} already exists"));
        }
        let spec = NamespaceSpec::new(last_segment(namespace).to_owned());
        let id = spec.id;
        self.config.namespaces.insert(namespace.to_owned(), spec);
        self.save()?;
        Ok(id)
    }

    /// A new document for `namespace`: its specification and the code on disk.
    pub fn document(&self, namespace: &str) -> Option<FunctionDocument> {
        let spec = self.config.namespaces.get(namespace)?;
        let mut doc = FunctionDocument::new(spec.name.clone());
        doc.id = spec.id;
        doc.namespace = Some(namespace.to_owned());
        spec.apply(&mut doc);
        doc.implementation = self.read_source(namespace).map(Implementation::manual);
        Some(doc)
    }

    /// Brings a stored document of this project up to date with `ClojureCraft.json` and the code
    /// on disk, which both win over the stored draft. Returns whether `doc` changed.
    pub fn sync(&mut self, doc: &mut FunctionDocument) -> Result<bool, String> {
        let Some(namespace) = self.namespace_of(doc.id).map(str::to_owned) else {
            return Ok(false);
        };
        let mut changed = false;
        if doc.namespace.as_deref() != Some(namespace.as_str()) {
            doc.namespace = Some(namespace.clone());
            changed = true;
        }
        if let Some(spec) = self.config.namespaces.get(&namespace) {
            changed |= spec.apply(doc);
        }
        let disk = self.read_source(&namespace);
        let file = self.file_mut(&namespace);
        file.content.clone_from(&disk);
        match (disk, &mut doc.implementation) {
            (Some(text), Some(implementation)) if implementation.source != text => {
                implementation.source = text;
                changed = true;
            }
            (Some(text), None) => {
                doc.implementation = Some(Implementation::manual(text));
                changed = true;
            }
            (None, Some(_)) => self.write_code(&namespace, doc)?,
            _ => {}
        }
        Ok(changed)
    }

    /// Takes `doc`'s specification into `ClojureCraft.json` (written after [`SAVE_DELAY`]) and
    /// writes its code to the namespace's file at once when it changed.
    pub fn record(&mut self, doc: &FunctionDocument) -> Result<(), String> {
        let Some(namespace) = self.namespace_of(doc.id).map(str::to_owned) else {
            return Ok(());
        };
        let spec = NamespaceSpec::from_doc(doc);
        if self.config.namespaces.get(&namespace) != Some(&spec) {
            self.config.namespaces.insert(namespace.clone(), spec);
            self.dirty_since.get_or_insert_with(Instant::now);
        }
        self.write_code(&namespace, doc)
    }

    /// Writes `ClojureCraft.json` when edits waited long enough, or at once with `force`.
    /// Returns how long to wait before calling again, when there is something left to write.
    pub fn save_pending(&mut self, force: bool) -> Result<Option<Duration>, String> {
        let Some(since) = self.dirty_since else {
            return Ok(None);
        };
        let waited = since.elapsed();
        if !force && waited < SAVE_DELAY {
            return Ok(Some(SAVE_DELAY - waited));
        }
        if let Err(e) = self.save() {
            self.dirty_since = Some(Instant::now());
            return Err(e);
        }
        Ok(None)
    }

    pub fn tree(&self) -> Vec<TreeNode> {
        let mut roots: Vec<TreeNode> = Vec::new();
        for namespace in self.config.namespaces.keys() {
            let mut level = &mut roots;
            let mut path = String::new();
            let segments: Vec<&str> = namespace.split('.').collect();
            for (i, segment) in segments.iter().enumerate() {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(segment);
                let index = match level.iter().position(|n| n.segment == *segment) {
                    Some(index) => index,
                    None => {
                        level.push(TreeNode {
                            segment: (*segment).to_owned(),
                            path: path.clone(),
                            namespace: false,
                            children: Vec::new(),
                        });
                        level.len() - 1
                    }
                };
                if i + 1 == segments.len() {
                    level[index].namespace = true;
                }
                level = &mut level[index].children;
            }
        }
        roots
    }

    fn save(&mut self) -> Result<(), String> {
        let mut text = serde_json::to_string_pretty(&self.config)
            .map_err(|e| format!("{CONFIG_FILE} could not be written: {e}"))?;
        text.push('\n');
        let path = self.root.join(CONFIG_FILE);
        let temporary = self.root.join(format!(".{CONFIG_FILE}.tmp"));
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, &path))
            .map_err(|e| format!("{CONFIG_FILE} could not be written: {e}"))?;
        self.dirty_since = None;
        Ok(())
    }

    fn file_mut(&mut self, namespace: &str) -> &mut SourceFile {
        let path = self.new_file_path(namespace);
        self.files
            .entry(namespace.to_owned())
            .or_insert(SourceFile { path, content: None })
    }

    fn new_file_path(&self, namespace: &str) -> PathBuf {
        let base = self
            .source_paths
            .first()
            .map_or(DEFAULT_SOURCE_PATH, String::as_str);
        self.root
            .join(base)
            .join(clojurecraft_workflow::export::namespace_path(namespace))
    }

    fn read_source(&self, namespace: &str) -> Option<String> {
        let path = self
            .files
            .get(namespace)
            .map_or_else(|| self.new_file_path(namespace), |f| f.path.clone());
        std::fs::read_to_string(path).ok()
    }

    fn write_code(&mut self, namespace: &str, doc: &FunctionDocument) -> Result<(), String> {
        let Some(source) = doc.implementation.as_ref().map(|i| &i.source) else {
            return Ok(());
        };
        let file = self.file_mut(namespace);
        if file.content.as_ref() == Some(source) {
            return Ok(());
        }
        if let Some(dir) = file.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("{} could not be created: {e}", dir.display()))?;
        }
        std::fs::write(&file.path, source)
            .map_err(|e| format!("{} could not be written: {e}", file.path.display()))?;
        file.content = Some(source.clone());
        Ok(())
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Err(e) = self.save_pending(true) {
            tracing::error!(error = %e, "project specification not saved");
        }
    }
}

/// Source paths from `deps.edn` (`:paths`) or `project.clj` (`:source-paths`), else `src`.
fn source_paths(root: &Path) -> Vec<String> {
    let found = std::fs::read_to_string(root.join("deps.edn"))
        .ok()
        .and_then(|text| deps_paths(&text))
        .or_else(|| {
            std::fs::read_to_string(root.join("project.clj"))
                .ok()
                .and_then(|text| leiningen_paths(&text))
        });
    match found {
        Some(paths) if !paths.is_empty() => paths,
        _ => vec![DEFAULT_SOURCE_PATH.to_owned()],
    }
}

fn deps_paths(text: &str) -> Option<Vec<String>> {
    let parsed = clojurecraft_edn::parse_value(text);
    let NodeKind::Map(entries) = &parsed.values.first()?.kind else {
        return None;
    };
    let (_, value) = entries.iter().find(|(k, _)| k.text(text) == ":paths")?;
    let NodeKind::Vector(items) = &value.kind else {
        return None;
    };
    Some(
        items
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
    )
}

fn leiningen_paths(text: &str) -> Option<Vec<String>> {
    let tokens = significant_tokens(text);
    let at = tokens
        .iter()
        .position(|t| t.kind == TokenKind::Keyword && &text[t.span.clone()] == ":source-paths")?;
    if tokens.get(at + 1)?.kind != TokenKind::Open(Delim::Bracket) {
        return None;
    }
    let mut paths = Vec::new();
    for t in &tokens[at + 2..] {
        match t.kind {
            TokenKind::String { terminated: true } => {
                let quoted = &text[t.span.clone()];
                paths.push(quoted[1..quoted.len() - 1].to_owned());
            }
            TokenKind::Close(_) => break,
            _ => {}
        }
    }
    Some(paths)
}

fn significant_tokens(text: &str) -> Vec<clojurecraft_edn::Token> {
    clojurecraft_edn::tokenize(text)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
        .collect()
}

/// Collects `.clj` and `.cljc` files under `dir`, keyed by namespace. A namespace found in an
/// earlier source path is kept.
fn scan(dir: &Path, base: &Path, files: &mut BTreeMap<String, SourceFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        let hidden = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            scan(&path, base, files);
            continue;
        }
        let source = path
            .extension()
            .is_some_and(|e| SOURCE_EXTENSIONS.iter().any(|x| e == *x));
        if !source {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(namespace) = namespace_name(&content).or_else(|| path_namespace(&path, base)) else {
            continue;
        };
        files.entry(namespace).or_insert(SourceFile {
            path,
            content: Some(content),
        });
    }
}

/// The name in the source's first `(ns …)` form, skipping metadata.
pub fn namespace_name(source: &str) -> Option<String> {
    let tokens = significant_tokens(source);
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        match t.kind {
            TokenKind::Open(Delim::Paren) if depth == 0 => {
                let is_ns = tokens
                    .get(i + 1)
                    .is_some_and(|n| n.kind == TokenKind::Symbol && &source[n.span.clone()] == "ns");
                if is_ns {
                    return symbol_after_metadata(source, &tokens[i + 2..]);
                }
                depth += 1;
            }
            TokenKind::Open(_) => depth += 1,
            TokenKind::Close(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

fn symbol_after_metadata(source: &str, tokens: &[clojurecraft_edn::Token]) -> Option<String> {
    let mut i = 0;
    loop {
        let t = tokens.get(i)?;
        match t.kind {
            TokenKind::Symbol => return Some(source[t.span.clone()].to_owned()),
            TokenKind::ReaderMacro if &source[t.span.clone()] == "^" => {
                i += 1;
                i += form_length(tokens.get(i..)?)?;
            }
            _ => return None,
        }
    }
}

/// How many tokens the form starting at `tokens[0]` spans.
fn form_length(tokens: &[clojurecraft_edn::Token]) -> Option<usize> {
    if !matches!(tokens.first()?.kind, TokenKind::Open(_)) {
        return Some(1);
    }
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        match t.kind {
            TokenKind::Open(_) => depth += 1,
            TokenKind::Close(_) => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The namespace a file's location implies (`a_b/core.clj` → `a-b.core`).
fn path_namespace(path: &Path, base: &Path) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?.with_extension("");
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().replace('_', "-"))
        .collect();
    (!parts.is_empty()).then(|| parts.join("."))
}

/// The main function's name for an existing namespace: its only public function, else the last
/// segment of the namespace.
fn default_name(namespace: &str, source: Option<&str>) -> String {
    let found = source.and_then(|s| {
        structure::check(
            s,
            &Expectations {
                namespace: "",
                function: "",
                allowed: &Default::default(),
            },
        )
        .function
    });
    found.unwrap_or_else(|| last_segment(namespace).to_owned())
}

fn last_segment(namespace: &str) -> &str {
    namespace.rsplit('.').next().unwrap_or(namespace)
}

/// Namespace names this application creates: dot-separated segments of ASCII letters, digits,
/// `-` and `_`, each starting with a letter.
pub fn validate_namespace(namespace: &str) -> Result<(), String> {
    if namespace.is_empty() {
        return Err("Enter a namespace name.".into());
    }
    for segment in namespace.split('.') {
        let valid = segment.starts_with(|c: char| c.is_ascii_alphabetic())
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid {
            return Err(format!(
                "“{segment}” is not a valid namespace segment: use letters, digits, - and _, starting with a letter."
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::Edn;

    use super::*;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("dir");
        std::fs::write(path, content).expect("write");
    }

    fn read_config(root: &Path) -> Config {
        let text = std::fs::read_to_string(root.join(CONFIG_FILE)).expect("config exists");
        serde_json::from_str(&text).expect("config parses")
    }

    #[test]
    fn namespace_names_are_read_from_the_ns_form() {
        assert_eq!(
            namespace_name(";; header\n(ns shop.orders\n  (:require [clojure.string :as str]))").as_deref(),
            Some("shop.orders")
        );
        assert_eq!(
            namespace_name("(ns ^{:doc \"x\"} ^:no-doc shop.meta)").as_deref(),
            Some("shop.meta")
        );
        assert_eq!(
            namespace_name("(comment (ns not.this))\n(ns shop.later)").as_deref(),
            Some("shop.later")
        );
        assert_eq!(namespace_name("(defn f [] 1)"), None);
        assert_eq!(namespace_name(""), None);
    }

    #[test]
    fn opening_a_project_without_config_creates_it_with_empty_specifications() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "deps.edn", "{:paths [\"src\" \"resources\"] :deps {}}");
        write(
            root,
            "src/shop/order_totals.clj",
            "(ns shop.order-totals)\n(defn- helper [x] x)\n(defn totals [xs] (helper xs))\n",
        );
        write(
            root,
            "src/shop/util.clj",
            "(ns shop.util)\n(defn a [] 1)\n(defn b [] 2)\n",
        );
        write(root, "src/shop/web.cljs", "(ns shop.web)");
        write(root, "test/shop/util_test.clj", "(ns shop.util-test)");

        let project = Project::open(root).expect("opens");
        let config = read_config(root);
        assert_eq!(
            config.namespaces.keys().collect::<Vec<_>>(),
            ["shop.order-totals", "shop.util"]
        );
        let totals = &config.namespaces["shop.order-totals"];
        assert_eq!(totals.name, "totals");
        assert!(totals.description.is_empty() && totals.steps.is_empty() && totals.examples.is_empty());
        assert_eq!(config.namespaces["shop.util"].name, "util");

        let doc = project.document("shop.order-totals").expect("listed");
        assert_eq!(doc.id, totals.id);
        assert_eq!(doc.namespace.as_deref(), Some("shop.order-totals"));
        assert!(
            doc.implementation
                .expect("existing code is imported")
                .source
                .contains("(defn totals")
        );
    }

    #[test]
    fn an_existing_config_is_kept_and_completed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/a/core.clj", "(ns a.core)");
        write(root, "src/a/extra.clj", "(ns a.extra)");
        let mut spec = NamespaceSpec::new("main".into());
        spec.description = "kept".into();
        let id = spec.id;
        let mut config = Config::default();
        config.namespaces.insert("a.core".into(), spec);
        config
            .namespaces
            .insert("a.planned".into(), NamespaceSpec::new("planned".into()));
        write(root, CONFIG_FILE, &serde_json::to_string(&config).expect("json"));

        let project = Project::open(root).expect("opens");
        let config = read_config(root);
        assert_eq!(config.namespaces["a.core"].id, id);
        assert_eq!(config.namespaces["a.core"].description, "kept");
        assert!(config.namespaces.contains_key("a.extra"));
        assert!(project.contains("a.planned"));
        assert!(!project.has_file("a.planned"));
        assert!(
            project
                .document("a.planned")
                .expect("listed")
                .implementation
                .is_none()
        );
    }

    #[test]
    fn a_malformed_config_is_not_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, CONFIG_FILE, "{not json");
        assert!(Project::open(root).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join(CONFIG_FILE)).expect("still there"),
            "{not json"
        );
    }

    #[test]
    fn leiningen_source_paths_are_used() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(
            root,
            "project.clj",
            "(defproject x \"0.1\"\n  :source-paths [\"src/clj\" \"src/cljc\"]\n  :dependencies [])",
        );
        write(root, "src/clj/x/core.clj", "(ns x.core)");
        write(root, "src/cljc/x/shared.cljc", "(ns x.shared)");
        let mut project = Project::open(root).expect("opens");
        assert!(project.has_file("x.core") && project.has_file("x.shared"));
        project.add_namespace("x.core.fresh").expect("added");
        let mut doc = project.document("x.core.fresh").expect("listed");
        doc.implementation = Some(Implementation::manual("(ns x.core.fresh)"));
        project.record(&doc).expect("recorded");
        assert!(root.join("src/clj/x/core/fresh.clj").exists());
    }

    #[test]
    fn new_namespaces_are_validated_and_written_once_they_have_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/shop/core.clj", "(ns shop.core)");
        let mut project = Project::open(root).expect("opens");
        assert!(project.add_namespace("shop.core").is_err());
        assert!(project.add_namespace("shop..x").is_err());
        assert!(project.add_namespace("shop.1x").is_err());
        assert!(project.add_namespace("").is_err());
        let id = project.add_namespace("shop.core.order-totals").expect("added");
        assert_eq!(read_config(root).namespaces["shop.core.order-totals"].id, id);
        assert_eq!(
            read_config(root).namespaces["shop.core.order-totals"].name,
            "order-totals"
        );

        let mut doc = project.document("shop.core.order-totals").expect("listed");
        doc.description = "Sum per customer".into();
        project.record(&doc).expect("recorded");
        let file = root.join("src/shop/core/order_totals.clj");
        assert!(!file.exists());
        assert!(project.save_pending(false).expect("saves").is_some());
        project.save_pending(true).expect("saves");
        assert_eq!(
            read_config(root).namespaces["shop.core.order-totals"].description,
            "Sum per customer"
        );

        doc.implementation = Some(Implementation::manual(
            "(ns shop.core.order-totals)\n(defn order-totals [x] x)",
        ));
        project.record(&doc).expect("recorded");
        assert_eq!(
            std::fs::read_to_string(&file).expect("written"),
            "(ns shop.core.order-totals)\n(defn order-totals [x] x)"
        );
    }

    #[test]
    fn syncing_a_stored_draft_takes_the_config_and_the_code_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/a/core.clj", "(ns a.core)\n(defn core [x] x)");
        let mut project = Project::open(root).expect("opens");
        let fresh = project.document("a.core").expect("listed");

        // A draft stored earlier, out of date with the project.
        let mut stored = fresh.clone();
        stored.namespace = None;
        stored.description = "old".into();
        stored.examples = vec![Example::direct("", Edn::new("1"), Edn::new("1"))];
        stored.implementation = Some(Implementation::manual("(ns a.core)\n(defn core [x] (inc x))"));
        assert!(project.sync(&mut stored).expect("synced"));
        assert_eq!(stored.namespace.as_deref(), Some("a.core"));
        assert!(stored.description.is_empty() && stored.examples.is_empty());
        assert_eq!(
            stored.implementation.as_ref().expect("code").source,
            "(ns a.core)\n(defn core [x] x)"
        );
        assert!(!project.sync(&mut stored).expect("synced"));

        // Changed on disk by another editor.
        write(root, "src/a/core.clj", "(ns a.core)\n(defn core [x] (dec x))");
        assert!(project.sync(&mut stored).expect("synced"));
        assert!(stored.implementation.expect("code").source.contains("(dec x)"));
    }

    #[test]
    fn unchanged_code_is_not_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/a/core.clj", "(ns a.core)");
        let mut project = Project::open(root).expect("opens");
        let doc = project.document("a.core").expect("listed");
        let file = root.join("src/a/core.clj");
        let before = std::fs::metadata(&file)
            .expect("exists")
            .modified()
            .expect("mtime");
        std::thread::sleep(Duration::from_millis(20));
        project.record(&doc).expect("recorded");
        assert_eq!(
            std::fs::metadata(&file)
                .expect("exists")
                .modified()
                .expect("mtime"),
            before
        );
    }

    #[test]
    fn the_tree_nests_namespaces_by_segment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/shop/core.clj", "(ns shop.core)");
        write(root, "src/shop/core/util.clj", "(ns shop.core.util)");
        write(root, "src/shop/orders/totals.clj", "(ns shop.orders.totals)");
        write(root, "src/single.clj", "(ns single)");
        let project = Project::open(root).expect("opens");
        let tree = project.tree();
        assert_eq!(tree.len(), 2);
        let shop = &tree[0];
        assert_eq!((shop.segment.as_str(), shop.namespace), ("shop", false));
        let core = &shop.children[0];
        assert_eq!((core.path.as_str(), core.namespace), ("shop.core", true));
        assert_eq!(core.children[0].path, "shop.core.util");
        assert_eq!(shop.children[1].path, "shop.orders");
        assert!(!shop.children[1].namespace);
        assert_eq!(shop.children[1].children[0].path, "shop.orders.totals");
        assert_eq!((tree[1].path.as_str(), tree[1].namespace), ("single", true));
    }

    #[test]
    fn a_namespace_without_an_ns_form_is_named_by_its_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/my_app/data_io.clj", "(defn f [] 1)");
        let project = Project::open(root).expect("opens");
        assert!(project.contains("my-app.data-io"));
    }
}
