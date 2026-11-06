# Platform validation log

Observations from running the code on real machines. Numbers are single observations, not
benchmarks; memory figures are measurements, unlike the estimates in the model catalog.

## Linux reference machine — 2026-11-06

Ubuntu 24.04.5 LTS, kernel 7.0.0-42, glibc 2.39, X11 session. AMD Ryzen 7 3700X (AVX2/FMA,
16 logical cores), 31.3 GiB RAM, NVIDIA GeForce RTX 3070 8 GiB (driver 595.99.02).
`kernel.apparmor_restrict_unprivileged_userns = 1` (user namespaces unavailable to unprivileged
programs, so bubblewrap-style sandboxes would not work without an AppArmor profile).

### Application window

`cargo run` opens the 1360×860 window (minimum 1024×640) on X11. The background system check
reports platform, RAM, GPU, recommended backend (CUDA), estimated budget (7.2 GiB device + 15.6
GiB spill), installed runtimes, and the suggested model.

### Runtime installation

| Component | Archive | Result |
| --- | --- | --- |
| llama.cpp b12138 CPU | 23 MB | installed, `--version` ok, no GPU devices (expected) |
| llama.cpp b12138 Vulkan | 36 MB | installed, `Vulkan0: NVIDIA GeForce RTX 3070 (8192 MiB)` |
| llama.cpp b12138 CUDA 13.4 | 170 MB + 440 MB cudart | installed, `CUDA0: NVIDIA GeForce RTX 3070 (7819 MiB)` |
| Temurin JRE 25.0.5+8 | 62 MB | installed, `java -version` ok |
| Clojure 1.12.6 + spec jars | 4.9 MB | installed |

Finding fixed during validation: the CUDA runtime archive unpacks into its own top-level
directory, so `libggml-cuda.so` (RUNPATH `$ORIGIN`) could not load it and the build silently
reported no devices while `--version` still succeeded. The manifest now merges that archive into
the server directory, and installation of a GPU backend fails unless `--list-devices` reports a
device.

A model file downloaded earlier by another tool into the `.part` location was picked up,
hash-verified, and promoted without re-downloading.

### Inference smoke tests (context 8192, prompt 119 tokens, temperature 0.2)

| Model | Backend | Load | Generation | Server peak RSS | GPU memory delta | Generated code in sandbox |
| --- | --- | --- | --- | --- | --- | --- |
| Qwen2.5-Coder 7B Q4_K_M | CUDA | 1.6 s | 77 tok/s | 4.7 GiB | 4.84 GiB | correct result |
| Qwen2.5-Coder 7B Q4_K_M | Vulkan | 2.3 s | 63 tok/s | 4.6 GiB | 4.69 GiB | not executed (earlier run) |
| Qwen2.5-Coder 7B Q4_K_M | CPU | 4.2 s | 8.7 tok/s | 7.8 GiB | — | correct result |
| Qwen2.5-Coder 1.5B Q4_K_M | CUDA | 0.8 s | 236 tok/s | 1.4 GiB | 1.39 GiB | rejected: `(:use …)` in `ns` |
| Qwen2.5-Coder 1.5B Q4_K_M | Vulkan | 1.0 s | 248 tok/s | 1.2 GiB | 1.24 GiB | rejected: `(:use …)` in `ns` |
| Qwen2.5-Coder 1.5B Q4_K_M | CPU | 1.2 s | 37 tok/s | 1.9 GiB | — | rejected: `(:use …)` in `ns` |

Every run also confirmed: a request without the session key gets HTTP 401; cancelling a streaming
request returns within 7–115 ms; the server answers a new request immediately afterwards.

Observations:

- GPU peak memory for the 7B model at 8k context (4.7–4.8 GiB) is below the catalog estimate for
  8k (about 5.5 GiB), so the GPU estimates are conservative.
- CPU-only peak RSS (7.8 GiB) exceeds the estimate. RSS counts the memory-mapped model file plus,
  most likely, the repacked CPU copy of the weights that llama.cpp creates for AVX2 kernels. The
  CPU estimate needs a separate factor; to be calibrated before CPU budgets are enforced.
- The 1.5B model ignored explicit `ns` instructions in every run; the static check produced a
  precise, repairable diagnostic. Larger models followed them.

