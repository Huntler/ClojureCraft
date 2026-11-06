//! Developer commands that exercise runtime installation, local inference, and sandboxed Clojure
//! execution on the current machine. Downloads go to the regular application data directory
//! (override with `CLOJURECRAFT_DATA_DIR`).
//!
//! ```text
//! clojurecraft-devtool hardware
//! clojurecraft-devtool install-runtime [cpu|cuda|vulkan|rocm|metal]
//! clojurecraft-devtool fetch-model <model-id> <quant>
//! clojurecraft-devtool smoke-model <model-id> <quant> <backend> [context]
//! clojurecraft-devtool sandbox-demo
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clojurecraft_clojure::{ClojureRuntime, EvalRequest, Limits};
use clojurecraft_hardware::{Backend, default_budget};
use clojurecraft_inference::engine::Engine;
use clojurecraft_inference::http::Control;
use clojurecraft_inference::protocol::ChatRequest;
use clojurecraft_inference::{
    Cancel as InfCancel, LlamaEngine, LoadSpec, Message, StreamEvent, list_devices,
};
use clojurecraft_runtime::catalog::{Catalog, format_gib};
use clojurecraft_runtime::{Cancel, DataDirs, Manifest, components, platform_key};

fn dirs() -> Result<DataDirs> {
    DataDirs::discover().ok_or_else(|| anyhow!("no data directory could be determined"))
}

fn backend_arg(s: &str) -> Result<Backend> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|_| anyhow!("unknown backend {s}"))
}

fn progress_printer() -> impl FnMut(&str, clojurecraft_runtime::Progress) {
    let mut last = Instant::now() - Duration::from_secs(10);
    move |label, p| {
        if last.elapsed() > Duration::from_millis(700) || p.received == p.total {
            last = Instant::now();
            #[allow(clippy::cast_precision_loss)]
            let pct = p.received as f64 * 100.0 / p.total.max(1) as f64;
            eprint!("\r  {label}: {pct:5.1}% of {}      ", format_gib(p.total));
            if p.received == p.total {
                eprintln!();
            }
        }
    }
}

fn hardware() -> Result<()> {
    let report = clojurecraft_hardware::detect();
    println!("{}", serde_json::to_string_pretty(&report)?);
    let backend = report.recommended_backend();
    println!(
        "\ncandidate backends: {:?} (recommended: {})",
        report.candidate_backends(),
        backend.label()
    );
    let budget = default_budget(&report, backend);
    println!(
        "estimated budget: device {} + host spill {} ({:?})",
        format_gib(budget.device_bytes),
        format_gib(budget.host_spill_bytes),
        budget.basis
    );
    let catalog = Catalog::bundled()?;
    let ctx = catalog.default_context;
    println!("\ncatalog at {ctx} tokens context (all figures are estimates):");
    for c in catalog.ranked(&budget, ctx) {
        println!(
            "  {:<32} {:<7} est. {:>9}  {:?}{}",
            c.model.name,
            c.quant.name,
            format_gib(c.estimate.total()),
            c.fit,
            if c.model.status == clojurecraft_runtime::catalog::Status::Unverified {
                "  [unverified]"
            } else {
                ""
            }
        );
    }
    match catalog.recommended(&budget, ctx) {
        Some(r) => println!("recommended: {} {}", r.model.name, r.quant.name),
        None => println!("recommended: none fits the budget"),
    }
    Ok(())
}

fn install_runtime(backend: Option<&str>) -> Result<()> {
    let manifest = Manifest::bundled()?;
    let dirs = dirs()?;
    let platform = platform_key();
    let report = clojurecraft_hardware::detect();
    let backend = match backend {
        Some(b) => backend_arg(b)?,
        None => report.recommended_backend(),
    };
    let cancel = Cancel::default();
    let mut progress = progress_printer();
    println!(
        "installing llama.cpp {} ({}) for {platform}",
        manifest.llama_cpp.release,
        backend.label()
    );
    let t = Instant::now();
    let llama = components::install_llama(&manifest, &dirs, &platform, backend, &cancel, &mut progress)?;
    println!("  active: {} in {:.1?}", llama.dir.display(), t.elapsed());
    println!("installing {} {}", manifest.jre.vendor, manifest.jre.version);
    let jre = components::install_jre(&manifest, &dirs, &platform, &cancel, &mut progress)?;
    println!("  active: {}", jre.dir.display());
    println!("installing Clojure {}", manifest.clojure.version);
    let clj = components::install_clojure(&manifest, &dirs, &cancel, &mut progress)?;
    println!("  active: {}", clj.dir.display());
    if let Some(server) = components::llama_server_for(&manifest, &dirs, backend) {
        println!(
            "\ndevices visible to this llama.cpp build:\n{}",
            list_devices(&server)?
        );
    }
    Ok(())
}

