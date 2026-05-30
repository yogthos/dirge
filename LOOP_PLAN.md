# DAP Port: oh-my-pi → dirge Implementation Plan

## Status (updated 2026-05-30)

- ✅ Phase 0: dirge-l3mi (feature flag)
- ✅ Phase 1: dirge-19k1 (types), dirge-untf (client), dirge-mfgs (session)
- ✅ Phase 2: dirge-9c1j (defaults), dirge-zfol (adapter resolution)
- ✅ Phase 3: dirge-knk0 (debug tool), dirge-6gf8 (prompt — covered inline), dirge-1thn (first-pass scope)
- ✅ Phase 4: dirge-vckb (docs), dirge-vlh6 (CI — feature gate verified, CI infrastructure TBD)
- ✅ Phase 4: dirge-f9x1 (integration tests — mock adapter smoke + 13 new adapter-resolution tests)
- ⬜ Phase 4: dirge-9h8r (smoke tests — blocked on adapters), dirge-x5hr (DAP↔LSP bridge), dirge-jjul (TUI panel)

Known issues:
- Integration tests use a Python mock adapter. Real-adapter smoke (dirge-9h8r) still needs debugpy/lldb-dap/dlv on PATH.
- `src/lib.rs` approach was abandoned — integration test lives in `src/dap/client.rs` tests module.

## Architecture Overview

```
omp (TypeScript/Bun)                     dirge (Rust/tokio)
══════════════════════                    ════════════════════
DapClient                                crate::dap::client::DapClient
├── spawn (stdio/socket)                 ├── spawn_stdio / spawn_socket
├── sendRequest / sendNotification       ├── request / notify (via RpcClient)
├── onEvent / waitForEvent               ├── on_event / wait_for_event
├── Content-Length framing               └── lsp::jsonrpc::decode_frame/encode_frame ✓
└── request/response seq matching        └── lsp::rpc::RpcClient ✓

DapSessionManager                        crate::dap::session::DapSession
├── launch / attach                      ├── launch / attach
├── breakpoint cache                     ├── breakpoints: HashMap<PathBuf, Vec<Breakpoint>>
├── capabilities caching                 ├── capabilities: Option<Capabilities>
├── initialize→configurationDone         ├── initialize → configuration_done
└── single-session enforcement           └── active_session: Option<Arc<Mutex<DapSession>>>

DebugTool                                crate::agent::tools::debug::DebugTool
├── Tool trait impl                      ├── Tool trait impl ✓ (exact same pattern)
├── 27-action dispatch                   ├── 27-action match arm
├── Zod schema → typed args              ├── serde::Deserialize struct
└── toolResult()                         └── Result<String, ToolError> ✓

Adapter resolution                       crate::dap::config
├── selectLaunchAdapter                  ├── select_launch_adapter
├── file-extension mapping               ├── extension → adapter name
├── PATH scan (resolveCommand)           ├── which::which for binary discovery
└── defaults.json                        └── include_str!("defaults.json") ✓
```

## Functional Equivalents (omp → dirge)

### 1. DapClient → crate::dap::client

| omp | dirge equivalent | Status |
|-----|-----------------|--------|
| `ptree.spawn([cmd, ...args])` detached, non-interactive env | `tokio::process::Command::new(cmd).args(args).kill_on_drop(true).spawn()` — dirge already does this in `lsp::spawn::ProcessSpawner` | ✓ Pattern exists |
| `Content-Length: N\r\n\r\n` framing | `lsp::jsonrpc::encode_frame` / `decode_frame` — identical framing, same spec | ✓ Reuse directly |
| `#pendingRequests: Map<seq, {resolve, reject}>` | `lsp::rpc::RpcClient` — `next_id: AtomicU64`, `pending: Mutex<HashMap<u64, oneshot::Sender>>` | ✓ Reuse directly |
| `sendRequest()` → `pendingRequests.set(seq, {resolve})` → write frame | `rpc.request(method, params, timeout)` — exact same pattern | ✓ Reuse directly |
| `onEvent(event, handler)` / `onAnyEvent(handler)` | `rpc.on_notification(method, handler)` — same dispatch pattern | ✓ Reuse directly |
| `waitForEvent(event, predicate, signal, timeout)` | Custom: register handler→oneshot channel, race against timeout+abort | Build new |
| `dispose()` → close socket, kill process | `tokio::process::Child::kill()` on drop (kill_on_drop) | ✓ Built-in |
| Socket mode (dlv) | `tokio::net::UnixStream` (Linux) or `tokio::net::TcpStream` (macOS) | Build new |
| `#spawnSocketUnix` — `--listen=unix:/tmp/dap-<name>.sock` | `tokio::net::UnixStream::connect(socket_path)` after spawn | Build new |
| `#spawnSocketClientAddr` — listen random port, `--client-addr=127.0.0.1:<port>` | `tokio::net::TcpListener::bind("127.0.0.1:0")` then accept | Build new |

