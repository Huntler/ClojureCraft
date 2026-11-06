# ClojureCraft architecture decisions

Status: foundation. Each decision lists what was chosen, why, and how it was verified on real
hardware. "Verified" means exercised on a machine; "unverified" means designed and compiled only.

## Workspace layout

| Crate | Responsibility |
| --- | --- |
| `clojurecraft-app` | egui/eframe UI, application state, background job dispatch (binary `clojurecraft`) |
| `clojurecraft-domain` | Versioned function records, results, content fingerprints, staleness rules, credential guard (no I/O) |
| `clojurecraft-edn` | Tolerant EDN/Clojure lexing for highlighting; recovering EDN parser with positioned diagnostics and a value tree (no I/O) |
| `clojurecraft-credentials` | Access tokens in the OS credential store (Secret Service on Linux, Keychain on macOS) behind one trait |
| `clojurecraft-store` | SQLite library: drafts, immutable revisions, results, migrations, autosave |
| `clojurecraft-hardware` | RAM, CPU feature, and GPU/VRAM detection; backend recommendation; default memory budget |
| `clojurecraft-runtime` | Runtime manifest, resumable SHA-256-verified downloads, atomic install/rollback, model catalog and fit estimates, GGUF header reading, Hugging Face resolution, model store, inference settings, disk usage |
| `clojurecraft-inference` | Replaceable inference interface; `llama-server` process on a private Unix socket; inference service (lifecycle, crash recovery, streaming, cancellation, timeouts); context policy; roles; structured output and tool validation |
| `clojurecraft-sandbox` | OS-level restriction of untrusted processes (library + `clojurecraft-sandbox` helper binary) |
| `clojurecraft-clojure` | Sandboxed JVM evaluation of candidate functions: static checks, lint, loading, calls with per-call limits, structural comparison with expected EDN, output-contract checks (harness); trusted scripts in the same sandbox |
| `clojurecraft-knowledge` | Versioned Clojure documentation: pinned sources, API extraction, chunking, SQLite/FTS5 index, embeddings, merged retrieval, atomic collection updates, prompt formatting |
| `clojurecraft-workflow` | Specification readiness and prompt sections, model requests and answer schemas (interpretation, questions, retrieval planning, contract proposals, generation, explanation), structural checks of generated code, allowed namespaces, validation plans and reports, and the assistant that sequences these requests with validation and repair (no I/O; inference, retrieval, and execution are reached through a trait) |
| `clojurecraft-devtool` | Developer commands that exercise installation, inference, and sandboxing on a real machine |

Later boundaries (export, benchmarking, updates)
get their own crates when they are implemented. Long-running work runs on background threads that
report typed events over a channel; the UI drains the channel each frame and never blocks
(`clojurecraft-app/src/jobs.rs`).

Data lives outside the repository in the platform data directory, overridable with
`CLOJURECRAFT_DATA_DIR`:

- Linux: `~/.local/share/clojurecraft/`
- macOS: `~/Library/Application Support/ClojureCraft/`

with `downloads/` (archives and resumable `.part` files), `runtimes/<component>/<version>/`,
`models/<catalog-id>/<quant>/` and `models/hf/<owner>/<name>/<commit12>/<quant>/` (each with a
`model.json` record), `harness/`, `knowledge/` (`collections/<version>/` with `index.sqlite3` and `collection.json`,
`downloads/` for verified source pages, `models/` for the embedding model), `library/library.sqlite3`
(saved functions, history, results),
`settings.json` (inference settings), and `ui-state.ron` (window geometry, column widths, text
size, last open function). Access tokens are never stored here.

## Decisions

### UI framework: egui/eframe 0.37 (glow renderer)

Immediate-mode, pure Rust, a single dependency tree for macOS and Linux, and the toolkit used by
the ArtCraft native client whose visual language we follow. Features enabled: `glow`, `wayland`,
`x11`, `accesskit` (screen-reader support), `persistence` (window geometry).

- Verified: window opens on Ubuntu 24.04 / X11 with live hardware and runtime status.
- Unverified: Wayland session (the backend is compiled in), macOS.

### Inference runtime: llama.cpp `llama-server` as a child process

Chosen over in-process bindings (`llama-cpp-2` and similar) because:

- a crash, GPU driver fault, or out-of-memory condition in inference cannot take down the UI;
- the backend (Metal/CUDA/Vulkan/ROCm/CPU) is a runtime choice: we download the matching upstream
  build instead of compiling every backend into our binary;
- upstream publishes prebuilt, per-backend release archives with SHA-256 digests;
- the OpenAI-compatible API gives streaming, chat templates (Jinja from GGUF metadata),
  grammar/JSON-schema constrained output, and tool-call parsing without reimplementation;
- unloading a model to free memory is just stopping the process.

Isolation of the server: it listens on a Unix-domain socket (`llama.sock`) inside a fresh
directory with mode `0700` under `$XDG_RUNTIME_DIR` (or the temporary directory), so no TCP port is
opened and other local users cannot connect; loopback TCP on an ephemeral port is used only if the
socket path would exceed 100 bytes. Requests also need a random 256-bit per-session key passed
through the environment (`LLAMA_API_KEY`, never on the command line). Environment variables
llama.cpp reads as configuration (`LLAMA_*`, `HF_*`) are removed, so they cannot enable its
built-in file tools or other features. Web UI and slot endpoints are disabled. The server's PID is
recorded in `llama-server.pid` in the data directory; a server left behind by a crash of the
application is stopped on the next load, but only if the recorded process is still that program.

- Verified on Linux: CPU, Vulkan and CUDA builds with two models; see `docs/platform-validation.md`.
- Unverified: Metal on macOS.

### Inference service

`clojurecraft-inference` separates the replaceable interface (`Engine` loads a model and returns a
`Session`, which counts tokens and streams chat turns) from the production `LlamaEngine`. A
scripted `MockEngine` (feature `mock`) implements the same interface for tests.

**Server options.** One slot (`--parallel 1`), the configured context and GPU layers exactly
(`--fit off`, so memory failures are reported rather than silently adjusted), no host prompt cache
outside the budget (`--cache-ram 0`), no context shifting (earlier context is never discarded
silently), the model's own Jinja chat template (`--jinja`), and reasoning text separated from the
answer (`--reasoning-format deepseek`).

**Transport.** A small HTTP/1.1 client (chunked and length-delimited bodies) polls the socket
every 100 ms, so cancellation, the request deadline, and a 60 s idle limit take effect even while
the server is silent during prompt processing; the server sends keep-alive comments every 10 s.
Closing the connection makes llama.cpp abort the generation.

