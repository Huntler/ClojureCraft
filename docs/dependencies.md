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
- `seccompiler` (Apache-2.0, from Firecracker) and `landlock` (MIT OR Apache-2.0) are used
  unmodified.

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
