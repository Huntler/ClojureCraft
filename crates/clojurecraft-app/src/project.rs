//! A Clojure project on disk. Every public function of a namespace under the project's source
//! paths is one function document: its specification lives in `ClojureCraft.json` at the
//! project root, its code is its definitions (with its own private helpers) in the namespace's
//! file. The library still keeps drafts, history, and results, keyed by the identifier recorded
//! in `ClojureCraft.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clojurecraft_domain::{
    Clarification, Contract, Example, FunctionDocument, FunctionId, Implementation, ProjectBinding, Step,
    TestCase,
};
use clojurecraft_edn::{Delim, NodeKind, TokenKind};
use clojurecraft_workflow::project as code;
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "ClojureCraft.json";
const CONFIG_VERSION: u32 = 2;
const DEFAULT_SOURCE_PATH: &str = "src";
const SOURCE_EXTENSIONS: [&str; 2] = ["clj", "cljc"];
/// Specification edits are written to `ClojureCraft.json` once typing pauses this long.
pub const SAVE_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub namespaces: BTreeMap<String, NamespaceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            namespaces: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NamespaceConfig {
    /// By function symbol.
    #[serde(default)]
    pub functions: BTreeMap<String, FunctionSpec>,
}

/// What the centre pane edits for one function, and which helpers belong to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionSpec {
    pub id: FunctionId,
    /// The display name.
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
    /// Private functions rewritten with this function.
    #[serde(default)]
    pub helpers: Vec<String>,
    #[serde(default = "one")]
    pub arguments: u32,
}

fn one() -> u32 {
    1
}