**Service and lifecycle.** `InferenceService` runs one worker thread that owns the loaded model
and handles load, unload, and generation commands in order; callers never block and receive typed
`ServiceEvent`s. Model status is `Unloaded`, `Loading` (starting, then reading the model),
`Ready` (context, build, tool support, load time), `Failed` (typed `LoadError`, remedies, log
tail), or `Crashed`. The app adds "runtime missing" and "no model" from its install state. The
worker checks the process every second while idle. After a crash the model is loaded again
automatically, at most twice within ten minutes; after that the user decides. A crash during a
request fails that request with `ServerCrashed` before recovery starts. Unload, a new load, and
application exit cancel the work in progress. The server is stopped with SIGTERM, then SIGKILL
after 3 s, and its socket directory and PID file are removed.

**Request identity.** Every request has an identifier carried by all of its events. Cancelling
takes effect immediately through a shared flag. A cancelled request emits no further output and
always ends with `Err(Cancelled)`, even if the server finished at the same moment. The interface
applies only events of the request it currently shows. Together, a cancelled or superseded
request can never overwrite newer results.

**Memory.** Before starting the server, the estimated need (GPU part and system-memory part of
the offload split; everything in system memory for CPU and Apple unified memory) is compared with
memory free right now (`nvidia-smi`/sysfs VRAM in use, `MemAvailable`). A shortfall stops the load
with `InsufficientMemory` and the remedies below, including "Load anyway". The server log is
classified after a failure: out of GPU or system memory (CUDA, Vulkan, ROCm, Metal messages), a
backend that cannot initialise, an unsupported architecture, an unparsable chat template, or an
unreadable model file. Out-of-memory remedies are concrete: three quarters of the current GPU
layers, half the context (not below 4096), or a smaller quantization or model. Exit by signal 9 is
reported as possibly stopped by the system for lack of memory.

**Context policy.** Prompts are built from labelled sections, each required or optional with a
drop order. Tokens are counted with the loaded model's tokenizer: the chat template is rendered by
`/apply-template` and the result tokenized, tools included. The space reserved for the answer is
the role's output limit, at most half the context. If the request does not fit, optional sections
(retrieved documentation, earlier attempts) are left out whole, highest drop order first, and the
result lists what was left out. Required sections (the user's specification, examples, contract,
instructions) are never shortened or dropped. If they alone do not fit, the request is refused
before anything is sent, with the token counts, the three largest parts, and the remedies "Increase
the context to N" (when the model supports N) and "Shorten <parts>". A server-side context
overflow is mapped to the same error.

**Roles.** Interpretation, clarification, retrieval planning, generation, and explanation each
have their own instructions, temperature, output limit, and time limit, and share one loaded
model. Every role's instructions state that reference material, examples, and tool results are
data, not instructions.

**Structured output.** A `Schema` (a closed subset of JSON Schema: strings with length and
enumerations, bounded integers, numbers, booleans, bounded arrays, closed objects) is sent as
`response_format` for llama.cpp's grammar sampling. The answer is still parsed and validated
(`Malformed` with the path of the first violation and the raw text). An answer cut off at the
token limit is `CutOff`, which records how much the model reasoned before answering. Every
request sends its role's temperature and top-p (0.9) explicitly, so the settings recorded with
generated code are the ones that ran.

**Tools.** The model may call only the application-defined tools passed with a request. Arguments
must be valid JSON matching the tool's schema before the tool runs. Each call has a 5 s limit and
results are truncated to 8,000 characters. At most 4 calls per turn and 4 tool rounds per request
are allowed. Refused and failed calls go back to the model as errors. llama.cpp's own built-in
tools are never enabled. Tools are offered only when the model's chat template supports them.

### llama.cpp binaries: pinned upstream release assets, downloaded on first start

`resources/runtime-manifest.toml` pins release `b12138` (commit `f7b41cc`) per platform/backend
with URL, size, and SHA-256. Upstream assets exist for every supported target:

| Platform | Backends | Asset |
| --- | --- | --- |
| macOS arm64 | Metal (+CPU) | `llama-b12138-bin-macos-arm64.tar.gz` |
| Linux x86_64 | CPU | `llama-b12138-bin-ubuntu-x64.tar.gz` |
| Linux x86_64 | Vulkan | `llama-b12138-bin-ubuntu-vulkan-x64.tar.gz` |
| Linux x86_64 | CUDA 13.4 | `llama-b12138-bin-ubuntu-cuda-13.4-x64.tar.gz` + `cudart-…` runtime archive |
| Linux x86_64 | ROCm 10.0 | `llama-b12138-bin-ubuntu-rocm-10.0-x64.tar.gz` |

Upstream marks per-build releases as GitHub "prereleases"; trust comes from our pinned hashes,
not from upstream's release channel. A CUDA 12.8 build also exists for older drivers; whether to
offer it as a fallback is open.

Install flow: download to `.part` (HTTP range resume) → verify size + SHA-256 → extract into
`.staging-<version>` (rejecting absolute paths, `..`, and escaping links) → smoke test
(`llama-server --version`) → rename into place → atomically swap `active.json`. The previous
version stays for rollback. Linux builds need glibc ≥ 2.34 and use `$ORIGIN` RUNPATH, so no
environment changes are needed.

### Model format: GGUF only

Initial architectures (all supported by the pinned build): `qwen2` (Qwen2.5-Coder), `qwen3moe`
(Qwen3-Coder-30B-A3B), `mistral3` (Devstral Small 2), `qwen3next` (Qwen3-Coder-Next). See
`docs/model-compatibility.md`.

### Clojure runtime: JVM Clojure 1.12.6 on Eclipse Temurin 25 JRE, both downloaded and pinned

The exported code must run on standard Clojure, and benchmarks must measure the real JVM, so we
use JVM Clojure rather than an interpreter such as SCI/babashka. Both are downloaded on first
start through the same verified installer as llama.cpp (pinned in the runtime manifest):

- JRE: Temurin 25.0.5+8 (LTS), `linux-x86_64` and `macos-aarch64` archives with Adoptium SHA-256.
- Clojure 1.12.6 + `spec.alpha` 0.5.238 + `core.specs.alpha` 0.4.74 from Maven Central.

No system Java is used, so results do not depend on what the user has installed. Startup of a
sandboxed evaluation is about 0.5 s on the reference Linux machine.

### Execution isolation

Every evaluation starts a fresh JVM under the `clojurecraft-sandbox` helper. The helper restricts
itself, then `exec`s the JVM, so restrictions are inherited and irreversible. The parent clears
the environment, starts the helper in its own process group, enforces a wall-clock timeout, caps
captured stdout/stderr, and kills the process on any violation. If any restriction cannot be
established the helper exits with status 97 and the program never runs (fail closed).

