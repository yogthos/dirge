# dirge

Minimal coding agent written in Rust, inspired by [pi](https://pi.dev/docs/latest/usage), [opencode](https://opencode.ai/), and [maki](https://github.com/tontinton/maki).

## Features

- **Multi-provider**: OpenRouter, OpenAI, Anthropic, Gemini, DeepSeek, GLM, Ollama, plus custom providers
- **Standard tools**: read, write, edit, bash, grep, find_files, glob, list_dir, write_todo_list, apply_patch
- **Line-numbered read output**: `read` tool prefixes each line with right-aligned line numbers (`123: content`)
- **Environment-aware**: system prompt includes OS, shell, working directory, and git branch for context
- **Semantic code tools** (tree-sitter): list_symbols, get_symbol_body, find_definition, find_callers, find_callees — supports TypeScript/TSX and Python
- **Claude-compatible skills**: discover skills from `.claude/skills/`, `.maki/skills/`, `.opencode/skills/`, `.dirge/skills/` directories. Agent can call the `skill` tool to load instructions on demand
- **Bash permissions** (tree-sitter): parses shell commands to split `&&`/`;`/`|` into individual segments, detects command substitution and complex constructs
- **Permission system**: four configurable modes with per-tool patterns, session allowlists, and external directory policies
- **Session management**: save/load/resume sessions, auto-compaction to stay within context windows
- **Terminal UI**: crossterm-based, markdown rendering, mouse selection/copy, scrollback, reasoning visibility toggle
- **Prompts system**: switch between system prompt modes at runtime (`code`, `plan`, `review`, `debug`, etc.)
- **Plan mode**: write restriction — when a plan/review prompt is active, writes and edits are restricted to `PLAN.md` only
- **Subagent support**: `task` tool spawns a subagent for research or general analysis subtasks
- **Memory tool**: persistent per-project memory store at `~/.dirge/memories/` — view, write, delete memories
- **MCP support**: connect MCP servers for extended tooling (optional compile-time feature)
- **Git Worktrees**: `/worktree` to create branch-per-task worktrees, `/wt-merge`, `/wt-exit`
- **Loop system**: iterative coding loop for long-horizon tasks
- **ACP support** (gated): Agent Communication Protocol server for editor integration
- **Plugin system** (Janet): embeddable scripting for custom workflows, hooks, and harness-driven agent orchestration

**NOTE**: Windows support is not tested, but feel free to try and open an issue if you encounter any bugs.

## Performance

_dirge_ is one of the smallest and most performant coding agents on the market.

- Lines of code: ~12k LoC
- Binary size: 12MB
- RAM footprint: ~8MB on an empty session, ~12MB when working (vs ~300MB for opencode or other JS-based coding agents)

### Tool result caching

Most tool calls (`read`, `write`, `edit`, `bash`, `grep`, `find_files`, `list_dir`) are cached per agent turn. Repeated calls with identical arguments within the same turn return cached results, avoiding redundant filesystem I/O. The cache clears automatically before each new prompt, and after `write`/`edit`/`bash` so a re-read sees fresh content.

### Error recovery

Transient API errors (network, rate limits) are automatically retried with exponential backoff (1s → 2s → 4s, max 3 retries) plus 0–25% jitter so concurrent agents don't retry in lockstep. Auth and unknown errors surface immediately. Context-length errors are not retried — surface a `/compress` hint instead. Stream events are buffered and only flushed on success, so retries don't duplicate tokens; if any tool calls were already dispatched (side effects applied), the partial buffer is flushed and the error is surfaced without retrying.

## Installation

```bash
# Default — MCP, loop, and git-worktree included
cargo install dirge

# With semantic code tools (tree-sitter: TS/TSX/Python/Bash)
cargo install dirge --features "semantic,semantic-ts,semantic-python,semantic-bash"

# With ACP (Agent Communication Protocol) support for editor integration
cargo install dirge --features acp

# All features
cargo install dirge --features "acp,loop,git-worktree,mcp,semantic,semantic-ts,semantic-python,semantic-bash,plugin"
```

### Optional: sandbox mode

Install [bubblewrap](https://github.com/containers/bubblewrap) for `--sandbox`, which runs every bash command inside an isolated environment:

```bash
# Debian/Ubuntu:  apt install bubblewrap
# Fedora:         dnf install bubblewrap
# Arch:           pacman -S bubblewrap
```

## Quick start

```bash
# Set your API key (OpenRouter is default)
export OPENROUTER_API_KEY="[api_key]"

# Interactive session (default prompt: code)
dirge

# One-shot mode
dirge -p "Explain this project"

# Continue last session
dirge -c

# Explicit provider/model
dirge --provider openrouter --model openai/gpt-4o

# DeepSeek and GLM are first-class providers
export DEEPSEEK_API_KEY="sk-..."
dirge --provider deepseek  # defaults to deepseek-v4-pro

export GLM_API_KEY="..."
dirge --provider glm       # defaults to glm-4
```

## Slash commands

| Command | Description |
|---------|-------------|
| `/model [name]` | Show or switch model |
| `/prompt [name]` | List or activate prompts (`code`, `plan`, `review`, etc.) |
| `/clear` | Clear conversation |
| `/cd [path]` | Change working directory |
| `/undo` | Undo last exchange |
| `/compress` | Compress conversation history |
| `/mode [mode]` | Set security mode (`standard`, `restrictive`, `accept`, `yolo`) |
| `/reasoning` | Toggle reasoning visibility |
| `/btw <question>` | Ask a quick question (no tools, doesn't affect session) |
| `/sessions` | List/save/load sessions |
| `/loop [prompt]` | Start iterative coding loop |
| `/worktree <name>` | Create a git worktree on branch |
| `/wt-merge [branch]` | Merge worktree branch |
| `/wt-exit` | Exit worktree |
| `/toggle` | Toggle features on/off (currently todo tools) |
| `/regen-prompts` | Restore built-in prompts |
| `/mcp` | List MCP servers and tools |
| `/quit` | Exit dirge |
| `/retry` | Retry last prompt |
| `/help` | Show all commands |

### Key bindings

**Input editing**

| Key | Action |
|-----|--------|
| Ctrl+A / Home | Start of line |
| Ctrl+E / End | End of line |
| Ctrl+B / Left | Char left |
| Ctrl+F / Right | Char right |
| Option+Left / Meta+B | Skip to previous word |
| Option+Right / Meta+F | Skip to next word |
| Ctrl+K | Kill to end of line |
| Ctrl+U | Kill to start of line |
| Ctrl+W | Kill word before cursor |
| Meta+Backspace | Delete word before cursor |
| Meta+D | Delete word after cursor |
| Ctrl+Y | Yank (paste) last kill |
| Meta+Y | Yank-pop (cycle kill ring after yank) |
| Ctrl+N / Down | History next (multi-line: next logical line, history at boundary) |
| Ctrl+P / Up | History previous (multi-line: previous logical line, history at boundary) |
| Shift+Enter / Meta+Enter / Ctrl+J | Insert newline (input box expands; Ctrl+J works in any terminal) |
| Tab | Insert 2 spaces |
| `@<query>` | File picker (Tab/Enter select, Esc cancel) |
| Paste (≥4 lines) | Collapses to `[N lines pasted]`; re-paste same content to expand inline |

**Agent control**

| Key | Action |
|-----|--------|
| Ctrl+C / Ctrl+D / Esc | Interrupt running agent |
| Esc-Esc (idle) | Open rewind picker (truncate history) |
| Ctrl+F | Search chat buffer |
| Ctrl+R | Toggle reasoning visibility |
| PgUp/PgDn | Scroll chat history |
| Home/End | Jump to top/bottom |
| `! cmd` | Run shell command (visible, injected into chat) |
| `!! cmd` | Run shell command (invisible) |
| Mouse drag | Select text (copies to clipboard on release) |
| (input) | Live token count shown next to input bar (`N tk`) |

**Tool output display**

| Feature | Detail |
|---------|--------|
| Tool results visible | Default on (`show_tool_details: true`), toggle in config |
| Smart truncation | Outputs >500 chars truncated with `[N more chars]` indicator (`tool_result_max_chars` in config) |
| Colorized edit diffs | `edit` tool results render with `-` (red), `+` (green), `@@` (cyan) coloring (`show_edit_diff: true` in config) |

## Prompts system

Built-in prompts that change the agent's behavior and tone:

| Prompt | Description |
|--------|-------------|
| **`code`** (default) | Coding mode with full tool access, TDD workflow |
| **`plan`** | Planning-only mode — explores and produces a plan (writes restricted to `PLAN.md`) |
| **`review`** | Code review mode — reviews for correctness, design, testing, and impact |
| **`debug`** | Debug mode — finds root cause before proposing fixes |
| **`ask`** | Read-only mode — only read/grep/glob permitted, no writes or bash |
| **`brainstorm`** | Design-only mode — explores ideas and presents designs without code |
| **`frontend-design`** | Frontend design mode — distinctive, production-grade UI |
| **`review-security`** | Security review mode — finds exploitable vulnerabilities |
| **`simplify`** | Code simplification mode — refines for clarity without changing behavior |
| **`write-prompt`** | Prompt writing mode — creates and optimizes agent prompts |
| **`default`** | Default system prompt — the base built-in prompt |

Custom prompts can be placed in `$XDG_CONFIG_HOME/dirge/prompts/` as `.md` files.

The agent automatically loads `AGENTS.md` or `CLAUDE.md` from the project root, ancestor directories, and `~/.config/dirge/agent/AGENTS.md` as a global fallback. Use `-n` / `--no-context-files` to disable.

## Claude-compatible skills

Place skill directories in `.claude/skills/`, `.maki/skills/`, or `.opencode/skills/` in your project or home directory. Each skill is a directory containing `SKILL.md` with optional YAML frontmatter:

```markdown
---
name: my-skill
description: A helpful skill
---
# Instructions
Detailed skill content here.
```

Skills are auto-discovered at agent startup and listed in the `skill` tool description. The agent can call `skill "my-skill"` to load the full content on demand. Project skills override global skills by name.

## Plugin system (Janet)

When built with `--features plugin`, dirge embeds the [Janet](https://janet-lang.org) scripting language for harness-driven workflows. Plugins are Janet scripts placed in `~/.config/dirge/plugins/` or `./.dirge/plugins/` that define hooks called at specific points in the agent lifecycle:

| Hook | When | Context |
|------|------|---------|
| `on-init` | Session start | `{:model "..." :cwd "..." :provider "..."}` |
| `on-prompt` | Before LLM call | `{:prompt "..."}` |
| `on-response` | After LLM response | `{:response "..."}` |
| `on-tool-start` | Before tool execution | `{:tool "bash" :args {...}}` |
| `on-tool-end` | After tool completes | `{:output "..."}` |
| `on-error` | Error occurred | `{:error "..."}` |
| `on-complete` | Agent turn finishes | (no context — use on-response) |

### Harness APIs

Janet scripts access harness capabilities through built-in functions:

```janet
(harness/log "message")          # Log to stderr
(harness/get-cwd)                 # Get current working directory
(harness/set-phase :architect)    # Set workflow phase
(harness/request-prompt "text")   # Queue prompt for the LLM
(harness/store-response "text")   # Not called directly — harness stores automatically
```

### Workflow plugin

The built-in `workflow.janet` plugin automatically drives architect → implementor → review phases when a feature request is detected. It uses **inversion of control** — the harness drives the model through scripted phases rather than waiting for the model to decide:

1. **Architect phase**: plans code layout, produces mermaid diagrams, plans file structure, creates function stubs
2. **Implementor phase**: enforces TDD — write failing test first, then implement, verify green
3. **Review phase**: reviews all changes, finds bugs, runs tests, fixes failures

The plugin detects feature requests by matching patterns like "add a feature", "implement", "build a", etc., and then orchestrates the entire development cycle automatically.

### Custom plugins

Create `~/.config/dirge/plugins/my-plugin.janet`:

```janet
(defn on-prompt [ctx]
  (let [prompt (ctx :prompt)]
    (if (string/find "security" prompt)
      (string "SECURITY REVIEW — check for:"
              "\n- SQL injection"
              "\n- XSS vectors"
              "\n- Authentication bypass"))))
```

## LSP integration

When built with the `lsp` feature (on by default), dirge attaches Language Server Protocol clients to your project and surfaces compile-time diagnostics directly in the agent's tool output. After every `write` or `edit`, the LSP server gets a `didChange`, waits for a fresh diagnostic publish, and any ERRORs land in the tool result as a `<diagnostics file="...">` block — so the agent corrects compile errors on the same turn instead of writing broken code and discovering it later via `cargo check`.

| Tool | Effect |
|------|--------|
| `read`  | Fire-and-forget `didOpen` so the server has the file in memory by the time the agent edits it. No diagnostic block in `read` output. |
| `write` | After write: `didChange` + wait for diagnostics + append errors-block. |
| `edit`  | Same as `write`. |
| `lsp`   | Agent-facing tool that exposes `definition`, `references`, `hover`, `documentSymbol`, `workspaceSymbol`, `implementation`, `prepareCallHierarchy`, `incomingCalls`, `outgoingCalls`. 1-based coordinates. |

Built-in server set:

| Server id | Binary | Extensions |
|-----------|--------|------------|
| `rust` | `rust-analyzer` | `.rs` |
| `typescript` | `typescript-language-server --stdio` | `.ts`, `.tsx`, `.mts`, `.cts`, `.js`, `.jsx`, `.mjs`, `.cjs` |
| `pyright` | `pyright-langserver --stdio` | `.py`, `.pyi` |
| `clojure-lsp` | `clojure-lsp` | `.clj`, `.cljs`, `.cljc`, `.edn`, `.bb` |

Workspace root resolution is per-server: rust-analyzer walks past nested member crates to the workspace `Cargo.toml` declaring `[workspace]`; typescript stops at the nearest `package.json`/`tsconfig.json` and yields to deno when a `deno.json` is closer; pyright looks for `pyproject.toml`/`setup.py`/etc.; clojure-lsp looks for `deps.edn`/`project.clj`/`shadow-cljs.edn`/`bb.edn`/`.clj-kondo`.

Disable: `--no-lsp` flag or `{ "lsp": false }` in the config. Per-server overrides (custom command, env, init options) live in the config — see [CONFIG.md](CONFIG.md).

## Semantic code tools

When built with `--features "semantic,semantic-ts,semantic-python"`, dirge gains AST-powered code analysis:

| Tool | Description |
|------|-------------|
| `list_symbols` | List functions, classes, methods, interfaces, and type aliases in a file or project. Filter by kind. |
| `get_symbol_body` | Full source of a named symbol via precise byte-range extraction. |
| `find_definition` | Locate where a symbol is defined across the project. |
| `find_callers` | Find all call sites of a function/method (word-boundary regex, excludes definition site). |
| `find_callees` | Extract all function/method calls made within a symbol's body (tree-sitter query). |

Supports TypeScript/TSX and Python. Index is built lazily on first use and cached by file mtime.

## Bash permissions

When built with `--features semantic-bash`, dirge uses tree-sitter to parse shell commands and split them by `&&`, `;`, and `|` operators. Each segment is checked individually against permission rules. Complex constructs like command substitution (`$(...)`), subshells (`(...)`), and process substitution (`<(...)`) trigger a full-command permission prompt.

## Permission system

Four modes, from safest to most permissive:

1. **restrictive** (`-R`): every tool action prompts for approval
2. **standard** (default): safe commands auto-approved; writes and destructive operations ask
3. **accept-all** (`--accept-all`): auto-approves inside working directory; external paths prompt
4. **yolo** (`--yolo`): auto-approves everything without prompting

Session allowlists persist approvals for the session. Doom-loop detection triggers after 3+ identical calls.

## Configuration

See [CONFIG.md](CONFIG.md) for config file location, accepted keys, provider aliases, permission rules, and MCP server configuration.

## Supported providers

- OpenRouter (default)
- OpenAI
- Anthropic
- Gemini
- DeepSeek — OpenAI-compatible, `DEEPSEEK_API_KEY` env var
- GLM (ZhipuAI) — OpenAI-compatible, `GLM_API_KEY` env var
- Ollama
- Custom — any OpenAI-compatible endpoint

Custom providers can be configured in `$XDG_CONFIG_HOME/dirge/config.json`:

```json
{
  "custom_providers": {
    "my-provider": {
      "provider_type": "openai",
      "base_url": "https://api.example.com/v1",
      "api_key_env": "MY_API_KEY"
    }
  }
}
```

## License

GPL-3.0-only

## Acknowledgements

This project builds on and is deeply indebted to:

- [**zerostack**](https://github.com/gi-dellav/zerostack) by Giuseppe Della Vedova — the original minimal coding agent that dirge was forked from. Provides the core agent architecture, permission system, TUI, and prompt infrastructure.
- [**maki**](https://github.com/tontinton/maki) by Tony Solomonik — a feature-rich Rust coding agent. The Claude-compatible skills system, bash tree-sitter permissions, memory tool, bang commands (`!`/`!!`), `/cd` command, `/btw` query, rewind picker, and task/subagent tool were all ported from maki.
