# ClojureCraft implementation plan

## Mandatory instructions for the coding agent

**Never refer to this plan, its milestones, or task instructions from inline comments or docstrings.** Comments and docstrings must explain the software itself.

This plan is an implementation artifact. It must not be included in the finished application, packaged resources, or published product documentation.

Implement milestones sequentially. **After completing each milestone, stop and provide:**

1. A summary of the implemented behavior.
2. Validation performed and its results.
3. Known limitations and unresolved decisions.
4. A suggested Git commit message.
5. Any concrete information needed before the next milestone.

**Never create Git commits, tags, pushes, or releases yourself.** The user handles commits and authorizes continuation. The release workflow implemented in the final milestone may publish releases automatically when the user pushes an appropriate version tag.

Do not begin the next milestone until the user instructs you to continue. If a milestone cannot meet its acceptance criteria, report it as incomplete.

Keep changes scoped to the current milestone. Preserve existing user changes. Use meaningful automated tests for contracts, state transitions, security boundaries, and integration behavior. Do not add tests that merely duplicate trivial implementation details.

## 1. Product definition

ClojureCraft is a native Rust desktop application for macOS and Linux that helps engineers create, understand, test, and optimize small, pure Clojure functions for data transformation.

Users can read Clojure but may not know its standard library and idioms. They describe transformations through:

- A high-level description.
- Input and expected-output examples.
- Optional explicit input/output contracts.
- Ordered natural-language transformation steps.

A local coding model, run through llama.cpp, generates the function using the specification and retrieved Clojure documentation. ClojureCraft validates it, runs examples, and attempts repairs when necessary.

The application must work offline after the inference runtime, models, Clojure runtime resources, and documentation have been installed.

### First-release capabilities

- ArtCraft-inspired native interface.
- Support for Apple Silicon macOS and x86_64 Linux.
- Saved-function library and search.
- Structured EDN examples and contracts.
- Ordered editable transformation steps.
- First-run setup that downloads the llama.cpp inference runtime from within the app.
- Hardware detection (GPU VRAM, or system RAM for CPU inference) and a curated list of coding-optimized models that fit the detected hardware.
- Local model download and selection, including custom Hugging Face GGUF repositories.
- Local documentation retrieval.
- Generation with automatic validation and three repair retries.
- Reviewable code changes and version history.
- User-triggered edge-case testing.
- Saved-function outputs as example input sources.
- Intermediate data inspection.
- Exportable functions and tests.
- Performance comparison as the final function-development stage.
- GitHub Releases distribution and application auto-update.

### Explicit exclusions

- General repository editing or autonomous software development.
- Functions with intended filesystem, network, or other external side effects.
- Visual pipeline composition.
- Generation languages other than Clojure.
- Cloud inference as a required service.
- Inference runtimes other than llama.cpp.
- Model formats other than GGUF.
- Arbitrary support for every Hugging Face model or every GGUF architecture.
- Windows and Intel macOS (later targets unless separately authorized).

## 2. Architecture and operating constraints

### Application structure

Use a Rust workspace with clear boundaries for:

- Native UI and application state.
- Domain models and workflow orchestration.
- Local storage and migrations.
- Hardware detection, inference-runtime installation, model management, and inference.
- Documentation ingestion and retrieval.
- Clojure execution and validation.
- Export and benchmarking.
- Updates and release integration.

Keep platform-specific code (sandboxing, hardware detection, credential storage, update installation, packaging) behind narrow traits with one implementation per supported platform.

An egui/eframe interface is the initial architectural preference, consistent with the ArtCraft reference. Confirm feasibility on both platforms in Milestone 1, including Wayland and X11 on Linux.

Keep long-running work off the UI thread. Use cancellable background jobs and typed progress events.

### Supported platforms

The first release supports:

