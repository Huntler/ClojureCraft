# Improvements: making local generation actually save Anthropic tokens

Goal: Claude specifies the code, the local model writes it, and the sandbox verifies it. Each
accepted function should cost Claude fewer tokens than writing it directly, and Anthropic's servers
should carry less load. `findings.md` shows that today this holds only for simple functions. The
deciding factor is **first-job pass rate**, because every failed job costs a Claude round trip plus
a spec rewrite.

## 1. Reliability of the local model (largest gains first)

1. **Make delimiters impossible to break.** This was the most common failure in `findings.md`.
   - Constrained decoding: llama-server accepts GBNF grammars. A grammar for balanced
     `()[]{}`, string literals (with escapes), regex literals and comments guarantees code that
     parses. Generation already runs through `clojurecraft-inference`, so this is local only.
   - Fallback without a grammar: a deterministic fixer in parinfer *indent mode* (infer closing
     delimiters from indentation) before the parse check. Count a fixed parse as a normal attempt.
2. **Best-of-N instead of sequential repairs.** Failures look stochastic (a fresh job with an
   unchanged spec sometimes passed after 4 failed repairs), and validation takes about 0.7 s. So
   sample N = 3–5 candidates for the first generation (different seeds), validate them all, and
   keep the first that passes. Repair only the best candidate. This makes local compute do the work
   that Claude round trips do today.
3. **Fix made-up vars automatically.** On `Unable to resolve symbol: str/contains?`, look up the
   nearest real var in the documentation index (`clojure.string/includes?`). Put
   "did you mean …" into the repair prompt, or rewrite it directly when the match is unambiguous.
4. **Feed lint warnings into repairs.** "Bound but never used" is a strong hint of discarded work
   (`exercise-lines`, a `reduce` result). When tests fail, include these warnings in the repair
   request.
5. **Strip what doesn't matter.** Drop top-level `(comment …)` forms and redundant
   `[clojure.core :as core]` requires instead of failing the attempt.
6. **Smaller repairs.** When the failing call's stack trace points into one `defn-`, send back only
   that form plus its failing examples and splice the answer in. Small model, small context.

## 2. Decomposition built into the product

The approach that worked in the test was helper-by-helper specs with worked values. Make that a
feature instead of a workaround Claude types out in prose:

- **Helpers in the spec:** `steps` (or a new `helpers` field) declare `name`, `args`,
  `description`, and examples such as `(day-of-year 2026 3 2) => 61`. Generate and validate each
  helper on its own (small functions pass reliably), then generate the public function with the
  helpers already fixed. This gives local diagnostics and short prompts.
- **Multiple arguments:** helpers need them anyway, and examples can be `input` vectors applied
  with `apply`.
- **Function dependencies / library namespace:** let a function `require` other saved functions
  (`iso-week` reused by `longest-week-streak`), or group functions into one library namespace. This
  removes duplicated code and hand-written glue.

## 3. A lean MCP surface (fewer Claude tokens per function)

- **`run_expression` (optional):** evaluate a form against saved functions in the sandbox, for
  end-to-end checks without a local `clojure` CLI.
- Accept EDN as structured fields where possible, so text descriptions don't pick up escape noise.

## 4. Skill updates (no code changes needed)

Add to `.claude/skills/clojurecraft/SKILL.md`:
- Write `steps` as small named helpers, each with one worked value. Make the public function a
  single expression.
- Spell out integer division as `quot` and write subtraction in words or as `(- a b)`, never
  `a - b`.
- Say how to destructure `group-by` results (`[[k vs] …]`).
- Split renderers or reports into per-item functions and do the joining in a hand-written facade.
- **Budget rule:** after 2 failed jobs on one function, simplify or split it. After 3, write it
  yourself (with `validate_code` once that exists). Don't keep rewriting the spec.
- Expect pure-library tasks to end with a hand-written facade namespace plus tests, until
  `export_project` exists.

## 5. Model choice

`model-compatibility.md` already notes that the 7B tier rarely repairs. Options to measure: a
larger coder model only for the repair role, or best-of-N with the 7B model (item 1.2). Pick by
pass rate per wall-clock minute on the benchmark below, not by single runs.

## 6. Plan and how to measure it

1. **Baseline (now):** turn this test into a benchmark (`clojurecraft-devtool`). Use the 8 functions
   from `TEST_PROMPT.md` plus the existing `order-totals` set. Record per function: jobs, attempts,
   failure category (parse / lint / runtime / wrong value / contract), wall time, and Claude tokens
   (MCP request and response sizes as a proxy).
2. **Reliability (items 1.1–1.5):** grammar or parinfer first, then best-of-N. Target: first-job
   pass rate ≥ 80 % on the benchmark (today 3 of 8 functions).
3. **Lean MCP + skill (sections 3–4):** batches, compact replies, `validate_code`,
   `export_project`, the skill budget rule. Target: Claude tokens per accepted function below the
   cost of writing it directly, for every benchmark function, worst case included.
4. **Decomposition (section 2):** helpers with their own examples, multiple arguments, dependencies
   between functions. Target: TEST_PROMPT-sized libraries with no hand-written code except the
   facade.
5. Run the benchmark again after each step, and track the results over time in
   `model-compatibility.md`.