**Linux** (no root, no user namespaces, works on Ubuntu's AppArmor userns restriction):

| Threat | Mechanism |
| --- | --- |
| File read | Landlock: read access only to the JRE, Clojure jars, harness script, system libraries, `/etc/ld.so.cache`, `/proc/self`, `/dev/urandom` |
| File write | Landlock grants no write rights except `/dev/null`; `RLIMIT_FSIZE=0` as backstop |
| Network | seccomp refuses `socket`/`socketpair`; Landlock TCP bind/connect rules and abstract-socket scoping when the kernel supports them (ABI ≥ 4/6) |
| Process spawning | seccomp refuses `fork`, `vfork`, `clone3`, and `clone` without `CLONE_THREAD` (threads still work); `execve` is allowed but only replaces the already-sandboxed process |
| Kernel attack surface | seccomp refuses `ptrace`, `process_vm_*`, `mount`, `unshare`, `setns`, `bpf`, `perf_event_open`, `userfaultfd`, `io_uring_*`, `keyctl`, module loading, and similar |
| Runaway computation | `RLIMIT_CPU` (SIGXCPU then SIGKILL) and the parent's wall-clock timeout |
| Memory | JVM `-Xmx`, metaspace and code-cache caps, `-XX:+ExitOnOutOfMemoryError` (reported as a memory-limit failure) |
| Excessive output | parent caps captured bytes and kills on overflow; harness caps `*out*` captured inside a call; `*print-length*`/`*print-level*` bound printed results |
| Secrets in environment | parent starts the child with an empty environment |

Requirements: Landlock ABI ≥ 1 (Linux 5.13) is mandatory; seccomp filtering via `seccompiler`
(x86_64 and aarch64). Matching syscalls return `ENOSYS`, which makes glibc fall back from `clone3`
to `clone` for thread creation.

Known gaps: thread count is bounded only indirectly (CPU/wall limits, JVM memory), because
`RLIMIT_NPROC` is per user; per-process memory is bounded by JVM flags, not `RLIMIT_AS` (the JVM
reserves large virtual ranges). A cgroup v2 child group would bound both when available; this is
a candidate improvement.

**macOS** (designed, compiled for the host only, not yet validated): the helper applies the same
rlimits and execs `/usr/bin/sandbox-exec` with a deny-by-default Seatbelt profile generated from
the same policy (read-only subpaths, `deny network*`, `deny process-fork`). `sandbox-exec` is
deprecated as a public interface but still shipped and used by Apple tools and browsers; if it
proves unworkable, the fallback is the `sandbox_init` C API from the helper.

### Java interop and dynamic evaluation

The OS sandbox is the security boundary. The harness (`resources/clojure/harness.clj`) adds
checks that give users and the repair loop clear diagnostics:

- source is read with `*read-eval*` false (no `#=`), and unknown tagged literals are rejected;
- the first form must be `ns` with only `:require` of an allowlist (`clojure.core`, `clojure.string`,
  `clojure.set`, `clojure.walk`, `clojure.edn`, `clojure.zip`, `clojure.data`), aliases only;
- top-level forms are limited to `defn`, `defn-`, `def`, `declare`;
- every symbol outside quoted data is checked: dynamic evaluation (`eval`, `load-*`, `read-string`,
  `resolve`, `var`, `intern`, `ns-publics`, …), state and concurrency (`binding`, `set!`, `future`,
  `agent`, …), class generation (`proxy`, `reify`, `deftype`, …), I/O (`slurp`, `spit`, …),
  `.method`/`Class.` interop, fully qualified class names, and static members of classes other
  than `Math`, `StrictMath`, `Long`, `Integer`, `Double`, `Character`, `Boolean` are rejected.

Allowing more interop (for example selected `String` methods) is a deliberate future change made
by extending the allowlist, with tests.

**Lint.** Before the code is evaluated, the harness interns the names the code defines (with the
parameter lists of `defn` forms) and analyses every top-level form after macroexpansion. Only
`clojure.core` macros can be expanded, because candidate code cannot define macros, so no
candidate code runs during linting. Errors (which fail validation): unresolved symbols, calls of
`clojure.*` functions or the code's own functions with a number of arguments no parameter list
accepts, macro calls rejected by `clojure.core.specs` (for example malformed `let` bindings), and
`def` inside a function. Warnings (reported, not failures): unused local bindings and parameters
(names starting with `_` and generated names are ignored), unused private functions, and
top-level names that shadow `clojure.core`. Findings carry line numbers; at most 40 are reported.
An external linter (clj-kondo) was not adopted: it would be another downloaded native binary per
platform to pin, verify, and sandbox, and these rules cover what generated code most often gets
wrong.

### Storage and retrieval index: SQLite (bundled via `rusqlite`)

Chosen for transactional writes with WAL journaling (crash safety for drafts and revisions),
schema migrations, and the built-in FTS5 full-text index for documentation. The documentation
corpus is small (thousands of chunks), so embedding vectors can be stored in SQLite and searched
by exact cosine similarity in memory, avoiding a separate vector-index dependency. Embeddings are
produced by Qwen3-Embedding-0.6B through `llama-server --embedding`, reusing the inference runtime
(see "Documentation retrieval").

### Domain model and persistence

**Records** (`clojurecraft-domain`). A function is a `FunctionDocument`: name, description,
optional contract (proposed or accepted), ordered steps with stable `StepId`s, examples (direct
EDN input, or a reference to another function's example output with a recorded snapshot of the
value), the implementation (current source plus, when generated, the generated source and its
provenance: model repository/revision/file hash/quantization, llama.cpp release and backend,
generation settings, documentation excerpts used), edge-case tests (proposed/accepted/rejected,
user- or model-originated), and benchmark configurations. EDN is stored as the exact text typed,
never re-serialised, so invalid EDN is never lost. Validation reports and benchmark results are
separate append-only records.

**Draft + immutable revisions.** Each function has one draft, written as a single JSON document
per save, and a linear history of revisions that freeze a document. Restoring an old revision
never deletes history: the current draft is first recorded (`BeforeRestore`) if it has
unrevisioned changes, then the restored content is appended as the newest revision. Revisions and
results are protected by SQLite triggers against `UPDATE`.

**Fingerprints and staleness.** Specification, code, and accepted tests are each hashed
(SHA-256 over a versioned canonical projection). The specification fingerprint covers the
description, the *accepted* contract, step texts in order, and examples (validated input value and
expected output); names, step identifiers, proposals, and rejected or ambiguous tests are
excluded. Every result stores the fingerprints it was computed from, so freshness is decided by
comparison: results are never deleted when content changes, they become `Stale([components])` and
stay inspectable. Benchmarks additionally depend on their configuration fingerprint.

**Durability.** SQLite (bundled 3.54) in WAL mode with `synchronous = FULL` and foreign keys on;
`quick_check` on open. Multi-row operations (commit, restore, purge) are single transactions. A
crash test kills a writer process at random points and verifies database integrity and that the
draft and every revision are internally consistent.

**Ordering.** Draft writes carry the editor's monotonic edit sequence; a write older than the
stored sequence is ignored (`Superseded`), so a delayed autosave cannot overwrite a restored
draft. The autosaver runs on its own thread and connection, coalesces edits (quiet period, with a
maximum wait while typing continues), retries transient failures with back-off, reports every
outcome as an event, and flushes on shutdown.

**Versioning.** Schema migrations are tracked in `PRAGMA user_version`; each runs in its own
transaction with its version bump, after a `VACUUM INTO` backup
(`library.sqlite3.v<N>.bak`). A library from a newer application is refused untouched. Each
stored JSON record carries a format number; older formats are upgraded in memory on read
(immutable records are never rewritten), newer formats are refused. An unreadable draft is
reported and can be replaced by its latest revision; a draft from a newer format cannot.

**References.** Example inputs referencing another function are indexed for the draft and every
revision. Functions are deleted to a trash and can be restored; permanent deletion is refused
while any other function's draft or history references them. `check_references` reports missing
or trashed functions, missing or mismatched revisions, and missing upstream examples. Cycle
detection is part of the referenced-input workflow, not yet implemented.

**Validation report format 2.** Reports gained the generation run they belong to, the checked
source (so candidates that were never applied stay inspectable), and the comparison settings
(float tolerance, unordered output, per-call time limit, element limit). The fields default to
empty, so format 1 reports upgrade unchanged.

**Format 2.** Documents gained clarifications, the model's interpretation, a whole-value type in
contract shapes, and the generated code's namespace, requirements, assumptions, left-out context,
and explanation. All new fields default to empty, so format 1 records upgrade unchanged; an
application that only knows format 1 refuses format 2 documents instead of dropping the new
fields on save. The specification fingerprint includes answered clarifications but omits them
(and a shape type of "any") when absent, so fingerprints of existing specifications did not change.

**Revisions from background work.** The autosaver also records revisions: a commit is queued on
the same worker as draft writes, replaces an older queued edit of that function, and is reported
as `Committed`. If recording fails, the content is still saved as an ordinary draft write.

**Credentials.** No record has a credential field; provenance uses repository coordinates, never
download URLs. A process-wide `SecretRegistry` holds every secret the application knows (inference
API keys, access tokens). Drafts and revisions containing a registered secret are refused;
machine-produced results are redacted (registered secrets and `hf_…` token shapes) before
storage. The same guard is intended for exports.

### Documentation retrieval

**Sources** (pinned in `resources/knowledge-manifest.toml`, revision `2026.11.1`):

- API documentation of `clojure.core`, `clojure.string`, `clojure.set`, `clojure.walk`,
  `clojure.edn`, `clojure.zip`, and `clojure.data` (the namespaces the harness allows), read from
  var metadata of the installed, pinned Clojure runtime by `resources/clojure/api-docs.clj`, run in
  the same sandbox as candidate code. It therefore always describes exactly the configured Clojure
  version (1.12.6: 732 documented vars and special forms). Each entry keeps its arglists, kind,
  `:added` version, and deprecation.
- 19 clojure.org reference and guide pages relevant to data transformation (data structures,
  sequences, transducers, special forms, laziness, destructuring, equality, comparators, threading
  macros, …) from `clojure/clojure-site` at commit `d664fff7`, each pinned by size and SHA-256 and
  downloaded from `raw.githubusercontent.com` with the verified, resumable downloader.

Both are EPL-1.0. Every chunk keeps its source URL (API page anchor or page section anchor),
title, symbol, namespace, `since` version, and its source's license, attribution, and revision.

**Chunking.** API documentation: one chunk per symbol (usage forms, kind, version, docstring).
Pages: an AsciiDoc subset is converted to text (links, cross references, tables, admonitions,
listing blocks; comments and pass-through HTML dropped) and split by section. Sections longer than
2,400 characters are split between paragraphs; a code block always stays with the paragraph that
introduces it and with a short explanation after it, so a single example may exceed the limit.
Links to API pages are kept as `mentions` and indexed. The result is 1,032 chunks.

**Index.** One immutable SQLite database per collection: chunks, sources, an FTS5 table (Porter
stemming; columns symbol, title, mentions, text with BM25 weights 8/4/2/1), and f32 vectors.
Readers open it with `immutable=1`.

**Retrieval.** Every query runs three retrievers. *Exact*: a query that is one symbol (`group-by`,
`str/join` with common aliases resolved, `clojure.set/union`, `` `keep` ``, `(if …)`) puts that
symbol first; code-like symbols inside a longer query are boosted. *Full text*: the query's words
(stopwords removed, hyphenated symbols also as phrases) OR-ed in FTS5. *Semantic*: cosine
similarity against all vectors, with Qwen3-Embedding's query instruction. With vectors, candidates
are ranked by similarity + 1.0 for a named symbol, + 0.15 for a mentioned one, and up to + 0.08
for matching words; without vectors, exact and full-text ranks are merged by reciprocal rank
fusion. Filters: symbols added after the configured Clojure version are left out, and symbols the
harness refuses (`slurp`, `eval`, …, read from the harness source so the lists cannot diverge)
are excluded unless explicitly requested; their text says they are not available.

Measured with `clojurecraft-devtool knowledge-eval` (36 paraphrased transformation descriptions,
`crates/clojurecraft-devtool/retrieval-eval.toml`, top 10):

| Ranking | hit@1 | hit@5 | MRR |
| --- | --- | --- | --- |
| exact + full text | 30.6 % | 50.0 % | 0.389 |
| semantic only | 50.0 % | 91.7 % | 0.700 |
| reciprocal rank fusion, equal weights | 44.4 % | 66.7 % | 0.552 |
| similarity + bonuses (chosen) | 61.1 % | 91.7 % | 0.747 |

Full text alone fails on paraphrases ("count how often each value occurs" does not contain
"frequencies"); rank fusion lets its noise displace good semantic matches, so similarity scores are
kept. **Reranking** (a cross-encoder) is not implemented: with hit@5 at 92 % and the generator
receiving several excerpts, the remaining gain does not justify another model in memory; to be
revisited if generation quality shows retrieval misses.

**Embedding model.** Qwen3-Embedding-0.6B Q8_0 (Apache-2.0, 610 MiB, pinned commit and SHA-256,
installed in its own model store so it never appears as a generator). It runs in a separate
`llama-server` in embedding mode (`--embedding --pooling last`, same private socket, key, and
environment scrubbing as the generator; no chat template). Building on the GPU takes 26 s for the
whole collection on the RTX 3070; the GPU is used only while no generator model is loaded, with an
automatic fall back to the CPU. Searches always use the CPU with a 512-token batch (peak RSS
0.83 GiB, of which 0.6 GiB are mapped weights; first search including start ≈ 0.7–0.9 s, later
searches ≈ 0.15–0.2 s), started on the first search and stopped after five idle minutes and at
exit. Without the model, search uses exact and full-text retrieval and says so.

**Collections and updates.** A collection's directory name records what it was built from
(`<manifest revision>-f<format>-clojure-<version>-<text|emb-sha12>`). Builds run in a
`.build-*` directory, are verified (integrity check, chunk and vector counts, dimensions), moved
into `.staging-*`, checked again, renamed into place, and activated by atomically replacing
`active.json`. The previous collection is kept for "Use the previous collection"; older ones are
removed. A failed or cancelled build removes its directory and leaves the active collection in use.
An update is offered when the manifest revision, the runtime's Clojure version, or the index format
differs, when the embedding model is installed but the collection has no vectors, or when it was
built with another embedding model. Building installs the pinned Java runtime and Clojure first if
they are missing.

**Reference data.** Retrieved text is presented to models only through
`clojurecraft_knowledge::prompt`: one optional section labelled as reference material, each excerpt
between numbered `[excerpt N]` delimiters with title, kind, URL, and license; delimiter-like text
inside excerpts is neutralised. The section is optional in the context policy (dropped whole, never
shortened). Generated implementations record the excerpts used as `DocumentationRef` (URL, title,
symbol, namespace, Clojure version, collection version).

**Interface.** The Knowledge sources page shows the active collection (version, Clojure version,
contents, semantic search, build date, size), update reasons, install/update/rebuild with phase
progress and cancellation, failures with "Try again" and (for embedding failures) "Build without
semantic search", rollback, sources with licenses and attribution, the embedding model (download
with progress, cancel, resume, repair, remove), and a search browser whose results expand to the
full text with link, license, revision, and attribution. The right column's Sources tab lists the
documentation recorded for the implementation and searches documentation related to the
specification (description and steps).