| Platform | Architecture | Inference backends |
| --- | --- | --- |
| macOS | Apple Silicon (arm64) | Metal, CPU |
| Linux | x86_64 | CUDA (NVIDIA), Vulkan, ROCm/HIP (AMD) where practical, CPU |

Record a concrete minimum macOS version and a minimum Linux baseline (glibc version and reference distributions, e.g. a current Ubuntu LTS and Fedora release) during the feasibility milestone.

Linux aarch64, Windows, and Intel macOS are later targets unless separately authorized. Do not claim support for any platform or backend without packaging and validation.

### Inference runtime: llama.cpp

llama.cpp is the only inference runtime for the first release.

- ClojureCraft does not bundle llama.cpp. On first start (and later from settings), the app downloads a prebuilt llama.cpp release matching the platform and the selected backend.
- Pin each supported llama.cpp release in an application-shipped runtime manifest containing the release tag, per-platform/backend asset names, download URLs, and expected SHA-256 hashes. Verify every download against that manifest before activation. Never execute an unverified binary.
- Prefer official upstream release assets. If no suitable upstream asset exists for a supported platform/backend, document the gap and decide in Milestone 1 whether ClojureCraft builds and publishes its own pinned build through its release pipeline.
- Run llama.cpp as a separate child process (`llama-server`) bound to the loopback interface on an ephemeral port with a per-session API key, or over a more restricted local transport if feasible. Do not expose it beyond the local machine.
- Detect the available backend (Metal, CUDA, Vulkan, ROCm, CPU), recommend the best one, and let the user override it. Fall back to CPU with a clear explanation when no GPU backend is usable.
- Keep the active runtime working until a replacement runtime has been downloaded, verified, and passed a smoke test. Support rollback to the previous runtime version.
- Updating the pinned llama.cpp version is a deliberate application release change, not an automatic background update of an arbitrary upstream build.

### Models

GGUF is the only supported model format. Support only the architectures validated against the pinned llama.cpp release and recorded in the model compatibility matrix.

On first start, after the runtime is installed, ClojureCraft presents a curated list of coding-optimized models:

- The catalog is a versioned data file shipped with the application (updatable through application releases), listing for each entry: display name, Hugging Face repository, pinned revision, GGUF file(s) per quantization, file hashes, license, context length, architecture, and estimated memory requirements per quantization and context size.
- Include several models across size tiers so that useful options exist for small GPUs, mid-range GPUs, large-memory Apple Silicon machines, and CPU-only systems. Candidates should be selected for code-generation quality at the time of implementation (for example, current Qwen Coder, Devstral, or comparable code-specialized models); confirm availability, licenses, and llama.cpp compatibility rather than assuming them.
- Filter and rank catalog entries using detected hardware: show which model/quantization combinations fit the memory budget, mark the recommended default, and still allow the user to see (but be warned about) entries that exceed it.
- Every catalog entry must have been loaded and run through a short generation smoke test with the pinned llama.cpp release before it is shipped.

Users may additionally install other GGUF models from a Hugging Face repository ID (Milestone 4), subject to the same compatibility checks.

### Hardware detection and memory budget

Detect, without requiring elevated privileges:

- Total and available system RAM.
- Apple Silicon unified memory and a usable fraction for GPU inference.
- NVIDIA GPUs and VRAM (NVML or `nvidia-smi` when present).
- AMD GPUs and VRAM (sysfs/ROCm tooling when present).
- Vulkan-capable devices and their reported device-local memory.
- CPU core count and relevant instruction-set features for CPU inference.

Derive a default memory budget from the detected hardware: GPU VRAM for fully offloaded inference, unified memory on Apple Silicon, or a conservative fraction of system RAM for CPU-only inference. Support partial GPU offload when VRAM is insufficient for a full model, with the split estimated and explained.

The budget is configurable and covers model weights, KV cache, and inference buffers. All memory figures are estimates and must be labeled as estimates. Hardware detection failures must degrade to manual configuration, never block the app.