fn model_paths(
    id: &str,
    quant: &str,
) -> Result<(Vec<(PathBuf, clojurecraft_runtime::manifest::Artifact)>, String)> {
    let catalog = Catalog::bundled()?;
    let model = catalog
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| anyhow!("unknown model {id}"))?;
    let q = model
        .quants
        .iter()
        .find(|q| q.name == quant)
        .ok_or_else(|| anyhow!("unknown quantization {quant}"))?;
    let base = dirs()?.models().join(&model.id).join(&q.name);
    let files = model
        .artifacts(q)
        .into_iter()
        .map(|(rel, a)| (base.join(rel.rsplit('/').next().unwrap_or(&rel)), a))
        .collect();
    Ok((files, model.name.clone()))
}

/// Installs a catalog model exactly as the application does (verified, recorded, atomic).
fn fetch_model(id: &str, quant: &str) -> Result<()> {
    let catalog = Catalog::bundled()?;
    let model = catalog.model(id).ok_or_else(|| anyhow!("unknown model {id}"))?;
    let q = model
        .quants
        .iter()
        .find(|q| q.name == quant)
        .ok_or_else(|| anyhow!("unknown quantization {quant}"))?;
    let d = dirs()?;
    let store = clojurecraft_runtime::models::ModelStore::new(d.models(), d.downloads());
    println!("fetching {} {quant}", model.name);
    let mut progress = progress_printer();
    let record = store.install(
        &clojurecraft_runtime::models::ModelRecord::from_catalog(model, q),
        &clojurecraft_runtime::hub::Hub::new(None),
        &Cancel::default(),
        &mut |f, p| progress(f, p),
    )?;
    println!(
        "  installed {} in {}",
        record.key,
        store.dir(&record.key).display()
    );
    Ok(())
}

fn gpu_used_mib() -> Option<u64> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .reduce(|a, b| a + b)
}

fn proc_status_kib(pid: u32, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines().find_map(|l| {
        l.strip_prefix(field)?
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()
    })
}

const SYSTEM_PROMPT: &str = "You write small, pure Clojure functions for data transformation. Reply with one \
    Clojure code block containing an ns form and a single public defn. Use only clojure.core and clojure.string, required with :as aliases. No :use, no :refer :all, no Java interop.";

const USER_PROMPT: &str = "Write a function `summarize-orders` that takes a vector of maps like \
    {:customer \"ann\" :amount 12.5} and returns a map from customer to the total amount, \
    sorted by customer name.";

fn smoke_model(id: &str, quant: &str, backend: &str, context: u32) -> Result<()> {
    let (files, name) = model_paths(id, quant)?;
    let model = files
        .first()
        .map(|(p, _)| p.clone())
        .ok_or_else(|| anyhow!("no files"))?;
    if !model.is_file() {
        bail!("{} is missing; run fetch-model {id} {quant}", model.display());
    }
    smoke_file(&model, &format!("{name} {quant}"), backend, context, None)
}