### Specification assistant and generation

The model works on a specification in three user-started jobs, run one at a time by the
`Assistant` (`clojurecraft-workflow`). Each job is a sequence of role requests; every response is
matched by request number, so a cancelled or superseded request never changes a function.

**Readiness.** Before any request, the specification is checked: example inputs and expected
outputs, accepted tests, and contract keys and defaults must be valid EDN; an example with only an
input or only an output blocks with a message naming it; empty examples and references without a
recorded output are left out with a note. Something to work from (description, a step, or an
example) is required.

**Prompt content.** Every part the user wrote is a *required* prompt section, never shortened or
dropped: description, non-empty steps (numbered), the accepted contract rendered as text, answered
questions, each usable example, and each accepted test. A proposed contract, open or dismissed
questions, and the model's interpretation are never sent as requirements. Retrieved documentation
is the only optional section. If required content does not fit, the request is refused with the
context policy's remedies.

**Interpretation** (Interpretation, then Clarification role). The model restates the
specification, lists assumptions, and reports contradictions and gaps that change the output;
when there are any, a second request turns them into at most six questions with suggested answers.
The restatement is stored next to the user's text with the specification fingerprint it was made
from and is marked as outdated once the specification changes. Questions stay open until the user
picks an option or writes an answer; only answered questions are requirements (and part of the
specification fingerprint). New interpretations replace open questions but keep answered and
dismissed ones, and dismissed questions are not asked again.