### Sandboxed Clojure execution

Fresh JVM per evaluation under Landlock (ABI fully enforced on kernel 7.0) + seccomp + rlimits:
about 0.53 s per evaluation including JVM start.

| Attempt (static checks off unless noted) | Outcome |
| --- | --- |
| Pure function with `clojure.string` | value returned |
| `eval`, `resolve`, `#'var`, `ns-publics`, `#=` reader eval, `System/getenv`, `java.io.File.`, `.method`, `clojure.java.shell` (checks on) | rejected by static check with the offending form |
| `slurp "/etc/hostname"`, listing `$HOME` | `Permission denied` / `AccessDeniedException` |
| `spit "/tmp/…"` | `Permission denied`, no file created |
| TCP connect via `java.net.Socket` | fails (socket creation refused) |
| `Runtime.exec "/bin/true"` | `posix_spawn failed, error: 38 (Function not implemented)` |
| infinite `loop`/`recur` | killed at the wall-clock limit |
| allocate unbounded `long-array`s | JVM exits on OOM → reported as memory limit |
| `println` in a loop | stops at the 64 Ki-char capture limit |
| raw `System.out` flood | process killed at the output cap |
| unbounded recursion | `StackOverflowError` reported as a call error |

The same boundary holds for ordinary programs (`/bin/sh`, `cat`, `yes`, `bash /dev/tcp`) in
`crates/clojurecraft-sandbox/tests/linux_restrictions.rs`.

### Local inference service — 2026-11-06

`clojurecraft-devtool smoke-model qwen2.5-coder-7b-instruct Q4_K_M <backend> 8192`, which loads
through the production engine (private Unix socket, per-session key). Prompt 119 tokens (counted
with the model's tokenizer before sending, and equal to the server's count), temperature 0.2.
Single observations.

| Backend | Load | Generation | Server peak RSS | GPU memory delta (whole device) | Cancel latency | Generated code in sandbox |
| --- | --- | --- | --- | --- | --- | --- |
| CUDA | 6.2 s (cold file cache) | 78.7 tok/s | 4.7 GiB | 4.84 GiB | 13.5 ms | correct result |
| Vulkan | 1.5 s | 77.8 tok/s | 4.6 GiB | 4.69 GiB | 13.8 ms | correct result |
| CPU | — | 9.3 tok/s | 7.8 GiB | — | 108.6 ms | correct result |

Peak RSS counts the memory-mapped model file. The CPU figure again exceeds the 8k-context estimate
(about 5.5 GiB); see the earlier note on repacked CPU weights. Each run answered a follow-up request
immediately after the cancellation.

| Check | Result |
| --- | --- |
| Real out-of-memory load (7B, 131,072-token context, all layers on the 8 GiB RTX 3070) | Classified as "not enough GPU memory" with the line `ggml_backend_cuda_buffer_type_alloc_buffer: allocating 7168.00 MiB on device 0: cudaMalloc failed: out of memory`; server exited and was cleaned up |
| App: Load model from the implementation column | Loading → reading the model → Ready (1.3 s warm) with backend, context, load time, and tool support shown |
| App: "Try the model" streaming | Text streams while the window stays responsive; Stop freezes the output and shows "Stopped."; "Ask again" replaced the running answer with no mixed output |
| App: changing the context limit while loaded | "Settings changed since the model was loaded" with Reload |
| App: closing the window with the model loaded | llama-server stopped; PID file and socket directory removed |
| Server reachability | No TCP listener; socket directory `drwx------`; requests without or with a wrong key get HTTP 401; the key is in the server's environment, not its command line |
| Killing llama-server with SIGKILL while the app runs (automatic restart) | not run here (killing the process was not permitted in this session); covered by automated tests with the mock engine and with a stand-in server process |
| Tool calls with Qwen2.5-Coder 7B / 1.5B | The models write the call as text (a fenced `<tools>` block or bare JSON) instead of their template's tool-call markup, so llama.cpp returns plain content; see `docs/model-compatibility.md` |

## macOS

Not yet run on hardware. The macOS-specific crates (`clojurecraft-hardware`,
`clojurecraft-sandbox` including the helper binary, `clojurecraft-clojure`) pass
`cargo check --target aarch64-apple-darwin`. Crates that pull in `ring` (TLS) need an Apple
toolchain to check and could not be cross-checked from Linux.