Reference machine: Apple Silicon Mac with 36 GB unified memory, approximately 18 GB reserved for inference; models around 30B parameters (in suitable quantizations) must remain eligible there.

Load one generative model at a time. Small retrieval models may coexist within the budget.

### Trust boundaries

Generated code is untrusted. Model-generated requests and retrieved documents must not gain direct access to arbitrary application actions.

Execute Clojure in a separate restricted process. A prompt requesting purity, linting, or symbol filtering alone is not a security boundary. Each supported platform needs its own OS-level restriction mechanism (e.g. the macOS sandbox on macOS; namespaces, seccomp, Landlock, and resource limits on Linux).

Downloaded llama.cpp binaries and model files are third-party artifacts: verify them against pinned hashes before use, run the inference server with the minimum privileges practical, and never execute scripts from model repositories.

Keep user examples and generated code local. Network access is limited to explicit downloads (inference runtime, models, documentation), release checks, and application updates.

## Milestone 1 — Feasibility and technical foundation

### Work

1. Inspect the workspace and applicable repository instructions.
2. Inspect the ArtCraft launcher implementation and identify reusable visual patterns. Record attribution obligations for any reused assets or code.
3. Establish the Rust workspace, formatting, linting, error handling, and application entry point.
4. Open a minimal native desktop window on macOS and Linux (Wayland and X11).
5. Evaluate and select:
   - UI framework.
   - llama.cpp integration method (`llama-server` child process versus library bindings), with a justified choice.
   - Source of llama.cpp binaries per platform/backend and how they are pinned and verified.
   - Initial supported GGUF architectures.
   - Clojure execution runtime.
   - Execution restriction mechanism for macOS and for Linux.
   - Hardware detection approach per platform and backend.
   - Storage engine and retrieval index approach.
6. Demonstrate downloading, verifying, and launching a pinned llama.cpp build on each platform, and running a small GGUF model through it with at least one GPU backend and the CPU backend.
7. Validate the restricted execution approach on both platforms against file access, network access, process spawning, runaway computation, and excessive output.
8. Determine how Java interop and dynamic evaluation will be restricted.
9. Identify whether a JVM Clojure runtime must be bundled or downloaded per platform and how its version will be pinned.
10. Define the minimum supported macOS version, the Linux baseline, the Linux packaging format(s) (e.g. AppImage and/or tarball), and the initial signing/update strategy for each platform.

### Deliverables

- Runnable Rust application skeleton.
- Concise architectural decisions and supported-platform matrix.
- Explicit model compatibility matrix tied to the pinned llama.cpp release.
- Draft curated model catalog with size tiers and memory estimates.
- Working execution-isolation proof of concept on both platforms.
- Dependency and license inventory, including llama.cpp and catalog model licenses.

### Acceptance criteria

- The application builds and opens on the target Mac and on a reference Linux machine.
- llama.cpp can be downloaded, verified, launched, and used for generation from a Rust prototype on both platforms.
- Execution restrictions are demonstrated locally on both platforms.
- Any blocker involving runtime distribution, GPU backends, isolation, or memory is reported before proceeding.

Suggested commit: `chore: establish ClojureCraft application foundation`

## Milestone 2 — Domain model, persistence, and history

### Work

Define versioned domain records for:

- Function identity, name, and description.
- Input/output contract.
- Ordered steps with stable identifiers.
- Multiple example pairs.
- Direct example inputs and function-output references.
- Generated and manually edited code.
- Validation reports and accepted edge-case tests.
- Documentation provenance.
- Model identity, file hash, quantization, llama.cpp runtime version and backend, and generation settings.
- Immutable function revisions.
- Benchmark configurations and results.

Implement local storage, schema migrations, autosave, and recovery from interrupted writes. Use platform-appropriate data locations (e.g. `~/Library/Application Support` on macOS; XDG base directories on Linux).

Use a working draft plus immutable revisions. Track specification, code, tests, and validation fingerprints independently.

