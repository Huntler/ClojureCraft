# Model compatibility

Runtime: llama.cpp `b12138` (pinned in `resources/runtime-manifest.toml`). Format: GGUF only.
Curated entries live in `resources/model-catalog.toml`, pinned to a repository commit, file size,
and SHA-256. The catalog's `status` applies to a model (architecture, chat template, and tensor
layout); each quantization file is still hash-verified on download.

All memory figures are **estimates**: weights + KV cache (f16, per-token figure × context) + a
compute allowance. Measured observations are in `docs/platform-validation.md`.

## Architectures

| GGUF architecture | Models | Status with b12138 |
| --- | --- | --- |
| `qwen2` | Qwen2.5-Coder 1.5B / 7B / 14B Instruct | 1.5B and 7B smoke-tested (CUDA, Vulkan, CPU); 14B unverified |
| `qwen3moe` | Qwen3-Coder 30B-A3B Instruct | supported upstream; unverified here |
| `mistral3` | Devstral Small 2 24B Instruct (2512) | supported upstream; unverified here |
| `qwen3next` | Qwen3-Coder-Next 80B-A3B | supported upstream; unverified here; split GGUF (4 files) |
| `mellum` | JetBrains Mellum2.1 12B-A2.5B Thinking (`JetBrains/Mellum2.1-12B-A2.5B-Thinking-GGUF`, not in the catalog) | loads and generates with b12138 CUDA; see below |

Anything else is rejected. The list ships as `[compatibility] architectures` in
`resources/model-catalog.toml`; catalog parsing fails if a catalog entry uses an architecture that
is not listed.

## Embedding model

| GGUF architecture | Model | Status with b12138 |
| --- | --- | --- |
| `qwen3` (pooling `last`) | Qwen3-Embedding-0.6B Q8_0, pinned in `resources/knowledge-manifest.toml` | smoke-tested on Linux: CUDA (whole collection, 1,032 inputs, 26 s) and CPU (`--device none`, queries); Metal unverified |

It is used only for documentation retrieval, from its own model store, and is never offered as a
generator. Vectors are 1,024-dimensional and normalised by the application.

## Tool calls

llama.cpp parses tool calls from the model's chat template. With b12138, Qwen2.5-Coder 7B and
1.5B Instruct report template support for tools but write the call as ordinary text (a fenced
`<tools>` block or bare JSON), so no tool call is recognised and the text arrives as the answer.
Application features must therefore not depend on tool calling with the current smoke-tested
models: retrieval is driven by the application, with structured output (JSON-schema constrained
and validated) where the model has to choose. JSON-schema output works with both models.

## Mellum validation (2026-11-06)

