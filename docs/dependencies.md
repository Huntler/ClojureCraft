# Dependency and license inventory

ClojureCraft's own code is dual-licensed MIT OR Apache-2.0 (workspace `Cargo.toml`). This page
lists what is linked into the application and what it downloads at runtime, with the obligations
each one carries.

## Downloaded at runtime (not redistributed in the application bundle)

| Component | Version | License | Obligations |
| --- | --- | --- | --- |
| llama.cpp (`ggml-org/llama.cpp`) | b12138 | MIT | Show the copyright and license notice (the archive ships `LICENSE`); mention in About/README |
| NVIDIA CUDA runtime (`cudart-…` archive) | 13.4 | NVIDIA CUDA EULA (redistributable components) | Only downloaded for the CUDA backend; link to the EULA in the setup UI before download |
| Eclipse Temurin JRE | 25.0.5+8 | GPL-2.0 with Classpath Exception | Show the license notice; we neither modify nor bundle it |
| Clojure | 1.12.6 | EPL-1.0 | Show the license notice; exported user code is not a derivative work of the JVM runtime |
| `spec.alpha`, `core.specs.alpha` | 0.5.238, 0.4.74 | EPL-1.0 | As above |
| Model weights | per catalog | Apache-2.0 (current catalog) | Show each model's license before download; never auto-accept gated-model agreements |
| Qwen3-Embedding-0.6B GGUF (`Qwen/Qwen3-Embedding-0.6B-GGUF`) | commit `370f27d7`, Q8_0 | Apache-2.0 | Optional; license shown before download |
| clojure.org pages (`clojure/clojure-site`) | commit `d664fff7`, 19 pages | EPL-1.0 | Indexed locally, never redistributed by us; attribution ("Copyright (c) Rich Hickey and contributors") shown with every excerpt and on the Knowledge sources page |
| Clojure docstrings | from the installed Clojure 1.12.6 | EPL-1.0 | Read from the runtime; attribution shown as above |

If a later release bundles any of these instead of downloading them, redistribution obligations
(notably the GPL-2.0 CE source offer for the JRE and the CUDA EULA) must be revisited.

## Linked Rust crates

445 third-party crates in the resolved graph (all targets, normal and build dependencies), all
under permissive licenses. Summary at the time of writing:

| License expression (as declared) | Crates |
| --- | --- |
| MIT and/or Apache-2.0 variants | ~380 |
| Unicode-3.0 (alone or combined) | 20 |
| Zlib variants | 18 |
| ISC, BSD-2/3-Clause, BSL-1.0, Unlicense, 0BSD variants | ~20 |
| MPL-2.0 | 1 (`option-ext`, via `directories`) |
| CDLA-Permissive-2.0 | 1 (`webpki-roots`, Mozilla CA list via `ureq`/`rustls`) |
| `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0` | 1 (`epaint_default_fonts`) |

Notable obligations:

- `epaint_default_fonts` embeds Ubuntu-Light (Ubuntu Font Licence 1.0), Hack (MIT/Bitstream Vera),
  Noto Emoji and emoji-icon-font (OFL-1.1). Font license texts must accompany distributed binaries.
- `option-ext` (MPL-2.0): file-level copyleft; unmodified use only requires making its source
  available, which crates.io already does. Do not vendor-modify it.
- `webpki-roots` (CDLA-Permissive-2.0): include the license text in the third-party notices.
- SQLite 3.54 is compiled into the application through `libsqlite3-sys` (`rusqlite`'s `bundled`
  feature); SQLite itself is public domain, the Rust bindings are MIT.
- `seccompiler` (Apache-2.0, from Firecracker) and `landlock` (MIT OR Apache-2.0) are used
  unmodified.
- Credential storage: `keyring-core`, `zbus-secret-service-keyring-store` and `secret-service`
  (Linux, pure Rust over the `zbus` already used by accesskit; session encryption through the
  RustCrypto `aes`, `cbc`, `hkdf` crates), `apple-native-keyring-store` and `security-framework`
  (macOS only). All MIT OR Apache-2.0.
- Test fixtures in `crates/clojurecraft-knowledge/tests/fixtures/` contain short EPL-1.0 excerpts
  of Clojure docstrings and clojure.org pages, with attribution in the fixture README.
- `rustix` (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT) for free-space queries
  (`statvfs`) and for signalling the llama.cpp server process, without unsafe code.
- `similar` (Apache-2.0) computes line differences for reviewing code changes
  (`clojurecraft-workflow`), used unmodified.
- The local inference client (`clojurecraft-inference`) has its own small HTTP/1.1
  implementation over Unix sockets; it adds no HTTP or TLS dependency.

A machine-checked inventory (`cargo deny check licenses` with an allowlist) belongs in CI.

## Design reference: ArtCraft

The visual direction (near-black surfaces, white hairlines, 3 px radii on controls, square large
surfaces, a single accent colour) follows the egui client in `storytold/artcraft`
(`artcraft-native/`, MIT OR Apache-2.0, inspected at commit `58f5944`).

- **No ArtCraft code or assets are copied.** `clojurecraft-app/src/theme.rs` is an independent
  implementation with its own colour values.
- If later work copies code from `artcraft-native` (for example window chrome or widgets), keep its
  copyright notice and license text (MIT or Apache-2.0, at our option) and note it in a
  `THIRD_PARTY_NOTICES` file.
- The ArtCraft name, wordmark, logos, and brand blue are trademarks and must not be used.
- Fonts used by ArtCraft (Inter, Archivo, Geist Mono) are OFL-1.1; if adopted, obtain them from
  their upstream projects and ship their OFL texts.
