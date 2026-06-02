# DAP — Debug Adapter Protocol

When built with the `dap` feature (opt-in), dirge attaches Debug Adapter Protocol
clients to your programs. Two interfaces are available: the `debug` agent tool
(the model drives it) and the `/debug` slash command (you drive it from the TUI).

Enable it in `Cargo.toml` or at build time:

```bash
cargo build --features dap
```

## Quick start

```
# 1. Start a conversation (initializes the debug session manager)
> hello

# 2. Launch a Python program
/debug launch src/tests/dap/fixtures/test_program.py

# 3. The right panel automatically switches to debug view (adapter, thread, stop reason)
# 4. Step through code
/debug step
/debug step_in
/debug evaluate "counter.value"

# 5. Continue to the next breakpoint
/debug continue

# 6. End the session
/debug terminate
```

Or use the Janet-powered `/dap-repl`:

```
/dap-repl launch src/tests/dap/fixtures/test_program.py
dap> bp src/tests/dap/fixtures/test_program.py 95
dap> c
dap> p "counter.value"
dap> n
dap> terminate
```

## Prerequisites

Install the debug adapter for your language:

| Language | Adapter | Install |
|----------|---------|---------|
| Python | debugpy | `pip install debugpy` |
| C/C++/Rust | gdb | `apt install gdb` (usually pre-installed) |
| C/C++/Swift/Rust/Zig | lldb-dap | `apt install lldb` or Xcode CLT |
| Go | dlv | `go install github.com/go-delve/delve/cmd/dlv@latest` |
| JS/TS | js-debug-adapter | `npm install -g js-debug-adapter` |
| Ruby | rdbg | bundled with Ruby 3.1+ |

## The `/debug` slash command

You control the debugger directly from the TUI. All subcommands are
tab-completable after `/debug `.

### Lifecycle

| Subcommand | What it does |
|------------|-------------|
| `/debug launch <file> [--adapter <name>]` | Start debugging a program. Adapter is auto-detected from extension. Stops on entry. |
| `/debug attach <pid> [--adapter <name>]` | Attach to a running process |
| `/debug terminate` | End the debug session |

### Execution control

| Subcommand | What it does |
|------------|-------------|
| `/debug continue` | Resume execution until next breakpoint or exit |
| `/debug step` | Step over current line (next) |
| `/debug step_in` | Step into function call |
| `/debug step_out` | Step out of current function |

### Inspection

| Subcommand | What it does |
|------------|-------------|
| `/debug sessions` | Show active session status, stop reason, thread ID |
| `/debug evaluate <expression>` | Evaluate an expression in the debuggee |
| `/debug bp <file> <line>` | Set a breakpoint |

### UI

| Subcommand | What it does |
|------------|-------------|
| `/debug panel` | Show the debug panel on the right (or use `/panel debug`) |

### Help

Type `/debug` with no subcommand to see the full usage summary.

### Breakpoints: two approaches

**Method 1 — `/debug bp` (DAP breakpoints, no file editing):**

```
/debug launch src/tests/dap/fixtures/test_program.py
/debug bp src/tests/dap/fixtures/test_program.py 99
/debug bp src/tests/dap/fixtures/test_program.py 107
/debug continue          → stops at line 99
/debug evaluate "number" → 42
/debug continue          → stops at line 107
/debug evaluate "doubled[:3]" → [2, 4, 6]
```

**Method 2 — `breakpoint()` in source:**

Add `breakpoint()` calls to your Python file. When the program hits them,
debugpy intercepts them as DAP stopped events — no raw pdb, no terminal
stealing. The program stops and you can inspect with `/debug evaluate`.

The test fixture at `src/tests/dap/fixtures/test_program.py` has five
numbered `breakpoint()` calls ready for step-through.

## The `debug` agent tool

The agent also gets a `debug` tool with 20 actions. Each action maps to
standard DAP requests — the agent selects the right action for the job.

| Action | Required args | What it does |
|--------|--------------|--------------|
| `launch` | `program` | Start a new debug session from a program |
| `attach` | — | Attach to a running process (pid/port) |
| `set_breakpoints` | `file`, `line` | Set a breakpoint in a source file |
| `remove_breakpoints` | `file` | Clear all breakpoints from a file |
| `continue` | — | Resume execution until next breakpoint or exit |
| `step_over` | `thread_id` | Execute next line, stepping over function calls |
| `step_in` | `thread_id` | Step into the next function call |
| `step_out` | `thread_id` | Step out of the current function |
| `pause` | — | Pause execution of a running program |
| `evaluate` | `expression` | Evaluate an expression in the debuggee |
| `stack_trace` | `thread_id` | Get the call stack for a thread |
| `threads` | — | List all threads |
| `scopes` | `frame_id` | Get variable scopes for a stack frame |
| `variables` | `variable_ref` | Get variables within a scope |
| `terminate` | — | Terminate the debuggee |
| `sessions` | — | Show active debug session info |
| `run_to_cursor` | `file`, `line` | Set bp at line, continue, show LSP hover at stop :zap: |
| `restart_frame` | `frame_id` | Re-execute current frame (edit-and-continue) :zap: |
| `backtrace_diagnostics` | `thread_id` | Stack trace with LSP diagnostics per frame :zap: |
| `error_analysis` | `thread_id` | Stack trace with error diagnostics + suggested breakpoints :zap: |

