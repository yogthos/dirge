# Backpressured plugin — trigger + system-prompt injection.
#
# Trigger: the user mentions "backpressure"/"backpressured" in a prompt
# (matching the original's explicit-opt-in philosophy), or runs
# /backpressured. While engaged, the four-phase loop discipline is
# appended to the system prompt on every run.

# The loop discipline. Distilled from lucasfcosta/backpressured's goal
# skill, adapted to dirge: the independent reviewer is a `task` subagent,
# checks run through the normal bash tool (so long suites stream and the
# permission engine still applies).
(def bp-discipline-text ```
# The backpressured loop

You are the producer's backpressure of LAST resort: replace yourself with
machines that say "no" first. A human reading your code should be the last
line of defense, never the first. Every "no" a machine can produce — a
failing test, a type error, a lint rule, a reviewer's objection — must be
produced BY THE MACHINE, before you hand anything back.

Do NOT stop when the code merely "works." Drive the goal through the full
loop below and stop ONLY when every acceptance criterion and every quality
check passes — or when you are genuinely blocked and can name exactly what
blocks you.

## First: pin down "done"
State the acceptance criteria up front; if the user didn't list them, infer
and state them before starting. Quality checks (lint, tests with new
behavior covered, typecheck/build, independent review) apply on top, every
iteration.

## Phase 1 — Plan, then get it reviewed (before any code)
Write a LIGHTWEIGHT plan: the approach and architecture, not implementation
details. Then dispatch an INDEPENDENT reviewer via the `task` tool to judge
whether the approach is sound; iterate until it approves. A wrong approach
caught here is free; caught after 300 lines it is not.

## Phase 2 — Implement in a loop, checks EVERY iteration
After each patch, before writing the next one, run the checks that apply
(see "Project checks" below):
- lint clean
- tests green, and NEW behavior covered by new tests
- typecheck / build clean
- dispatch the `task` reviewer on the diff
Run checks EVERY iteration, not batched at the end — confronting the
consumer's expectations often is what catches issues early. Keep them fast
per patch (lint + targeted tests on touched code), but never skip them, and
run the FULL suite before leaving the loop. Do not write the next patch
until everything you ran is green.

## Phase 3 — Before you call it done
1. Run the FULL test suite + lint (and full benchmarks if perf-sensitive).
   Cheapest signal first.
2. Then RUN IT FOR REAL: exercise each acceptance criterion through the
   running system (curl the API; a real browser for UI). Green automated
   tests are necessary, not sufficient — they exercise the code, not the
   running system.
3. Dispatch the `task` reviewer over the WHOLE changeset, not just the last
   patch.

## Phase 4 — Ship
If the project ships via PRs: open the PR, then watch it until it lands (CI
to completion, late review comments, merge conflicts). A PR opening is not
"done" — done is merged clean, or blocked on something only a human can
resolve. If the project merges from a local branch instead, follow that.

## Independent reviewer (not optional)
Reviews must come from a `task` subagent that did NOT write the code — your
own re-read shares the blind spots you wrote with. Spawn it with the `task`
tool, hand it the diff (or plan), and have it judge against:
- Correctness & edge cases; needless complexity or duplication; is the new
  behavior actually covered by a test?
- Type design: booleans/optionals standing in for real state, union cases
  not exhausted, unsafe casts, primitive `string`/`number` ids.
Act on the real findings; push back on wrong ones. Apply this to the plan
(Phase 1), each iteration (Phase 2), and the whole changeset (Phase 3).
```)

(defn bp-discipline []
  (string bp-discipline-text "\n\n" (bp-detect-checks)))

# Keyword opt-in: engage the loop when the user explicitly invokes it.
# Does NOT rewrite the prompt — the user's message flows to the model
# normally; we only flip the mode + (below) inject the discipline.
(defn on-prompt [ctx]
  (def prompt (string/ascii-lower (or (ctx :prompt) "")))
  (when (and (not bp-active)
             (or (string/find "backpressured" prompt)
                 (string/find "backpressure" prompt)))
    (set bp-active true)
    (harness/notify
      "▶ backpressured loop engaged — plan → implement → verify → ship, checks every iteration"
      :info))
  nil)

# While engaged, append the discipline to every run's system prompt.
(defn before-agent-start [ctx]
  (when bp-active
    (harness/append-system-prompt (bp-discipline)))
  nil)

# Honor `plugins.backpressured.auto_start` from config.json: engage the
# loop from the first prompt without needing the keyword. Captured at
# LOAD time, while harness-plugin-config holds THIS plugin's settings.
(let [cfg (harness/plugin-config)]
  (when (and cfg (get cfg :auto-start))
    (set bp-active true)))
