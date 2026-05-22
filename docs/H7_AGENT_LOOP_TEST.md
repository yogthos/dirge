# Phase 4.5h-7 — agent_loop end-to-end test runbook

After the 4.5h-6 cutover, `provider::spawn_runner` routes every
streaming run through the new agent_loop path (port of pi's
`runAgentLoop`). The 845 default-build tests verify the
component-level behavior against mocked streams + tools, but
the path has never seen actual LLM traffic. This document is
the manual smoke test you run after `cargo build` to validate
the new path against real providers before declaring 4.5 done.

The scenarios are ordered roughly by complexity. Each:
- declares the **Goal** (what behavior is being verified)
- lists **Setup** (env / model / inputs)
- spells out the **Steps**
- describes **Expected** observable behavior
- lists **Fail modes** to watch for, with phrasing that maps
  back to a specific phase if it breaks

If anything diverges from the expected behavior, that's a
bug — file a focused commit per fix, cite the failing
scenario's number.

---

## Prerequisites

- `cargo build` succeeds on `main`.
- At least one provider API key set as env var. The runbook
  uses `ANTHROPIC_API_KEY` as the canonical example; substitute
  your provider:
  - `ANTHROPIC_API_KEY` (Claude)
  - `OPENAI_API_KEY` (gpt-4*)
  - `OPENROUTER_API_KEY` (multi-vendor)
  - `DEEPSEEK_API_KEY` (DeepSeek)
  - `GEMINI_API_KEY` (Gemini)
- Two terminals: one for `dirge`, one for `tail -F` on the
  log (optional but useful for tracing warnings).
- A scratch directory for filesystem scenarios. The dirge
  repo itself works.

Run each scenario against **at least two different providers**
to catch provider-specific quirks. Anthropic + OpenAI is the
minimum useful pairing; OpenRouter is a third good option
because it exposes the most providers under one key.

---

## Scenario 1 — simple text Q&A (no tools)

**Goal**: verify the basic streaming path. AgentEvent::Token
events fire incrementally; final response renders correctly.

**Setup**: any provider; default model.

**Steps**:
1. `cargo run --release`.
2. At the prompt, type: `What is 2+2? Reply with just the number.`
3. Press Enter.

**Expected**:
- Response streams in (you see "4" appear, possibly preceded by
  brief reasoning tokens depending on the model).
- The response chamber closes cleanly.
- No error banner.
- Total turns counter shows `1`.

**Fail modes**:
- **No streaming, response appears all at once** → bridge's
  Token delta computation (`bridge.rs::text_delta_emits_token_chunks`)
  not firing in production. Check that
  `MessageUpdate { phase: TextDelta }` events reach the bridge.
- **`Error` banner with "rig stream call failed"** → the
  `rig_stream_factory.rs::invoke_one_stream` error path. Check
  the surrounding `tracing::warn` for the underlying provider
  message.
- **Hangs forever, no response** → chunk timeout (`rig_stream.rs`)
  not firing OR the stream never produced an event. After
  `stream_chunk_timeout_secs` (default 300) you should see
  a `stream chunk timed out after 300s` Error event. If you
  see nothing → the timeout integration is broken (4.5h-3 regression).

---

## Scenario 2 — single tool call (`read`)

**Goal**: verify the tool dispatch path. ToolCall + ToolStarted +
ToolResult events fire; the model uses the result to compose a
follow-up answer.

**Setup**: any provider; cwd containing the dirge repo (so
`README.md` exists).

**Steps**:
1. `cargo run --release`.
2. Type: `Read README.md and tell me what dirge is in one sentence.`
3. Press Enter.

**Expected**:
- A "read" tool call appears in the UI (collapsed by default).
- The tool result shows the file contents.
- The model then streams its summary sentence.
- Total turns: `2` (one assistant tool call, one assistant text).

**Fail modes**:
- **Tool call attempted but errors with "Tool ... not found"** →
  `build_loop_tools` isn't producing the expected registry, or
  the LoopTool's `name()` returns something different than what
  rig sent in the tool call. Check
  `RigToolAdapter::new` cached the name correctly.