**Design decision**: DAP uses the SAME framing as LSP (`Content-Length: N\r\n\r\n`). Dirge already has a fully-tested `lsp::jsonrpc` framing module and `lsp::rpc::RpcClient` that handles seq→response matching, notification dispatch, and timeout. We should extract the shared framing into `src/framing.rs` (or `src/dap/framing.rs` that re-exports from lsp) rather than duplicating it. The `RpcClient` itself can be reused as-is — it has no LSP-specific knowledge.

**Plan**: Move `lsp::jsonrpc` to a shared `framing` module, or add a `pub use` re-export. `DapClient` wraps `RpcClient` and adds DAP-specific initialization handshake + event waiting.

### 2. DapSessionManager → crate::dap::session

| omp | dirge equivalent | Status |
|-----|-----------------|--------|
| `#sessions: Map<string, DapSession>` | `HashMap<String, DapSession>` behind `Arc<Mutex<>>` | Build new |
| `#activeSessionId: string \| null` | `active_session_id: Option<String>` | Build new |
| Single-session enforcement (`#ensureLaunchSlot`) | Terminate existing before launching new | Build new |
| `client.initialize(args)` | Call `client.request("initialize", args, timeout)` via RpcClient | Build new |
| `#registerSession` → store client, breakpoints, state | Create `DapSession { client, breakpoints: HashMap::new(), ... }` | Build new |
| `#completeConfigurationHandshake` → `initialized` event → `configurationDone` | Same flow: wait for `initialized` event, send `configurationDone` | Build new |
| `#prepareStopOutcome` — collect stop event + output | Handler on `stopped` event + `output` event | Build new |
| `launch(args, signal, timeout)` | public async fn launch(&self, args, signal, timeout) | Build new |
| `attach(args, signal, timeout)` | public async fn attach(&self, args, signal, timeout) | Build new |
| `setBreakpoints(args)` | `client.request("setBreakpoints", args, timeout)` | Build new |
| `continue_(args)` → race stop event vs timeout | Send continue, wait for stopped/terminated/exited event | Build new |
| `stackTrace(args)` → `DapStackTraceResponse` | `client.request("stackTrace", args, timeout)` | Build new |
| `scopes(args)` → `DapScopesResponse` | `client.request("scopes", args, timeout)` | Build new |
| `variables(args)` → `DapVariablesResponse` | `client.request("variables", args, timeout)` | Build new |
| `evaluate(args)` → `DapEvaluateResponse` | `client.request("evaluate", args, timeout)` | Build new |
| Step operations (`stepIn`, `stepOut`, `next`) | `client.request("stepIn"/"stepOut"/"next", args, timeout)` then wait for stop | Build new |
| `terminate(args)` | `client.request("terminate", args, timeout)` then dispose session | Build new |
| `buildSummary(session)` → `DapSessionSummary` | `fn summary(&self) -> SessionSummary` — struct with Serialize | Build new |
| `IDLE_TIMEOUT_MS = 10min`, cleanup timer | Not needed for agent tool (tool calls are fire-and-forget) | Skip |
| `MAX_OUTPUT_BYTES = 128KB`, output truncation | `super::tools::head_cap` already exists | ✓ Reuse |
| `debugpy missing module` heuristic | `ToolError::Msg("debugpy not found: pip install debugpy")` | Build new |

### 3. DebugTool → crate::agent::tools::debug

| omp | dirge equivalent | Status |
|-----|-----------------|--------|
| `z.object({ action: z.enum([...]), ... })` | `#[derive(Deserialize)] struct DebugArgs { action: DebugAction, ... }` | Build new |
| `ToolError` | `crate::agent::tools::ToolError::Msg(...)` | ✓ Exists |
| `toolResult({ content, details })` | `impl Tool for DebugTool { type Output = String; fn call() -> Result<String, ToolError>` | ✓ Exists |
| Permission check (`ToolApprovalDecision`) | `check_perm(&self.perm, &self.ask_tx, "debug", &input).await?` | ✓ Exists |
| `clampTimeout("debug", timeout)` | `timeout.clamp(5, 300).unwrap_or(30)` inline | Build new |
| `AbortSignal.any([toolSignal, timeoutSignal])` | `AbortSignal` already in agent_loop, `tokio::time::timeout` | ✓ Exists |
| CWD resolution (`resolveToCwd`) | `std::env::current_dir()` or session cwd | Build new |
| Read-only action gate | `DEBUG_READONLY_ACTIONS` set → conditionally skip permission for readonly ops | Build new |
| `debugDescription` prompt | `prompts/tools/debug.md` via `include_str!` | Build new |
| Renderer (TUI components) | Tool returns text; TUI rendering is separate concern. Text output is sufficient for v1 | Skip TUI for v1 |
| `renderStatusLine`, `CachedOutputBlock` | dirge status line already renders tool results generically | ✓ Exists |

### 4. Adapter Resolution → crate::dap::config