`JetBrains/Mellum2.1-12B-A2.5B-Thinking-GGUF` Q4_K_M (commit `4159d684`, 7.5 GiB, Apache-2.0),
llama.cpp b12138 CUDA on the RTX 3070 8 GB, context 8192, 20 of 28 layers on the GPU (the
application's offload estimate for a 7.2 GiB budget):

- Loads in 2.2 s; 52 tok/s; GPU peak 5.7 GiB; server peak RSS 7.9 GiB. Header: 28 layers,
  131k context, KV estimate 56 KiB per token.
- It is a reasoning model: llama-server returns its thinking in `reasoning_content` and the answer
  in `content`. The standard prompt took 1,450–5,100 completion tokens (about 30–100 s) per answer.
- Code quality on the standard `summarize-orders` prompt: 0 of 3 samples ran in the sandbox (two
  compile errors from invalid map destructuring, one wrong call shape). Qwen2.5-Coder 7B passes
  the same prompt.
- With every layer requested on the GPU it fails with a CUDA out-of-memory error; it needs
  partial offload on 8 GB cards.

The architecture is therefore allowed for installation from its repository, but the model is not
offered in the curated catalog.

## Generation with the specification assistant (2026-11-06)

`clojurecraft-devtool generate` with llama.cpp b12138 CUDA, context 8192, on the four sample
specifications (single runs; quality varies between seeds):

- Qwen2.5-Coder 7B Q4_K_M: every request returns schema-valid JSON once prose fields are bounded
  (see `docs/architecture.md`). Generation 2.6–3.2 s, explanation 4.8–6.2 s, interpretation
  4–6 s, contract proposal 4 s. Code: `distinct-tags` correct; `order-totals` wrong in three runs
  (a nil customer key, nested maps, a reduce over grouped values) and correct once in the app
  after two questions had been answered; `top-scorers` returns a list
  where a vector is expected; `normalize-headers` rejected by the structural check (used
  `clojure.walk` without requiring it). Repairs are needed for most specifications.
- Mellum2.1 12B-A2.5B Thinking Q4_K_M (Vulkan build, partial offload): reasoning used up the
  2,500-token answer budget; the retry with the reasoning allowance answered after about 4
  minutes in total, with code that escaped its line breaks twice (now unescaped automatically).

## Validation and repair (2026-11-06)

`clojurecraft-devtool generate` with validation in the sandbox (llama.cpp b12138 CUDA, context
8192, documentation on, Qwen2.5-Coder 7B Q4_K_M); each sandboxed validation run took 0.7 s.

- `top-scorers`, `normalize-keys`, `nested-tags`: passed every check on the first attempt.
- `order-totals`, 13 runs in total: passed on the first attempt in 3; in the other 10 every repair
  failed. Diagnostics were precise (path and values of the first difference; for exceptions the
  line and the `clojure.core` function), but the model returned its failing approach nearly
  unchanged (the same `reduce` over grouped values). Raising the repair temperature to 0.5 and
  telling the model that an attempt failed exactly like the previous one did not change this in
  6 runs. Repairs need a stronger model than the 7B tier.
- Mellum2.1 12B-A2.5B Thinking (Vulkan, partial offload): in the application, generation attempts
  ran out of their answer budget while reasoning (cut off at 8,192 tokens with a 16,384-token
  context) and counted as failed attempts, at about 2–4 minutes each.

## Harness changes and measurements (2026-11-08)

Apple M3 Pro (36 GB), llama.cpp b12138 Metal, Qwen2.5-Coder 7B Q8_0, context 8192,
`clojurecraft-devtool generate`. Changes, each measured on the sample specifications and the
workout-log parser from the MCP example:

- **Speed.** Raw decoding is 17 tok/s (memory-bandwidth bound); the JSON grammar costs nothing.
  `--spec-type ngram-mod` (draft tokens from n-grams already seen, no draft model) gives 33 tok/s
  on repairs and up to 115 tok/s when an answer repeats an earlier one, with identical output.
  Step references are capped at one per step (the model listed 18 for 6 steps), assumptions at
  four, and the output limit is 1800 tokens (runaway answers that redefined `let`, `mapv`, … until
  the 2500-token limit cost up to 8 minutes). A six-attempt job went from 4–14 min to 2.5–3 min.
- **Plain-text answers.** Code is no longer a JSON string: the answer is `PLAN:`, a
  ```` ```clojure ```` block, `STEPS:` lines, and `ASSUMPTIONS:`, still grammar-constrained. The
  grammar requires `(ns …)` first and only `defn`/`defn-`/`def`/`declare` after it, and allows no
  comments (the model rambled in `;;` comments until cut off).
- **Plan first.** The answer starts with a short trace of example 1 before the code.
- **Checks.** Expected outputs that contradict the accepted contract block generation before
  any attempt. A new lint error reports a form whose value a `let`/`fn`/`do` body throws away (a
  misplaced parenthesis, or `(assoc m …)` written as if it changed `m`). Thrown errors quote the
  source line. Cleanup adds a missing `clojure.string`/`clojure.set` require, makes extra public
  functions private, and replaces invented `clojure.string` names (`contains?` → `includes?`).
- **Idioms.** A short, keyword-chosen list of one-line idioms (descending sort with tie-breaker,
  `cond->` for optional keys, `parse-long`/`parse-double`, …) is sent with the specification.

| Specification | Before | After |
| --- | --- | --- |
| `order-totals` | first attempt passed in 3 of 13 runs | attempt 1 (1 of 1 run) |
| `top-scorers` | attempt 1 (single run) | failed 6/6 until the idioms were added, then attempt 1 (2 of 2 runs) |
| `normalize-keys` | attempt 1 | attempt 1 |
| `nested-tags` | attempt 1 | attempt 2–3 (2 runs) |
| workout-log parser (whole log, or one line with four optional fields) | 0 of 6 | 0 of 6 in every variant (10 runs), now in about 3 min instead of 4–14 |

Single-token parsers split out of it (`"3@10"` → `{:sets 3 :reps 10}`, `"7.5kg"` → `7.5`)
passed on the first attempt in 3 of 4 runs. The 7B model cannot write the line parser: it keeps treating `"3@10"` as one number or as two
tokens. Splitting such parsers into one function per field format is the workaround (see the
skill); a larger model (Qwen3-Coder 30B-A3B, ~3B active parameters) is the next thing to measure.

## Models from other Hugging Face repositories

1. The repository id (`owner/name`) and optional branch, tag, or commit are validated, and the
   revision is resolved to a commit that every later download uses.
2. GGUF files are grouped into options: one file per option, or all parts of a split model
   (`-0000N-of-0000M.gguf`). Vision projector files (`mmproj`) are not offered. A repository
   without GGUF files is explained (for example safetensors-only repositories).
3. Before downloading, the first file's header is read with HTTP range requests (2 MiB, growing
   only when the metadata is larger). The option is rejected when the architecture is missing or
   not in the list above, when split parts are missing, when the header's `split.count` disagrees
   with the file names, when a file has no SHA-256 in the Hub's LFS metadata, or when a
   single-file option holds fewer than two tensors per declared layer (an add-on such as a
   speculative-decoding head published next to the model, not a model of its own). Each option
   shows its file path, since a repository can hold several files with the same quantization.
4. Memory is estimated from the file sizes and the header's layer and attention geometry
   (`block_count`, `attention.head_count_kv`, key/value lengths), labelled as an estimate, and
   compared with the budget, including the partial-offload split.
5. Files are verified against the Hub's SHA-256; the hash and commit are recorded in `model.json`.

Gated or private repositories need a Hugging Face token (stored in the OS credential store).
ClojureCraft never accepts a repository's license or access conditions; the user must do that on
huggingface.co.

## Curated catalog

| Model | Quant | File size | Est. total @ 16k ctx | Typical fit | License | Status |
| --- | --- | --- | --- | --- | --- | --- |
| Qwen2.5-Coder 1.5B Instruct | Q4_K_M | 1.1 GB | 1.9 GiB | any machine, CPU-only | Apache-2.0 | smoke-tested |
| Qwen2.5-Coder 1.5B Instruct | Q8_0 | 1.9 GB | 2.6 GiB | any machine | Apache-2.0 | same architecture as Q4_K_M; file not yet run |
| Qwen2.5-Coder 7B Instruct | Q4_K_M | 4.7 GB | 6.0 GiB | 8 GB GPU | Apache-2.0 | smoke-tested |
| Qwen2.5-Coder 7B Instruct | Q6_K / Q8_0 | 6.3 / 8.1 GB | 7.4 / 9.1 GiB | 10–12 GB GPU | Apache-2.0 | same architecture as Q4_K_M; files not yet run |
| Qwen2.5-Coder 14B Instruct | Q4_K_M / Q6_K | 9.0 / 12.1 GB | 12.4 / 15.4 GiB | 12–16 GB GPU | Apache-2.0 | unverified |
| Devstral Small 2 24B | Q4_K_M / Q6_K | 14.3 / 19.3 GB | 17.3 / 22.0 GiB | 24 GB GPU, 32 GB+ Mac | Apache-2.0* | unverified |
| Qwen3-Coder 30B-A3B | Q3_K_M / IQ4_XS / Q4_K_M | 14.7 / 16.4 / 18.0 GB | 15.7 / 17.3 / 18.8 GiB | 24 GB GPU, 36 GB Mac, partial offload | Apache-2.0 | unverified |
| Qwen3-Coder-Next 80B-A3B | Q4_K_M | 48.4 GB (4 files) | 46.0 GiB | 64 GB+ Mac, large GPU + RAM | Apache-2.0 | unverified |

\* License of the upstream `mistralai` weights; the GGUF repository's own terms must be
confirmed before release.

The reference Mac budget (18 GiB of 36 GB unified memory) keeps a ~30B-class model eligible:
Qwen3-Coder 30B-A3B IQ4_XS or Q3_K_M at 8k–16k context, and Devstral Small 2 Q4_K_M. Because
Qwen3-Coder 30B-A3B activates only ~3B parameters per token, it also remains usable with partial
offload on 8 GB GPUs and on CPU-only machines with enough RAM.

Excluded on purpose: `Qwen2.5-Coder-3B` (Qwen Research license restricts commercial use) and
community fine-tunes/merges (provenance and license unclear).

## Selection rule

1. Detect hardware and pick a backend; derive the default budget.
2. Rank every catalog quantization by fit (full device → partial offload → too large), then by
   quality rank, then by larger quantization.
3. Recommend the best `smoke-tested` entry that fits fully, else the best that fits with partial
   offload. Unverified entries are shown but never recommended.