Optional args: `condition` (conditional breakpoints), `context` (eval context:
watch/repl/hover), `levels` (stack frame count), `timeout` (5–300s, default
30), `stop_on_entry` (launch), `restart` (disconnect with restart).

:zap: requires both `dap` and `lsp` features.

### Agent usage examples

**Crash investigation:**

```
debug launch { program: "./buggy_binary" }
→ stopped at entry

debug set_breakpoints { file: "src/main.rs", line: 42 }
debug continue
→ stopped at breakpoint (thread 1)

debug stack_trace { thread_id: 1 }
→ 5 frames, exception at frame 0

debug variables { variable_ref: 1000 }
→ local variables at crash site
```

**Run to cursor (DAP:LSP bridge):**

```
debug run_to_cursor { file: "src/auth.py", line: 87 }
→ stopped at src/auth.py:87
→ Hover info at src/auth.py:87: { "type": "str", ... }
```

**Conditional breakpoints:**

```
debug set_breakpoints {
  file: "src/loop.rs",
  line: 128,
  condition: "i > 1000"
}
debug continue
→ stops only when i > 1000
```

**Attach to running process:**

```
debug attach { pid: 89342 }
→ attached to pid 89342

debug threads
→ list of threads

debug stack_trace { thread_id: 1 }
→ current call stack
```

## Built-in adapter set

| Adapter | Binary | Languages | Extensions |
|---------|--------|-----------|------------|
| `lldb-dap` | `lldb-dap` | C, C++, ObjC, Swift, Rust, Zig | `.c`, `.cc`, `.cpp`, `.cxx`, `.m`, `.mm`, `.swift`, `.rs`, `.zig` |
| `gdb` | `gdb -i dap` | C, C++, Rust | `.c`, `.cc`, `.cpp`, `.cxx`, `.h`, `.hh`, `.hpp`, `.hxx`, `.rs` |
| `codelldb` | `codelldb --port 0` | C, C++, Rust, Zig | `.c`, `.cc`, `.cpp`, `.cxx`, `.rs`, `.zig` |
| `debugpy` | `python -m debugpy.adapter` | Python | `.py` |
| `dlv` | `dlv dap` | Go | `.go` |
| `js-debug-adapter` | `js-debug-adapter` | JavaScript, TypeScript | `.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.cjs` |
| `rdbg` | `rdbg --open --command --` | Ruby | `.rb`, `.rake`, `.gemspec` |
| `elixir-ls-debugger` | `elixir-ls-debugger` | Elixir | `.ex`, `.exs`, `.heex`, `.eex` |
| `jdtls-debug` | `jdtls` | Java | `.java` |
| `clojure-lsp-debug` | `clojure-lsp-debug` | Clojure | `.clj`, `.cljs`, `.cljc`, `.edn` |

### Adapter auto-detection

When the agent calls `debug launch` (or you use `/debug launch`) without an
explicit `adapter` argument, dirge auto-detects the right adapter from the
program's file extension:

- `.py` -> `debugpy`
- `.go` -> `dlv`
- `.rs` -> `lldb-dap` (falls back to `gdb` if lldb-dap not found)
- `.js`/`.ts` -> `js-debug-adapter`
- `.rb` -> `rdbg`
- `.java` -> `jdtls-debug`
- Extensionless binaries -> `lldb-dap` > `gdb` > `codelldb`

Explicit adapter selection: `/debug launch foo.py --adapter debugpy`.

### Root marker detection

For projects without an obvious entry point (e.g. extensionless binaries),
dirge checks the working directory for root markers:

| Adapter | Root markers |
|---------|-------------|
| Rust / lldb-dap | `Cargo.toml` |
| C/C++ / gdb | `Makefile`, `CMakeLists.txt`, `compile_commands.json` |
| Python / debugpy | `pyproject.toml`, `setup.py`, `requirements.txt` |
| Go / dlv | `go.mod`, `go.sum` |
| JS/TS | `package.json`, `tsconfig.json` |