| omp | dirge equivalent | Status |
|-----|-----------------|--------|
| `defaults.json` → `DEFAULT_ADAPTERS` map | `include_str!("defaults.json")` → `serde_json::from_str` → `HashMap<String, AdapterConfig>` | Build new |
| `resolveCommand(config.command, cwd)` → PATH lookup | `which::which(command)` — returns `Option<PathBuf>` | Build new |
| `getAvailableAdapters(cwd)` | Iterate defaults, call `resolveAdapter`, filter successes | Build new |
| `selectLaunchAdapter(program, cwd, adapterName?)` | Extension-based matching + adapter priority ordering | Build new |
| `selectAttachAdapter(cwd, adapterName?, port?)` | Adapter selection, debugpy preference when port present | Build new |
| `EXTENSIONLESS_DEBUGGER_ORDER = ["gdb", "lldb-dap"]` | Same ordering for extensionless binaries | Build new |
| `sortAdaptersForLaunch` → ext match > root markers > priority | Same sorting logic | Build new |
| `hasRootMarkers(cwd, markers)` — glob match | `glob::Pattern` or simple `Path::join(cwd, marker).exists()` | Build new |

### 5. Types → crate::dap::types

All omp `Dap*` interfaces → Rust `#[derive(Debug, Clone, Serialize, Deserialize)]` structs.
The `dap-types` crate already provides many of these — use it where possible,
hand-roll only the missing ones.

| omp type | Rust source |
|----------|-------------|
| `DapProtocolMessage`, `DapRequestMessage`, `DapResponseMessage`, `DapEventMessage` | Hand-roll (5 lines each, serde tagged enum) |
| `DapLaunchArguments`, `DapAttachArguments` | Hand-roll (mostly `dap-types` compatible, minor field differences) |
| `DapCapabilities` | Hand-roll (flat struct with 20 optional bools) |
| `DapStackFrame`, `DapSource`, `DapScope`, `DapVariable`, `DapThread` | Hand-roll (standard DAP spec) |
| `DapBreakpoint`, `DapSourceBreakpoint`, `DapFunctionBreakpoint` | Hand-roll |
| `DapEvaluateResponse`, `DapStackTraceResponse`, `DapScopesResponse`, `DapVariablesResponse`, `DapThreadsResponse` | Hand-roll |
| `DapContinueOutcome`, `DapSessionSummary`, `DapSessionStatus` | Hand-roll |
| `DapResolvedAdapter`, `DapAdapterConfig` | Hand-roll |
| `DapOutputEventBody`, `DapStoppedEventBody`, `DapExitedEventBody`, `DapTerminatedEventBody` | Hand-roll |

**Design decision**: The `dap-types` crate on crates.io provides Rust types for the DAP spec.
Check version compatibility — if it covers our 27 operations, prefer it over hand-rolling.
If it's stale or incomplete, hand-roll with serde. The types are simple data structs,
not behavior — so either path is low-risk.

## File Structure

```
src/dap/
├── mod.rs              — re-exports, feature gate
├── types.rs            — all DAP protocol types (Request/Response/Event shapes)
├── client.rs           — DapClient: spawn + RPC wrapper
├── session.rs          — DapSessionManager: launch/attach, breakpoints, state
├── config.rs           — adapter resolution, PATH discovery, extension mapping
└── defaults.json       — bundled adapter definitions

src/agent/tools/
└── debug.rs            — DebugTool: Tool impl, 27-action dispatch

prompts/tools/
└── debug.md            — model-facing prompt teaching DAP vs printf

docs/
└── dap.md              — user-facing documentation
```

## Bead Dependency Graph

```
Phase 0 — Foundation
  dirge-l3mi ✓ DONE — Cargo.toml feature flag

Phase 1 — Protocol transport
  dirge-19k1  ✓ DONE — DAP types module
       ↓
  dirge-untf  ✓ DONE — DAP client transport
       ↓
  dirge-mfgs  DAP session manager (needs client)

Phase 2 — Adapter discovery
  dirge-9c1j  Adapter defaults JSON
  dirge-zfol  Adapter resolution (can start in parallel with Phase 1)

Phase 3 — Agent tool
  dirge-knk0  Debug tool (needs session manager + adapter resolution)
  dirge-6gf8  Model-facing prompt (can start in parallel with knk0)
  dirge-1thn  First-pass scope (informational — guides knk0 implementation)

Phase 4 — Polish
  dirge-x5hr  DAP↔LSP bridge (needs debug tool + LSP)
  dirge-jjul  TUI debug panel (needs debug tool)
  dirge-f9x1  Integration tests (needs debug tool)
  dirge-9h8r  Per-language smoke (needs debug tool + all adapters)
  dirge-vckb  Docs (can start anytime after Phase 2)
  dirge-vlh6  CI wiring (needs feature flag)
```

## Implementation Sequence

### dirge-19k1: DAP Types

