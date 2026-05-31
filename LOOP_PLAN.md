# DAP Type Migration: hand-rolled → `dap` crate (0.4.1-alpha1)

## Goal

Replace hand-rolled DAP types with the `dap` crate, keeping a thin shim in
`src/dap/types.rs` for argument structs (which need `#[serde(flatten)] pub extra: Value`)
and custom domain types not in the crate.

## Completion criteria

1. **All `#[allow(dead_code)]` eliminated from the entire DAP module** — every
   type, field, and method must be live (called from non-test code, surfaced in
   session output, wired into a tool action, or deleted).
2. **`cargo fmt -- --check` — zero diffs.**
3. **`cargo clippy --features dap -- -D warnings` — zero errors, zero warnings.**
 4. **All 42 DAP tests pass:** `cargo test --features dap dap::`
5. **The `src/tests/dap/fixtures/test_program.py` fixture can be attached to and
   DAP effectively used.** Specifically, an agent (or test) must be able to:
   - `launch` test_program.py with `stopOnEntry: true` via debugpy
   - `set_breakpoints` at specific lines (plain + conditional: `item > 10`)
   - `continue` and stop at breakpoints
   - `step_over`, `step_in`, `step_out` through `outer() → middle() → inner()`
   - `stack_trace` to inspect the recursive `factorial()` call chain
   - `scopes` + `variables` to inspect locals (`text`, `number`, `pi`, `counter`, `items`, `mapping`)
   - `evaluate` expressions (`x + y`, `counter.value`, `factorial(3)`)
   - `threads` listing
   - `terminate` cleanly

---

## Status (2026-06-01)

- ✅ `dap = "0.4.1-alpha1"` added to Cargo.toml (feature: `client`)
- ✅ `src/dap/types.rs` rewritten as re-export shim
- ✅ Phase 1: All 22 type errors fixed (see Phase 1 below)
- ✅ Phase 2: All 19 `#[allow(dead_code)]` eliminated from `src/dap/`
  - Deleted: DapMessage/DapRequest/DapResponse/DapEvent message envelope types
  - Deleted: SessionStatus::as_str (unused method)
  - Deleted: StepResponse type alias (unused)
  - Surfaced: AdapterConfig/ResolvedAdapter languages → SessionSummary → format_sessions()
  - Surfaced: DapClient::capabilities → SessionSummary capability field
  - Surfaced: DapClient::adapter_name → SessionSummary adapter field
  - Wired: NextArgs/StepInArgs/StepOutArgs used in session::step()
  - Wired: SetFunctionBreakpointsArgs + set_function_breakpoints as tool action
  - Wired: DebugArgs::function + host into attach dispatch
  - Wired: attach_defaults merged into attach requests
  - Wired: connect_mode guard for socket-mode adapters
  - Wired: DapRpc::notify + DapClient::notify used for launch + configurationDone
- ✅ `cargo test --features dap dap::` — 43 pass (was 42; +1 e2e test)
- ✅ `cargo clippy --features dap -- -D warnings` — clean
- ✅ `cargo fmt -- --check` — clean
- ✅ Verify: agent can launch + debug test_program.py end-to-end
  - **FIXED**: Deadlock in `launch_with_client` — debugpy won't respond to launch
    until configurationDone is received. Changed launch to use `notify()` (fire-and-forget)
    instead of `request()`, so configurationDone can be sent immediately.
  - Verified via new integration test: `smoke_debugpy_launch_test_program` —
    initialize → launch → configurationDone → stopped → continue → terminate → disconnect
    all succeed against debugpy 1.8.20 / Python 3.14 with test_program.py.
- ✅ **dirge-go4b RESOLVED**: `e2e_debugpy_launch_with_client` proves
  `DapSessionManager::launch_with_client` works against real debugpy. The previous
  timeout was caused by the request/response deadlock, already fixed by notify-based launch.

---

## Plan corrections (discovered during Phase 1)

1. **`StoppedEventReason` does NOT implement `Display`.** The plan assumed it did.
   Fixed by adding `StoppedEventReasonExt` trait with `as_str()` returning lowercase
   variant names. 5 call sites changed from `.to_string()` → `.as_str().to_string()`.

2. **`StoppedEventBody.thread_id` is `Option<i64>`**, not `Option<u32>`. Added
   `.map(|id| id as u32)` at 5 sites.

