//! Runs the application's specification assistant against a real model: optional interpretation
//! and contract proposal, then generation with documentation retrieval, validation in the
//! sandbox (parse, structure, lint, load, examples, contract), automatic repair, and explanation.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clojurecraft_clojure::{ClojureRuntime, EvalRequest, Limits};
use clojurecraft_domain::{
    CheckStatus, Edn, Example, ExecutionEnvironment, FunctionDocument, FunctionId, InferenceRuntime,
    ModelIdentity, Step, ValidationReport,
};
use clojurecraft_hardware::Backend;
use clojurecraft_inference::{InferenceService, LlamaEngine, LoadSpec, ModelStatus, RequestId, ServiceEvent};
use clojurecraft_knowledge::{Embedder, KnowledgeDirs, LlamaEmbedder, Retriever};
use clojurecraft_runtime::catalog::Catalog;
use clojurecraft_runtime::models::ModelRecord;
use clojurecraft_runtime::{Manifest, components};
use clojurecraft_workflow::assistant::{
    self, Assistant, Attempt, Execution, Gather, Gathered, ModelContext, Phase, Ports, Report, Update,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct SpecFile {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    steps: Vec<String>,
    #[serde(default)]
    examples: Vec<ExampleFile>,
}

#[derive(Deserialize)]
struct ExampleFile {
    #[serde(default)]
    name: String,
    input: String,
    expected: String,
}

fn document(path: &str) -> Result<FunctionDocument> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    if path.ends_with(".json") {
        // A library draft as stored by the application, including its contract.
        let mut doc: FunctionDocument =
            serde_json::from_str(&text).with_context(|| format!("parsing {path}"))?;
        doc.implementation = None;
        return Ok(doc);
    }
    let spec: SpecFile = toml::from_str(&text).with_context(|| format!("parsing {path}"))?;
    let mut doc = FunctionDocument::new(spec.name);
    doc.description = spec.description;
    doc.steps = spec.steps.into_iter().map(Step::new).collect();
    doc.examples = spec
        .examples
        .into_iter()
        .map(|e| Example::direct(e.name, Edn::new(e.input), Edn::new(e.expected)))
        .collect();
    Ok(doc)
}

pub struct Options {
    /// Print each request's raw answer.
    pub verbose: bool,
    pub interpret: bool,
    pub contract: bool,
    pub documentation: bool,
    /// Generate edge cases from the examples instead of code.
    pub edge_cases: bool,
}

struct DevPorts<'a> {
    service: &'a InferenceService,
    docs: Option<(Retriever, Option<LlamaEmbedder>, String)>,
    gathered: Option<(u64, Result<Gathered, String>)>,
    runtime: Result<ClojureRuntime, String>,
    environment: ExecutionEnvironment,
    runs: u64,
    validated: Option<(u64, Execution)>,
}