**File**: `src/dap/types.rs` (new, ~300 lines)
**Dependencies**: None — pure serde structs, no dirge imports
**Pattern**: Every struct is `#[derive(Debug, Clone, Serialize, Deserialize)]`

**What to build** (port from omp `packages/coding-agent/src/dap/types.ts` lines 1-600):

1. `DapMessage` — tagged enum `#[serde(tag = "type")]` with `Request`, `Response`, `Event` variants
2. `DapRequest` — `{ seq: u64, command: String, arguments: Option<Value> }`
3. `DapResponse` — `{ seq: u64, request_seq: u64, success: bool, command: String, message: Option<String>, body: Option<Value> }`
4. `DapEvent` — `{ seq: u64, event_type: String, body: Option<Value> }`
5. `Capabilities` — flat struct with ~20 `Option<bool>` fields matching DAP spec
6. `LaunchArgs`, `AttachArgs` — program/cwd/args/pid/port/host
7. `Source`, `StackFrame`, `Scope`, `Variable`, `Thread` — standard DAP shapes
8. `Breakpoint`, `SourceBreakpoint`, `FunctionBreakpoint`
9. Response types: `StackTraceResponse`, `ScopesResponse`, `VariablesResponse`, `ThreadsResponse`, `EvaluateResponse`, `SetBreakpointsResponse`
10. `SessionSummary`, `ContinueOutcome`, `SessionStatus` enum
11. `OutputEventBody`, `StoppedEventBody`, `ExitedEventBody`, `TerminatedEventBody`
12. `InitializeArgs` — clientID, adapterID, linesStartAt1, pathFormat, etc.
13. `DisconnectArgs` — restart, terminateDebuggee

**Fields to skip** (deferred per `dirge-1thn`): `DisassembleArgs`, `DisassembledInstruction`, `ReadMemoryArgs`, `WriteMemoryArgs`, `Module`, `DataBreakpoint`, `InstructionBreakpoint`, `LoadedSourcesResponse`, `DataBreakpointInfoArgs`.

**Tests**: `#[cfg(test)] mod tests` — serialize/deserialize round-trip for each message type, at least the `DapMessage` tagged-enum round-trip, and one capability struct.

### dirge-untf: DAP Client Transport

**File**: `src/dap/client.rs` (new, ~150 lines)
**Dependencies**: `src/dap/types.rs`, `src/dap/framing.rs` (copy of lsp framing)

**Surgical reuse**:
- Copy `src/lsp/jsonrpc.rs` lines 16-80 into `src/dap/framing.rs` — `encode_frame` + `decode_frame` + `MAX_BODY_BYTES` constant. Add header: `// Ported from src/lsp/jsonrpc.rs — identical Content-Length framing used by DAP.`
- Copy the process-spawn pattern from `src/lsp/spawn.rs` lines 175-220 into a standalone `pub async fn spawn_adapter(cmd: &Path, args: &[String], cwd: &Path) -> io::Result<AdapterProcess>` in `src/dap/client.rs`. This is a ~30-line function with tokio::process::Command + piped stdio + background stderr drain.
- For JSON-RPC request/response: implement a minimal `DapRpc` directly (~80 lines) rather than depending on `lsp::rpc::RpcClient`. The DAP variant is simpler — no string-ID handling, no server→client requests, no complex dispatch. Just: `next_id: AtomicU64`, `pending: Mutex<HashMap<u64, oneshot::Sender>>`, a `request(method, params, timeout)` method, and a `read_loop` that routes responses by id.

**What to build**:
```rust
pub struct DapClient {
    child: Mutex<Option<tokio::process::Child>>,
    rpc: DapRpc,
    capabilities: Mutex<Option<Capabilities>>,
    adapter_name: String,
}

pub struct AdapterProcess {
    pub child: tokio::process::Child,
    pub reader: BufReader<ChildStdout>,
    pub writer: ChildStdin,
}
```

`DapClient::spawn_stdio(adapter: &ResolvedAdapter, cwd: &Path) -> io::Result<Self>`:
1. Build `tokio::process::Command` from adapter command + args
2. Set `current_dir(cwd)`, `stdin(Stdio::piped())`, `stdout(Stdio::piped())`, `stderr(Stdio::piped())`, `kill_on_drop(true)`
3. Spawn, take stdin/stdout, spawn background stderr drain task
4. Create `DapRpc` from reader/writer (spawn read loop task)
5. Return `DapClient`

`DapClient::request<P, R>(&self, method: &str, params: &P, timeout: Duration) -> Result<R, RpcError>`:
Delegates to `self.rpc.request(method, params, timeout)`.

`DapClient::notify<P>(&self, method: &str, params: &P) -> Result<(), RpcError>`:
Fire-and-forget — write frame, don't wait for response.

**Socket mode (dlv)**: Defer. Start with stdio-only. dlv supports `dlv dap --listen=stdio` on some versions. Document the limitation in `docs/dap.md`.