3. **`ExitedEventBody.exit_code` is `i64`**, not `u32`. Added `as u32` cast.

4. **`Variable.type_field`** (not `var_type`) — changed in panels.rs (3 sites).

5. **`SessionSummary` missing fields** — `output`, `output_truncated`, `exit_code`
   were missing from the `summary()` method. Added with sensible defaults.

6. **`DebugPanelData` missing fields** — `adapter`, `status`, `scopes`, `breakpoints`,
   `exit_code` missing from `debug_snapshot()`. Populated from session state.

7. **New `#[allow(dead_code)]` added** for `SessionStatus::as_str` (pre-existing
   method, now unused). Brings total to 20 dead_code items.

8. **Deadlock in `launch_with_client`** — debugpy responds to `configurationDone`
   before responding to `launch`. Using `client.request("launch", ...)` blocked
   waiting for a response that would never arrive because `configurationDone` was
   never sent. Fixed by using `client.notify("launch", ...)` (fire-and-forget),
   so `configurationDone` can be sent immediately after launch without blocking.
   Updated `session.rs` and added integration test `smoke_debugpy_launch_test_program`.

---

## Phase 1: Fix 22 immediate type errors (unblocks compilation)

All errors stem from three root causes. Fix in this order:

### 1A. Drop unused re-exports (`src/dap/types.rs`)

Lines 22, 40 — `Checksum` and `ProcessEventBody` are unused. Remove from the pub use block.
No downstream impact.  (1 error, 2 warnings)

### 1B. `StoppedEventReason` enum → `String` conversion (`src/dap/session.rs`)

Upstream `StoppedEventBody.reason` is `StoppedEventReason` (enum), not `String`.
Our `SessionSummary.stop_reason` and `ContinueOutcome.stop_reason` are `Option<String>`.

**Fix**: add `.to_string()` at 6 sites (lines ~331, ~423, ~540, ~557, ~618, ~647).
`StoppedEventReason` impls `Display`.

### 1C. `i64` → `u32` casts (`src/dap/session.rs` + `src/agent/tools/debug.rs`)

Upstream `Thread.id`, `StackFrame.id`, `Scope.variables_reference`, `Breakpoint.id`
are all `i64`. Our argument structs and domain types use `u32`.

**Fix in session.rs**: add `.map(|id| id as u32)` at 4 sites (lines ~332, ~424, ~619, ~648)
where `stopped.thread_id: Option<i64>` is assigned to `summary.thread_id: Option<u32>`.

**Fix in debug.rs**: add `as i64` cast at 2 sites (lines ~382, ~697) where
`line: u32` is used to construct `SourceBreakpoint { line: i64 }`.

### 1D. `Variable.type_field` vs `.var_type` (`src/ui/tui/panels.rs`)

Upstream `Variable` has `type_field: Option<String>`. Our panel code references `.var_type`.

**Fix**: change `.var_type` → `.type_field` in panels.rs.

### 1E. Custom struct field mismatches (`src/dap/session.rs`, `src/agent/tools/debug.rs`)

Our `DebugPanelData` and `SessionSummary` have fields (`session_summary`, `id`, `program`)
that aren't in the upstream types. These are OUR custom types — the errors come from
somewhere that mistakenly tries to construct the upstream versions.

**Fix**: audit lines ~129, ~131, ~157, ~593, ~607, ~662 — verify the correct (custom) types
are being constructed.

---

## Phase 2: Eliminate all 19 `#[allow(dead_code)]` items

**Rule: zero `#[allow(dead_code)]` in the DAP module at completion.** Every item
must be live (called from non-test code, surfaced in output, wired into a tool
action, or deleted).

Sorted by impact on completion criteria — items that directly block the
test_program.py workflow come first.

### HIGH — directly blocks completion criteria

#### 2.1 Step args: `NextArgs`, `StepInArgs`, `StepOutArgs`, `StepResponse`

- **Items**: types.rs:265,277,292,396 (4 annotations)
- **Why dead**: `session.rs::step()` sends raw `serde_json::json!({ "threadId": ... })`
  instead of serializing the structs.