**Contracts.** The contract editor covers input and output value types, key rules (key as EDN,
required or optional, value type, nil behaviour: not allowed, same as missing, passed through,
or a default EDN value), output order, allowed namespaces, and notes. "Propose a contract" asks the
model (Interpretation role) for the same structure; the proposal is stored as *proposed*, is not
part of the specification fingerprint or any prompt, and must be accepted in the editor. A proposal
never replaces an accepted contract (a contract accepted while a proposal is being written wins)
and never changes the allowed namespaces.

**Allowed namespaces.** The harness's namespace allowlist (read from `harness.clj`, like its
denied symbols, so the lists cannot diverge) defines what is *supported*. Without an accepted
contract a function may require `clojure.string` and `clojure.set`; with one, exactly its list.
Generated code that requires another supported namespace is held until the user approves it,
which adds the namespace to the accepted contract (creating one when there is none).

**Generation.** (1) Retrieval planning: the model names up to twelve standard-library symbols and
six concepts. (2) Documentation: the description and steps, each step, and each planned symbol are
searched (4, 2, and 1 results each), merged without duplicates to at most ten excerpts, and the
derived function name is checked against `clojure.core`. Without a documentation collection these
two steps are skipped and the result says so; if planning fails, the description and steps are
used. (3) Generation: the task names the namespace (`<name>.core`) and function (the display name
in kebab case, suffixed `-fn` if it would shadow a `clojure.core` var or special form), the
allowed namespaces, purity and interop rules, and the Clojure version; the answer is
`{code, assumptions}` with a per-request random seed. (4) Structural check (below). (5)
Explanation: a summary, the code implementing each numbered step, and the standard-library
functions used; quoted code is located in the source (ignoring whitespace; shortened quotes by
their first and last lines; ambiguous quotes are not placed) and shown with its line range or as
approximate. A failed explanation keeps the code.

**Structural check** (before any execution). The source is read with the editors' lexer into
forms and checked against the harness's rules: an `ns` form first, with only `:require` of
supported namespaces (`[ns :as a]`, `:refer [..]`; no `:refer :all`); top-level forms limited to
`defn`, `defn-`, `def`, `declare`; exactly one public function, with an arity of one argument; no
denied `clojure.core` symbols, `#'`, `#=`, Java interop, or classes other than the allowed numeric
ones; qualified symbols must name a required namespace (stricter than the harness, so exported code
never relies on namespaces being loaded by accident). Quoted data is not checked. A different
namespace or function name is a warning, not a failure. Problems carry line numbers and are shown
with the rejected code; the current implementation is unchanged. The OS sandbox remains the
security boundary; this check exists for fast, precise feedback.

**Applying code.** Checked code replaces the implementation only when there is none or it is
exactly what the model generated earlier; code written or edited by hand is never replaced
silently, and code needing a namespace approval waits. Held code is shown for review with "Use
this code" or "Allow … and use this code". Applied code is committed as a `Generated` revision
recording the model (repository, revision, file, SHA-256, quantization, architecture), llama.cpp
release and backend, temperature, top-p, seed, context, output limit, the documentation excerpts
actually sent (none if the documentation section was left out to fit), the specification
fingerprint it was generated from, assumptions, and the explanation. If the specification changes
afterwards, the implementation shows that it was generated from an earlier version.

