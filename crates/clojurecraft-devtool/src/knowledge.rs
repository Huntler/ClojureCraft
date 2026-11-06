//! Documentation collection commands: install the embedding model, build the collection, search
//! it, and measure retrieval quality.

use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use clojurecraft_knowledge::build::{Sources, build_collection, extract_api, fetch_pages};
use clojurecraft_knowledge::embed::{Embedder, LlamaEmbedder, LlamaEmbedderConfig};
use clojurecraft_knowledge::{KnowledgeDirs, KnowledgeManifest, Query, Retriever, Retrievers, Semantic};
use clojurecraft_runtime::hub::Hub;
use clojurecraft_runtime::{Cancel, Manifest, components};
use serde::Deserialize;

use crate::{clojure_runtime, dirs, progress_printer};

/// `cpu`, `gpu`, or `text` (no embeddings).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EmbedMode {
    Text,
    Cpu,
    Gpu,
}

impl EmbedMode {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "text" => Self::Text,
            "cpu" => Self::Cpu,
            "gpu" => Self::Gpu,
            _ => bail!("embedding mode must be text, cpu, or gpu"),
        })
    }
}

pub fn fetch_embedding_model() -> Result<()> {
    let manifest = KnowledgeManifest::bundled()?;
    let kdirs = KnowledgeDirs::new(&dirs()?);
    let record = manifest.embedding.record();
    println!("installing {} ({})", record.display_name, record.quant);
    let mut progress = progress_printer();
    let installed =
        kdirs
            .model_store()
            .install(&record, &Hub::new(None), &Cancel::default(), &mut progress)?;
    println!(
        "  installed in {}",
        kdirs.model_store().dir(&installed.key).display()
    );
    Ok(())
}

fn embedder(mode: EmbedMode) -> Result<Option<LlamaEmbedder>> {
    if mode == EmbedMode::Text {
        return Ok(None);
    }
    let manifest = KnowledgeManifest::bundled()?;
    let runtime = Manifest::bundled()?;
    let dirs = dirs()?;
    let kdirs = KnowledgeDirs::new(&dirs);
    let (_, server) = components::llama_server(&runtime, &dirs)
        .ok_or_else(|| anyhow!("no llama.cpp runtime is installed; run install-runtime"))?;
    let store = kdirs.model_store();
    let record = store
        .get(&manifest.embedding.record().key)
        .ok_or_else(|| anyhow!("the embedding model is not installed; run fetch-embedding-model"))?;
    let model_file = store.primary_file(&record).context("model file")?;
    let t = Instant::now();
    let e = LlamaEmbedder::start(
        &LlamaEmbedderConfig {
            server,
            model_file,
            model: manifest.embedding.clone(),
            cpu_only: mode == EmbedMode::Cpu,
            threads: None,
            pid_file: Some(kdirs.embed_pid_file()),
            max_input_tokens: (mode == EmbedMode::Cpu).then_some(clojurecraft_knowledge::embed::QUERY_TOKENS),
        },
        &|| false,
    )?;
    eprintln!("embedding model loaded in {:.1?}", t.elapsed());
    Ok(Some(e))
}

pub fn build(mode: EmbedMode) -> Result<()> {
    let manifest = KnowledgeManifest::bundled()?;
    let runtime = Manifest::bundled()?;
    let dirs = dirs()?;
    let kdirs = KnowledgeDirs::new(&dirs);
    let cancel = Cancel::default();
    let t = Instant::now();
    let mut last_phase = None;
    let mut progress = |p: clojurecraft_knowledge::BuildProgress| {
        if last_phase != Some(p.phase) {
            if last_phase.is_some() {
                eprintln!();
            }
            last_phase = Some(p.phase);
        }
        eprint!(
            "\r  {:<52} {:>5}/{:<5} {:>6.1?}",
            p.phase.label(),
            p.done,
            p.total,
            t.elapsed()
        );
    };
    let pages = fetch_pages(&manifest, &kdirs.downloads(), &cancel, &mut progress)?;
    let clojure = clojure_runtime()?;
    let ta = Instant::now();
    let api = extract_api(&clojure, &runtime.clojure.version)?;
    eprintln!(
        "\n  API documentation: {} entries in {:.1?}",
        api.entries.len(),
        ta.elapsed()
    );
    let mut embedder = embedder(mode)?;
    let sources = Sources { api, pages };
    let installed = build_collection(
        &manifest,
        &kdirs.collections(),
        &sources,
        embedder.as_mut().map(|e| e as &mut dyn Embedder),
        &|| cancel.is_cancelled(),
        &mut progress,
    )?;
    eprintln!();
    let i = &installed.info;
    println!(
        "active collection {}: {} chunks ({} API entries, {} page sections from {} pages), embeddings: {}, {:.1?} total",
        installed.version,
        i.chunks,
        i.api_entries,
        i.page_sections,
        i.pages,
        i.embedding.as_ref().map_or("none".to_owned(), |e| e.name.clone()),
        t.elapsed()
    );
    println!(
        "size on disk: {:.1} MiB",
        kdirs.collections().disk_usage() as f64 / 1048576.0
    );
    Ok(())
}