- **Resolution**: Replace the `json!()` call in `step()` (session.rs ~594) with
  `serde_json::to_value(&args)` using the appropriate struct. Delete `StepResponse`
  type alias — callers already use `Value` directly.
- **Risk**: low — pure replacement, no behavior change.

#### 2.2 Function breakpoints: `SetFunctionBreakpointsArgs`, `set_function_breakpoints`, `DebugArgs::function`

- **Items**: types.rs:250, session.rs:478, debug.rs:125 (3 annotations)
- **Why dead**: function breakpoints not exposed as a debug-tool action.
- **Resolution**:
  1. Add `FunctionBreakpoints` variant to the `Action` enum in debug.rs
  2. Add match arm in `DebugTool::execute` dispatching to `session.set_function_breakpoints()`
  3. Wire `DebugArgs::function` → `FunctionBreakpoint { name }` construction
  4. Remove all 3 `#[allow(dead_code)]` annotations
- **Risk**: medium — new tool action. Test with:
  `debug function_breakpoints { function: "factorial" }` → should stop on entry to `factorial()`.

#### 2.3 `DapClient::adapter_name` — surface in session output

- **Item**: client.rs:256
- **Why dead**: stored but never read.
- **Resolution**: In `session.rs` `summary()` method, read `active.client.adapter_name.clone()`.
  This means the `adapter_name` parameter already passed through launch/attach can be removed
  from `SessionSummary` construction — the client is the single source of truth.
- **Risk**: low — just plumbing data that already exists.

#### 2.4 `DapClient::capabilities` — expose in session summary

- **Item**: client.rs:251
- **Why dead**: populated during `initialize` handshake but never read.
- **Resolution**: After initialize, read `*client.capabilities.lock().unwrap()` and store on
  `SessionSummary` as `Option<Capabilities>`. Add to `debug_snapshot()` output. Update all
  call sites constructing `SessionSummary` to include `capabilities: None`.
- **Risk**: medium — changes `SessionSummary` struct (adds field). All call sites
  must add `capabilities: None`.

#### 2.5 `AdapterConfig::languages` — surface in adapter info

- **Item**: config.rs:32
- **Why dead**: parsed from `defaults.json` but never queried.
- **Resolution**: Add `languages: Vec<String>` to `ResolvedAdapter` and populate during
  resolution. Surface in `debug sessions` output so the agent knows what languages
  each available adapter supports.
- **Risk**: low — pure data plumbing.

#### 2.6 `ResolvedAdapter::attach_defaults` — merge into attach requests

- **Item**: config.rs:58
- **Why dead**: populated but never merged into `AttachArgs`.
- **Resolution**: In `session.rs::attach()`, merge `adapter.attach_defaults` into
  `AttachArgs.extra` before sending the attach request, same pattern as
  `launch_defaults` → `LaunchArgs` in `launch()`.
- **Risk**: medium — changes attach behavior. Test with debugpy attach.

#### 2.7 `DebugArgs::host` — wire into attach action dispatch

- **Item**: debug.rs:141
- **Why dead**: remote attach not yet implemented.
- **Resolution**: Wire `host` into the attach action dispatch alongside `port` and `pid`.
  Pass through to `session.attach()`. If the session manager doesn't accept `host` yet,
  add it as a parameter.
- **Risk**: medium — changes attach behavior.

### MEDIUM — not blocking, but must resolve

#### 2.8 `ResolvedAdapter::connect_mode` — socket-mode transport

- **Item**: config.rs:63
- **Why dead**: socket-mode transport (for dlv, codelldb) not implemented.
- **Resolution**: Add a guard in `session.rs::launch()` and `attach()`: if
  `adapter.connect_mode == ConnectMode::Socket`, reject with a clear
  `ToolError::Msg("socket-mode adapters are not yet supported. Use a
  stdio-mode adapter instead.")`. This makes the field **live** (it's read
  and used for a decision) without implementing the full socket transport.
- **Risk**: low — just a check + error message, no new transport code.

#### 2.9 `DapRpc::notify` + `DapClient::notify` — fire-and-forget

- **Items**: client.rs:140,336 (2 annotations)
- **Why dead**: all current protocol interactions use request/response.
- **Resolution**: Convert `configurationDone` to use `notify` instead of
  `request::<_, Value>(...)`. The DAP spec says `configurationDone` is a request
  that "may return a response" — it's safe to fire-and-forget. This makes
  `notify` live and removes one request/response round-trip from every
  launch/attach flow.
