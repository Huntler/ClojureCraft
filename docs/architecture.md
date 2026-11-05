# ClojureCraft architecture decisions

Status: foundation. Each decision lists what was chosen, why, and how it was verified on real
hardware. "Verified" means exercised on a machine; "unverified" means designed and compiled only.

## Workspace layout

| Crate | Responsibility |
| --- | --- |
| `clojurecraft-app` | egui/eframe UI, application state, background job dispatch (binary `clojurecraft`) |
| `clojurecraft-hardware` | RAM, CPU feature, and GPU/VRAM detection; backend recommendation; default memory budget |
| `clojurecraft-runtime` | Runtime manifest, resumable SHA-256-verified downloads, atomic install/rollback, model catalog and fit estimates |
| `clojurecraft-inference` | `llama-server` child process lifecycle, loopback HTTP client, streaming, cancellation |
| `clojurecraft-sandbox` | OS-level restriction of untrusted processes (library + `clojurecraft-sandbox` helper binary) |
| `clojurecraft-clojure` | Sandboxed JVM evaluation of candidate functions; Clojure-side static checks (harness) |
| `clojurecraft-devtool` | Developer commands that exercise installation, inference, and sandboxing on a real machine |

Later boundaries (domain model, storage, documentation retrieval, export, benchmarking, updates)
get their own crates when they are implemented. Long-running work runs on background threads that
report typed events over a channel; the UI drains the channel each frame and never blocks
(`clojurecraft-app/src/jobs.rs`).

Data lives outside the repository in the platform data directory, overridable with
`CLOJURECRAFT_DATA_DIR`:

- Linux: `~/.local/share/clojurecraft/`
- macOS: `~/Library/Application Support/ClojureCraft/`

with `downloads/`, `runtimes/<component>/<version>/`, `models/<id>/<quant>/`, and `harness/`.

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

Isolation of the server: bound to `127.0.0.1` on an ephemeral port, authenticated with a random
256-bit per-session key passed through the environment (`LLAMA_API_KEY`, so it never appears in
process listings), Web UI disabled. The pinned build also accepts a Unix-domain socket path for
`--host`; switching to a socket in a private directory would remove even the loopback port and is
the preferred hardening for the production client.

- Verified on Linux: CPU, Vulkan and CUDA builds with two models; see `docs/platform-validation.md`.
- Unverified: Metal on macOS.

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

### Storage and retrieval index: SQLite (bundled via `rusqlite`)

Chosen for transactional writes with WAL journaling (crash safety for drafts and revisions),
schema migrations, and the built-in FTS5 full-text index for documentation. The documentation
corpus is small (thousands of chunks), so embedding vectors can be stored in SQLite and searched
by exact cosine similarity in memory, avoiding a separate vector-index dependency. Embeddings will
be produced by a small GGUF embedding model through `llama-server --embedding` (for example
Qwen3-Embedding-0.6B), reusing the inference runtime. Not yet implemented.

### Hardware detection and budgets

Linux: `/proc/meminfo`, `/proc/cpuinfo`, `nvidia-smi` (when installed), and
`/sys/class/drm/*/device` for AMD VRAM and other display controllers. macOS: `sysctl` (memory,
Apple Silicon, CPU features). After the runtime is installed, `llama-server --list-devices` reports
what the chosen backend actually sees. Failures degrade to notes and manual configuration.

Default budgets (estimates, user-adjustable): dedicated GPU = VRAM − 768 MiB plus up to half of
system RAM for partial offload; Apple Silicon = 50 % of unified memory (36 GB → 18 GB); CPU-only =
50 % of RAM, keeping at least 4 GiB free. Model estimates = weights + per-token KV cache × context
+ a compute allowance; the KV figures come from each model's published configuration and will be
cross-checked against GGUF metadata when models are installed.

## Supported-platform matrix

| | Apple Silicon macOS | x86_64 Linux |
| --- | --- | --- |
| Minimum version | macOS 14 Sonoma (proposed, unverified) | glibc ≥ 2.34 and kernel ≥ 5.13 with Landlock enabled (Ubuntu 22.04+, Fedora 36+, Debian 12+) |
| Reference machine | 36 GB M-series (not yet tested) | Ubuntu 24.04.5, kernel 7.0, RTX 3070 8 GB, 31 GiB RAM (tested) |
| UI | unverified | verified (X11); Wayland compiled, unverified |
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
   downloaded binaries). This needs access to an Apple Silicon Mac.
2. Whether to offer the CUDA 12.8 llama.cpp build for drivers older than CUDA 13.4 support.
3. Whether to move `llama-server` to a Unix-domain socket.
4. cgroup v2 limits for thread count and memory on Linux, when a delegated cgroup is available.
5. Catalog entries marked `unverified` must be smoke-tested on suitable hardware (≥ 16 GB VRAM or
   unified memory) before release.