**Answer schemas.** Prose fields carry length limits in the grammar (120–800 characters). Without
them, Qwen2.5-Coder 7B repeated one sentence inside a string until the token limit. Code has no
grammar bound (llama.cpp expands each bound into rules) and is refused above 20,000 characters.
Code that arrives escaped twice (no real line breaks, `\n` text between forms) is unescaped once
when it is a valid JSON string body, with a note.

**Reasoning models.** Their thinking counts against the answer budget. When a structured answer
is cut off after the model reasoned, the request is sent once more with 6,144 extra tokens (the
service still caps the answer at half the context), and later requests keep the allowance while
that model is loaded.

**Model loading.** Starting a job loads the selected model with the current settings when it is
not loaded or loading; the request is queued behind the load.

### Automatic validation and repair

**Sequence** (`clojurecraft_workflow::validation`). (1) Parse and (2) check structure and
dependencies with the structural check above; parse or structure failures stop the sequence and
every later check is listed as not run. (3) Lint, (4) load, (5) run every usable example and every
accepted test with a defined expectation, and check the accepted output contract, all in one fresh
sandboxed JVM. (6) Compare each result with its expected EDN in the JVM. (7) Record diagnostics,
per-call timing, and bounded output (printed text up to 64 KiB captured, 2,000 characters per
diagnostic, 20,000 characters per value). A report passes only when every check passed; a
timeout, crash, forbidden operation (sandbox denial or static-check violation), result over the
element limit, memory or output limit, and a malformed harness response are failures. Code that
needs a namespace approval is validated normally; the approval only gates applying it.

**Harness protocol.** The harness writes one JSON object per line as it works (`read`, `check`,
`lint`, `load`, `start` with the call index, `result`, `halted`, `done`). If the JVM dies (out of
memory, output cap, wall-clock limit, crash), the report attributes the failure to the stage or
call that had started and lists the rest as not run. Each call runs on its own thread with a 5 s
limit that includes realizing its result; on a timeout the harness reports it and stops the JVM,
because a running thread cannot be stopped from outside. The process limit (20 s for start-up
plus 5.5 s per call, at most 10 min) is only a backstop. Runs are cancellable: the sandbox runner
kills the process when a shared flag is set. Call errors report the line in the candidate's
namespace (from the stack trace) and the `clojure.core` function the exception surfaced in, or
for inlined functions such as `inc` the runtime method (`clojure.lang.Numbers.inc`).

**Equality** (`clojurecraft_clojure::Equality`, compared in the JVM against the value read with
`clojure.edn`). Clojure's `=` semantics, made explicit: sequential collections (vector, list,
sequence) are equal when their elements are equal in order; maps by keys and values regardless of
order; sets by elements; integers regardless of class (`1` = `1N`). Numbers of different
categories are never equal: integer vs. floating point (`1` ≠ `1.0`), ratio vs. floating point
(`1/2` ≠ `0.5`), BigDecimal vs. anything else; BigDecimals compare by value (`1.50M` = `1.5M`).
Floating-point numbers are equal within a relative tolerance of 1e-9 (absolute 1e-12 near zero),
and `##NaN` equals `##NaN`; map keys and set elements are compared exactly. When the accepted
contract says output order does not matter, a sequential top-level output is compared as a
multiset. The first difference is reported with its path, for example
`at [0 :total]: expected an integer 25 but got a floating-point number 25.0`.

**Laziness.** A result is realized inside the call's time limit, walking every nested
collection, with a budget of 1,000,000 elements and 512 levels of nesting. More is reported as too
large ("it may be an infinite lazy sequence"); an exception thrown while realizing belongs to the
call that produced the sequence. The value is printed (bounded) only after realization.

**Output contract.** From the accepted contract: the output's value type, and for each key rule
(applied to the output map, or to each map in an output collection) presence of required keys,
the value type of present non-nil values, and nil where nil is disallowed. Types `other` and
`any` are not checked; ordering requirements other than "does not matter", nil defaults, and the
input shape are not checked mechanically. Violations from all results form one `Contract` check.

**Repair loop** (`Assistant`). One generation plus at most three repairs. Every attempt's code is
validated against a snapshot of the specification taken when the job started; its report
(attempt number, generation run, checked source) is stored. The first passing attempt ends the
loop and is explained, then applied or held for review under the rules above. A failing attempt
leads to a repair request (Generation role, a new seed, temperature 0.5 instead of 0.2, because
the model otherwise tends to return its failing code nearly unchanged) containing the whole
specification as required sections, the previous code, the failures (each failing check with its
diagnostics, input, expected and actual values, named as in the specification, at most 6,000
characters), and the same documentation excerpts. When the newest attempt has the same code or
exactly the same failures as an earlier one, the request says so and asks for a different
approach. A malformed or cut-off answer is a failed attempt without code; the next attempt
repairs the most recent code, or generates afresh if there is none. After the fourth failure the
job ends with every attempt's diagnostics visible; nothing is applied, and the user may choose
the best candidate (most passing checks among those that loaded), recorded as not passing. Other
inference failures (timeouts, crashes) end the job and keep the attempts made so far. Examples,
contracts, and accepted tests are never modified by the loop. The revision records the attempts
used and the seed and temperature of the attempt that produced the code.

**Stale results.** If the specification or accepted tests change while a job runs, its reports
stay recorded against the snapshot (so they show as outdated), and passing code is held for review
instead of being applied. Validation results arriving for a cancelled or superseded run are
ignored.

**Unavailable runtime.** If the Java runtime or the sandbox helper is missing, generated code is
handled as before validation existed (applied or held), marked as not run, and the interface
offers to install the pinned Java runtime and Clojure.

**Checks of current code.** The Result tab runs the same validation on the current implementation
(written, edited, or generated), records the report against the function's content at the start,
and shows the newest report for exactly the current code with its freshness, plus earlier reports
(including unapplied candidates and their code).

### Main workspace

Three resizable columns: the function library (search, new function, and navigation to knowledge
sources, settings, and trash) on the left; the specification editor (name, description, example
tabs, contract, ordered steps, interpretation and questions) in the centre; implementation
(generation, live output, review of held or rejected code, explanation, assumptions, provenance),
result (validation: run checks, every check with diagnostics and expected/actual values, freshness,
earlier reports and candidates), step (step-to-code mapping), and source views on the right. Column widths persist. The centre keeps at least 440 points; the side columns give way
first. The documented minimum window is 1024 × 640 points; increasing the text size reduces the
available points, and the layout then wraps rather than clipping.