**Tests**: Spawn `cat` as a fake adapter — write a DAP request frame, verify the response frame parses. Test timeout (request against a process that never responds). Test cleanup (child killed on drop).

### dirge-mfgs: DAP Session Manager

**File**: `src/dap/session.rs` (new, ~300 lines)
**Dependencies**: `src/dap/client.rs`, `src/dap/types.rs`, `src/dap/config.rs`

**Surgical reuse**: `tokio::sync::Mutex` (same as `BackgroundStore` in `src/agent/tools/background.rs`), `tokio::time::timeout` (same as everywhere in dirge).

**What to build**:
```rust
pub struct DapSessionManager {
    sessions: Mutex<HashMap<String, DapSession>>,
    active_id: Mutex<Option<String>>,
}

struct DapSession {
    id: String,
    client: DapClient,
    adapter_name: String,
    cwd: PathBuf,
    program: Option<PathBuf>,
    status: SessionStatus,  // Launching | Configuring | Stopped | Running | Terminated
    breakpoints: HashMap<PathBuf, Vec<BreakpointRecord>>,
    function_breakpoints: Vec<FunctionBreakpoint>,
    output: String,
    output_truncated: bool,
    exit_code: Option<i32>,
    capabilities: Option<Capabilities>,
    initialized_seen: bool,
    configuration_done_sent: bool,
}
```

**Launch flow** (port from omp `session.ts` lines 245-295):
1. Terminate existing active session (single-session enforcement)
2. `DapClient::spawn_stdio(adapter, cwd)`
3. `client.request("initialize", InitializeArgs, timeout)` → store capabilities
4. If `capabilities.supportsConfigurationDoneRequest`, set `needs_configuration_done = true`
5. Register stop/output event listeners BEFORE sending launch
6. `client.request("launch", LaunchArgs, timeout)` — fire and track
7. `client.request("configurationDone")` if needed
8. Wait for `stopped` event (stopOnEntry) or timeout
9. Return `SessionSummary`

**Key methods** — each is ~20 lines of validate-active-session + delegate-to-client:
```rust
pub async fn launch(&self, opts: &LaunchOptions, signal: &AbortSignal, timeout: Duration) -> Result<SessionSummary, ToolError>;
pub async fn attach(&self, opts: &AttachOptions, signal: &AbortSignal, timeout: Duration) -> Result<SessionSummary, ToolError>;
pub async fn set_breakpoints(&self, args: &SetBreakpointsArgs, timeout: Duration) -> Result<Vec<Breakpoint>, ToolError>;
pub async fn continue_(&self, thread_id: u32, signal: &AbortSignal, timeout: Duration) -> Result<ContinueOutcome, ToolError>;
pub async fn step(&self, thread_id: u32, granularity: &str, signal: &AbortSignal, timeout: Duration) -> Result<SessionSummary, ToolError>;
pub async fn stack_trace(&self, thread_id: u32, levels: u32, timeout: Duration) -> Result<Vec<StackFrame>, ToolError>;
pub async fn scopes(&self, frame_id: u32, timeout: Duration) -> Result<Vec<Scope>, ToolError>;
pub async fn variables(&self, variables_ref: u32, timeout: Duration) -> Result<Vec<Variable>, ToolError>;
pub async fn evaluate(&self, expression: &str, frame_id: Option<u32>, context: &str, timeout: Duration) -> Result<EvaluateResponse, ToolError>;
pub async fn threads(&self, timeout: Duration) -> Result<Vec<Thread>, ToolError>;
pub async fn pause(&self, thread_id: u32, timeout: Duration) -> Result<SessionSummary, ToolError>;
pub async fn terminate(&self, timeout: Duration) -> Result<SessionSummary, ToolError>;
pub async fn disconnect(&self, restart: bool, timeout: Duration) -> Result<(), ToolError>;
pub fn active_session(&self) -> Option<SessionSummary>;
pub fn list_sessions(&self) -> Vec<SessionSummary>;
```

**Tests**: Mock the DapClient (trait or manual mock) — verify launch calls initialize → configurationDone, verify single-session enforcement, verify breakpoint caching.

### dirge-zfol: Adapter Resolution

**File**: `src/dap/config.rs` (new, ~200 lines)
**Dependencies**: `src/dap/defaults.json` (bundled via `include_str!`), `which` crate

**Surgical reuse**: The `which` crate is added to Cargo.toml. No dirge-internal dependencies.

**What to build**:
```rust
pub struct AdapterConfig {
    pub command: String,
    pub args: Vec<String>,
    pub languages: Vec<String>,
    pub file_types: Vec<String>,
    pub root_markers: Vec<String>,
    pub launch_defaults: serde_json::Value,
    pub attach_defaults: serde_json::Value,
    pub connect_mode: ConnectMode,  // Stdio or Socket
}

pub struct ResolvedAdapter {
    pub name: String,
    pub resolved_command: PathBuf,
    pub args: Vec<String>,
    pub file_types: Vec<String>,
    pub root_markers: Vec<String>,
    pub launch_defaults: serde_json::Value,
    pub attach_defaults: serde_json::Value,
    pub connect_mode: ConnectMode,
}
```