fn open() -> Result<(Retriever, Option<LlamaEmbedder>)> {
    let kdirs = KnowledgeDirs::new(&dirs()?);
    let active = kdirs
        .collections()
        .active()
        .ok_or_else(|| anyhow!("no documentation collection is installed; run knowledge-build"))??;
    let retriever = Retriever::open(&active.dir)?;
    let embedder = if retriever.info().embedding.is_some() {
        embedder(EmbedMode::Cpu)?
    } else {
        None
    };
    Ok((retriever, embedder))
}

pub fn search(text: &str, all: bool) -> Result<()> {
    let (mut retriever, mut embedder) = open()?;
    let mut q = Query::new(text);
    q.include_restricted = all;
    let t = Instant::now();
    let res = retriever.search(&q, embedder.as_mut().map(|e| e as &mut dyn Embedder))?;
    println!(
        "{} results in {:.1?} (semantic: {:?})",
        res.hits.len(),
        t.elapsed(),
        res.semantic
    );
    for (i, h) in res.hits.iter().enumerate() {
        let s = &h.signals;
        println!(
            "{:>2}. {:<60} score {:.4}  exact:{} text:{} semantic:{}",
            i + 1,
            h.chunk.title,
            h.score,
            if s.exact { "yes" } else { "-" },
            s.text_rank.map_or("-".into(), |r| r.to_string()),
            s.semantic
                .map_or("-".into(), |(r, sim)| format!("{r} ({sim:.3})")),
        );
        println!(
            "    {} · {} · {}",
            h.chunk.url, h.provenance.license, h.provenance.revision
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct EvalFile {
    query: Vec<EvalQuery>,
}

#[derive(Deserialize)]
struct EvalQuery {
    text: String,
    expect: Vec<String>,
}

fn accepts(expect: &[String], id: &str) -> bool {
    expect.iter().any(|e| {
        if e.starts_with("page:") {
            id.starts_with(e.as_str())
        } else if e.contains('/') && e.len() > 1 {
            id == format!("api:{e}")
        } else {
            id == format!("api:clojure.core/{e}")
        }
    })
}

/// Hit@1, hit@5, and mean reciprocal rank (over the top 10) for several retriever combinations.
pub fn eval() -> Result<()> {
    let file: EvalFile = toml::from_str(include_str!("../retrieval-eval.toml"))?;
    let (mut retriever, mut embedder) = open()?;
    if embedder.is_none() {
        println!("note: no embeddings available; only text retrieval is measured");
    }
    let configs = [
        (
            "exact + full text",
            Retrievers {
                exact: true,
                text: true,
                semantic: false,
            },
        ),
        (
            "semantic only",
            Retrievers {
                exact: false,
                text: false,
                semantic: true,
            },
        ),
        ("hybrid (all)", Retrievers::default()),
    ];
    for (name, retrievers) in configs {
        if retrievers.semantic && embedder.is_none() {
            continue;
        }
        let (mut at1, mut at5, mut rr) = (0usize, 0usize, 0.0f64);
        let mut misses = Vec::new();
        let t = Instant::now();
        for q in &file.query {
            let mut query = Query::new(q.text.clone());
            query.retrievers = retrievers;
            let res = retriever.search(&query, embedder.as_mut().map(|e| e as &mut dyn Embedder))?;
            if retrievers.semantic && res.semantic != Semantic::Used {
                bail!("semantic search unavailable: {:?}", res.semantic);
            }
            match res.hits.iter().position(|h| accepts(&q.expect, &h.chunk.id)) {
                Some(p) => {
                    at1 += usize::from(p == 0);
                    at5 += usize::from(p < 5);
                    rr += 1.0 / (p as f64 + 1.0);
                    if p >= 5 {
                        misses.push(format!("{} (rank {})", q.text, p + 1));
                    }
                }
                None => misses.push(format!(
                    "{} (not in top 10; got {})",
                    q.text,
                    res.hits.first().map_or("nothing", |h| h.chunk.id.as_str())
                )),
            }
        }
        let n = file.query.len() as f64;
        println!(
            "{name:<18} hit@1 {:>5.1}%  hit@5 {:>5.1}%  MRR {:.3}  ({} queries, {:.1?})",
            at1 as f64 * 100.0 / n,
            at5 as f64 * 100.0 / n,
            rr / n,
            file.query.len(),
            t.elapsed()
        );
        for m in misses {
            println!("    miss: {m}");
        }
    }
    Ok(())
}
