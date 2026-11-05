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

Anything else is rejected by default. Custom Hugging Face repositories must resolve to a GGUF
whose `general.architecture` is in this table.

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