Changing relevant content must mark previous results stale. Retain stale results for inspection without presenting them as current passes.

Keep llama.cpp binaries, model files, and indexes outside the source repository and exported function files.

### Acceptance criteria

- Drafts survive application restart.
- Revision restoration does not delete later history.
- Interrupted writes do not corrupt saved functions.
- Tests verify migrations, reference integrity, and stale-result behavior.
- Secrets are excluded from function history and exports.

Suggested commit: `feat: add persistent function specifications and revision history`

## Milestone 3 — Main workspace and specification editing

### Work

Implement the approved three-column layout:

- Left: searchable saved functions, new function, knowledge sources, settings.
- Center: description, examples, contract, ordered transformation steps.
- Right: implementation and result/steps/sources views.

Add:

- Function creation, renaming, duplication, and deletion with recovery or confirmation.
- Resizable panes.
- Multiline step fields, reorder controls, add/remove actions.
- EDN syntax highlighting and useful parse diagnostics.
- Multiple example tabs.
- Expandable example editors and nested-data inspector.
- Keyboard navigation, visible focus, accessible contrast, and scalable text.
- Autosave status and unsaved-error feedback.

Use consistent native styling inspired by ArtCraft. Avoid copying branding that implies affiliation. Verify rendering, fonts, HiDPI scaling, and keyboard shortcuts on both macOS and Linux.

### Acceptance criteria

- An engineer can create and reopen a complete specification.
- Large nested examples remain usable.
- Invalid EDN does not erase user text.
- Reordering steps preserves their identities and associated history.
- The layout remains usable at the documented minimum window size on both platforms.

Suggested commit: `feat: build the function specification workspace`

## Milestone 4 — Inference runtime installation, hardware detection, and model management

### Work

#### First-run setup

Implement a first-run setup flow, also reachable later from settings:

1. Detect platform, GPU devices, VRAM, unified memory, system RAM, and CPU features. Show the results and allow manual correction.
2. Recommend an inference backend and let the user choose another supported one.
3. Download the pinned llama.cpp release for the chosen platform/backend with progress, cancellation, and safe resume; verify it against the runtime manifest; extract it to the application data directory; and run a version and backend smoke test before activation.
4. Present the curated coding-model catalog filtered and ranked for the detected hardware, with the recommended default highlighted and estimated memory use, disk size, context length, and license shown for each option.
5. Let the user select one or more models to download, and skip setup entirely to configure things later.

The app must remain usable for specification editing while setup is incomplete, with clear indications of what is missing.

#### Model installation

Implement model installation from the curated catalog and from an arbitrary Hugging Face repository ID:

1. Resolve repository metadata and files.
2. Detect GGUF files and architecture compatibility with the pinned llama.cpp release.
3. Let the user select a compatible quantization/file.
4. Handle multi-file (split) GGUF models when supported; otherwise clearly reject them.
5. Pin downloads to a resolved repository revision (catalog entries are pre-pinned).
6. Show size, disk requirements, and estimated runtime memory against the current budget, including partial-offload estimates.
7. Download to temporary files with progress, cancellation, and safe resume.
8. Verify integrity using catalog hashes, available trusted metadata, and recorded hashes.
9. Atomically activate completed installations.
10. Detect incomplete, changed, or corrupt files.
11. Support model and runtime removal and disk-usage reporting.

Add settings for the inference backend, llama.cpp runtime version, active generator, context limit, GPU layer offload, and memory budget.

Handle gated/private repository authentication with OS-backed credential storage (Keychain on macOS; Secret Service/libsecret on Linux, with a clear message if no secret service is available). Never log tokens. Do not automatically accept licenses or access agreements on the user’s behalf.

Do not execute downloaded repository scripts.

### Acceptance criteria