impl FunctionSpec {
    fn new(name: &str, helpers: Vec<String>, arguments: u32) -> Self {
        let mut spec = Self::from_doc(&FunctionDocument::new(name));
        spec.helpers = helpers;
        spec.arguments = arguments;
        spec
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
            helpers: doc
                .project
                .as_ref()
                .map(|p| p.helpers.clone())
                .unwrap_or_default(),
            arguments: doc.arguments(),
        }
    }

    /// Makes `doc`'s specification this one; returns whether anything changed.
    fn apply(&self, doc: &mut FunctionDocument) -> bool {
        let mut changed = false;
        macro_rules! take {
            ($field:ident) => {
                if doc.$field != self.$field {
                    doc.$field.clone_from(&self.$field);
                    changed = true;
                }
            };
        }
        take!(name);
        take!(description);
        take!(contract);
        take!(steps);
        take!(examples);
        take!(clarifications);
        take!(tests);
        changed
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
    /// The namespace's functions: those in the file in file order, then planned ones.
    pub functions: Vec<FunctionLeaf>,
    pub children: Vec<TreeNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionLeaf {
    pub name: String,
    /// Whether the file defines it yet.
    pub defined: bool,
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
    /// Reads the project at `root`: its namespaces, their functions, and `ClojureCraft.json`,
    /// which is created (or completed with empty specifications for functions it does not list
    /// yet).
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
        let (mut config, upgraded) = read_config(&root.join(CONFIG_FILE))?;
        let mut changed = upgraded;
        for (ns, file) in &files {
            let entry = config.namespaces.entry(ns.clone()).or_insert_with(|| {
                changed = true;
                NamespaceConfig::default()
            });
            let content = file.content.as_deref().unwrap_or_default();
            for (function, arguments) in code::public_functions(content) {
                let helpers = code::owned_helpers(content, &function);
                match entry.functions.get_mut(&function) {
                    // The first version recorded neither.
                    Some(spec) if upgraded => {
                        spec.helpers = helpers;
                        spec.arguments = arguments;
                    }
                    Some(_) => {}
                    None => {
                        changed = true;
                        entry
                            .functions
                            .insert(function.clone(), FunctionSpec::new(&function, helpers, arguments));
                    }
                }
            }
        }
        let mut project = Self {
            root,
            source_paths,
            config,
            files,
            dirty_since: None,
        };
        if changed {
            project.save()?;
        }
        Ok(project)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Absolute source directories, the classpath before dependencies are resolved.
    pub fn source_dirs(&self) -> Vec<PathBuf> {
        self.source_paths.iter().map(|p| self.root.join(p)).collect()
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

    pub fn id_of(&self, namespace: &str, function: &str) -> Option<FunctionId> {
        self.spec(namespace, function).map(|s| s.id)
    }

    fn spec(&self, namespace: &str, function: &str) -> Option<&FunctionSpec> {
        self.config.namespaces.get(namespace)?.functions.get(function)
    }

    /// The namespace and symbol of a project function.
    pub fn locate(&self, id: FunctionId) -> Option<(&str, &str)> {
        self.config.namespaces.iter().find_map(|(ns, c)| {
            c.functions
                .iter()
                .find(|(_, s)| s.id == id)
                .map(|(f, _)| (ns.as_str(), f.as_str()))
        })
    }

    /// Adds a namespace and writes its file with just the `ns` declaration.
    pub fn add_namespace(&mut self, namespace: &str) -> Result<(), String> {
        let namespace = namespace.trim();
        validate_namespace(namespace)?;
        if self.contains(namespace) {
            return Err(format!("{namespace} already exists"));
        }
        let path = self.new_file_path(namespace);
        if !path.exists() {
            write_file(&path, &format!("(ns {namespace})\n"))?;
        }
        let content = std::fs::read_to_string(&path).ok();
        self.files
            .insert(namespace.to_owned(), SourceFile { path, content });
        self.config
            .namespaces
            .insert(namespace.to_owned(), NamespaceConfig::default());
        self.save()
    }

    /// Adds a function to a namespace; its code is written once it has some.
    pub fn add_function(
        &mut self,
        namespace: &str,
        function: &str,
        arguments: u32,
    ) -> Result<FunctionId, String> {
        let function = function.trim();
        validate_symbol(function)?;
        let file = self.content(namespace);
        let defined = code::layout(&file).is_ok_and(|l| l.definitions.iter().any(|d| d.name == function));
        let entry = self
            .config
            .namespaces
            .get_mut(namespace)
            .ok_or_else(|| format!("{namespace} is not a namespace of this project"))?;
        if defined || entry.functions.contains_key(function) {
            return Err(format!("{function} is already defined in {namespace}"));
        }
        let spec = FunctionSpec::new(function, Vec::new(), arguments);
        let id = spec.id;
        entry.functions.insert(function.to_owned(), spec);
        self.save()?;
        Ok(id)
    }

    fn binding(&self, namespace: &str, function: &str, spec: &FunctionSpec) -> ProjectBinding {
        ProjectBinding {
            namespace: namespace.to_owned(),
            function: function.to_owned(),
            helpers: spec.helpers.clone(),
            arguments: spec.arguments,
            file: self.content(namespace),
        }
    }

    /// A new document for a function: its specification and its code in the file.
    pub fn document(&self, namespace: &str, function: &str) -> Option<FunctionDocument> {
        let spec = self.spec(namespace, function)?;
        let mut doc = FunctionDocument::new(spec.name.clone());
        doc.id = spec.id;
        spec.apply(&mut doc);
        let binding = self.binding(namespace, function, spec);
        doc.implementation = code::fragment(&binding).map(Implementation::manual);
        doc.project = Some(binding);
        Some(doc)
    }

    /// Brings a stored document of this project up to date with `ClojureCraft.json` and the file
    /// on disk, which both win over the stored draft. Returns whether `doc` changed.
    pub fn sync(&mut self, doc: &mut FunctionDocument) -> Result<bool, String> {
        let Some((namespace, function)) = self.locate(doc.id).map(|(n, f)| (n.to_owned(), f.to_owned()))
        else {
            return Ok(false);
        };
        self.reread(&namespace);
        let Some(spec) = self.spec(&namespace, &function).cloned() else {
            return Ok(false);
        };
        let mut changed = spec.apply(doc);
        let binding = self.binding(&namespace, &function, &spec);
        // The draft's code stands when putting it into the file gives the file as it is.
        let consistent = doc
            .implementation
            .as_ref()
            .is_some_and(|i| code::compose(&binding, &i.source).is_ok_and(|c| c.code == binding.file));
        let on_disk = code::fragment(&binding);
        if doc.project.as_ref() != Some(&binding) {
            doc.project = Some(binding);
            changed = true;
        }
        match (on_disk, &mut doc.implementation) {
            (Some(_), Some(_)) if consistent => {}
            (Some(text), Some(implementation)) => {
                implementation.source = text;
                changed = true;
            }
            (Some(text), None) => {
                doc.implementation = Some(Implementation::manual(text));
                changed = true;
            }
            (None, Some(_)) => {
                self.record(doc)?;
                changed = true;
            }
            (None, None) => {}
        }
        Ok(changed)
    }

    /// Takes `doc`'s specification into `ClojureCraft.json` (written after [`SAVE_DELAY`]) and
    /// writes its code into the namespace's file at once when it changed. The document's binding
    /// follows: its helpers are now the private definitions of its code, and its file the file
    /// as written.
    pub fn record(&mut self, doc: &mut FunctionDocument) -> Result<(), String> {
        let Some((namespace, function)) = self.locate(doc.id).map(|(n, f)| (n.to_owned(), f.to_owned()))
        else {
            return Ok(());
        };
        let mut binding = doc.project.clone().unwrap_or_else(|| ProjectBinding {
            namespace: namespace.clone(),
            function: function.clone(),
            helpers: Vec::new(),
            arguments: 1,
            file: String::new(),
        });
        binding.file = self.content(&namespace);
        let mut result = Ok(());
        if let Some(source) = doc.implementation.as_ref().map(|i| i.source.clone()) {
            match code::compose(&binding, &source) {
                Ok(composed) => {
                    if composed.code != binding.file {
                        let path = self.file_mut(&namespace).path.clone();
                        write_file(&path, &composed.code)?;
                        self.file_mut(&namespace).content = Some(composed.code.clone());
                    }
                    binding.helpers = composed.helpers;
                    binding.file = composed.code;
                }
                Err(e) => result = Err(format!("{namespace}/{function} was not written to its file: {e}")),
            }
        }
        if doc.project.as_ref() != Some(&binding) {
            doc.project = Some(binding);
        }
        let spec = FunctionSpec::from_doc(doc);
        if let Some(entry) = self.config.namespaces.get_mut(&namespace)
            && entry.functions.get(&function) != Some(&spec)
        {
            entry.functions.insert(function, spec);
            self.dirty_since.get_or_insert_with(Instant::now);
        }
        result
    }

    /// Gives a document of this project the namespace file as it is now, for example after
    /// another function of the same namespace was written. Returns whether it changed.
    pub fn refresh_file(&self, doc: &mut FunctionDocument) -> bool {
        let Some(binding) = doc.project.as_mut() else {
            return false;
        };
        let current = self.content(&binding.namespace);
        if self.locate(doc.id).is_none() || binding.file == current {
            return false;
        }
        binding.file = current;
        true
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
        for (namespace, entry) in &self.config.namespaces {
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
                            ..TreeNode::default()
                        });
                        level.len() - 1
                    }
                };
                if i + 1 == segments.len() {
                    level[index].namespace = true;
                    level[index].functions = self.functions(namespace, entry);
                }
                level = &mut level[index].children;
            }
        }
        roots
    }

    fn functions(&self, namespace: &str, entry: &NamespaceConfig) -> Vec<FunctionLeaf> {
        let in_file: Vec<String> = code::public_functions(&self.content(namespace))
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        let mut out: Vec<FunctionLeaf> = in_file
            .iter()
            .filter(|f| entry.functions.contains_key(*f))
            .map(|f| FunctionLeaf {
                name: f.clone(),
                defined: true,
            })
            .collect();
        out.extend(
            entry
                .functions
                .keys()
                .filter(|f| !in_file.contains(f))
                .map(|f| FunctionLeaf {
                    name: f.clone(),
                    defined: false,
                }),
        );
        out
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

    /// The namespace file's content as last read or written ("" before it exists).
    fn content(&self, namespace: &str) -> String {
        self.files
            .get(namespace)
            .and_then(|f| f.content.clone())
            .unwrap_or_default()
    }

    /// Reads the namespace's file again.
    fn reread(&mut self, namespace: &str) {
        let file = self.file_mut(namespace);
        file.content = std::fs::read_to_string(&file.path).ok();
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
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Err(e) = self.save_pending(true) {
            tracing::error!(error = %e, "project specification not saved");
        }
    }
}