- **Tool runs but the second LLM call doesn't happen** →
  `run.rs::run_loop` inner loop's `has_more_tool_calls` logic.
  A successful tool with `terminate: None` should keep the loop
  going. If you see only one assistant turn, suspect the
  `should_terminate_tool_batch` invariant.
- **Tool runs, second call happens, but result text is empty** →
  bridge's `flatten_content` lost the text payload. Verify the
  `ToolResult` AgentEvent carries the actual file contents.
- **Permission prompt doesn't appear (read against `/etc/passwd`)** →
  Permission threading regression. The PermCheck is built into
  `ReadTool` at construction; if it's not firing, build_loop_tools
  may have lost the PermCheck arg.

---

## Scenario 3 — multi-turn tool sequence

**Goal**: verify the loop iterates across multiple tool calls.
Sequential dispatch (because `edit`/`bash` are Sequential per
build_loop_tools).

**Setup**: any provider; cwd is a temp directory.

**Steps**:
1. `mkdir -p /tmp/dirge_h7 && cd /tmp/dirge_h7`
2. `cargo run --release` (from the dirge repo, but cwd set above).
   Actually easier: `cd /tmp/dirge_h7 && /path/to/dirge/target/release/dirge`.
3. Type:
   ```
   Create a file hello.txt with the content "world", then read
   it back to confirm, then delete it via bash.
   ```
4. Press Enter.
5. Approve each permission prompt (Y).

**Expected** (sequential ordering):
- Three tool calls fire IN ORDER: `write` → `read` → `bash`.
- Each completes before the next dispatches (no parallel
  interleaving in the chamber).
- Final assistant message confirms the sequence completed.
- File `/tmp/dirge_h7/hello.txt` does NOT exist after the run.

**Fail modes**:
- **Two tools fire concurrently** → `build_loop_tools`' Sequential
  tagging is wrong. `WriteTool`, `EditTool`, `BashTool`,
  `ApplyPatchTool` must declare `with_execution_mode(Sequential)`.
  Phase 3's umbrella dispatcher forces the whole batch sequential
  on any Sequential tool's inclusion.
- **Permission prompt for write but not for bash** → permission
  config / sandbox interaction. Re-check the cwd is a writable
  scratch dir and the dirge config has `permission_mode: ask`
  (default).
- **The model's "delete via bash" step uses `rm` but the file
  remains** → BashTool not actually invoking; check the
  ToolResult text for the bash output.

---

## Scenario 4 — mid-run interjection (cancel + restart)

**Goal**: verify the `interject_tx → signal.cancel()` bridge in
`LoopRunner::into_agent_runner`. After the cutover, mid-run
typed messages cancel the run and restart with the new prompt
(legacy behavior preserved). Pi-style continuous interjection
is NOT yet wired — see scenario notes.

**Setup**: any provider; pick a slow model (Claude 3.5 Sonnet
or gpt-4 with reasoning) so the run is long enough to interject.

**Steps**:
1. `cargo run --release`.
2. Type a long-running prompt:
   ```
   Read README.md, then write a 1000-word essay analyzing
   dirge's architecture in detail.
   ```
3. Press Enter. Wait until the model is mid-essay (you see
   tokens streaming).
4. WHILE the run is in flight, type:
   ```
   Actually just summarize in one paragraph.
   ```
   and press Enter.

**Expected**:
- The first run cancels cleanly (`Interjected` UI handler fires,
  or just `Done` per the new path).
- A second run kicks off with the new prompt; the UI shows
  `<you> Actually just summarize in one paragraph.` and the
  agent responds with a short summary.
- No "interject_tx send failed" or panics in logs.

**Fail modes**:
- **First run keeps going, second prompt queued but ignored** →
  the `LoopRunner::into_agent_runner` bridge isn't translating
  `interject_tx.send(())` to `signal.cancel()`. Inspect the bridge
  task's spawn in `integration.rs::into_agent_runner`.
- **First run cancels but the second prompt never spawns** → UI
  drain handler in `ui/mod.rs:2660` not firing. Check the
  interjection_queue drain path.
