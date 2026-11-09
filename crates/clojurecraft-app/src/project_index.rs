//! Building the index of an open project: its own sources and the Clojure sources in the jars
//! and directories of its classpath, written next to the documentation collections.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use clojurecraft_knowledge::chunk::SourceKey;
use clojurecraft_knowledge::embed::{Cancelled, Embedder};
use clojurecraft_knowledge::project::build_project_index;
use clojurecraft_knowledge::{KnowledgeDirs, Provenance};
use clojurecraft_workflow::project_index::{self, SourceFile};

const EXTENSIONS: [&str; 2] = ["clj", "cljc"];
/// Larger source files are skipped; they are generated or data, not code to reuse.
const MAX_FILE_BYTES: u64 = 1_000_000;
/// Files read at most, so a huge classpath cannot stall indexing.
const MAX_FILES: usize = 5000;
/// Jars whose namespaces the documentation collection already covers.
const SKIPPED_JARS: [&str; 3] = ["clojure-1.", "spec.alpha-", "core.specs.alpha-"];

/// A finished project index.
#[derive(Clone, Debug)]
pub struct Indexed {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub graph: BTreeMap<String, BTreeSet<String>>,
    pub chunks: usize,
    /// Definitions generated code may not call.
    pub restricted: usize,
    /// Namespaces from dependencies.
    pub libraries: usize,
    /// Chunks embedded anew; the rest kept their vectors.
    pub embedded: usize,
    pub semantic: bool,
    pub unreadable: Vec<String>,
}

/// Where the index of the project at `root` is kept.
pub fn index_dir(dirs: &KnowledgeDirs, root: &Path) -> PathBuf {
    // FNV-1a: a stable name for the project's folder.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_string_lossy().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dirs.root().join("projects").join(format!("{name}-{h:016x}"))
}

fn is_source(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| EXTENSIONS.iter().any(|x| e == *x))
}

fn scan_dir(dir: &Path, root: &Path, library: bool, out: &mut Vec<SourceFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        if out.len() >= MAX_FILES {
            return;
        }
        let hidden = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            scan_dir(&path, root, library, out);
        } else if is_source(&path.to_string_lossy())
            && std::fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_FILE_BYTES)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            let location = path.strip_prefix(root).unwrap_or(&path).display().to_string();
            out.push(SourceFile {
                location,
                text,
                library,
            });
        }
    }
}

fn scan_jar(jar: &Path, out: &mut Vec<SourceFile>) {
    let name = jar
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if SKIPPED_JARS.iter().any(|s| name.starts_with(s)) {
        return;
    }
    let Ok(file) = std::fs::File::open(jar) else {
        return;
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return;
    };
    for i in 0..zip.len() {
        if out.len() >= MAX_FILES {
            return;
        }
        let Ok(mut entry) = zip.by_index(i) else {
            continue;
        };
        let entry_name = entry.name().to_owned();
        if !entry.is_file() || !is_source(&entry_name) || entry.size() > MAX_FILE_BYTES {
            continue;
        }
        let mut text = String::new();
        if entry.read_to_string(&mut text).is_ok() {
            out.push(SourceFile {
                location: format!("{name}!/{entry_name}"),
                text,
                library: true,
            });
        }
    }
}

/// The project's own sources first, then the Clojure sources of the rest of its classpath.
pub fn collect(root: &Path, source_dirs: &[PathBuf], classpath: &[PathBuf]) -> Vec<SourceFile> {
    let mut out = Vec::new();
    for dir in source_dirs {
        scan_dir(dir, root, false, &mut out);
    }
    for entry in classpath.iter().filter(|e| !source_dirs.contains(e)) {
        if entry.is_dir() {
            // Resource and test directories of the project itself are not dependencies.
            let library = !entry.starts_with(root);
            if library {
                scan_dir(entry, entry, true, &mut out);
            }
        } else if entry.extension().is_some_and(|e| e == "jar") {
            scan_jar(entry, &mut out);
        }
    }
    out
}

fn provenances(root: &Path) -> [Provenance; 2] {
    [
        Provenance {
            source: SourceKey::Project,
            name: "project sources".into(),
            license: "the project's own code".into(),
            attribution: String::new(),
            revision: String::new(),
            origin: root.display().to_string(),
        },
        Provenance {
            source: SourceKey::Library,
            name: "dependency sources".into(),
            license: "see each dependency".into(),
            attribution: String::new(),
            revision: String::new(),
            origin: "the project's classpath".into(),
        },
    ]
}

/// Reads and indexes the project into `dir`.
pub fn build(
    root: &Path,
    source_dirs: &[PathBuf],
    classpath: &[PathBuf],
    dir: &Path,
    embedder: Option<&mut dyn Embedder>,
    clojure_version: &str,
    cancelled: &Cancelled<'_>,
) -> Result<Indexed, String> {
    let files = collect(root, source_dirs, classpath);
    let index = project_index::index(&files);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let semantic = embedder.is_some();
    let (info, embedded) = build_project_index(
        dir,
        &index.chunks,
        &provenances(root),
        clojure_version,
        embedder,
        cancelled,
    )
    .map_err(|e| e.to_string())?;
    Ok(Indexed {
        root: root.to_owned(),
        dir: dir.to_owned(),
        chunks: info.chunks,
        restricted: index.chunks.iter().filter(|c| c.restricted).count(),
        libraries: index.library.len(),
        embedded,
        semantic,
        unreadable: index.unreadable,
        graph: index.graph,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn sources_and_dependency_jars_are_collected_but_the_clojure_jar_is_not() {
        let root = tempfile::tempdir().expect("temp dir");
        let src = root.path().join("src/shop");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("util.clj"), "(ns shop.util)\n(defn clean [s] s)\n").unwrap();
        std::fs::write(src.join("notes.txt"), "not code").unwrap();
        let jar = |name: &str, entries: &[(&str, &str)]| {
            let path = root.path().join(name);
            let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            for (entry, text) in entries {
                w.start_file(*entry, zip::write::SimpleFileOptions::default())
                    .unwrap();
                w.write_all(text.as_bytes()).unwrap();
            }
            w.finish().unwrap();
            path
        };
        let lib = jar(
            "xlib-1.0.jar",
            &[
                ("xlib/ids.clj", "(ns xlib.ids)\n(defn make [n] n)\n"),
                ("META-INF/MANIFEST.MF", "x"),
            ],
        );
        let core = jar("clojure-1.12.4.jar", &[("clojure/core.clj", "(ns clojure.core)")]);
        let sources = [root.path().join("src")];
        let files = collect(root.path(), &sources, &[sources[0].clone(), lib, core]);
        let found: Vec<(&str, bool)> = files.iter().map(|f| (f.location.as_str(), f.library)).collect();
        assert_eq!(
            found,
            [("src/shop/util.clj", false), ("xlib-1.0.jar!/xlib/ids.clj", true)]
        );
    }
}
