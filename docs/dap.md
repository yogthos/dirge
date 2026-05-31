# DAP — Debug Adapter Protocol

When built with the `dap` feature (opt-in), dirge attaches Debug Adapter Protocol
clients to your programs and provides a `debug` agent tool for launch, attach,
breakpoints, stepping, expression evaluation, and stack/variable inspection.

Enable it in `Cargo.toml` or at build time:

```bash
cargo build --features dap
```

## The `debug` tool

The agent gets one `debug` tool with 16 actions. Each action maps to standard
DAP requests — the agent selects the right action for the job.

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

Optional args: `condition` (conditional breakpoints), `context` (eval context: watch/repl/hover),
`levels` (stack frame count), `timeout` (5–300s, default 30), `stop_on_entry`
(launch), `restart` (disconnect with restart).

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

When the agent calls `debug launch` without an explicit `adapter` argument,
dirge auto-detects the right adapter from the program's file extension:

- `.py` → `debugpy`
- `.go` → `dlv`
- `.rs` → `lldb-dap` (falls back to `gdb` if lldb-dap not found)
- `.js`/`.ts` → `js-debug-adapter`
- `.rb` → `rdbg`
- `.java` → `jdtls-debug`
- Extensionless binaries → `lldb-dap` > `gdb` > `codelldb`

Explicit adapter selection: `{ "adapter": "gdb" }` bypasses auto-detection.

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

Missing binaries are surfaced as a clear error ("adapter not found on PATH")
rather than a cryptic spawn failure.

## Configuration

Adapter commands are resolved via `which` (PATH lookup). Override the command
or add arguments per adapter in `config.json`:

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

Adapter config keys must match the adapter names in the defaults table above.
When an adapter config is present, its `command` + `args` replace the built-in
defaults entirely. `cwd` resolution and `launch_defaults`/`attach_defaults`
merging still applies.

Disable DAP entirely: omit `dap` from the feature flags (it's opt-in).

## Adapter defaults

Each adapter ships with `launch_defaults` and `attach_defaults` that are
merged into the DAP request arguments. Examples:

- **debugpy**: `justMyCode: false` (always show library frames), `stopOnEntry: true`
- **gdb**: `stopAtBeginningOfMainSubprogram: true`
- **dlv**: `mode: "debug"` for launch, `mode: "local"` for attach
- **elixir-ls-debugger**: `type: "mix_task"`, `task: "run"`

The agent can override any default by passing the corresponding argument in
the tool call — agent-supplied values always win over defaults.

## Session model

- **Single active session**: launching a new debug session terminates any
existing one. Attach behaves the same way.
- **Breakpoint cache**: dirge tracks breakpoints per file locally so the
agent can query "what breakpoints do I have?" without a DAP round-trip.
- **Output capture**: program stdout/stderr from DAP `output` events is
accumulated (up to 128 KB) and surfaced in `continue` outcomes.
- **Timeout**: every operation has a configurable timeout (5–300s, default
30s). Operations that race against stop events (continue, step) use the
timeout as a ceiling.

## Agent usage patterns

### Crash investigation

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

### Run to cursor (with LSP)

```
debug launch { program: "test.py" }
debug set_breakpoints { file: "src/auth.py", line: 87 }
debug continue
→ stopped at src/auth.py:87

# LSP provides diagnostics on the current file
lsp hover { file: "src/auth.py", line: 87, character: 5 }
→ type info at cursor
```

### Conditional breakpoints

```
debug set_breakpoints {
  file: "src/loop.rs",
  line: 128,
  condition: "i > 1000"
}
debug continue
→ stops only when i > 1000
```

### Attach to running process

```
debug attach { pid: 89342 }
→ attached to pid 89342

debug threads
→ list of threads

debug stack_trace { thread_id: 1 }
→ current call stack
```

### Edit and continue

```
debug launch { program: "server.py" }
→ stopped at entry

debug set_breakpoints { file: "src/handler.py", line: 55 }
debug continue
→ stopped at src/handler.py:55 (exception)

# Agent fixes the bug with edit tool
edit { file: "src/handler.py", ... }

debug terminate { restart: true }
→ disconnect with restart

debug launch { program: "server.py" }
debug continue
→ runs with fix applied
```

## Limitations

- **Socket-mode adapters**: `dlv` and `codelldb` ship with `connect_mode: "socket"`
in the defaults but socket-mode transport is not implemented yet. These
adapters fail with a clear error. Use `lldb-dap` or `gdb` for Go/C/C++ for now.
- **No disassemble / memory read/write**: advanced DAP surface deferred to v2.
- **No data/instruction breakpoints**: source breakpoints only in v1.
- **Janet, Bash**: no built-in adapters exist for these languages. The agent
can still attach via `adapter: "custom-adapter"` if one is available on PATH.
- **Interactive TUI debug panel**: when a DAP session is active, toggle
`/panel debug` to see threads, frames, variables, breakpoints, and program
output in a right-side panel. `/panel auto` switches back to the system panel.
- **DAP↔LSP bridge**: the `debug` tool includes `run_to_cursor`,
`restart_frame`, `backtrace_diagnostics`, and `error_analysis` actions
(when both `dap` and `lsp` features are enabled).

## Example session transcript

```
User: debug this segfault in ./target/debug/parser

Agent: I'll launch the debugger and investigate.

[Tool: debug launch { program: "./target/debug/parser", args: ["input.txt"] }]

Launched with lldb-dap (session parser-1).
Program stopped: entry (thread 1)

[Tool: debug continue]

Execution stopped: signal SIGSEGV (thread 1)
Program output:
Reading input.txt...
Segmentation fault

[Tool: debug stack_trace { thread_id: 1, levels: 5 }]

Stack trace for thread 1 (5 frames):
[
  { "id": 0, "name": "parse_token", "source": { "path": "src/lexer.rs", "line": 234 } },
  { "id": 1, "name": "lex_next", "source": { "path": "src/lexer.rs", "line": 112 } },
  { "id": 2, "name": "parse_expr", "source": { "path": "src/parser.rs", "line": 87 } },
  { "id": 3, "name": "parse_file", "source": { "path": "src/parser.rs", "line": 42 } },
  { "id": 4, "name": "main", "source": { "path": "src/main.rs", "line": 15 } }
]

[Tool: debug evaluate { expression: "*token_ptr", frame_id: 0 }]

{ "result": "0x0", "type": "pointer", "variablesReference": 0 }

Agent: The crash is in `parse_token` at `src/lexer.rs:234`. The expression
`*token_ptr` evaluates to `0x0` — a null pointer dereference. The fix is
a null check before dereferencing `token_ptr`.
```