/// Loads any GGUF file with the pinned llama.cpp build for `backend`, generates a Clojure function,
/// runs it in the sandbox, and checks authentication and cancellation. Used to validate models
/// and architectures before they are allowed.
fn smoke_file(
    model: &std::path::Path,
    label: &str,
    backend: &str,
    context: u32,
    layers: Option<u32>,
) -> Result<()> {
    let manifest = Manifest::bundled()?;
    let dirs = dirs()?;
    let backend = backend_arg(backend)?;
    let server = components::llama_server_for(&manifest, &dirs, backend).with_context(|| {
        format!(
            "llama.cpp for {} is not installed; run install-runtime",
            backend.label()
        )
    })?;
    let header = clojurecraft_runtime::gguf::read_header(model)?;
    let facts = header.facts();
    println!(
        "  GGUF: architecture {:?}, {:?} layers, context {:?}, KV estimate {:?} bytes/token",
        facts.architecture,
        facts.block_count,
        facts.context_length,
        facts.kv_bytes_per_token()
    );
    let gpu_layers = if backend == Backend::Cpu { Some(0) } else { layers };
    println!(
        "starting {} with {label}, context {context}, gpu layers {gpu_layers:?}",
        backend.label()
    );
    let gpu_base = gpu_used_mib();
    let spec = LoadSpec {
        server,
        model: model.to_owned(),
        label: label.to_owned(),
        backend: backend.label().to_owned(),
        runtime: components::llama_version_id(&manifest, backend),
        context,
        gpu_layers,
        total_layers: facts.block_count,
        model_max_context: facts.context_length,
        threads: None,
        load_timeout: Duration::from_secs(300),
        memory: None,
        skip_memory_check: true,
        pid_file: None,
    };
    let mut session = LlamaEngine::default()
        .load(&spec, &InfCancel::default(), &mut |p| println!("  {p:?}"))
        .map_err(|(e, log)| anyhow!("{e}\n{log}"))?;
    let info = session.info().clone();
    println!(
        "  ready after {:.1?} (pid {:?}, context {}, build {:?}, tool calls {})",
        info.load_time, info.pid, info.context, info.build, info.supports_tools
    );

    let gpu_peak = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (gpu_peak, done) = (gpu_peak.clone(), done.clone());
        thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                if let Some(v) = gpu_used_mib() {
                    gpu_peak.fetch_max(v, Ordering::SeqCst);
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let request = ChatRequest {
        messages: vec![Message::system(SYSTEM_PROMPT), Message::user(USER_PROMPT)],
        max_tokens: 600,
        temperature: 0.2,
        response_schema: None,
        tools: vec![],
        seed: None,
    };
    let control = || {
        Control::new(
            InfCancel::default(),
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(60)),
        )
    };
    println!(
        "  prompt tokens (model tokenizer): {}",
        session.count_prompt(&request, &control())?
    );
    println!("--- generation ---");
    let start = Instant::now();
    let turn = session.chat(&request, &control(), &mut |e| {
        if let StreamEvent::Content(t) = e {
            print!("{t}");
            let _ = std::io::stdout().flush();
        }
    })?;
    println!("\n--- end ---");
    done.store(true, Ordering::SeqCst);
    let _ = sampler.join();
    println!(
        "  tokens: prompt {:?}, completion {:?}; {:.1} tok/s (server-reported); wall {:.1?}; finish {:?}",
        turn.usage.prompt_tokens,
        turn.usage.completion_tokens,
        turn.usage.tokens_per_second.unwrap_or(f64::NAN),
        start.elapsed(),
        turn.finish
    );
    if let Some(pid) = info.pid
        && let (Some(rss), Some(hwm)) = (proc_status_kib(pid, "VmRSS:"), proc_status_kib(pid, "VmHWM:"))
    {
        println!("  server RSS {} MiB, peak RSS {} MiB", rss / 1024, hwm / 1024);
    }
    if let Some(base) = gpu_base {
        let peak = gpu_peak.load(Ordering::SeqCst);
        println!(
            "  GPU memory: baseline {base} MiB, peak {peak} MiB (delta {} MiB, whole device)",
            peak.saturating_sub(base)
        );
    }
    run_generated(&turn.content);

    let cancel = InfCancel::default();
    let c2 = cancel.clone();
    let t = Instant::now();
    let r = session.chat(&request, &Control::new(cancel, None, None), &mut move |_| {
        c2.cancel()
    });
    println!(
        "  cancellation: {:?} after {:.1?}",
        r.err().map(|e| e.to_string()),
        t.elapsed()
    );
    let follow = session.chat(
        &ChatRequest {
            messages: vec![Message::user("Reply with the word ok.")],
            max_tokens: 8,
            temperature: 0.0,
            ..request.clone()
        },
        &control(),
        &mut |_| {},
    )?;
    println!("  server responsive after cancel: {:?}", follow.content.trim());
    session.shutdown();
    Ok(())
}

/// Runs the generated function on a fixed example inside the sandbox, when it can be extracted.
fn run_generated(reply: &str) {
    let code = reply
        .split("```")
        .nth(1)
        .map(|block| block.trim_start_matches("clojure").trim().to_owned());
    let Some(code) = code else {
        println!("  generated code: no code block found");
        return;
    };
    let rt = match clojure_runtime() {
        Ok(rt) => rt,
        Err(e) => return println!("  generated code: not executed ({e})"),
    };
    let req = EvalRequest {
        code,
        function: "summarize-orders".into(),
        inputs: vec![
            r#"[{:customer "bob" :amount 3} {:customer "ann" :amount 12.5} {:customer "bob" :amount 1.5}]"#
                .into(),
        ],
        static_checks: true,
    };
    match rt.evaluate(&req, &Limits::default()) {
        Ok(out) => println!(
            "  sandboxed execution ({:?}, {:.1?}): {}",
            out.status,
            out.elapsed,
            out.response.trim()
        ),
        Err(e) => println!("  sandboxed execution failed to start: {e}"),
    }
}