**Editing model.** The library worker thread owns one store connection and performs library
operations in order (open, create, duplicate, trash, restore, purge, recover); the autosaver owns a
second connection. Every edit bumps the document's edit sequence and is submitted to the autosaver
(600 ms quiet period). The editor keeps every document opened in the session in memory and never
reloads it from disk, so reopening a function cannot resurrect an older draft that is still waiting
to be written. Save state is shown per function (saved, saving, retrying, not saved, conflict); a
non-retryable failure shows the error and a retry action, and closing the window with unsaved
changes asks first. Edits are flushed on exit.

**Recovery.** Moving a function to the trash, removing a step, removing an example, and removing
the contract are undoable from a notification. Notifications stay while hovered or focused, and
trashed functions remain restorable from the trash page; permanent deletion asks for confirmation
with the safe choice focused.

**Identity.** Steps and examples are edited through `FunctionDocument` operations that preserve
identifiers: reordering moves the same `Step`, and undoing a removal reinserts the same identifier
at its old position, so the specification fingerprint returns to its previous value.

**EDN fields.** Text is stored exactly as typed. Each field shows syntax highlighting with
depth-coloured brackets, a status (empty, valid with a summary such as `map · 3 entries`, or the
number of problems), and positioned diagnostics; clicking a diagnostic selects the offending text.
The parser accepts what `clojure.edn/read` accepts without custom readers (including `#:ns{}`,
`#_`, `#inst`, `#uuid`, `##Inf`, ratios, radix and octal integers) and reports Clojure code syntax
(`'`, `#(`, `@`, `^`, regex literals, `::kw`, `#=`), unknown tags, duplicate map keys and set
elements (using Clojure equality: `1` = `1N`, `[1]` = `(1)`), odd map forms, and more than one
top-level value. Nesting deeper than 256 levels is reported instead of parsed. A tree inspector
shows the parsed value with children paged 100 at a time; an expanded editor shows text and
inspector side by side. In a release build, a 3 MB example with 30 000 maps renders at the display
frame rate in both text and tree views; each keystroke in such a value costs roughly 50–100 ms
(re-layout and re-parse). Debug builds are much slower with large values.

**Keyboard.** Every control is reachable with Tab/Shift+Tab and shows an accent focus outline;
text editors do not trap Tab. Shortcuts (Cmd on macOS, Ctrl elsewhere): New function (N), Search
(F), Duplicate (D), Save now (S), Rename (F2), move the focused step (Alt/Option+Up/Down), add a step
after it (Cmd/Ctrl+Enter), text size (Cmd/Ctrl +, −, 0), Esc to close dialogs or clear the search.
Labels use words rather than symbols such as ⌘ because the bundled fonts lack them.

### Hardware detection and budgets

Linux: `/proc/meminfo`, `/proc/cpuinfo`, `nvidia-smi` (when installed), and
`/sys/class/drm/*/device` for AMD VRAM and other display controllers. macOS: `sysctl` (memory,
Apple Silicon, CPU features). After the runtime is installed, `llama-server --list-devices` reports
what the chosen backend actually sees. Failures degrade to notes and manual configuration.

Default budgets (estimates, user-adjustable): dedicated GPU = VRAM − 768 MiB plus up to half of
system RAM for partial offload; Apple Silicon = 50 % of unified memory (36 GB → 18 GB); CPU-only =
50 % of RAM, keeping at least 4 GiB free. Model estimates = weights + per-token KV cache × context
+ a compute allowance; catalog KV figures come from each model's published configuration, and
for other repositories from the GGUF header (`block_count × head_count_kv × (key + value
length) × 2 bytes`).

**Manual correction.** Detected values are never edited in place. The user's corrections
(system memory; a GPU vendor and memory, replacing the largest detected GPU of that vendor or
adding one when none was found) are stored in `settings.json` and applied on top of detection,
marked as "entered manually". When nothing could be detected the budget is a 4 GiB placeholder
and setup asks for the memory size. The budget itself can be overridden (device and offload
bytes).

**Partial offload.** When an estimate exceeds the device budget but fits within device plus
offload memory, whole layers are split: the KV cache and compute buffers are assumed to stay on
the device, and as many layers as fit go to the GPU. Catalog entries carry their layer count;
other models take it from the GGUF header. The split is shown as an estimate in setup and
settings, and "GPU layers" can be set to automatic, all, or a fixed count.

### Inference setup and model management

**First run.** When `settings.json` has no completed or skipped setup, the app opens the setup
page instead of the editor: hardware (with corrections and budget), backend (recommended first;
backends without matching hardware are marked; CPU fallback explained), llama.cpp runtime
(download with progress, cancellation, and resume; verification; smoke test), and the catalog
ranked for the budget (recommended entry highlighted; estimated memory, download size, context,
license; entries over budget hidden behind a toggle and marked when shown). Setup can be skipped
and reopened from the sidebar or settings. While the runtime or an intact selected model is
missing, the sidebar and the right column say what is missing; specification editing is
unaffected.

**Background work.** Detection, downloads, hashing, Hub lookups, header inspection, and
credential-store access run on worker threads that report typed events. One runtime install and
one model download run at a time; further models queue. Cancelling keeps `.part` files, so the
next attempt resumes with an HTTP range request.

**Runtime builds.** Each backend build is installed as `runtimes/llama.cpp/<release>-<backend>/`.
Settings list installed builds and allow activating another, rolling back to the previously
active one, and removing inactive ones (the active build cannot be removed). Choosing a backend
whose build is installed activates it. Before downloading, free space for the archives and their
extraction is checked.

**Model store.** A model is installed from a `ModelRecord` (repository, pinned commit, files with
size and SHA-256, architecture, layers, KV estimate, license). Files download to
`downloads/<sha12>-<name>`, are verified, moved into `.staging-<quant>` next to the final
directory together with `model.json`, and the staging directory is renamed into place; only then
does the model appear. Files already present in the final directory with the recorded size and
hash are reused (this also adopts files fetched by the developer tool). Free space for the
remaining bytes plus 512 MiB is checked first; the error tells the user that finished files are
kept. Listing performs a size check of every file (missing or changed files mark the model as
needing repair); "Verify" re-hashes everything; "Repair" downloads only what is broken. Removal
renames the directory away first, so a half-deleted model never looks installed. A single
installed model is selected automatically for generation.