## Implementation details

### Terminal isolation

The debug adapter (and its debuggee) runs in its own session with no
controlling terminal. This prevents the adapter from calling `tcsetpgrp()`
to steal the foreground, which would SIGTTOU-stop dirge and corrupt the TUI.
The isolation is done via `setsid()` in `spawn_stdio` — `/dev/tty` opens
fail with ENXIO and `tcsetpgrp()` is rejected.

Additionally, `"console": "internalConsole"` is set in debugpy's launch
defaults to tell debugpy not to try setting up a TTY for the debuggee.

### Launch runs in background

`/debug launch` spawns the adapter handshake + initial stop on a
`tokio::spawn` task. The slash command returns immediately after printing
"launching..." and switching the right panel to debug mode. This keeps the
TUI responsive even if the adapter takes seconds to initialize.

### Session model

- **Single active session**: launching a new debug session terminates any
  existing one. Attach behaves the same way.
- **Breakpoint cache**: dirge tracks breakpoints per file locally so the
  agent can query "what breakpoints do I have?" without a DAP round-trip.
- **Output capture**: program stdout/stderr from DAP `output` events is
  accumulated (up to 128 KB) and surfaced in `continue` outcomes.
- **Timeout**: every operation has a configurable timeout (5–300s, default
  30s). Operations that race against stop events (continue, step) use the
  timeout as a ceiling.
- **DAP manager lifetime**: `DAP_MANAGER` is initialized when the first
  conversation starts (the `debug` tool constructor creates the singleton).
  Before that, `/debug` subcommands that need a session return "start a
  conversation first".

### Janet FFI bridge and plugins

When built with both `dap` and `plugin` features, dirge exposes the DAP
session manager to Janet plugins through a FFI bridge (`src/dap/janet_bindings.rs`).
Plugins can call 12 DAP functions directly — no agent middleman needed.

**Janet FFI functions:**

| Janet function | Args | What it does |
|---|---|---|
| `(dap/launch file adapter?)` | file path, optional adapter name | Spawn adapter, launch debuggee |
| `(dap/attach pid adapter?)` | process ID, optional adapter name | Attach to running process |
| `(dap/step)` | — | Step over current line |
| `(dap/step-in)` | — | Step into function call |
| `(dap/step-out)` | — | Step out of current function |
| `(dap/continue)` | — | Resume execution |
| `(dap/bp file line)` | file path, line number | Set breakpoint |
| `(dap/eval expr)` | expression string | Evaluate in debuggee |
| `(dap/stack-trace)` | — | Get call stack (JSON) |
| `(dap/threads)` | — | List threads (JSON) |
| `(dap/sessions)` | — | Active session summary (JSON) |
| `(dap/vars var-ref)` | variablesReference number | Drill into scope variables |
| `(dap/terminate)` | — | End debug session |
| `(dap/available?)` | — | Feature detection predicate |
| `(dap/session-active?)` | — | True when a session is active |

Architecture: plugin calls Janet FFI function → C function extracts args,
builds `DapCommand`, sends via thread-local `DAP_TX` (tokio `UnboundedSender`)
→ `spawn_dap_bridge()` tokio task → `DapSessionManager` async methods
→ JSON result back via std `mpsc` channel → Janet string (or nil on error).
Follows the same channel-bridge pattern as `harness/confirm` and `harness/lsp`
in `src/plugin/worker.rs`.

**Bundled Janet plugins:**

| Plugin | Slash command | What it does |
|--------|-------------|-------------|
| `dap_repl.janet` | `/dap-repl` | gdb-like interactive debug sub-mode (launch, step, continue, bp, eval, bt, sessions, terminate) |
| `dap_profiler.janet` | `/dap-profile start <interval-ms>` | Statistical sampling profiler — periodic `dap/stack-trace` → per-function aggregation → top-20 hotspot report |
| `dap_watch.janet` | `/dap-watch add <expr>` | Expression watchpoints — evaluates registered expressions via `dap/eval` after every stop |
| `dap_context.janet` | (auto) | Auto-injects rich debug context (session summary, stack trace, inspect hints) after every DAP stop via `on-tool-end` hook |

**Quick start with `/dap-repl`:**

```
/dap-repl launch src/tests/dap/fixtures/test_program.py
dap> bp src/tests/dap/fixtures/test_program.py 95
dap> c
dap> bt                    # full stack trace
dap> p "counter.value"     # evaluate expression
dap> n                     # step over
dap> p "counter.value"     # see value change
dap> terminate
```

**Dirge-debugging-dirge via attach:**