fn clojure_runtime() -> Result<ClojureRuntime> {
    let manifest = Manifest::bundled()?;
    let dirs = dirs()?;
    let (java, java_home) = components::java(&manifest, &dirs, &platform_key())
        .ok_or_else(|| anyhow!("Java runtime not installed; run install-runtime"))?;
    let jars = components::clojure_jars(&manifest, &dirs).ok_or_else(|| anyhow!("Clojure not installed"))?;
    let helper = std::env::current_exe()?
        .parent()
        .map(|d| d.join(clojurecraft_sandbox::HELPER_NAME))
        .filter(|p| p.is_file())
        .ok_or_else(|| {
            anyhow!("sandbox helper not built next to this binary; run cargo build --workspace")
        })?;
    Ok(ClojureRuntime {
        java,
        java_home,
        jars,
        sandbox_helper: helper,
        work_dir: dirs.root().join("harness"),
    })
}

fn sandbox_demo() -> Result<()> {
    let rt = clojure_runtime()?;
    let limits = Limits::default();
    let cases: &[(&str, &str, bool)] = &[
        (
            "pure function",
            "(ns demo.sum (:require [clojure.string :as str]))\n(defn f [xs] (str/join \",\" (map inc xs)))",
            true,
        ),
        (
            "static check: eval",
            "(ns demo.e)\n(defn f [_] (eval '(+ 1 2)))",
            true,
        ),
        (
            "static check: interop",
            "(ns demo.i)\n(defn f [_] (System/getenv \"HOME\"))",
            true,
        ),
        (
            "OS: read /etc/hostname",
            "(ns demo.r)\n(defn f [_] (slurp \"/etc/hostname\"))",
            false,
        ),
        (
            "OS: write /tmp",
            "(ns demo.w)\n(defn f [_] (spit \"/tmp/clojurecraft-escape\" \"x\"))",
            false,
        ),
        (
            "OS: network",
            "(ns demo.n)\n(defn f [_] (.connect (java.net.Socket.) (java.net.InetSocketAddress. \"1.1.1.1\" 80) 2000))",
            false,
        ),
        (
            "OS: process",
            "(ns demo.p)\n(defn f [_] (.waitFor (.exec (Runtime/getRuntime) (into-array String [\"/bin/true\"]))))",
            false,
        ),
        (
            "runaway loop",
            "(ns demo.l)\n(defn f [_] (loop [] (recur)))",
            true,
        ),
        ("huge output", "(ns demo.o)\n(defn f [_] (range 1e9))", true),
        (
            "memory bomb",
            "(ns demo.m)\n(defn f [_] (vec (repeatedly 1e9 #(long-array 1024))))",
            true,
        ),
    ];
    for (label, code, checks) in cases {
        let req = EvalRequest {
            code: (*code).to_owned(),
            function: "f".into(),
            inputs: vec!["[1 2 3]".into()],
            static_checks: *checks,
        };
        let limits = if label.starts_with("runaway") {
            Limits {
                wall: Duration::from_secs(8),
                ..limits
            }
        } else {
            limits
        };
        let out = rt.evaluate(&req, &limits)?;
        let detail = if out.response.is_empty() {
            out.stderr.lines().last().unwrap_or("").to_owned()
        } else {
            out.response.trim().chars().take(260).collect()
        };
        println!("[{label}] {:?} in {:.1?}\n    {detail}", out.status, out.elapsed);
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["hardware"] => hardware(),
        ["install-runtime"] => install_runtime(None),
        ["install-runtime", b] => install_runtime(Some(b)),
        ["fetch-model", id, q] => fetch_model(id, q),
        ["smoke-model", id, q, b] => smoke_model(id, q, b, 8192),
        ["smoke-model", id, q, b, ctx] => smoke_model(id, q, b, ctx.parse()?),
        ["smoke-file", path, b] => smoke_file(std::path::Path::new(path), path, b, 8192, None),
        ["smoke-file", path, b, ctx] => smoke_file(std::path::Path::new(path), path, b, ctx.parse()?, None),
        ["smoke-file", path, b, ctx, layers] => smoke_file(
            std::path::Path::new(path),
            path,
            b,
            ctx.parse()?,
            Some(layers.parse()?),
        ),
        ["sandbox-demo"] => sandbox_demo(),
        ["run-reply", path] => {
            run_generated(&std::fs::read_to_string(path)?);
            Ok(())
        }
        _ => bail!(
            "usage: hardware | install-runtime [backend] | fetch-model <id> <quant> | smoke-model <id> <quant> <backend> [ctx] | smoke-file <path.gguf> <backend> [ctx] [gpu-layers] | run-reply <reply.txt> | sandbox-demo"
        ),
    }
}
