# Development

Requirements: Rust 1.95 (pinned in `rust-toolchain.toml`). On Linux, the usual X11/Wayland
development headers are not needed (winit loads them at runtime), but a working OpenGL driver is.

```sh
cargo run                      # opens the ClojureCraft window
cargo test --workspace         # unit and integration tests
cargo clippy --workspace --all-targets
cargo fmt --all
```

`cargo test --workspace` builds the `clojurecraft-sandbox` helper that the sandbox and Clojure
integration tests use. The sandboxed Clojure tests skip themselves (with a message) until the Java
runtime and Clojure jars are installed:

```sh
cargo build --workspace
./target/debug/clojurecraft-devtool hardware                 # detection, budget, ranked catalog
./target/debug/clojurecraft-devtool install-runtime [cpu|vulkan|cuda|rocm|metal]
./target/debug/clojurecraft-devtool fetch-model qwen2.5-coder-1.5b-instruct Q4_K_M
./target/debug/clojurecraft-devtool smoke-model qwen2.5-coder-1.5b-instruct Q4_K_M cpu
./target/debug/clojurecraft-devtool sandbox-demo
```

The devtool writes to the normal application data directory (see `docs/architecture.md`); set
`CLOJURECRAFT_DATA_DIR` to use a scratch location instead. `install-runtime` makes the backend it
installs the active one; installing several backends keeps each under
`runtimes/llama.cpp/<release>-<backend>/`.

To validate a new GGUF architecture before adding it to `[compatibility] architectures` in
`resources/model-catalog.toml`, load any local file with the pinned build and run the standard
prompt (the reply is run in the sandbox); record the result in `docs/model-compatibility.md`:

```sh
./target/debug/clojurecraft-devtool smoke-file path/to/model.gguf cuda 8192 20   # backend, context, GPU layers
./target/debug/clojurecraft-devtool run-reply reply.txt                          # run a saved model reply
```

`fetch-model` installs a catalog model through the same verified model store the application
uses, so the application lists it afterwards.

The credential-store round trip writes and deletes a test entry in the real Keychain or Secret
Service, so it only runs when asked for:

```sh
cargo test -p clojurecraft-credentials -- --ignored
```

Code that drives the inference service can be tested without llama.cpp: the
`clojurecraft-inference` crate's `mock` feature provides `mock::MockEngine`, a scripted engine
(streamed pieces with pauses, tool calls, failures, hangs, and crashes). The crate's own tests also
run its HTTP client and the llama.cpp engine against a scripted server on a Unix socket and a
shell script standing in for `llama-server`.

`smoke-model` and `smoke-file` load the model through the same engine the application uses
(private Unix socket, per-session key) and report load time, prompt tokens counted with the
model's tokenizer, generation speed, peak server RSS, GPU memory, and cancellation latency.

Documentation retrieval can be exercised from the command line with the same code the application
uses (`knowledge-build` needs the Java runtime and Clojure; embeddings need the llama.cpp runtime):

```sh
./target/debug/clojurecraft-devtool fetch-embedding-model          # optional, 610 MiB
./target/debug/clojurecraft-devtool knowledge-build [text|cpu|gpu] # default gpu
./target/debug/clojurecraft-devtool knowledge-search "count how often each value occurs" [--all]
./target/debug/clojurecraft-devtool knowledge-eval                 # hit@1, hit@5, MRR per ranking
```

`knowledge-eval` runs the queries in `crates/clojurecraft-devtool/retrieval-eval.toml` against the
installed collection with exact + full text, semantic only, and the combined ranking. Re-run it
after changing chunking, weights, sources, or the embedding model, and record the result in
`docs/architecture.md`.

The specification assistant (interpretation, contract proposal, and generation with retrieval,
validation in the sandbox, automatic repair, and explanation) can be run against a real model with
the same code the application uses. Every attempt's checks are printed (failing checks with their
diagnostics and expected and actual values):

```sh
./target/debug/clojurecraft-devtool generate crates/clojurecraft-devtool/generation-specs/order-totals.toml \
    qwen2.5-coder-7b-instruct Q4_K_M cuda 8192 --interpret --contract   # add --verbose for raw answers
```

`--no-docs` generates without the documentation collection. Validation needs the Java runtime and
Clojure (installed by `install-runtime` or a documentation build); without them the code is
reported as not run. The sample specifications in
`crates/clojurecraft-devtool/generation-specs/` cover grouping and summing, sorting with a
tie-breaker, string keys (needs `clojure.string`), and nested data with a deliberate gap.

The sandboxed Clojure tests (`crates/clojurecraft-clojure/tests/sandboxed_eval.rs`) cover the
harness: comparison rules, lazy and infinite results, per-call time limits, memory and output
limits, lint findings, contract checks, call-error locations, and cancellation. The repair loop
is tested with scripted model answers and real sandboxed execution in
`crates/clojurecraft-workflow/tests/repair_loop.rs`. Exact output printing and instrumented runs
are covered in `crates/clojurecraft-clojure/tests/probes.rs`, and referenced inputs and step values
end to end in `crates/clojurecraft-workflow/tests/references_and_steps.rs`. All of these skip
themselves without the Java runtime or the sandbox helper.

The application runs the API extraction through the sandbox helper next to its own executable, so
build the whole workspace (`cargo build --workspace`) before `cargo run` when installing the
documentation from the app.

Inference checks (`smoke-model`) are local and manual: they need real weights and, for GPU
backends, a GPU. They are never part of automated CI.
