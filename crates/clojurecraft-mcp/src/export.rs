//! Writing an exported project (`export_project`) and the agent's view of it.
//!
//! Nothing is written unless every file can be: existing files with other content are listed
//! and left alone unless the agent asked to overwrite them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clojurecraft_domain::FunctionId;
use clojurecraft_workflow::export::Project;
use serde_json::{Value, json};

/// What happened to one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    Created,
    Updated,
    Unchanged,
}

/// Writes `project` under `root`.
pub fn write(root: &Path, project: &Project, overwrite: bool) -> Result<Vec<(String, Written)>, String> {
    let mut plan = Vec::new();
    let mut conflicts = Vec::new();
    for f in &project.files {
        let path = root.join(&f.path);
        let state = match std::fs::read(&path) {
            Ok(old) if old == f.content.as_bytes() => Written::Unchanged,
            Ok(_) => {
                conflicts.push(f.path.clone());
                Written::Updated
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Written::Created,
            Err(e) => return Err(format!("Could not read {}: {e}", path.display())),
        };
        plan.push((f, path, state));
    }
    if !conflicts.is_empty() && !overwrite {
        return Err(format!(
            "Nothing was written: these files already exist in {} with other content: {}. Call \
             again with overwrite=true to replace them, or choose another directory.",
            root.display(),
            conflicts.join(", ")
        ));
    }
    let mut out = Vec::new();
    for (f, path, state) in plan {
        if state != Written::Unchanged {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
            }
            std::fs::write(&path, &f.content)
                .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
        }
        out.push((f.path.clone(), state));
    }
    Ok(out)
}

/// The installed Java runtime and Clojure jars, for running the tests without the Clojure CLI.
pub struct Runtime<'a> {
    pub java: &'a Path,
    pub jars: &'a [PathBuf],
}

/// The reply: files written, each function with its check state, and how to run the tests.
/// `passes` holds whether each function's current code passes its checks (`None`: not checked
/// against the current specification and tests).
pub fn reply(
    root: &Path,
    project: &Project,
    written: &[(String, Written)],
    passes: &BTreeMap<FunctionId, Option<bool>>,
    runtime: Option<Runtime<'_>>,
) -> Value {
    let mut files: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (path, state) in written {
        let key = match state {
            Written::Created => "created",
            Written::Updated => "updated",
            Written::Unchanged => "unchanged",
        };
        files.entry(key).or_default().push(path);
    }
    let mut warnings = Vec::new();
    let functions: Vec<Value> = project
        .functions
        .iter()
        .map(|f| {
            let pass = passes.get(&f.id).copied().flatten();
            match pass {
                Some(true) => {}
                Some(false) => warnings.push(format!("{} fails some of its checks.", f.namespace)),
                None => warnings.push(format!(
                    "{} was not checked against its current specification and tests.",
                    f.namespace
                )),
            }
            for l in &f.left_out {
                warnings.push(format!("{}: left out {l}.", f.namespace));
            }
            json!({
                "id": f.id.to_string(),
                "require": format!("[{} :refer [{}]]", f.namespace, f.function),
                "assertions": f.assertions,
                "passes_all_checks": pass,
            })
        })
        .collect();
    let namespaces = project.test_namespaces();
    let quoted: Vec<String> = namespaces.iter().map(|n| format!("'{n}")).collect();
    let mut run = json!({"clojure_cli": "clojure -X:test"});
    if let Some(r) = runtime {
        let mut cp = vec!["src".to_owned(), "test".to_owned()];
        cp.extend(r.jars.iter().map(|j| j.display().to_string()));
        run["java"] = json!(format!(
            "cd \"{}\" && \"{}\" -cp \"{}\" clojure.main -e \"(require 'clojure.test {}) \
             (System/exit (if (clojure.test/successful? (clojure.test/run-tests {})) 0 1))\"",
            root.display(),
            r.java.display(),
            cp.join(":"),
            quoted.join(" "),
            quoted.join(" "),
        ));
    }
    let mut v = json!({
        "directory": root.display().to_string(),
        "files": files,
        "functions": functions,
        "run_tests": run,
        "not_exported": "output contracts (checked only inside ClojureCraft)",
    });
    if !warnings.is_empty() {
        v["warnings"] = json!(warnings);
    }
    v
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{Edn, Example, FunctionDocument, Implementation};
    use clojurecraft_workflow::export;

    use super::*;

    fn project() -> (Project, FunctionId) {
        let mut d = FunctionDocument::new("Sum numbers");
        d.examples = vec![Example::direct("pair", Edn::new("[1 2]"), Edn::new("3"))];
        d.implementation = Some(Implementation::manual(
            "(ns sum-numbers.core)\n(defn sum-numbers [xs] (reduce + xs))",
        ));
        (export::project(&[&d], "1.12.6").unwrap(), d.id)
    }

    #[test]
    fn files_are_written_and_differing_ones_only_replaced_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let (p, id) = project();
        let written = write(dir.path(), &p, false).unwrap();
        assert!(written.iter().all(|(_, s)| *s == Written::Created));
        assert!(dir.path().join("src/sum_numbers/core.clj").is_file());
        assert!(dir.path().join("deps.edn").is_file());

        let again = write(dir.path(), &p, false).unwrap();
        assert!(again.iter().all(|(_, s)| *s == Written::Unchanged));

        std::fs::write(dir.path().join("deps.edn"), "{}").unwrap();
        let e = write(dir.path(), &p, false).unwrap_err();
        assert!(e.contains("deps.edn") && e.contains("overwrite"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("deps.edn")).unwrap(),
            "{}"
        );
        let replaced = write(dir.path(), &p, true).unwrap();
        assert!(replaced.contains(&("deps.edn".into(), Written::Updated)));

        let passes = BTreeMap::from([(id, Some(false))]);
        let jars = [PathBuf::from("/r/clojure.jar")];
        let v = reply(
            dir.path(),
            &p,
            &replaced,
            &passes,
            Some(Runtime {
                java: Path::new("/r/java"),
                jars: &jars,
            }),
        );
        assert_eq!(v["files"]["updated"][0], "deps.edn");
        assert_eq!(
            v["functions"][0]["require"],
            "[sum-numbers.core :refer [sum-numbers]]"
        );
        assert!(v["warnings"][0].as_str().unwrap().contains("fails"));
        let java = v["run_tests"]["java"].as_str().unwrap();
        assert!(java.contains("src:test:/r/clojure.jar"));
        assert!(java.contains("(clojure.test/run-tests 'sum-numbers.core-test)"));
    }
}
