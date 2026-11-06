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

Inference checks (`smoke-model`) are local and manual: they need real weights and, for GPU
backends, a GPU. They are never part of automated CI.