- **Tools keep running in background after cancel** → the bug
  we fixed in #4 (orphaned inner task). If this regresses, check
  that `spawn_loop_runner` is still using `tokio::join!` not
  nested `tokio::spawn`.

**Note**: this scenario tests legacy cancel-and-restart behavior.
Pi-style continuous interjection (model observes mid-run message
in the same run) is item #10 from the code review — deferred.
When that lands, this scenario should be revised to verify the
in-flight injection instead of restart.

---

## Scenario 5 — rate-limit recovery

**Goal**: verify `retrying_stream_fn` (phase 4.5g) actually
retries on transient errors. Honoring Retry-After.

**Setup**: provider with a known low rate limit. Anthropic's
free-tier limits work; OpenAI's per-minute limits also do.

**Approach**:

**Option A** (passive observation): spam dirge with short prompts
until you hit a 429. Inspect the response — the new path should
retry transparently.

**Option B** (active forcing): set
```
export ANTHROPIC_API_KEY=invalid
```
to force a 401, then change back mid-run. The 401 is Auth (not
retryable) so this verifies the NON-retry path.

**Option C** (deterministic): use a custom provider pointing at
a local mock server that returns 429 with `Retry-After: 2` on
the first request and 200 on the second. Most thorough but
requires setup.

**Steps** (option A):
1. `cargo run --release` against a rate-limited provider.
2. Issue 5-10 rapid prompts until you hit a 429.
3. Watch the UI behavior.

**Expected**:
- The UI shows a brief delay (retry backoff) then the response
  arrives normally.
- Logs (if visible) show `tracing` warns from
  `retrying_stream_fn` with "retrying after Network error".
- **Critical**: no duplicate tokens in the response — the
  `committed` gate in `retry.rs::is_content_delta` should suppress
  retry once content has streamed.

**Fail modes**:
- **Hard error surfaced immediately, no retry** → `retry.rs`
  classification wrong; check `recovery::classify_error` matches
  on the actual provider error string.
- **Duplicate tokens in response** → `committed` gate failing.
  This is THE bug #4 from review (orphan task) cousin — if
  retry fires after streaming starts, the consumer sees
  doubled output. Check that the first delta arriving sets
  `committed = true`.
- **Hangs waiting for retry forever** → backoff longer than
  expected. Retry-After cap is 5min per `RecoveryPolicy::backoff_duration_for_msg`;
  if you wait longer something's wrong.

---

## Scenario 6 — context overflow → auto-compact

**Goal**: verify `bridge.rs` ContextOverflow classification.
After the bridge sees a long-prompt rejection from the provider,
it should emit `AgentEvent::ContextOverflow` (not `Error`); the
UI handles this by running `/compress` and respawning.

**Setup**: a model with a smallish context window (or a very
long initial prompt). Claude 3.5 Sonnet @ 200K tokens is hard
to overflow; pick a smaller model like Gemini Flash or use a
deliberately-long input.

**Easiest approach**: paste a huge text blob (10K+ words) as
the user prompt.

**Steps**:
1. `cat /usr/share/dict/words | head -5000 > /tmp/big.txt`
2. `cargo run --release`.
3. Type:
   ```
   Read /tmp/big.txt. Then summarize each word's first letter
   pattern grouped by initial letter, exhaustively.
   ```
4. Watch — the read alone may not overflow, but the model's
   verbose response combined with another tool call could.

**Expected**:
- Eventually a `ContextOverflow` banner appears in the UI
  with the prompt that triggered the overflow.
- The UI auto-compacts the session (`/compress` runs).
- A fresh run respawns with the same prompt against the now-
  compacted history.

**Fail modes**:
- **`Error` banner instead of `ContextOverflow`** → bridge's
  classification missing. Check
  `bridge.rs::agent_end_context_length_error_emits_context_overflow`
  in tests — it asserts the right path. Production may surface
  different error strings; widen `recovery::classify_error`'s
  context-length patterns to match.
- **`ContextOverflow` fires but the UI's auto-compact doesn't run** →
  UI handler in `ui/mod.rs` not catching the new variant; this
  is UI code that wasn't changed by the cutover, so it should
  still work — verify by grepping for `ContextOverflow` in `ui/`.