- First-run setup installs a verified llama.cpp runtime and at least one catalog model on both platforms.
- Hardware detection produces sensible recommendations on Apple Silicon, an NVIDIA GPU Linux machine, and a CPU-only Linux machine, and falls back to manual configuration when detection fails.
- Tampered or mismatched runtime and model downloads are rejected.
- Supported public models can be downloaded and selected.
- Interrupted downloads resume safely.
- Unsupported repositories are explained before large downloads.
- Authentication failures and insufficient disk space are recoverable.
- Automated tests use mock metadata, mock hardware reports, and tiny fixture files, not real runtime or model downloads.

Suggested commit: `feat: add llama.cpp runtime setup, hardware-aware model catalog, and model installation`

## Milestone 5 — Local inference service

### Work

Implement a replaceable inference interface, with llama.cpp (`llama-server` or the integration chosen in Milestone 1) as its production implementation, supporting:

- llama.cpp process lifecycle: start, health checks, restart after crash, and clean shutdown with the application.
- Model load/unload.
- Streaming output.
- Cancellation and timeouts.
- Structured responses (using llama.cpp grammar/JSON-schema constrained output where reliable).
- Context accounting and explicit truncation policy.
- Typed load and generation errors, including backend initialization failures and out-of-memory conditions.
- Supported chat templates and tool-call parsing.

Keep interpretation, clarification, retrieval planning, generation, and explanations as separate roles that initially reuse one model.

Expose only application-defined tools with validated arguments and bounded execution. Do not expose shell or arbitrary file access.

Avoid silently discarding original specification content to fit context. Report oversized requests and offer a concrete remedy.

Add lifecycle progress and memory-pressure handling. Clearly distinguish runtime-missing, downloaded, loading, ready, and failed model states. When a GPU load fails for memory reasons, offer reduced offload, a smaller context, or a smaller quantization.

### Acceptance criteria

- A compatible catalog model runs locally on the target Mac (Metal) and on Linux with at least one GPU backend and the CPU backend.
- Streaming and cancellation keep the interface responsive.
- A cancelled request cannot overwrite newer results.
- A crashed llama.cpp process is detected and reported, and the service can recover.
- Mock inference tests cover malformed outputs and lifecycle failures.
- Record manual peak-memory observations using a representative model and context on each platform/backend validated.

Suggested commit: `feat: integrate cancellable local llama.cpp inference`

## Milestone 6 — Versioned local Clojure knowledge retrieval

### Work

Build a documentation collection from:

- Official Clojure reference and guides.
- API documentation or runtime metadata for the supported version.
- Relevant standard-library namespaces.

Preserve source URL, document title, symbol, namespace, version, revision, and license/attribution information.

Chunk API entries by symbol and guides by coherent sections. Keep examples with surrounding explanations.

Implement:

- Exact symbol lookup.
- Full-text search.
- Embedding search (embedding models run through llama.cpp or another local, platform-portable mechanism selected in Milestone 1).
- Merged ranking.
- Optional reranking only if measured benefit justifies the overhead.

Add knowledge-source settings with download/index progress, installed version, update action, and recovery from failed updates.

Update indexes atomically. Retain a working collection until its replacement is ready.

Treat retrieved material as reference data, not application instructions.

### Acceptance criteria

- Retrieval works offline.
- Queries can find both known symbols and natural-language transformation concepts.
- Results respect the configured Clojure version.
- Sources remain inspectable in the UI.
- A small curated retrieval fixture verifies relevant results and metadata preservation.

Suggested commit: `feat: add offline Clojure documentation retrieval`

## Milestone 7 — Contracts, interpretation, and generation

### Work

Implement an optional structured contract editor covering:

- Input and output shapes.
- Required and optional keys.
- Value types.
- Missing-value and `nil` behavior.
- Ordering requirements.
- Allowed dependencies.

Allow the model to propose contracts, but require visible acceptance before treating proposals as user requirements.

Interpret the specification and surface material contradictions or ambiguities as focused questions. Preserve the original user text alongside the interpretation.