`fn load_defaults() -> HashMap<String, AdapterConfig>` — parse `include_str!("defaults.json")`.

`fn resolve_adapter(name: &str) -> Option<ResolvedAdapter>` — look up in defaults, run `which::which(command)`, return resolved.

`fn select_launch_adapter(program: &Path, cwd: &Path, adapter_name: Option<&str>) -> Option<ResolvedAdapter>`:
1. If adapter_name given → `resolve_adapter(adapter_name)`
2. Get file extension, filter adapters by `file_types`
3. Sort: extension match > root markers match > native debugger priority
4. Return first

`fn select_attach_adapter(cwd: &Path, adapter_name: Option<&str>, port: Option<u16>) -> Option<ResolvedAdapter>`:
1. If adapter_name given → `resolve_adapter(adapter_name)`
2. If port given → prefer debugpy
3. Otherwise prefer gdb/lldb-dap → any available

**Tests**: Extension matching (`rs → lldb-dap`, `.py → debugpy`, `.go → dlv`), extensionless binary → gdb/lldb-dap priority, explicit adapter bypass, root-marker detection (mock `Path::exists`).

### dirge-knk0: Debug Tool

**File**: `src/agent/tools/debug.rs` (new, ~400 lines)
**Dependencies**: `src/dap/session.rs`, `src/dap/config.rs`, `src/agent/tools/mod.rs`

**Surgical reuse**:
- `check_perm(&self.perm, &self.ask_tx, "debug", &input).await?` — exact same call pattern as BashTool (`src/agent/tools/bash.rs`)
- `ToolError::Msg(...)` — same error type as every tool
- `head_cap(...)` — for output truncation
- `AbortSignal` — from `crate::agent::agent_loop::tool::AbortSignal`
- `#[derive(Deserialize)]` args struct — same pattern as every dirge tool
- `impl Tool for DebugTool` — identical trait impl pattern to `LspTool`, `BashTool`, etc.

**Tool registration** (surgical changes to non-DAP files):

In `src/agent/tools/mod.rs`:
```rust
// Add ONE line in the mod declarations:
#[cfg(feature = "dap")]
pub mod debug;

// Add ONE line in BUILTIN_TOOL_NAMES:
#[cfg(feature = "dap")]
"debug",
```

In `src/agent/builder.rs` (find the existing `.tool(...)` chain, add ONE block):
```rust
#[cfg(feature = "dap")]
{
    use crate::agent::tools::debug::DebugTool;
    agent = agent.tool(DebugTool::new(permission.clone(), ask_tx.clone()));
}
```

**What to build**:
```rust
#[derive(Deserialize)]
pub struct DebugArgs {
    pub action: String,             // "launch" | "attach" | "set_breakpoint" | ...
    pub program: Option<String>,
    pub args: Option<Vec<String>>,
    pub adapter: Option<String>,
    pub cwd: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub function: Option<String>,
    pub condition: Option<String>,
    pub expression: Option<String>,
    pub frame_id: Option<u32>,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub levels: Option<u32>,
    pub scope_id: Option<u32>,
    pub variable_ref: Option<u32>,
    pub timeout: Option<u32>,
}

pub struct DebugTool {
    perm: Option<PermCheck>,
    ask_tx: Option<AskSender>,
}

impl Tool for DebugTool {
    const NAME: &'static str = "debug";
    type Error = ToolError;
    type Args = DebugArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition { ... }
    async fn call(&self, args: DebugArgs) -> Result<String, ToolError> { ... }
}
```

`call()` dispatches to 16 private methods matching the 16 first-pass actions.
Each method: validate required args (`required_nonblank`), timeout clamp
(5..300s, default 30), resolve adapter if needed, call session manager method,
format output as human-readable text.

**Tests**: Tool definition matches expected schema. Arg validation: missing
`program` on launch → error. Missing `expression` on evaluate → error.
Permission check short-circuit test.

## What NOT to Port (Out of Scope for V1)

1. **Firecracker sandbox** — omp's `pi-iso` crate for APFS/btrfs/overlayfs isolation. Dirge uses bubblewrap.
2. **Interactive debug selector in TUI** — omp's `debug/index.ts` with menu components. V1 is text-only.
3. **Raw SSE viewer** — omp-specific debugging tool for streaming events.
4. **CPU/heap profiling** — omp's `debug/profiler.ts`. Separate concern.
5. **Report bundle** — omp's `debug/report-bundle.ts` for .tar.gz reports.
6. **Advanced DAP ops** — disassemble, read_memory, write_memory, data_breakpoints, instruction_breakpoints, loaded_sources, modules, custom_request. Deferred per `dirge-1thn`.
7. **Process exit monitoring / heartbeat** — omp's cleanup loop. Agent tool calls are short-lived; no need.
8. **Stderr capture from adapter** — dirge's LSP spawner already drains stderr in background. Reuse that pattern.
9. **`agent://` output resolution** — omp's internal URL system. Dirge doesn't have this; results flow through tool return values directly.
10. **`report_finding` / `yield`** — omp's subprocess tool handlers. Dirge's subagents are one-shot, no need.

