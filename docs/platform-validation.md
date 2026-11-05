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

## macOS

Not yet run on hardware. The macOS-specific crates (`clojurecraft-hardware`,
`clojurecraft-sandbox` including the helper binary, `clojurecraft-clojure`) pass
`cargo check --target aarch64-apple-darwin`. Crates that pull in `ring` (TLS) need an Apple
toolchain to check and could not be cross-checked from Linux.

Outstanding on an Apple Silicon Mac: window, Metal inference, Seatbelt profile against the same
attack list, JRE and llama.cpp execution from the app data directory under Gatekeeper, peak memory
of a ~30B model within the 18 GB budget.