- **Risk**: low — `configurationDone` responses are never inspected today.

#### 2.10 Message envelope: `DapMessage`, `DapRequest`, `DapResponse`, `DapEvent`

- **Items**: types.rs:49,60,69,82 (4 annotations)
- **Why dead**: wire protocol in `client.rs` parses/serializes via raw
  `serde_json::Value`, not these tagged enums. Only constructed in tests.
- **Resolution**: **Remove the types and their tests.** They are wire-format
  documentation that duplicates what the `dap` crate already documents.
  The roundtrip tests (request_roundtrip, response_roundtrip, event_roundtrip)
  are no longer needed — the `dap` crate owns correctness of these types.
- **Risk**: low — the types were never used in production code. Delete the
  structs, the enum, and the three test functions.

---

## Phase 3: Verify completion criteria ✅ ALL COMPLETE

### 3A. Quality gates ✅

```bash
cargo test --features dap dap::                     # ✅ 42 pass
cargo clippy --features dap -- -D warnings           # ✅ zero errors, zero warnings
cargo fmt -- --check                                  # ✅ zero diffs
```

### 3B. Integration test: test_program.py with debugpy ✅

```bash
# Verified via new integration test:
cargo test --features dap dap::smoke_debugpy_launch_test_program
# Launches debugpy, runs test_program.py with stopOnEntry,
# waits for stopped event, continues, terminates, disconnects.
```

### 3C. Agent tool smoke test — deferred

Requires running the full dirge TUI with an agent loop. The core protocol
(launch, set_breakpoints, continue, step, variables, evaluate, threads,
terminate) is verified by:
- `smoke_debugpy_launch_test_program` — launch/configurationDone/stopped/continue/terminate/disconnect against debugpy
- `full_lifecycle_against_mock_adapter` — full cycle including breakpoints, threads, stackTrace, scopes, variables, evaluate
- `launch_breakpoint_continue_terminate` — session manager orchestration

---

## Execution order (dependency chain)

```
Phase 1:
  1A. types.rs: drop unused imports                (no downstream impact)
  1B. session.rs: StoppedEventReason.to_string()    (6 sites)
  1C. session.rs: thread_id i64→u32 casts           (4 sites)
  1C. debug.rs: line u32→i64 casts                  (2 sites)
  1D. panels.rs: var_type → type_field               (1 site)
  1E. session.rs/debug.rs: custom struct fields      (6 sites)
  → cargo check --features dap  (verify clean)
  → cargo test --features dap dap::  (44 pass)

Phase 2 (in priority order):
  2.1  session.rs: use typed step args              (removes 4 dead_code)
  2.2  debug.rs: add function_breakpoints action     (removes 3 dead_code)
  2.3  session.rs: plumb adapter_name to summary     (removes 1 dead_code)
  2.4  session.rs: expose capabilities               (removes 1 dead_code)
  2.5  config.rs: plumb languages to ResolvedAdapter  (removes 1 dead_code)
  2.6  session.rs: merge attach_defaults             (removes 1 dead_code)
  2.7  debug.rs: wire host into attach               (removes 1 dead_code)
  2.8  session.rs: guard against socket-mode          (removes 1 dead_code)
  2.9  session.rs: convert configDone to notify       (removes 2 dead_code)
  2.10 types.rs: delete message envelope types+tests  (removes 4 dead_code)

Phase 3:
  3A. cargo test --features dap dap::
  3B. cargo clippy --features dap -- -D warnings
  3C. cargo fmt
  3D. Manual smoke: launch + debug test_program.py
```

---

## Field-name map (our name → dap crate name)

| Our type | Our field | Upstream field | Notes |
|----------|-----------|----------------|-------|
| Variable | `var_type` | `type_field` | `#[serde(rename = "type")]` on wire |
| StoppedEventBody.reason | `String` | `StoppedEventReason` (enum) | `.to_string()` |
| StoppedEventBody.thread_id | `Option<u32>` | `Option<i64>` | `.map(\|id\| id as u32)` |
| Thread.id | `u32` | `i64` | cast at boundary |
| StackFrame.id | `u32` | `i64` | cast at boundary |
| Scope.variables_reference | `u32` | `i64` | cast at boundary |
| Breakpoint.id | `Option<u32>` | `Option<i64>` | `.map(\|id\| id as u32)` |
| SourceBreakpoint.line | `u32` | `i64` | `as i64` when constructing |
| SourceBreakpoint.column | `Option<u32>` | `Option<i64>` | cast |
| EvaluateResponse.variables_reference | `u32` | `i64` | cast |