## Shared Infrastructure — Surgical Reuse (No Module Refactors)

**Rule**: DAP must NOT refactor or broaden existing modules. Every non-DAP change
is a single-purpose addition (add an import, add a `cfg`-gated `mod` line, add
a tool name to a const array). DAP reuses dirge internals by **copying small
proven pieces into `src/dap/`** with a `// Ported from src/lsp/jsonrpc.rs` header.

### Framing (Content-Length: N\r\n\r\n) — Copy, Don't Re-export

The LSP framing module (`src/lsp/jsonrpc.rs`) is 80 lines of pure
`encode_frame`/`decode_frame`. DAP uses the identical wire format.
**Do not** move it, re-export it, or make it shared.

**Action**: Copy `encode_frame` and `decode_frame` into `src/dap/framing.rs`
with a header `// Ported from src/lsp/jsonrpc.rs — identical wire format for DAP`.
Delete the constants and tests (tests stay in lsp). This is ~60 lines.

### RPC Client — Depend, Don't Lift

`src/lsp/rpc.rs::RpcClient` is a generic JSON-RPC 2.0 client. It has zero
LSP-specific knowledge — just `request(method, params, timeout)` and
`on_notification(method, handler)`. DAP uses the exact same protocol.

**Action**: Add one import in `src/dap/client.rs`:
```rust
use crate::lsp::rpc::RpcClient;
```
This compiles because both `lsp` and `dap` are optional features. When only
`dap` is enabled, the `use` fails — guard with `#[cfg(feature = "lsp")]` and
provide a hand-rolled fallback when lsp is off:

```rust
#[cfg(feature = "lsp")]
use crate::lsp::rpc::RpcClient;

#[cfg(not(feature = "lsp"))]
mod rpc_stub { /* minimal JSON-RPC client, ~100 lines */ }
```

Actually simpler: just **always pull RpcClient into dap**. The lsp module
declares `pub mod rpc` whether the feature is on or not (the feature only
gates `dep:lsp-types` and the spawner). Check if this is true... if `rpc.rs`
is NOT behind `#[cfg(feature = "lsp")]`, the import always works. If it IS
behind the gate, the stub approach above is the surgical path.

### Process Spawning — Copy the Pattern, Not the Module

`src/lsp/spawn.rs` spawns tokio processes with piped stdio + stderr drain.
DAP adapters need identical mechanics.

**Action**: Copy the spawn pattern into `src/dap/spawn.rs` (~50 lines):
```rust
pub async fn spawn_adapter(program: &Path, args: &[String], cwd: &Path)
    -> io::Result<(Child, BufReader<ChildStdout>, ChildStdin)>
```
This is a standalone function — no dependencies on lsp types. The stderr
drain is identical (background `tokio::spawn` reading lines to tracing).

### Permission Check — Reuse the Function, Add One Name

**Action**: In `src/agent/tools/mod.rs`, add `"debug"` to `BUILTIN_TOOL_NAMES`:
```rust
#[cfg(feature = "dap")]
"debug",
```
Add `#[cfg(feature = "dap")]` on the `pub mod debug;` line.

**Action**: In `src/agent/builder.rs`, add one `cfg`-gated tool registration:
```rust
#[cfg(feature = "dap")]
{
    use crate::agent::tools::debug::DebugTool;
    agent = agent.tool(DebugTool::new(permission.clone(), ask_tx.clone()));
}
```

### AbortSignal — Use Directly, Already Public

`src/agent/agent_loop/tool.rs::AbortSignal` is `pub`. DAP code imports it:
```rust
use crate::agent::agent_loop::tool::AbortSignal;
```

### Secret Redaction — Already Applied at Tool-Result Boundary

`src/agent/agent_loop/tools.rs::content_value_to_block` already runs
`sandbox::redact_secrets()` on every tool result. DAP tool results flow
through the same path — no DAP-specific change needed.

### Output Truncation — Reuse `head_cap`

`src/agent/tools/mod.rs::head_cap` is `pub`. DAP tool uses it for
large evaluate/stack_trace/variables output:
```rust
use crate::agent::tools::head_cap;
let capped = head_cap(raw, 128 * 1024, "debug output");
```

### Feature Gate Template — Every DAP File