**Other Hugging Face repositories.** The repository id and revision are validated, and the
revision is resolved to a commit through `/api/models/<repo>/revision/<rev>?blobs=true`; all
downloads use that commit. GGUF files are grouped into options (split files
`name-0000N-of-0000M.gguf` become one option; vision projector `mmproj` files are not offered).
Before any download, "Check" reads the first file's header through HTTP range requests (2 MiB,
growing up to 64 MiB only if the metadata is larger) and reports, before anything large is
fetched: unsupported or missing architecture (the list ships in the catalog file), missing split
parts, a split count that disagrees with the file names, files without a Hub-published
SHA-256, and single files with fewer than two tensors per declared layer (add-ons such as
speculative-decoding heads). Repositories without GGUF files (for example safetensors only) are explained.
Installed files are verified against the Hub's LFS SHA-256 and that hash is recorded for later
verification. Nothing from a repository is ever executed.

**Access tokens.** A Hugging Face token is stored in the OS credential store under the service
`org.clojurecraft.ClojureCraft` (`keyring-core` with the zbus Secret Service store on Linux,
the login Keychain on macOS), read and written on worker threads because the store may prompt
to unlock. When no store is available the token is used for the session only and the user is
told so. A loaded token is registered with the `SecretRegistry`, so it can never enter function
drafts, revisions, or results; `Secret` and `Hub` redact it from `Debug` output, errors never
contain it, and it is sent only to the Hub host (redirects to storage hosts drop the header).
Gated repositories are never accepted on the user's behalf: a refusal explains that the
conditions must be accepted on huggingface.co.

**Settings file.** `settings.json` (setup state, backend, active model, context limit, GPU layers,
budget override, hardware corrections) is written with write-then-rename. An unreadable file is
kept as `settings.json.unreadable` and defaults apply; a file from a newer format is left
untouched; both are reported in settings.

**Storage.** Settings show the space used by models, runtimes, cached runtime archives
(deletable), and unfinished downloads (discardable), and the free space on the data volume.

## Supported-platform matrix

| | Apple Silicon macOS | x86_64 Linux |
| --- | --- | --- |
| Minimum version | macOS 14 Sonoma (proposed, unverified) | glibc ≥ 2.34 and kernel ≥ 5.13 with Landlock enabled (Ubuntu 22.04+, Fedora 36+, Debian 12+) |
| Reference machine | 36 GB M-series (not yet tested) | Ubuntu 24.04.5, kernel 7.0, RTX 3070 8 GB, 31 GiB RAM (tested) |
| UI | unverified | verified on X11 at 1× and simulated 2× scale; Wayland compiled, unverified |
| Inference backends | Metal (unverified) | CUDA, Vulkan, CPU (verified); ROCm (pinned, unverified) |
| Sandbox | Seatbelt via `sandbox-exec` (unverified) | Landlock + seccomp + rlimits (verified) |
| Packaging | signed + notarized `.app` in `.dmg`/`.zip` | AppImage (primary) and `.tar.gz` |
| Updates | signed update payload, bundle replacement | signed AppImage replacement; package-manager installs update through the package manager |

macOS 14 is proposed because it is the oldest release still receiving security updates at the
time of writing and covers all Apple Silicon Macs; it must be confirmed on hardware.

## Signing and update strategy (initial)

- Application updates: a dedicated Ed25519 updater key signs the release metadata (version,
  platform, artifact hashes); the app embeds the public key and rejects unsigned, mismatched,
  wrong-platform, or downgrade payloads. Checksums alone are never treated as authenticity.
- macOS: Developer ID signing + notarization of the app bundle; the sandbox helper is signed as
  part of the bundle. Downloaded llama.cpp and JRE binaries are not quarantined when fetched by the
  app itself, but whether Gatekeeper/library validation accepts them from the app's data
  directory must be verified on a Mac.
- Linux: no OS-level signing; authenticity comes from the updater signature (and a detached
  signature published next to each AppImage for manual verification).
- Third-party runtimes are never auto-updated from upstream: a new llama.cpp/JRE/Clojure version
  is a new pinned manifest in a new application release.

## Open decisions and risks

1. **macOS validation is outstanding** (UI, Metal, Seatbelt profile, notarization interaction with
   downloaded binaries, Keychain token storage, first-run setup). This needs access to an Apple
   Silicon Mac; the macOS credential store is compiled only for macOS targets and has not been
   built.
2. Whether to offer the CUDA 12.8 llama.cpp build for drivers older than CUDA 13.4 support.
3. Whether to move `llama-server` to a Unix-domain socket.
4. cgroup v2 limits for thread count and memory on Linux, when a delegated cgroup is available.
5. Catalog entries marked `unverified` must be smoke-tested on suitable hardware (≥ 16 GB VRAM or
   unified memory) before release.
6. Wayland rendering, fonts, and shortcuts are compiled in but unverified; no Wayland session or
   compositor was available on the reference Linux machine.
7. The bundled fonts lack some symbols (⌘, arrows, ✕); the interface uses words instead. Adding a
   symbol font (or system fonts) is optional polish.
8. Model catalog entries are recommended only when `smoke-tested`; Qwen2.5-Coder 1.5B and 7B are
   the only ones so far, so machines with large budgets are still recommended a 7B model.
9. Per-model GPU layer and context settings are global, not per model.
10. The application finds the sandbox helper next to its own executable. `cargo run` builds only
    the application; run `cargo build --workspace` once so the helper exists. Packaging must place
    it next to the binary.
11. The embedding model has been validated on Linux (CUDA and CPU) only; Metal is untested.
12. The retrieval evaluation set is small (36 queries) and written by us; it guides weights but is
    not a benchmark. It should grow with real specifications once generation exists.
13. Repair quality depends on the model. With Qwen2.5-Coder 7B Q4_K_M, generations that fail
    their first validation were rarely repaired: the model tends to repeat its approach even when
    told it is stuck. Mellum 12B (thinking) often spends its whole answer budget reasoning, which
    counts as a failed attempt. Larger catalog models should be evaluated for repairs.
14. Interpretation and contract-proposal quality depends heavily on the model: Qwen2.5-Coder 7B
    asks some irrelevant questions and proposed an input type of "map" for a vector of maps. The
    user reviews both, so neither changes intent, but larger catalog models should be evaluated.
15. Assistant results (held code, last errors) are kept for the session only; validation reports
    of every attempt, with their code, are stored and survive restarts.
16. Only one model job runs at a time across all functions; a second one is refused with a message
    naming the busy function.
17. Exported `clojure.test` tests will need the same comparison as validation (float tolerance,
    unordered output); plain `=` would disagree for floating-point results.
18. Validation of example inputs against the accepted input shape is not mechanical; contradictions
    are left to interpretation.
19. A per-call time limit stops the whole JVM, so the calls after a timed-out call are not run in
    that validation.
20. The sandbox on macOS (Seatbelt) has not run the new harness; thread creation inside the JVM is
    needed for per-call limits.