Assemble structured generation requests containing:

- Original specification.
- Accepted contract.
- All applicable examples and tests.
- Retrieved excerpts and provenance.
- Runtime and dependency constraints.
- Output schema.

Generate one public transformation function plus declared namespace requirements. Require explicit approval before expanding supported dependencies.

Show generated code, explanations, and a mapping from logical steps to code regions where feasible.

### Acceptance criteria

- Generation uses all specification components.
- Contract proposals do not silently modify intent.
- Output is validated structurally before execution.
- Unsupported dependencies and malformed responses produce actionable feedback.
- Saved revisions identify the model, llama.cpp runtime version, and documentation used.

Suggested commit: `feat: generate Clojure functions from structured specifications`

## Milestone 8 — Automatic validation and repair

### Work

Implement the validation sequence:

1. Parse generated code.
2. Check allowed code structure and dependencies.
3. Run linting.
4. Load and execute within the restricted runtime.
5. Run every accepted example and applicable contract check.
6. Compare EDN values structurally.
7. Record diagnostics, timing, and bounded output.

Define equality behavior explicitly, including numeric comparisons, unordered collections, and realization limits for lazy sequences.

Treat timeout, process crash, forbidden access, malformed output, and resource-limit violations as failures.

On failure, send the model:

- Original specification and accepted contract.
- Candidate code.
- Relevant documentation.
- Concrete diagnostics.
- Expected and actual values when available.

Allow **one initial generation plus three repair attempts**. Do not modify user examples, contracts, or accepted tests during repair.

Implement progress states and cancellation. Preserve all candidates and validation reports.

### Acceptance criteria

- A passing candidate stops the loop immediately.
- A fourth failed attempt ends the workflow with visible diagnostics.
- Repair never weakens accepted tests.
- Editing during execution prevents stale results from replacing current work.
- Automated tests use mock model responses and real lightweight execution fixtures on both platforms.

Suggested commit: `feat: validate generated functions with bounded automatic repair`

## Milestone 9 — Review, manual editing, and edge-case tests

### Work

Add code differences, revision comparison, restoration, and manual editing.

Regeneration must not silently overwrite manual changes. Present proposed replacements for review.

Enable **Test edge cases** after current initial checks pass. Clicking it records the current function as accepted for deeper testing.

Generate proposed cases from the specification and contract, including relevant empty, missing, duplicate, boundary, ordering, and nested-data cases.

Distinguish:

- Defined cases that can be run.
- Ambiguous cases requiring clarification.
- Proposed expectations awaiting acceptance.

Do not derive expected output solely by executing the candidate function.

Let engineers accept, edit, reject, and save cases. Keep model-generated tests visibly identified until accepted.

Add **Fix failing cases**, using the existing three-retry loop and all accepted tests. Show the resulting code changes before replacing an accepted version.

### Acceptance criteria

- Edge-case discovery never silently changes accepted code.
- Ambiguities cannot become hidden requirements.
- Accepted cases become regression tests.
- Manual edits invalidate relevant validation status.
- History supports recovery of the previously accepted function.

Suggested commit: `feat: add reviewable edge-case testing and code revisions`

## Milestone 10 — Function input references and intermediate inspection

### Work

Allow an example input to reference:

- A saved function.
- A pinned function revision.
- A selected upstream example.

Execute that upstream example and use its output as the downstream example input.

Show provenance, preview, expanded data inspection, and an explicit refresh action when newer revisions exist.

Validate missing references, upstream failures, incompatible contracts, and dependency cycles.

Do not automatically insert upstream calls into exported implementations. References supply example data; visual composition is outside this release.

Implement intermediate step inspection using executed instrumentation or validated stage functions. Never present model-predicted data as executed results.

When optimized code fuses steps, explain the mapping. If a trustworthy intermediate value cannot be obtained, show it as unavailable.

### Acceptance criteria