- **The compact runs but the respawned run hits the same overflow** →
  `/compress`'s summary isn't reducing the size, or the system
  prompt grew. Out of scope for h-7; file as a separate bug.

---

## Scenario 7 — plugin hook flow

**Goal**: verify `plugin_hooks::before_hook_from_plugin_manager`
and `plugin_hooks::after_hook_from_plugin_manager` dispatch
through the live `PluginManager` on real runs.

**Setup**:
- Build with `--features plugin`: `cargo build --release --features plugin`
- A scratch Janet plugin file. Save as `~/.config/dirge/plugins/test_hook.janet`:

  ```janet
  # Block any bash command containing the literal "rm -rf /"
  (defn deny-rm-rf [ctx]
    (def tool (get ctx :tool))
    (def args (get ctx :args))
    (when (and (= tool "bash") (string/find "rm -rf /" args))
      (harness/block "policy: rm -rf / is forbidden")))

  (harness/register-hook "on-tool-start" "deny-rm-rf")
  ```

**Steps**:
1. `dirge --features plugin` (or run from the cargo built binary).
2. Verify the plugin loaded: in the UI run `/plugins` if
   available, or check stderr for plugin-load messages.
3. Type: `Run "echo hello" via bash and tell me the output.`
4. Approve. **Expected**: succeeds; "hello" comes back.
5. Type: `Run "rm -rf /tmp/this_does_not_exist_dirge" via bash.`
   (Real `rm -rf /` would be catastrophic — using a non-existent
   path makes the test safe.)
6. Approve.

**Expected**:
- Step 4 succeeds — non-blocked bash runs as normal.
- Step 6 → the tool result contains "blocked by plugin: policy:
  rm -rf / is forbidden". The model sees the block reason and
  reports it back instead of executing.

**Fail modes**:
- **Plugin didn't load** → check the plugin install path; look
  for `plugin loaded: test_hook.janet` (or similar) on stderr.
  Not a new-path issue.
- **Both bash commands succeed (no block)** → plugin hook
  isn't being dispatched. Verify `crate::plugin::hook::global()`
  returns Some in your build; that
  `before_hook_from_plugin_manager` is installed in
  LoopSpawnConfig.before_tool_call.
- **Block fires but the model sees the success output** →
  the dispatcher isn't honoring `BeforeToolCallResult.block`.
  Check `tools.rs::prepare_tool_call` for the early-return
  path on block.
- **Block fires but with a generic message, not "policy: ..."** →
  the block reason isn't threading through. Check that
  `mutate_input` / `block` are read from the Janet hook
  context in `plugin_hooks.rs::before_hook_from_plugin_manager`.

---

## Smoke test summary (run after each round)

After completing scenarios 1-7, do a final pass:

| Scenario | Anthropic | OpenAI | Provider 3 | Notes |
|---|---|---|---|---|
| 1 — simple Q | | | | |
| 2 — single tool | | | | |
| 3 — multi-turn | | | | |
| 4 — interject | | | | |
| 5 — rate limit | | | | |
| 6 — context overflow | | | | |
| 7 — plugin | | | | |

Mark each cell:
- ✓ — passed verbatim
- ⚠ — passed but with deviation (note what)
- ✗ — failed (file a bug)

---

## Filing bugs from h-7

Each scenario's "Fail modes" section calls out the most likely
culprit module. When you file a fix-forward commit:

1. **Title**: `fix(agent_loop): h-7 scenario N — <one-line>`
2. **Body**:
   - Quote the scenario goal + actual behavior
   - Cite the suspect module from the Fail modes list
   - Show the minimal reproducer (provider, model, prompt)
   - Note which gates re-run cleanly after fix

Aim for one focused commit per bug. Group related fixes only
if they share a single root cause.

---

## Done condition

H-7 is complete when:
1. All 7 scenarios pass on at least 2 providers.
2. Any bugs surfaced have landed as focused fix-forward commits.
3. The summary table is filled out and stored alongside this
   doc (append to the bottom).
4. PLAN.md's 4.5h-7 row marked ✓.

After h-7 passes, phase 4.5 is truly done. Phase 5+ work can
proceed against a known-good production agent_loop path.