fn write_file(path: &Path, content: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{} could not be created: {e}", dir.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("{} could not be written: {e}", path.display()))
}

/// Reads `ClojureCraft.json`, upgrading the first version (one specification per namespace) to
/// one function per entry. Returns whether it was upgraded (or did not exist).
fn read_config(path: &Path) -> Result<(Config, bool), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Config::default(), true)),
        Err(e) => return Err(format!("{CONFIG_FILE} could not be read: {e}")),
    };
    let unreadable = |e: serde_json::Error| format!("{CONFIG_FILE} could not be read: {e}");
    let mut value: serde_json::Value = serde_json::from_str(&text).map_err(unreadable)?;
    let upgraded = value.get("version").and_then(serde_json::Value::as_u64) == Some(1);
    if upgraded && let Some(namespaces) = value.get_mut("namespaces").and_then(|n| n.as_object_mut()) {
        for entry in namespaces.values_mut() {
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let spec = std::mem::take(entry);
            *entry = serde_json::json!({ "functions": { name: spec } });
        }
        value["version"] = CONFIG_VERSION.into();
    }
    let config = serde_json::from_value(value).map_err(unreadable)?;
    Ok((config, upgraded))
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

/// The project's classpath with its dependencies, from the Clojure CLI (`deps.edn`) or
/// Leiningen (`project.clj`), run in a login shell so a desktop launch finds them on the user's
/// `PATH`. Relative entries are made absolute; the source directories always come first.
pub fn resolve_classpath(root: &Path, source_dirs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let command = if root.join("deps.edn").is_file() {
        "clojure -Spath"
    } else if root.join("project.clj").is_file() {
        "lein classpath"
    } else {
        return Ok(source_dirs.to_vec());
    };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let output = std::process::Command::new(shell)
        .args(["-lc", command])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("`{command}` could not be run: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output");
        return Err(format!("`{command}` failed: {last}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default();
    Ok(classpath_entries(root, line, source_dirs))
}

fn classpath_entries(root: &Path, line: &str, source_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = source_dirs.to_vec();
    for entry in line.trim().split(':').filter(|e| !e.is_empty()) {
        let path = Path::new(entry);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
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

/// Function names this application creates: a plain Clojure symbol without a namespace.
pub fn validate_symbol(symbol: &str) -> Result<(), String> {
    let starts_well = symbol.starts_with(|c: char| c.is_ascii_alphabetic() || "*+!-_?<>=".contains(c));
    let number_like = symbol.starts_with(['-', '+']) && symbol[1..].starts_with(|d: char| d.is_ascii_digit());
    let characters = symbol
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "*+!-_?<>='".contains(c));
    if starts_well && !number_like && characters {
        Ok(())
    } else {
        Err(format!(
            "“{symbol}” is not a valid function name: use letters, digits and * + ! - _ ? < > =, not starting with a digit."
        ))
    }
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

    fn read(root: &Path, relative: &str) -> String {
        std::fs::read_to_string(root.join(relative)).expect("readable")
    }

    fn read_config(root: &Path) -> Config {
        let text = std::fs::read_to_string(root.join(CONFIG_FILE)).expect("config exists");
        serde_json::from_str(&text).expect("config parses")
    }

    const ORDERS: &str = "\
(ns shop.orders
  (:require [clojure.string :as str]))

(defn- clean [s] (str/trim s))

(defn- shared [x] x)

(defn label [order]
  (shared (clean (:name order))))

(defn price [order fee]
  (+ (shared (:amount order)) fee))
";

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
    }

    #[test]
    fn every_public_function_gets_an_empty_specification_with_its_helpers_and_arguments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "deps.edn", "{:paths [\"src\" \"resources\"] :deps {}}");
        write(root, "src/shop/orders.clj", ORDERS);
        write(root, "src/shop/web.cljs", "(ns shop.web)");
        write(root, "test/shop/orders_test.clj", "(ns shop.orders-test)");

        let project = Project::open(root).expect("opens");
        let config = read_config(root);
        assert_eq!(config.version, 2);
        assert_eq!(config.namespaces.keys().collect::<Vec<_>>(), ["shop.orders"]);
        let functions = &config.namespaces["shop.orders"].functions;
        assert_eq!(functions.keys().collect::<Vec<_>>(), ["label", "price"]);
        assert_eq!(functions["label"].helpers, ["clean"], "shared is not label's own");
        assert!(functions["price"].helpers.is_empty());
        assert_eq!(functions["price"].arguments, 2);
        assert!(functions["label"].description.is_empty() && functions["label"].examples.is_empty());

        let doc = project.document("shop.orders", "label").expect("listed");
        assert_eq!(doc.id, functions["label"].id);
        let binding = doc.project.as_ref().expect("bound");
        assert_eq!(
            (binding.namespace.as_str(), binding.function.as_str()),
            ("shop.orders", "label")
        );
        assert_eq!(binding.file, ORDERS);
        let source = doc.implementation.expect("existing code is imported").source;
        assert!(source.starts_with("(ns shop.orders\n  (:require [clojure.string :as str]))"));
        assert!(source.contains("(defn- clean") && source.contains("(defn label"));
        assert!(!source.contains("(defn price") && !source.contains("(defn- shared"));
        assert_eq!(
            project
                .document("shop.orders", "price")
                .expect("listed")
                .arguments(),
            2
        );
    }

    #[test]
    fn a_first_version_config_is_upgraded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(
            root,
            "src/a/core.clj",
            "(ns a.core)\n(defn- helper [x] x)\n(defn main-fn [x y] (helper x))\n",
        );
        write(
            root,
            CONFIG_FILE,
            r#"{"version":1,"namespaces":{"a.core":{"id":"5b0f8a3e-2f1c-4a51-9d0e-0b6a3f1c2d4e","name":"main-fn","description":"kept"}}}"#,
        );
        Project::open(root).expect("opens");
        let config = read_config(root);
        assert_eq!(config.version, 2);
        let spec = &config.namespaces["a.core"].functions["main-fn"];
        assert_eq!(spec.description, "kept");
        assert_eq!(spec.id.to_string(), "5b0f8a3e-2f1c-4a51-9d0e-0b6a3f1c2d4e");
        assert_eq!(spec.helpers, ["helper"]);
        assert_eq!(spec.arguments, 2);
    }

    #[test]
    fn a_malformed_config_is_not_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, CONFIG_FILE, "{not json");
        assert!(Project::open(root).is_err());
        assert_eq!(read(root, CONFIG_FILE), "{not json");
    }

    #[test]
    fn recording_writes_only_the_function_into_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/shop/orders.clj", ORDERS);
        let mut project = Project::open(root).expect("opens");
        let mut doc = project.document("shop.orders", "label").expect("listed");
        doc.description = "Clean label".into();
        doc.implementation = Some(Implementation::manual(
            "(ns shop.orders\n  (:require [clojure.string :as str]\n            [clojure.set :as set]))\n\n\
             (defn- shout [s] (str/upper-case s))\n\n(defn label [order]\n  (shout (shared (:name order))))\n",
        ));
        project.record(&mut doc).expect("recorded");
        let file = read(root, "src/shop/orders.clj");
        assert!(!file.contains("(defn- clean"));
        assert!(file.contains("(defn- shout [s] (str/upper-case s))\n\n(defn label [order]"));
        assert!(file.contains("(defn- shared [x] x)") && file.contains("(defn price [order fee]"));
        assert!(file.contains("[clojure.set :as set]"));
        let binding = doc.project.as_ref().expect("bound");
        assert_eq!(binding.helpers, ["shout"]);
        assert_eq!(binding.file, file);

        project.save_pending(true).expect("saves");
        let spec = &read_config(root).namespaces["shop.orders"].functions["label"];
        assert_eq!(spec.description, "Clean label");
        assert_eq!(spec.helpers, ["shout"]);

        // The other function of the namespace sees the new file.
        let mut price = project.document("shop.orders", "price").expect("listed");
        price.project.as_mut().expect("bound").file = ORDERS.into();
        assert!(project.refresh_file(&mut price));
        assert_eq!(price.project.expect("bound").file, file);
    }

    #[test]
    fn code_that_cannot_be_put_into_the_file_is_not_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/shop/orders.clj", ORDERS);
        let mut project = Project::open(root).expect("opens");
        let mut doc = project.document("shop.orders", "label").expect("listed");
        doc.implementation = Some(Implementation::manual("(ns shop.orders)\n(defn label [o] (str o"));
        assert!(project.record(&mut doc).is_err());
        doc.implementation = Some(Implementation::manual(
            "(ns shop.orders)\n(defn- shared [x] 1)\n(defn label [o] o)",
        ));
        let e = project.record(&mut doc).expect_err("conflict");
        assert!(e.contains("shared is already defined"), "{e}");
        assert_eq!(read(root, "src/shop/orders.clj"), ORDERS);
    }

    #[test]
    fn syncing_keeps_a_consistent_draft_and_takes_outside_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/shop/orders.clj", ORDERS);
        let mut project = Project::open(root).expect("opens");
        let mut doc = project.document("shop.orders", "price").expect("listed");
        // Written with its own formatting: the draft keeps it.
        let mine = "(ns shop.orders (:require [clojure.string :as str]))\n(defn price [order fee]\n  (+ (:amount order) fee))";
        doc.implementation = Some(Implementation::manual(mine));
        project.record(&mut doc).expect("recorded");
        assert!(!project.sync(&mut doc).expect("synced"));
        assert_eq!(doc.implementation.as_ref().expect("code").source, mine);

        // A stored draft with an outdated specification and code changed by another editor.
        doc.description = "old".into();
        doc.examples = vec![Example::direct("", Edn::new("1"), Edn::new("1"))];
        let changed =
            read(root, "src/shop/orders.clj").replace("(+ (:amount order) fee)", "(- (:amount order) fee)");
        write(root, "src/shop/orders.clj", &changed);
        assert!(project.sync(&mut doc).expect("synced"));
        assert!(doc.description.is_empty() && doc.examples.is_empty());
        assert!(
            doc.implementation
                .expect("code")
                .source
                .contains("(- (:amount order) fee)")
        );
        assert_eq!(doc.project.expect("bound").file, changed);
    }

    #[test]
    fn new_namespaces_and_functions_are_validated_and_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/shop/orders.clj", ORDERS);
        let mut project = Project::open(root).expect("opens");
        assert!(project.add_namespace("shop.orders").is_err());
        assert!(project.add_namespace("shop..x").is_err());
        project.add_namespace("shop.orders.tax").expect("added");
        assert_eq!(read(root, "src/shop/orders/tax.clj"), "(ns shop.orders.tax)\n");
        assert!(read_config(root).namespaces.contains_key("shop.orders.tax"));

        assert!(project.add_function("shop.orders", "label", 1).is_err());
        assert!(
            project.add_function("shop.orders", "shared", 1).is_err(),
            "a private helper's name"
        );
        assert!(project.add_function("shop.orders", "2fast", 1).is_err());
        assert!(project.add_function("shop.missing", "f", 1).is_err());
        let id = project.add_function("shop.orders.tax", "vat?", 2).expect("added");
        assert_eq!(
            read_config(root).namespaces["shop.orders.tax"].functions["vat?"].id,
            id
        );

        let mut doc = project.document("shop.orders.tax", "vat?").expect("listed");
        assert!(doc.implementation.is_none());
        assert_eq!(doc.arguments(), 2);
        doc.implementation = Some(Implementation::manual(
            "(ns shop.orders.tax)\n\n(defn vat? [a b] (> a b))\n",
        ));
        project.record(&mut doc).expect("recorded");
        assert_eq!(
            read(root, "src/shop/orders/tax.clj"),
            "(ns shop.orders.tax)\n\n(defn vat? [a b] (> a b))\n"
        );
    }

    #[test]
    fn the_tree_lists_namespaces_and_their_functions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/shop/orders.clj", ORDERS);
        write(
            root,
            "src/shop/orders/tax.clj",
            "(ns shop.orders.tax)\n(defn vat [x] x)\n",
        );
        write(root, "src/single.clj", "(ns single)");
        let mut project = Project::open(root).expect("opens");
        project.add_function("shop.orders", "discount", 1).expect("added");
        let tree = project.tree();
        assert_eq!(tree.len(), 2);
        let shop = &tree[0];
        assert_eq!((shop.segment.as_str(), shop.namespace), ("shop", false));
        let orders = &shop.children[0];
        assert_eq!((orders.path.as_str(), orders.namespace), ("shop.orders", true));
        assert_eq!(
            orders.functions,
            vec![
                FunctionLeaf {
                    name: "label".into(),
                    defined: true
                },
                FunctionLeaf {
                    name: "price".into(),
                    defined: true
                },
                FunctionLeaf {
                    name: "discount".into(),
                    defined: false
                },
            ]
        );
        assert_eq!(orders.children[0].path, "shop.orders.tax");
        assert_eq!(orders.children[0].functions[0].name, "vat");
        assert_eq!((tree[1].path.as_str(), tree[1].functions.len()), ("single", 0));
    }

    #[test]
    fn classpath_entries_are_absolute_with_the_sources_first() {
        let root = Path::new("/p");
        let src = vec![PathBuf::from("/p/src")];
        assert_eq!(
            classpath_entries(root, "src:resources:/m2/clojure.jar\n", &src),
            [
                PathBuf::from("/p/src"),
                PathBuf::from("/p/resources"),
                PathBuf::from("/m2/clojure.jar")
            ]
        );
    }

    /// The installed runtime, or `None` (the test then passes without running).
    fn runtime() -> Option<clojurecraft_clojure::ClojureRuntime> {
        use clojurecraft_runtime::{DataDirs, Manifest, components, platform_key};
        let manifest = Manifest::bundled().ok()?;
        let dirs = DataDirs::discover()?;
        let (java, java_home) = components::java(&manifest, &dirs, &platform_key())?;
        let jars = components::clojure_jars(&manifest, &dirs)?;
        let helper = std::env::current_exe()
            .ok()?
            .parent()?
            .parent()?
            .join(clojurecraft_sandbox::HELPER_NAME);
        helper.is_file().then(|| clojurecraft_clojure::ClojureRuntime {
            java,
            java_home,
            jars,
            sandbox_helper: helper,
            work_dir: std::env::temp_dir().join(format!("clojurecraft-app-test-{}", std::process::id())),
            project_classpath: Vec::new(),
        })
    }

    #[test]
    fn a_function_using_a_dependency_and_another_namespace_passes_in_the_sandbox() {
        let Some(mut rt) = runtime() else {
            eprintln!("skipping: runtime or sandbox helper not installed");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(
            root,
            "deps.edn",
            "{:paths [\"src\"] :deps {org.clojure/data.csv {:mvn/version \"1.1.0\"}}}",
        );
        write(
            root,
            "src/shop/util.clj",
            "(ns shop.util)\n(defn clean [s] (.trim ^String s))\n",
        );
        write(
            root,
            "src/shop/report.clj",
            "(ns shop.report\n  (:require [clojure.data.csv :as csv]\n            [shop.util :as util]))\n\n\
             (def registry (atom {}))\n\n\
             (defn rows [text column]\n  (mapv #(util/clean (nth % column)) (csv/read-csv text)))\n",
        );
        let project = Project::open(root).expect("opens");
        let classpath = match resolve_classpath(project.root(), &project.source_dirs()) {
            Ok(cp) => cp,
            Err(e) => {
                eprintln!("skipping: {e}");
                return;
            }
        };
        assert!(
            classpath.iter().any(|p| p.to_string_lossy().contains("data.csv")),
            "{classpath:?}"
        );
        rt.project_classpath = classpath;

        let mut doc = project.document("shop.report", "rows").expect("listed");
        assert_eq!(doc.arguments(), 2);
        doc.examples = vec![Example::direct(
            "",
            Edn::new("[\"a, b\\n c ,d\" 1]"),
            Edn::new("[\"b\" \"d\"]"),
        )];
        let source = doc.implementation.clone().expect("imported").source;
        let names = clojurecraft_workflow::Names::for_document(&doc, |_| false);
        let plan = clojurecraft_workflow::validation::plan(&doc, &source, &names);
        assert!(
            plan.structure.problems.is_empty(),
            "{:?}",
            plan.structure.problems
        );
        let request = plan.request.clone().expect("runnable");
        let outcome = rt
            .evaluate(
                &request,
                &clojurecraft_clojure::Limits::for_request(&request),
                None,
            )
            .expect("ran");
        let report = clojurecraft_workflow::validation::report(
            &plan,
            clojurecraft_workflow::validation::ReportContext {
                basis: doc.basis(),
                environment: clojurecraft_domain::ExecutionEnvironment {
                    clojure_version: String::new(),
                    java_version: String::new(),
                    platform: String::new(),
                },
                attempt: None,
                run: None,
                outcome: Ok(&outcome),
            },
        );
        assert!(report.passed(), "{:#?}\n{}", report.checks, outcome.stderr);
    }

    #[test]
    fn function_names_are_validated() {
        for ok in ["price", "valid?", "->map", "swap!", "a*b", "<=x"] {
            assert!(validate_symbol(ok).is_ok(), "{ok}");
        }
        for bad in ["", "2x", "a/b", "a.b", "-1x", "a b"] {
            assert!(validate_symbol(bad).is_err(), "{bad}");
        }
    }
}