```
# Terminal 1: normal dirge session (the target)
dirge
> hello

# Terminal 2: controlling dirge with DAP
dirge --features dap,plugin
> hello
/dap-repl attach 12345 --adapter lldb-dap
dap> bp src/dap/session.rs 277
dap> c             # dirge in terminal 1 resumes
# ... interact in terminal 1; breakpoint hits in terminal 2 ...
dap> bt            # stack trace at breakpoint
dap> terminate
```

Requires `kernel.yama.ptrace_scope=0` or launching the target dirge
via `lldb-dap` directly (which sidesteps ptrace restrictions).

### TUI debug panel

The right panel shows live session state (adapter name, status, stop reason,
thread ID) updated each UI tick from `DAP_MANAGER.debug_snapshot()`. Switch
to it with `/panel debug` or `/debug panel`. It auto-shows on `/debug launch`.

## Configuration

Override adapter commands per adapter in `config.json`:

```json
{
  "dap": {
    "debugpy": {
      "command": "/home/user/venv/bin/python",
      "args": ["-m", "debugpy.adapter", "--log-to-stderr"]
    },
    "gdb": {
      "command": "/opt/gdb-15/bin/gdb"
    }
  }
}
```

## Limitations

- **Socket-mode adapters**: `dlv` and `codelldb` ship with `connect_mode:
  "socket"` in the defaults but socket-mode transport is not implemented
  yet. These adapters fail with a clear error. Use `lldb-dap` or `gdb` for
  Go/C/C++ for now.
- **No disassemble / memory read/write**: not implemented in the DAP types yet.
- **Single session only**: only one debug session can be active at a time.
  Launching a new session terminates the previous one.
- **No inline variable display in editor**: the DAP panel shows variables
  in a table but there's no source-level data view (VS Code-style hover or
  inline values) in the TUI.

## Full worked example (Python)

```
# Terminal 1: start dirge
$ cargo run --features dap

# In the TUI:
> hello, I need to debug test_program.py

/debug launch src/tests/dap/fixtures/test_program.py
# → "launching src/tests/dap/fixtures/test_program.py with adapter debugpy..."
# → "  (launch runs in background — use /debug sessions to check result)"
# → right panel switches to debug view
# → "Session dap-1 (debugpy) — Stopped, Stop reason: entry (thread 1)"

/debug evaluate "mapping"
# → mapping = {"key_a": 100, "key_b": 200}

/debug bp src/tests/dap/fixtures/test_program.py 107
# → set 1 breakpoint(s), line 107 — verified: true

/debug continue
# → continue → Stopped (stop reason: breakpoint)
# → Program output: text = Hello, DAP!\nnumber = 42\nHello, World!\n

/debug evaluate "doubled[:5]"
# → doubled[:5] = [2, 4, 6, 8, 10]

/debug step
# → stopped — reason: step, thread: 1

/debug evaluate "fact"
# → fact = 120

/debug continue    # hits the next breakpoint()

/debug evaluate "counter.value"
# → counter.value = 12

/debug terminate
# → debug session terminated. exit code: none
```

## Full worked example (C)

```
# Compile the fixture first (one-time)
$ gcc -g src/tests/dap/fixtures/test_program.c -o src/tests/dap/fixtures/test_program_c

# In the TUI:
> debug test_program_c

/debug launch src/tests/dap/fixtures/test_program_c --adapter lldb-dap
# → right panel switches to debug view
# → "Session dap-2 (lldb-dap) — Stopped"

/debug bp src/tests/dap/fixtures/test_program.c 149
# → set 1 breakpoint(s), line 149 — verified: true

/debug continue
# → stopped at breakpoint
# → Program output: number = 42\npi = 3.14159...\n...

/debug evaluate "conn.adapter.name"
# → "\"debugpy\""

/debug evaluate "conn.counter.value"
# → 10

/debug evaluate "conn.last_error"
# → ERR_TIMEOUT

/debug step
# → stopped — reason: step, thread: 213354

/debug evaluate "c.value"
# → 10

/debug terminate
```

## Full worked example (Rust)

```
# Compile the fixture first (one-time)
$ rustc -g src/tests/dap/fixtures/test_program.rs -o src/tests/dap/fixtures/test_program_rs

# In the TUI:
> debug test_program_rs

/debug launch src/tests/dap/fixtures/test_program_rs --adapter lldb-dap
/debug bp src/tests/dap/fixtures/test_program.rs 124
/debug continue
# → stopped at breakpoint

/debug evaluate "counter.value"
# → 10

/debug evaluate "adapter.name"
# → "debugpy"

/debug evaluate "last_error"
# → Timeout(30)

/debug terminate
```

## Full worked example (Python)