- References remain stable across restarts.
- Upstream changes do not silently alter downstream examples.
- Refresh marks downstream validation stale.
- Cycles and failed upstream execution are reported clearly.
- Intermediate previews are bounded and accurately labeled.

Suggested commit: `feat: support referenced inputs and executed step previews`

## Milestone 11 — Export and complete workflow polish

### Work

Export:

- Clojure source with namespace declaration and docstring.
- Required standard-library imports.
- `clojure.test` tests for accepted examples and edge cases.
- Optional EDN specification and provenance manifest.

Ensure the exported function accepts data directly, including when its example input came from another function.

Use valid Clojure naming and escaping. Prevent accidental inclusion of credentials, local absolute paths, internal prompts, or this plan.

Complete empty, loading, cancelled, failed, stale, and successful UI states, including setup states for a missing or failed llama.cpp runtime and missing models. Add local troubleshooting diagnostics (including detected hardware, backend, and runtime version) with deliberate export and redaction.

Verify offline operation across the complete workflow on both platforms.

### Acceptance criteria

- Exported source and tests run in a clean supported Clojure environment.
- Exported code has no dependency on ClojureCraft.
- All primary workflows are keyboard accessible.
- A documented manual end-to-end check passes on macOS and Linux.

Suggested commit: `feat: export standalone Clojure functions and regression tests`

## Milestone 12 — Performance comparison

This is the final function-development stage of the first release.

### Work

Enable comparison after the current implementation passes accepted tests.

Allow the engineer to:

- Preserve a baseline.
- Request an optimized candidate or provide another implementation.
- Supply representative benchmark inputs and size variants.
- Review code differences and optimization explanations.
- Choose the preferred implementation.

Require every candidate to pass the same accepted tests and contracts before benchmarking.

Benchmark using a pinned runtime and consistent settings:

- Warmup phase.
- Repeated measured runs.
- Full result realization where required.
- Work performed inside the measurement boundary.
- Median, spread, sample count, and input size.
- Clear separation of startup time and steady-state execution.
- Allocation metrics only when reliably supported.

Run candidates under equivalent restrictions. Bound duration and memory. Avoid treating uncertain or noisy differences as proven improvements. Do not run benchmarks while a model is generating, and record whether the inference server was loaded during measurement.

Record runtime version, platform, candidate hashes, input hashes, and benchmark settings. Keep benchmark data separate from inference latency. Do not compare results across machines or platforms as if they were equivalent.

Do not claim semantic equivalence merely because finite tests pass.

### Acceptance criteria

- Incorrect candidates cannot receive a performance winner label.
- Repeated comparisons retain the baseline.
- Lazy results do not produce misleading measurements.
- Results can be reproduced locally from saved configurations.
- Choosing an optimized candidate preserves history and reruns validation.

Suggested commit: `feat: compare validated Clojure implementations with local benchmarks`

## Milestone 13 — GitHub CI/CD, auto-release, auto-update, and README

### Work

Implement the final distribution pipeline.

#### GitHub CI

Run on pull requests and relevant branch updates, on macOS (Apple Silicon) and Linux (x86_64) runners:

- Rust formatting and linting.
- Unit and integration tests.
- UI/build checks appropriate to each supported platform.
- Lightweight Clojure validation fixtures, including sandbox restriction tests per platform.
- Storage migration and export checks.
- Model-management, hardware-detection, and runtime-installation tests using mocked services and fixtures.
- Validation of the llama.cpp runtime manifest and model catalog data files (schema, hash format, consistency).
- Dependency/license checks where practical.

**Do not run inference tests in GitHub Actions.** Do not download real llama.cpp binaries or model weights, or require inference credentials or GPUs. Keep local inference evaluations separately documented and explicitly invoked.

#### Release pipeline

Use protected version-tag pushes as the automatic release trigger.

The workflow must:

