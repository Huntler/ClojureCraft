---
name: clojurecraft
description: Generate small pure Clojure functions with the ClojureCraft MCP server instead of writing them by hand. Use when a task needs a Clojure data transformation (one input value in, one value out, no side effects) such as grouping, summing, filtering, reshaping maps or vectors, or parsing strings, and the clojurecraft MCP tools (mcp__clojurecraft__*) are available.
---

# Generating Clojure code with ClojureCraft

ClojureCraft writes, runs, and repairs Clojure functions with a local model in a sandbox. You
specify **what** the function must do (description, steps, EDN examples, tests); it returns code
that passes those checks. Spend your effort on a precise specification, not on writing or
debugging the code yourself.

**Fits:** pure functions of one argument returning one value, using `clojure.core` (plus
`clojure.string` and `clojure.set` by default). **Does not fit:** I/O, Java interop, state,
macros, multi-function namespaces. Write those yourself.

## Tools

| Tool | Purpose |
|---|---|
| `list_functions` | Saved functions (id, name, description). Reuse one instead of creating a duplicate. |
| `create_function` | New function from `name`, `description`, `steps`, `examples`, optional `contract`. Returns its `id` and `readiness`. |
| `create_functions` | Several at once: `functions` is a list of such specifications (all or nothing). |
| `get_function` | Specification and test cases, with case ids for review. |
| `update_function` | Change fields. `steps` and `examples` replace the whole list. `contract: null` removes the contract. |
| `generate_test_cases` | Background job: the model proposes edge cases (`source`: `examples` or `specification`). |
| `review_test_cases` | `actions` (accept/reject/reopen/remove/edit by case id) and `add` (your own cases, accepted by default). All or nothing. |
| `generate` | Background job: write, validate, and repair up to 3 times. `function_ids` instead of `function_id` generates several; jobs for a busy model are **queued** and run one after another. `mode: "fix"` repairs the current code from its failing checks. |
| `run_tests` | Background: check the current code against examples and accepted tests. |
| `status` | Progress, the last job's outcome, check results with failures, and a `next` hint. Pass `wait_seconds` (≤ 300) to wait for background work (queued jobs included) instead of polling. With `function_ids`: one short line per function (`passed`, `first_failure`, `last_job`). Failures are compact (first of each kind; long values replaced by a `difference` such as `at [0 :total]: expected 5 but got 4`); `verbose: true` shows all values. |
| `result` | Final code with `namespace`, `function`, `passes_all_checks`, and any code waiting for acceptance. |
| `submit_code` | Your own code becomes the function's code (history keeps the old one) and is checked like generated code. Then `status` with `wait_seconds`. Only under the budget rule below. |
| `accept_code` | Use code that is held for review (a fix, or code needing a new namespace). |
| `export_project` | Write functions (`function_ids`) into an absolute `directory` as a Clojure project: `src/` code, one `clojure.test` file per function from its examples and accepted tests, `deps.edn`. Returns `require` forms and commands to run the tests (`clojure -X:test`, or a `java` command using ClojureCraft's own runtime). Differing files are kept unless `overwrite: true`. |
| `cancel` | Stop the running model job and empty the queue. |

All EDN values (`input`, `expected`, keys, defaults) are **strings**: `"[{:id 1 :n 2}]"`, `"{1 5}"`.

## Workflow

1. **Specify.** Call `create_function` (or `create_functions` for several) with:
   - a clear `name` (it becomes the function name in kebab-case, in namespace `<name>.core`);
   - a `description` that states the input shape, the output shape, and the rules;
   - `steps` for the transformation, in order;
   - **at least 2–3 `examples`** covering the normal case and one tricky case;
   - a `contract` when output types or keys matter, for example
     `{"input": {"type": "vector"}, "output": {"type": "map"}, "ordering": "unordered"}`.

   If `readiness.blocking` is not empty, fix it with `update_function`.
2. **Edge cases (recommended).** Call `generate_test_cases`, then `status` with
   `wait_seconds: 120`, then `get_function`. Review each proposed case **critically**: the
   local model sometimes writes nonsense expectations (for example `"defined"`). Accept cases
   whose `expected` is right. Correct wrong ones with `edit` (giving `expected`) and then
   `accept`. Reject the rest. Add your own cases for behaviour you care about (empty input,
   nil, duplicates).
3. **Generate.** Call `generate`, then `status` with `wait_seconds: 300`. Repeat `status` while
   `working` is `true`. For a library, create all functions first, then call `generate` and
   `status` once each with `function_ids`; open a single function's `status` only when it fails.
4. **Read the outcome.**
   - `checks.passed: true`: call `result` and use `code` as is. Mention `namespace` and
     `function` when you insert it.
   - `last_job.outcome: "all_attempts_failed"` or failing `checks`: read the `failures`
     (input, expected, actual, diagnostics). Usually the specification is ambiguous or an
     expected value is wrong. Fix it with `update_function` or `review_test_cases`, then
     `generate` again. Use `generate` with `mode: "fix"` only after `run_tests` shows
     failures of existing code.
   - `last_job.outcome: "code_waiting_for_acceptance"`: inspect `result.waiting_for_acceptance`,
     then call `accept_code` if it is acceptable.
   - `model_unavailable` or `runtime_unavailable`: the person must finish setup in the
     ClojureCraft window. Tell them, and do not retry in a loop.
5. Follow the `next` hint in `status` when unsure what to do.

## Rules

- One model job runs at a time; `generate` queues behind it. Other model jobs
  (`generate_test_cases`) are refused while the model is busy: wait with `status`, then retry.
- Do not patch generated code by hand. Change the specification or the tests and regenerate,
  so the checks stay the source of truth.
- **Budget rule:** after 2 failed `generate` jobs on one function, simplify or split its
  specification. After 3, write the code yourself and send it with `submit_code` (keep the
  `namespace` and `function` from `result`), then fix it from the `status` failures. Do not keep
  rewriting the specification.
- Use the code from `result`, not from earlier messages. It is the version that was checked.
- When the functions belong in a project, use `export_project` instead of copying code and
  writing tests by hand. Write only the glue (a facade namespace) yourself, and run the returned
  test command once at the end.
- The ClojureCraft window must stay open (not minimized). It starts automatically if it is not
  running.