Outstanding on an Apple Silicon Mac: window, Metal inference, Seatbelt profile against the same
attack list, JRE and llama.cpp execution from the app data directory under Gatekeeper, peak memory
of a ~30B model within the 18 GB budget.

### Specification workspace — 2026-11-06

Driven with synthetic X11 input against a scratch library (`CLOJURECRAFT_DATA_DIR`), release build
unless noted.

| Check | Result |
| --- | --- |
| Create (Ctrl+N), name selected for typing, rename, duplicate, move to trash, undo, restore, permanent delete with confirmation | works |
| Edits survive restart, including after the process was killed with SIGTERM once "Saved" showed | works |
| Last open function, column widths, and text size restored after a normal close | works |
| Steps: add, Ctrl+Enter inserts after the focused step, Alt+Up/Down reorders with focus following the step, remove and undo at the original position | works |
| Invalid EDN: text kept, problems listed with line/column, offending text underlined, click selects it | works |
| 3 000-item example (≈300 KB): text and tree views | ~60 fps (release); about 1.5 s per frame in a debug build |
| 30 000-item example (≈3 MB): text and tree views | ~60 fps (release); ~50–100 ms per keystroke |
| Read-only library file: save fails with a visible, non-retrying error and "Try again"; closing asks before discarding | works |
| Minimum window 1024 × 640: all three columns usable, centre column 440 points | works |
| Text size 120 % and 150 % (Ctrl+Plus): layout wraps, no clipping of controls | works |
| Simulated HiDPI (`WINIT_X11_SCALE_FACTOR=2`): crisp text, correct layout | works |
| Wayland | not tested (no compositor available) |

### First-run setup and model management — 2026-11-06

Release build against an empty scratch data directory (`CLOJURECRAFT_DATA_DIR`), real network
(about 12 MB/s), driven with synthetic X11 input.

| Check | Result |
| --- | --- |
| First start opens setup; hardware shows RTX 3070 8 GiB (nvidia-smi), 31.3 GiB RAM, AVX2/FMA; budget 7.2 GiB GPU + 15.6 GiB offload (estimate) | works |
| Backend list: CUDA recommended; Vulkan, CPU offered; ROCm marked "no matching GPU detected" | works |
| llama.cpp b12138 CUDA (162 MiB + 420 MiB): download with progress, cancel during the second archive (first archive and a 66 MB `.part` kept), "Try again" resumed from the partial file, verified, smoke-tested, activated; llama.cpp reports `CUDA0: NVIDIA GeForce RTX 3070 (7833 MiB)` | works |
| Catalog ranked for the budget: 7B Q4_K_M recommended and highlighted; partial-offload splits shown (e.g. 30B-A3B Q4_K_M: 14 of 48 layers); unverified entries and license notes marked | works |
| Qwen2.5-Coder 1.5B Q4_K_M (1.1 GB) downloaded in about 60 s, hash-verified, activated atomically, selected automatically; "Finish setup" enabled | works |
| Restart: setup not shown again; settings, active model, and runtime intact | works |
| `unsloth/gemma-3-1b-it-GGUF`: architecture `gemma3` rejected after reading only the header | works |
| `Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF` Q4_0: header check (24 layers, est. 1005 MiB at 16k), installed under its pinned commit with the Hub's SHA-256; the record was briefly visible only in `.staging-Q4_0` before the rename | works |
| `Qwen/Qwen2.5-Coder-0.5B-Instruct` (safetensors only) explained; nonexistent repository reports "not found, or private or gated"; malformed id rejected before any request | works |
| Truncated an installed model file: listed as "needs repair"; Repair re-downloaded it and the hash matched | works |
| Token saved through the UI: stored in GNOME Keyring (Secret Service), masked while typing, absent from every file in the data directory and from the log, restored after restart, removed from the keyring by "Remove token" | works |
| Clipboard: text copied in another application pasted into a field with Ctrl+V, and text copied from a field read back from the X clipboard (X11) | works |
| CPU-only and Apple Silicon recommendations | covered by tests with mock hardware reports only |
| macOS (Keychain, Metal, setup) | not tested (no Mac available) |