1. Verify the tag matches application metadata.
2. Run required checks.
3. Build Apple Silicon macOS and x86_64 Linux artifacts.
4. Sign and notarize macOS application artifacts.
5. Produce the macOS installer/archive, the Linux package(s) selected in Milestone 1, and updater-compatible payloads for each platform.
6. Generate checksums and release metadata.
7. Sign update metadata or payloads with a dedicated updater key (this is the authenticity mechanism for Linux artifacts as well).
8. Publish a GitHub Release only after all required artifacts for all supported platforms succeed.

Keep signing, notarization, and updater secrets in GitHub’s protected secret storage. Use least-privilege workflow permissions.

Treat GitHub Releases as the distribution channel for the first release. Handle public-release access appropriately if development occurs in a private repository.

#### Application auto-update

Implement on both platforms:

- Automatic background checks with configurable frequency.
- A manual **Check for updates** action.
- Stable release channel; exclude drafts and prereleases by default.
- Clear current/available version information.
- Download progress, cancellation, and retry.
- Signature verification before installation.
- Update installation through a supported mechanism per platform (e.g. app bundle replacement on macOS; self-replacing AppImage or equivalent on Linux). When ClojureCraft was installed by a system package manager, defer to that package manager and say so instead of self-updating.
- A controlled restart prompt after work is saved.
- Preservation of user data, models, the llama.cpp runtime, documentation, and history.
- Migration of the llama.cpp runtime when a new release pins a different version, keeping the previous runtime until the new one is verified.
- Clear recovery behavior when download or installation fails.

Never replace the running application during active generation, testing, or benchmarking.

Do not rely on checksums alone as authenticity protection. Reject invalid signatures, incompatible artifacts (including wrong platform or architecture), and unintended downgrades.

#### README and supporting documentation

Write a concise README covering:

- Product purpose and screenshot.
- Supported platforms, GPU backends, and requirements.
- Installation and auto-update on macOS and Linux.
- First-run setup: llama.cpp runtime download and backend selection.
- First-function walkthrough.
- Curated models, custom model installation, and compatibility.
- Hardware detection and memory-budget guidance (GPU, unified memory, CPU-only).
- Documentation retrieval and offline behavior.
- Validation, repair, edge cases, and benchmarking.
- Export.
- Privacy and local data locations per platform.
- Build and contribution instructions.
- Local inference evaluation instructions.
- Known limitations and troubleshooting (including GPU driver issues).
- Required licenses and attribution (including llama.cpp and model licenses).

Exclude this implementation plan and milestone references.

### Acceptance criteria

- CI passes on both platforms without model weights, llama.cpp downloads, or inference access.
- Release workflows are validated without publishing an unauthorized release.
- Required secrets and maintainer setup are documented.
- A signed update is tested between two local or staging versions on each supported platform.
- Tampered updates are rejected.
- Updates preserve user data, models, and the installed runtime, and wait for active jobs to finish.
- README instructions are exercised on a clean supported environment for each platform.

If credentials or signing access are unavailable, finish all independent work and clearly identify the remaining release-validation blocker. Do not describe an untested release path as complete.

Suggested commit: `ci: add signed macOS and Linux releases, automatic updates, and product documentation`

## First-release completion criteria

The release is ready when an engineer, on Apple Silicon macOS or x86_64 Linux, can:

1. Install ClojureCraft.
2. Download and activate the llama.cpp inference runtime from within the app.
3. Choose and download a recommended coding model suited to their GPU VRAM or system RAM.
4. Install the local Clojure knowledge collection.
5. Define a transformation with examples, contracts, and steps.
6. Generate and automatically repair a function.
7. Inspect code, documentation, and execution results.
8. Accept and run edge-case tests.
9. Reuse saved function outputs as example inputs.
10. Compare validated implementations for performance.
11. Export standalone Clojure source and tests.
12. Reopen the application with work intact.
13. Receive a verified application update through GitHub Releases.

All required milestone checks must pass on both supported platforms, and any remaining limitations must be documented accurately.