impl Ports for DevPorts<'_> {
    fn generate(&mut self, request: clojurecraft_inference::GenerateRequest) -> Result<RequestId, String> {
        Ok(self.service.generate(request))
    }

    fn cancel(&mut self, request: RequestId) {
        self.service.cancel(request);
    }

    fn gather(&mut self, g: Gather) -> Option<u64> {
        let (retriever, embedder, version) = self.docs.as_mut()?;
        let t = Instant::now();
        let result = assistant::gather(
            retriever,
            embedder.as_mut().map(|e| e as &mut dyn Embedder),
            &g,
            version,
        )
        .map(|(found, _)| found);
        if let Ok(found) = &result {
            println!(
                "  documentation: {} excerpts from {} queries in {:.1?}: {}",
                found.hits.len(),
                g.queries.len(),
                t.elapsed(),
                found
                    .hits
                    .iter()
                    .map(|h| h.chunk.title.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        self.gathered = Some((1, result));
        Some(1)
    }

    /// Runs synchronously; the result is delivered by the driving loop.
    fn validate(&mut self, request: EvalRequest) -> Result<u64, String> {
        let rt = self.runtime.as_ref().map_err(Clone::clone)?;
        let t = Instant::now();
        let outcome = rt
            .evaluate(&request, &Limits::for_request(&request), None)
            .map_err(|e| e.to_string());
        println!(
            "    sandboxed run of {} calls in {:.1?}",
            request.calls.len(),
            t.elapsed()
        );
        self.runs += 1;
        self.validated = Some((
            self.runs,
            Execution {
                outcome,
                environment: self.environment.clone(),
            },
        ));
        Ok(self.runs)
    }

    fn cancel_validation(&mut self, _: u64) {}
}

fn print_validation(r: &ValidationReport) {
    let failing: Vec<_> = r
        .checks
        .iter()
        .filter(|c| !matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped))
        .collect();
    println!(
        "    attempt {}: {}",
        r.attempt.unwrap_or(0),
        if r.passed() {
            "all checks passed".to_owned()
        } else {
            format!("{} of {} checks failed", failing.len(), r.checks.len())
        }
    );
    for c in failing {
        println!("      {:?} {:?}", c.subject, c.status);
        for d in &c.diagnostics {
            println!("        {}", d.chars().take(300).collect::<String>());
        }
        if let (Some(e), Some(a)) = (&c.expected, &c.actual) {
            println!(
                "        expected {}",
                e.as_str().chars().take(200).collect::<String>()
            );
            println!(
                "        actual   {}",
                a.as_str().chars().take(200).collect::<String>()
            );
        }
    }
}

fn print_attempts(attempts: &[Attempt]) {
    for a in attempts {
        match (&a.report, &a.note) {
            (Some(r), _) => print_validation(r),
            (None, Some(n)) => println!("    attempt {}: {n}", a.number),
            (None, None) => println!("    attempt {}: not checked", a.number),
        }
    }
}

fn open_docs() -> Option<(Retriever, Option<LlamaEmbedder>, String)> {
    let dirs = clojurecraft_runtime::DataDirs::discover()?;
    let active = KnowledgeDirs::new(&dirs).collections().active()?.ok()?;
    let retriever = Retriever::open(&active.dir).ok()?;
    let embedder = crate::knowledge::cpu_embedder().ok().flatten();
    Some((retriever, embedder, active.version))
}

/// Feeds service events to the assistant until its job ends.
fn drive(
    assistant: &mut Assistant,
    docs: &mut HashMap<FunctionId, FunctionDocument>,
    ports: &mut DevPorts<'_>,
    rx: &Receiver<ServiceEvent>,
    verbose: bool,
) {
    let started = Instant::now();
    let mut phase: Option<(Phase, Instant)> = None;
    let mut chars = 0usize;
    let mut output = String::new();
    while let Some(job) = assistant.job() {
        if phase.map(|(p, _)| p) != Some(job.phase) {
            if let Some((p, since)) = phase {
                println!(
                    "    {} took {:.1?} ({chars} characters)",
                    p.label(),
                    since.elapsed()
                );
                if verbose {
                    println!("{output}");
                }
            }
            println!("  {}…", job.phase.label());
            phase = Some((job.phase, Instant::now()));
            chars = 0;
        }
        output.clone_from(&job.output);
        if let Some((id, result)) = ports.gathered.take() {
            assistant.on_gathered(id, result, ports);
            continue;
        }
        if let Some((id, execution)) = ports.validated.take() {
            for u in assistant.on_validated(id, execution, docs, ports) {
                if let Update::Validated(_, r) = u {
                    print_validation(&r);
                }
            }
            continue;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServiceEvent::Status(s)) => match s {
                ModelStatus::Ready { .. } | ModelStatus::Loading { .. } => {}
                other => println!("  model: {other:?}"),
            },
            Ok(event) => {
                if let ServiceEvent::Stream {
                    event: clojurecraft_inference::StreamEvent::Content(t),
                    ..
                } = &event
                {
                    chars += t.len();
                }
                for u in assistant.on_model(&event, docs, ports) {
                    match u {
                        Update::Edited(_) => {}
                        Update::Commit(_, r) => println!("  would record revision: {:?}", r.kind),
                        Update::Validated(_, r) => print_validation(&r),
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some((p, since)) = phase {
        println!(
            "    {} took {:.1?} ({chars} characters)",
            p.label(),
            since.elapsed()
        );
        if verbose {
            println!("{output}");
        }
    }
    println!("  total {:.1?}", started.elapsed());
}

fn print_report(report: Option<&Report>) {
    match report {
        Some(Report::Failed {
            what, message, raw, ..
        }) => {
            println!("  could not {what}: {message}");
            if let Some(raw) = raw {
                println!("  raw answer:\n{}", raw.chars().take(3000).collect::<String>());
            }
        }
        Some(Report::Unresolved {
            attempts,
            best,
            notes,
        }) => {
            println!("  unresolved after {} attempts:", attempts.len());
            for n in notes {
                println!("  note: {n}");
            }
            if let Some(code) = attempts.iter().rev().find_map(|a| a.code.as_ref()) {
                println!("  last code:\n{code}");
            }
            if let Some(b) = best {
                println!(
                    "  best candidate offered for review: attempt {:?}",
                    b.validation.as_ref().and_then(|r| r.attempt)
                );
            }
        }
        Some(Report::Pending(p)) => {
            println!(
                "  held for review (needs approval: {:?}, replaces manual code: {}, passed: {}):\n{}",
                p.needs_approval,
                p.replaces_manual,
                p.passed(),
                p.candidate.code
            );
        }
        Some(Report::Blocked(why)) => println!("  blocked: {why:?}"),
        Some(other) => println!("  {other:?}"),
        None => println!("  (no report)"),
    }
}

pub fn run(
    spec_path: &str,
    model_id: &str,
    quant: &str,
    backend: &str,
    context: u32,
    options: &Options,
) -> Result<()> {
    let doc = document(spec_path)?;
    let manifest = Manifest::bundled()?;
    let dirs = crate::dirs()?;
    let backend = crate::backend_arg(backend)?;
    let catalog = Catalog::bundled()?;
    let model = catalog
        .model(model_id)
        .ok_or_else(|| anyhow!("unknown model {model_id}"))?;
    let q = model
        .quants
        .iter()
        .find(|x| x.name == quant)
        .ok_or_else(|| anyhow!("unknown quantization {quant}"))?;
    let record = ModelRecord::from_catalog(model, q);
    let (files, name) = crate::model_paths(model_id, quant)?;
    let path = files
        .first()
        .map(|(p, _)| p.clone())
        .ok_or_else(|| anyhow!("no files"))?;
    if !path.is_file() {
        bail!(
            "{} is missing; run fetch-model {model_id} {quant}",
            path.display()
        );
    }
    let server = components::llama_server_for(&manifest, &dirs, backend)
        .with_context(|| format!("llama.cpp for {} is not installed", backend.label()))?;
    let label = format!("{name} · {quant}");
    let spec = LoadSpec {
        server,
        model: path,
        label: label.clone(),
        backend: backend.label().to_owned(),
        runtime: components::llama_version_id(&manifest, backend),
        context,
        gpu_layers: if backend == Backend::Cpu { Some(0) } else { None },
        total_layers: record.layers,
        model_max_context: record.context_length,
        threads: None,
        load_timeout: Duration::from_secs(600),
        memory: None,
        skip_memory_check: true,
        pid_file: None,
    };
    let (tx, rx) = mpsc::channel();
    let mut service = InferenceService::new(
        Box::new(LlamaEngine::default()),
        None,
        Box::new(move |e| {
            let _ = tx.send(e);
        }),
    );
    println!("loading {label} on {} with context {context}", backend.label());
    let t = Instant::now();
    service.load(spec.clone());
    loop {
        match rx.recv_timeout(Duration::from_secs(600))? {
            ServiceEvent::Status(ModelStatus::Ready { .. }) => break,
            ServiceEvent::Status(s @ (ModelStatus::Failed { .. } | ModelStatus::Crashed { .. })) => {
                bail!("model did not load: {s:?}")
            }
            _ => {}
        }
    }
    println!("  ready after {:.1?}", t.elapsed());

    let file = record.files.first().ok_or_else(|| anyhow!("no files"))?;
    let model_ctx = ModelContext {
        label,
        model: ModelIdentity {
            display_name: record.display_name.clone(),
            repository: record.repo.clone(),
            revision: record.revision.clone(),
            file: file.repo_path.clone(),
            sha256: file.sha256.clone(),
            quantization: record.quant.clone(),
            architecture: record.architecture.clone(),
        },
        runtime: InferenceRuntime {
            llama_cpp_release: manifest.llama_cpp.release.clone(),
            backend: backend.label().to_lowercase(),
        },
        context_length: context,
        clojure_version: manifest.clojure.version.clone(),
    };
    let mut ports = DevPorts {
        service: &service,
        docs: if options.documentation { open_docs() } else { None },
        gathered: None,
        runtime: crate::clojure_runtime().map_err(|e| e.to_string()),
        environment: ExecutionEnvironment {
            clojure_version: manifest.clojure.version.clone(),
            java_version: format!("{} {}", manifest.jre.vendor, manifest.jre.version),
            platform: clojurecraft_runtime::platform_key(),
        },
        runs: 0,
        validated: None,
    };
    if let Err(e) = &ports.runtime {
        println!("  (code cannot be run: {e})");
    }
    if options.documentation && ports.docs.is_none() {
        println!("  (no documentation collection installed; generating without documentation)");
    }
    let id = doc.id;
    let mut docs = HashMap::from([(id, doc)]);
    let mut assistant = Assistant::default();

    if options.edge_cases {
        println!("\nedge cases from the title, description, and examples");
        let t = Instant::now();
        assistant.generate_tests_from_examples(&docs[&id], &mut ports);
        drive(&mut assistant, &mut docs, &mut ports, &rx, options.verbose);
        print_report(assistant.report(id));
        for c in &docs[&id].tests {
            let expectation = match &c.expectation {
                clojurecraft_domain::Expectation::Value { expected } => expected.as_str().to_owned(),
                clojurecraft_domain::Expectation::NeedsClarification { question } => {
                    format!("? {question}")
                }
            };
            println!(
                "  {}\n    in:  {}\n    out: {}\n    ({})",
                c.name,
                c.input.as_str(),
                expectation,
                c.notes
            );
        }
        println!("  total {:.1?}", t.elapsed());
        return Ok(());
    }

    if options.interpret {
        println!("\ninterpretation");
        assistant.interpret(&docs[&id], &mut ports);
        drive(&mut assistant, &mut docs, &mut ports, &rx, options.verbose);
        let d = &docs[&id];
        if let Some(i) = &d.interpretation {
            println!("  restatement: {}", i.restatement);
            for a in &i.assumptions {
                println!("  assumption: {a}");
            }
            for issue in &i.issues {
                println!("  {:?}: {} {:?}", issue.kind, issue.detail, issue.refers_to);
            }
        }
        for c in &d.clarifications {
            println!(
                "  question: {} [{}] options {:?}",
                c.question, c.context, c.options
            );
        }
        if !matches!(assistant.report(id), Some(Report::Interpreted { .. })) {
            print_report(assistant.report(id));
        }
    }
    if options.contract {
        println!("\ncontract proposal");
        assistant.propose_contract(&docs[&id], &mut ports);
        drive(&mut assistant, &mut docs, &mut ports, &rx, options.verbose);
        match &docs[&id].contract {
            Some(c) => println!(
                "  proposed ({:?}):\n{}",
                c.status,
                clojurecraft_workflow::spec::contract_text(c)
                    .lines()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            None => print_report(assistant.report(id)),
        }
        // The proposal is not accepted, so it does not influence generation.
    }

    println!("\ngeneration");
    let has_docs = ports.docs.is_some();
    assistant.generate(&docs[&id], model_ctx, has_docs, &mut ports);
    drive(&mut assistant, &mut docs, &mut ports, &rx, options.verbose);
    let d = &docs[&id];
    match (assistant.report(id), &d.implementation) {
        (Some(Report::Generated { notes, attempts }), Some(i)) => {
            for n in notes {
                println!("  note: {n}");
            }
            println!("  attempts:");
            print_attempts(attempts);
            println!("\n{}", i.source);
            if let Some(g) = &i.generation {
                println!(
                    "  {} {} · requires {:?} · {} documentation refs · seed {:?}",
                    g.namespace,
                    g.function_name,
                    g.requires,
                    g.documentation.len(),
                    g.settings.seed
                );
                for a in &g.assumptions {
                    println!("  assumption: {a}");
                }
                if let Some(e) = &g.explanation {
                    println!("  explanation: {}", e.summary);
                    for m in &e.steps {
                        println!(
                            "    step {:?}: {:?} {}",
                            m.step_text,
                            m.lines,
                            m.excerpt.replace('\n', " ")
                        );
                    }
                    for s in &e.symbols {
                        println!("    {}: {}", s.symbol, s.purpose);
                    }
                }
            }
        }
        (report, _) => print_report(report),
    }
    service.shutdown();
    Ok(())
}