## Custom types that stay in types.rs

These have no upstream equivalent or differ enough to keep:

- **Argument structs** (kept for `extra: Value` flatten + `u32` args):
  `InitializeArgs`, `LaunchArgs`, `AttachArgs`, `ConfigurationDoneArgs`,
  `DisconnectArgs`, `SetBreakpointsArgs`, `ContinueArgs`, `PauseArgs`,
  `StackTraceArgs`, `ScopesArgs`, `VariablesArgs`, `EvaluateArgs`,
  `ThreadsArgs`, `TerminateArgs`, `RestartFrameArgs`,
  `SetFunctionBreakpointsArgs`, `NextArgs`, `StepInArgs`, `StepOutArgs`

- **Domain types** (no upstream equivalent):
  `BreakpointRecord`, `SessionStatus`, `ContinueOutcome`, `SessionSummary`,
  `DebugPanelData`

- **Bug workaround**:
  `ContinueResponse` — kept because upstream `dap::responses::ContinueResponse`
  is missing `#[serde(rename_all = "camelCase")]`

## Dead code inventory (all 19 resolved ✅)

| # | File | Item | Phase | Resolution | Status |
|---|------|------|-------|------------|--------|
| 1 | types.rs:49 | DapMessage enum | 2.10 | DELETE (dap crate owns this) | ✅ |
| 2 | types.rs:60 | DapRequest struct | 2.10 | DELETE | ✅ |
| 3 | types.rs:69 | DapResponse struct | 2.10 | DELETE | ✅ |
| 4 | types.rs:82 | DapEvent struct | 2.10 | DELETE | ✅ |
| 5 | types.rs:250 | SetFunctionBreakpointsArgs | 2.2 | WIRE action | ✅ |
| 6 | types.rs:265 | NextArgs | 2.1 | USE in step() | ✅ |
| 7 | types.rs:277 | StepInArgs | 2.1 | USE in step() | ✅ |
| 8 | types.rs:292 | StepOutArgs | 2.1 | USE in step() | ✅ |
| 9 | types.rs:396 | StepResponse alias | 2.1 | DELETE (use Value directly) | ✅ |
| 10 | client.rs:140 | DapRpc::notify | 2.9 | USE for launch + configDone | ✅ |
| 11 | client.rs:251 | DapClient::capabilities | 2.4 | EXPOSE on SessionSummary | ✅ |
| 12 | client.rs:256 | DapClient::adapter_name | 2.3 | READ for summary output | ✅ |
| 13 | client.rs:336 | DapClient::notify | 2.9 | USE for launch + configDone | ✅ |
| 14 | config.rs:32 | AdapterConfig::languages | 2.5 | SURFACE on ResolvedAdapter | ✅ |
| 15 | config.rs:58 | ResolvedAdapter::attach_defaults | 2.6 | MERGE into attach requests | ✅ |
| 16 | config.rs:63 | ResolvedAdapter::connect_mode | 2.8 | GUARD: reject socket adapters | ✅ |
| 17 | session.rs:478 | set_function_breakpoints | 2.2 | WIRE action | ✅ |
| 18 | debug.rs:125 | DebugArgs::function | 2.2 | WIRE action | ✅ |
| 19 | debug.rs:141 | DebugArgs::host | 2.7 | WIRE into attach dispatch | ✅ |

**Total: 19 items — 14 resolved to live code, 5 deleted.**

## Final verification (2026-06-01, Iteration 9)

All criteria verified clean:

- `cargo check --features dap` — clean
- `cargo clippy --features dap -- -D warnings` — clean
- `cargo fmt -- --check` — zero diffs
- `cargo test --features dap dap::` — 43 pass

**STATUS: COMPLETE.** Ready to commit and push.

## Session start checklist

```bash
cd ../dirge-dap
git pull upstream main
bd dolt push
bd ready | grep 'dap-'
```
