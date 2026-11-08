# Findings: building a small library with ClojureCraft from Claude Code (2026-11-08)

Task: `TEST_PROMPT.md` (workout-log analysis library: parsing, ISO weeks, volume per session/week,
personal records with Epley e1RM, week streak, weight drops, plain-text weekly report). Driven by
Claude Code (Opus 5.5) through the MCP server and `.claude/skills/clojurecraft/SKILL.md`.
Result: in `/home/jonas/Dev/Test-ClojureCraft`, eight generated functions, all passing their checks, plus a hand-written
facade namespace and 34 `clojure.test` assertions (all green).

## Outcome per function

| Function | `generate` jobs until pass | What went wrong before it passed |
|---|---|---|
| `volume-per-session` | 1 | — |
| `volume-per-week` | 1 | — |
| `weight-drops` | 1 (1 repair) | — |
| `longest-week-streak` | 2 | wrong `group-by` destructuring, off-by-one, `reduce` result discarded |
| `personal-records` | 3 | sequences instead of numbers, then unbalanced delimiters |
| `parse-workout-log` | 3 | unbalanced delimiters, invented `str/contains?` (×3), broken line splitting |
| `iso-week` | 8 | delimiters, recursion/StackOverflow, `-weekday` symbol, `/` instead of `quot`, mis-nested `let` |
| `weekly-report` → `week-block` | 11 | delimiters (most attempts), destructuring, ignored sort step, unused computed lines |

About 29 jobs (~100 model attempts) for 8 functions. Functions with a simple shape (one `group-by`,
then a sum or max) passed on the first job. Every function with date arithmetic, several helpers, or
string assembly needed many rounds of rewriting the spec.

## Where the local model fails

1. **Unbalanced delimiters (the main failure).** About half of all failed attempts were parse
   failures (`( is never closed`, `unmatched )`, `[ is closed by )`). The repair loop practically
   never fixes them: the same parse error came back 3–4 times in a row (`line 5: unmatched )` ×4 in
   one job). A parse failure stops validation, so the model gets no other feedback. It got worse as
   functions grew (more than about 30 lines, string-heavy code). Whether code snippets in the steps
   make it worse is unclear: prose-only steps failed the same way.
2. **Made-up API.** `clojure.string/contains?` (should be `includes?`) appeared in 3 attempts in a row
   even though the lint error named it.
3. **Turning prose arithmetic into code.** `ordinal - wd` became the symbol `-weekday`. It used `/`
   instead of `quot`, so a Ratio reached `format` (`IllegalFormatConversionException`). It got signs
   wrong (`(- a b 4)` for `a - b + 4`).
4. **Collections and destructuring.** `filter` over a `group-by` map treated as if it held values.
   `(:week %)` used on map entries. `keys` on a filtered seq. It computed values and then dropped them
   (`exercise-lines` bound but unused, `reduce` result followed by a literal `0`, a `let` placed
   after the result). The linter flagged some of these as *warnings* ("bound but never used"), but
   nothing acted on them.
5. **Ignored steps.** It left out "sort drops by :exercise" even though both the step and an example
   required it.
6. **Structure rule.** Rejecting a top-level `(comment …)` form cost a whole attempt for no benefit.
7. **The repair loop doesn't converge** (as already noted in `model-compatibility.md`). Repairs
   mostly kept the failing approach. A fresh `generate` with the *same* spec sometimes passed where
   the repairs had not. This points to sampling variance, not to the spec.

What worked: specifying the code as **small named `defn-` helpers, each with a worked value** ("for
2026 3 2 it returns 61"), and making the public function a single expression. Every hard function
passed only after being rewritten this way. At that point the steps were close to pseudocode, so
Claude was effectively writing the code in prose.

## Limitations of the app / MCP

- **One function, one argument, one namespace.** A library of 8 functions became 8 namespaces
  (`parse-workout-log.core` …). Generated functions can't call each other, so `leap-year?` and the
  Sakamoto weekday formula exist twice (`iso-week`, `longest-week-streak`). Composition (`with-weeks`,
  `weekly-report`, `analyze`) and the whole test file had to be written by hand.
- **The report had to be split by hand** (`week-block` generated, join written by hand) because the
  model couldn't produce the whole renderer.
- **No project export.** There's no `deps.edn` and no test file built from the examples. Running the
  tests locally needed the runtime jars from `~/.local/share/clojurecraft/runtimes/clojure/1.12.6/`
  (no `clojure` CLI on the machine).
- **Only sequential jobs.** One model job at a time, and no queue: 8 functions meant 29 rounds of
  `generate` → `status(wait)`, each needing Claude to come back.
- **Verbose responses.** `create_function` echoes the full spec back. `status`/`result` repeat full
  inputs and expected values for every failure (`diagnostics_as_above` helps only partly). This uses
  Claude context for nothing.
- **Escaping.** Descriptions containing `"\n\n"` were stored with literal escape noise
  (`\"\\n\\n\"`) because of EDN inside JSON inside prose.
- **No fallback for code Claude writes itself.** The skill forbids editing generated code, and there is
  no "validate this code I wrote" tool. When the local model fails repeatedly, the only option left
  is more spec rewriting.
- **Not evaluated:** `generate_test_cases` was skipped to save time, so the quality of
  model-proposed edge cases is unknown.

## Cost observation

For this task, going through ClojureCraft almost certainly used **more** Anthropic tokens than writing
the ~200 lines directly. That was driven by `iso-week` and the report (19 jobs together, each with a
spec rewrite plus a `status`/`result` round trip). The simple functions were cheap: one
`create_function` and two short calls each. The savings exist only when first-job success is high.