Every file in `src/dap/` starts with:
```rust
//! DAP (Debug Adapter Protocol) integration. Feature-gated behind
//! `#[cfg(feature = "dap")]` — all public types in this module are
//! invisible when the feature is off.
```

`src/dap/mod.rs`:
```rust
#[cfg(feature = "dap")]
pub mod client;
#[cfg(feature = "dap")]
pub mod config;
#[cfg(feature = "dap")]
mod framing;
#[cfg(feature = "dap")]
pub mod session;
#[cfg(feature = "dap")]
pub mod types;
```

`src/main.rs` or `src/lib.rs` — add ONE line:
```rust
#[cfg(feature = "dap")]
pub mod dap;
```

### Summary of Non-DAP Changes (5 Files, ~15 Lines Total)

| File | Change | Lines |
|------|--------|-------|
| `src/main.rs` | `pub mod dap;` (cfg-gated) | 2 |
| `src/agent/tools/mod.rs` | `pub mod debug;` (cfg-gated) + 1 entry in `BUILTIN_TOOL_NAMES` | 3 |
| `src/agent/builder.rs` | One cfg-gated `.tool(DebugTool::new(...))` block | 5 |
| `Cargo.toml` | `dap` feature + 2 deps | 4 |
| Maybe `src/agent/tools/mod.rs` | `check_perm` call-site for debug — reuses existing function | 0 (already pub) |

**Total non-DAP changes: ~15 lines across 5 files. Zero refactors.**

## Risks & Mitigations

1. **`dap-types` crate compatibility**: The crates.io `dap-types` may not cover all 27 operations.
   **Mitigation**: Check before coding. If incomplete, hand-roll with serde — the types are ~300 lines total.

2. **Socket-mode adapters (dlv)**: dirge doesn't currently have Unix socket or TCP listener patterns outside of the LSP spawner.
   **Mitigation**: dlv's socket mode is the outlier. Implement stdio mode first; add socket mode in a follow-up. Many dlv builds support stdio mode directly.

3. **LSP RPC client reuse**: `RpcClient` is in `lsp::rpc` — importing it from `dap` creates a dependency direction issue (dap depends on lsp).
   **Mitigation**: Either move `jsonrpc.rs` + `rpc.rs` to a shared `rpc` module at crate root, or accept the dependency (lsp and dap are both optional features — either can be on independently, or both).

4. **Adapter stderr can block**: LSP spawner drains stderr; DAP adapters may have different stderr behavior.
   **Mitigation**: Always drain stderr in a background task, same as LSP spawner does.

5. **`which` crate may not find adapters in non-standard locations**.
   **Mitigation**: Allow explicit adapter command paths in config.json, same as LSP server commands are configurable.

---

## Beads Workflow Commands

All work is tracked in the `dap-port` worktree at `../dirge-dap`. The bead IDs below are stable across the session — use them verbatim.

### Query — find what's ready, blocked, or claimed

```bash
# All DAP beads (filtered by prefix dap)
bd list | grep 'dap-'

# Only DAP features (skips the non-DAP dirge-* cleanup issues)
bd list --type=feature | grep 'dap-'

# Show full details for one bead
bd show dirge-19k1

# Ready work (no blockers, available to claim)
bd ready
```

### Claim — take ownership of a bead before starting

```bash
bd update dirge-19k1 --claim
```

### Close — mark a bead complete after implementation + tests pass

```bash
bd close dirge-19k1
```

### Session end — push beads data upstream

```bash
bd dolt push
```

### Bead dependency graph (execution order)

```
dirge-l3mi ✓ CLOSED — Cargo.toml feature flag (prerequisite for all)

Phase 1 — Protocol transport (both can start in parallel)
  dirge-19k1  DAP types module          ← ✓ DONE
       ↓
  dirge-untf  DAP client transport      ← ✓ DONE
       ↓
  dirge-mfgs  DAP session manager       ← ✓ DONE

Phase 2 — Adapter discovery (parallel with Phase 1)
  dirge-9c1j  Adapter defaults JSON     ← ✓ DONE
  dirge-zfol  Adapter resolution        ← ✓ DONE

Phase 3 — Agent tool (needs Phase 1 + 2 complete)
  dirge-knk0  Debug tool                ← needs session + adapter resolution
  dirge-6gf8  Model-facing prompt       ← parallel with knk0
  dirge-1thn  First-pass scope          ← informational; guides knk0

Phase 4 — Polish (needs Phase 3 complete)
  dirge-x5hr  DAP↔LSP bridge            ← needs debug tool + LSP
  dirge-jjul  TUI debug panel           ← needs debug tool
  dirge-f9x1  Integration tests         ← needs debug tool
  dirge-9h8r  Per-language smoke        ← needs debug tool + adapters
  dirge-vckb  Docs                      ← can start anytime
  dirge-vlh6  CI wiring                 ← needs feature flag
```

### Session start checklist

```bash
# 1. Navigate to the worktree
cd ../dirge-dap

# 2. Pull latest upstream
git pull upstream main

# 3. Sync beads
bd dolt push

# 4. See what's available
bd ready | grep 'dap-'

# 5. Claim the first unblocked bead
bd update dirge-19k1 --claim

# 6. Implement, test, then close
bd close dirge-19k1

# 7. Repeat from step 4 for next bead
```
