---
name: clojurecraft
description: Generate small pure Clojure functions with the ClojureCraft MCP server instead of writing them by hand. Use when a task needs a Clojure data transformation (one input value in, one value out, no side effects) such as grouping, summing, filtering, reshaping maps or vectors, or parsing strings, and the clojurecraft MCP tools (mcp__clojurecraft__*) are available.
---

# Generating Clojure code with ClojureCraft

ClojureCraft writes, runs, tests, and repairs Clojure functions with a **local** model. Its work
costs nothing on your side; every call you make and every token you read does. So:

- **Your job is the specification.** ClojureCraft does the coding, testing, and repairing.
- **Minimize calls.** Target: one call per batch of functions, plus one call per correction.
  Think the specification through *before* the first call; do not discover it by trial.
- **Never poll.** Always pass `wait_seconds: 300`. Every tool that changes or starts work takes it
  and answers with the finished `status`.
- **Read only what you need.** Do not call `get_function`, `result`, `list_functions`, or
  `status` to "check" things: replies already contain the outcome, failures, and code.

**Fits:** pure functions over `clojure.core` (plus `clojure.string`, `clojure.set`), one
argument (several in an open project via `arguments`). **Does not fit:** I/O, Java interop,
state, macros. Write those yourself.

All EDN values are **strings**: `"[{:id 1 :n 2}]"`. `function_id` accepts an id, the name, or
(in a project) `namespace/function`; you never need to look ids up.

## 1. Specify and generate: one call

```json
create_function {
  "name": "sum-by-id",
  "description": "Input: vector of maps {:id int, :n int}. Output: map id -> sum of :n. Empty input -> {}.",
  "steps": ["group the maps by :id", "sum :n within each group"],
  "examples": [
    {"input": "[{:id 1 :n 2} {:id 1 :n 3} {:id 2 :n 1}]", "expected": "{1 5 2 1}"},
    {"input": "[]", "expected": "{}"},
    {"input": "[{:id 1 :n 0}]", "expected": "{1 0}"}
  ],
  "contract": {"input": {"type": "vector"}, "output": {"type": "map"}},
  "generate": true,
  "wait_seconds": 300
}
```

A good first specification is what saves round trips:

- **Description** states input shape, output shape, and every rule, including edge behaviour
  (empty, nil, missing keys, duplicates, ties, ordering).
- **Steps**: the transformation in order, one operation each.
- **Examples: 3–5, one case each**: the normal case and every edge case you care about. These
  are the tests. Put edge cases here instead of reviewing generated ones later.
- **Contract** when types, required keys, or ordering matter (`"ordering": "unordered"` when
  order is irrelevant, so correct code is not failed for it).
- `expected` may be `#throws ExceptionClass` when the call must throw.
- **Keep each function small**: one grouping, sort, aggregation, or one token format. Split
  anything bigger into several functions and compose them in your own glue code.

**Several functions: still one call.** `create_function` with `functions: [ {...}, {...} ]`,
`generate: true`, `wait_seconds: 300`. Generations queue and run one after another; the reply has
one line per function (`passed`, `first_failure`). If the batch is still `working` after 300 s,
call `status` with `function_ids` and `wait_seconds: 300` once more.

**Open project:** add `namespace` (created when missing) and use the function's symbol as `name`;
with `arguments` ≠ 1, each example input is a vector of the arguments. The code is written into
the project's `.clj` file automatically.

## 2. Read the reply

- **`result` present** (single function): all checks passed. `result.code` is the code with its
  `namespace` and `function`. Use it unchanged. Done.
- **Batch, all `passed: true`:** get the code without reading it: in an open project it is
  already in the files; otherwise `export_project` into the target directory writes `src/`,
  `clojure.test` files, and `deps.edn` (differing files are kept unless `overwrite`).
- **`readiness.blocking` or `generate_error`:** the specification is incomplete; fix it (step 3).
- **Failures** (`checks.failures`, `first_failure`, `last_job.outcome: "all_attempts_failed"`):
  go to step 3.
- **`code_waiting_for_acceptance`:** `accept_code` if `passed` is true.
- **`model_unavailable` / `runtime_unavailable`:** the person must finish setup in the
  ClojureCraft window. Tell them and stop; do not retry.

## 3. Correct: one call per correction

ClojureCraft already ran up to five repair attempts inside the job. A remaining failure almost
always means the **specification** is ambiguous or an `expected` value is wrong, not that it
needs another try. Read the failure, decide which, and fix it in a single call that also
regenerates:

```json
update_function {"function_id": "sum-by-id", "description": "...", "examples": [...],
                 "generate": true, "wait_seconds": 300}
```

`steps` and `examples` replace the whole list, so send all of them. Add `verbose: true` to a
`status` only when the compact failure does not tell you enough.

**Budget per function:** first call plus at most 2 corrections. After that, split the function or
write the code yourself and send it with `submit_code` (`wait_seconds: 300`; keep `namespace` and
`function` from the earlier reply) so ClojureCraft still runs every check.

## Optional: generated edge cases

Use only for tricky functions where you cannot list the edge cases yourself (costs extra calls).
`generate_test_cases` with `wait_seconds: 120` returns `tests_to_review`. Answer in **one**
`review_test_cases` call: accept correct cases, `edit` wrong `expected` values and accept them,
reject the rest, `add` your own, and pass `generate: true, wait_seconds: 300`. The local model
sometimes writes wrong expectations; check each one.

## Rules

- Do not patch generated code by hand; change the specification and regenerate, so the checks
  stay the source of truth.
- Use code only from the latest reply.
- Run the test command `export_project` returns once at the end, not per function.
- The ClojureCraft window must stay open. It starts automatically when not running.
