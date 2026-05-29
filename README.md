# dirge

Minimal coding agent written in Rust, inspired by [pi](https://pi.dev/docs/latest/usage), [opencode](https://opencode.ai/), and [maki](https://github.com/tontinton/maki).

## Features

- **Multi-provider**: OpenRouter, OpenAI, Anthropic, Gemini, DeepSeek, GLM, Ollama, plus custom OpenAI-compatible endpoints. A single `providers` map declares aliases; top-level role keys (`provider`, `review_provider`, `escalation_provider`, `summarization_provider`, `subagent_provider`) point each role at one of those aliases. See [Configuration](#configuration).
- **Standard tools**: read, write, edit, bash, grep, find_files, glob, list_dir, write_todo_list, apply_patch, repo_overview, session_search, webfetch, websearch, question, memory, skill, task, task_status, tool_search
- **Line-numbered read output**: `read` tool prefixes each line with right-aligned line numbers (`123: content`)
- **Environment-aware**: system prompt includes OS, shell, working directory, and git branch for context
- **Semantic code tools** (tree-sitter): list_symbols, get_symbol_body, find_definition, find_callers, find_callees — supports TypeScript/TSX, Python, Clojure (clj/cljs/cljc/edn/bb), Go, Ruby, Rust, Java, C, and C++
- **Claude-compatible skills**: discover skills from `.claude/skills/`, `.opencode/skills/`, `.dirge/skills/` directories. Agent can call the `skill` tool to load instructions on demand
- **Bash permissions** (tree-sitter): parses shell commands to split `&&`/`;`/`|` into individual segments, detects command substitution and complex constructs
- **Permission system**: four configurable modes with per-tool patterns, session allowlists, and external directory policies
- **Session management & compaction**: save/load/resume sessions with lineage-aware session search. `/compress` runs an LLM-summarization compaction pass (via the auxiliary `summarization_provider`) that folds the conversation middle into a structured summary. Automatic turn-boundary compaction keeps the runtime within the context window without an LLM call: it caps oversized tool results (per-result token cap) and proactively folds when the context-ratio crosses a high threshold, on top of tool-output pruning.
- **Terminal UI**: crossterm-based, markdown rendering, soft-wrapping input box, mouse selection/copy, scrollback, reasoning visibility toggle
- **Info panel**: optional right-hand sidebar showing cwd, MCP/LSP server status, pending todos, and recently-modified files. Auto-shown at ≥100 cols; toggle via `/panel`
- **Mid-execution interjection**: type while the agent is running to queue a follow-up message — the runner stops at the next tool-result boundary so it's picked up promptly instead of waiting for the whole multi-turn run. `Ctrl+X` drops queued messages, `Ctrl+C` cancels both the run and the queue
- **Prompts system**: switch between system prompt modes at runtime (`code`, `plan`, `review`, `debug`, etc.)
- **Per-prompt tool restrictions**: each prompt (`prompts/<name>.md`) can declare a `deny_tools` frontmatter list. The permission checker refuses those tools while the prompt is active — replaces the previous prose-based "plan mode" gate with a real security boundary. `plan`, `review`, and `review-security` ship with `edit`, `write`, `apply_patch`, `bash`, and `webfetch` denied
- **Subagent support**: `task` tool spawns a subagent for research or general analysis subtasks
- **Memory & self-improvement**: persistent per-project memory at `.dirge/memory/` (`MEMORY.md` facts + `PITFALLS.md`), injected into the system prompt as a frozen snapshot. The agent edits it via the `memory` tool. After an idle session, a unified post-session orchestrator runs (in order, fire-and-forget): a background review that extracts learnings into memory + skills, a skills curator and a memory curator (stale-detection + lifecycle + LLM consolidation, with audit reports under each store's `.curator_reports/`), and a cross-session pass that promotes sub-threshold patterns recurring across past sessions.
- **MCP support**: connect MCP servers for extended tooling (optional compile-time feature)
- **Git Worktrees**: `/worktree` to create branch-per-task worktrees, `/wt-merge`, `/wt-exit`
- **Loop system**: iterative coding loop for long-horizon tasks
- **ACP support** (gated): Agent Communication Protocol server for editor integration. ACP locks the active prompt at launch — use `--prompt <name>` on startup to opt into a restricted mode (the protocol has no mid-session prompt-switch message)
- **Plugin system** (Janet, on a dedicated worker thread): hooks across the full session/agent/tool lifecycle. Plugins can intercept tool calls (block/mutate/replace), register slash commands / tools / keyboard shortcuts / LLM providers, augment the system prompt (`before-agent-start`), transform the message context before each LLM call (`transform-context`), rewrite finalized assistant messages (`message-end`), supply custom compaction summaries (`on-compact`), post notifications, and prompt the user with blocking `confirm`/`select` dialogs. See [`docs/plugins.md`](docs/plugins.md).

### Robust agent loop

Hardening against the failure modes that plague long sessions and weaker models. See [`docs/agent-loop.md`](docs/agent-loop.md) for the architecture and [`docs/tool-input-repair.md`](docs/tool-input-repair.md) for the repair layer.

- **Tool-input repair layer**: catches and fixes common malformed tool calls before they hit the tool — strips `null` optional fields, parses JSON-string arrays, unwraps markdown links in path fields, applies relational defaults declared in the tool's schema. Failed repairs emit a structured `tool_input_invalid` log with the original args.
- **Schema-aware contract hints** (`dirge-hints`): per-tool schemas can declare `semantic: "absolute_path"`, `relational: [{requires, defaults}]`, etc. The repair layer reads these to drive automatic defaults + agent-facing `Note:` text — removing per-tool hardcoded heuristics.
- **Tree-sitter pre-write validation**: every `write` / `edit` / `apply_patch` is parsed through the matching tree-sitter grammar before bytes hit disk. Syntactically-broken code is rejected with line/column-precise errors so the model corrects it on the same turn. Languages: Rust, TS/TSX, Python, Go, Ruby, Java, C, C++, Clojure, Bash (each gated on its `semantic-<lang>` feature).
- **Dynamic `tool_search`** (opt-in via `dynamic_tool_search: true`): ships only `tool_search` + a small always-on set in each request; the model calls `tool_search(query)` to discover and load more tools on demand. ~30% token savings on MCP-heavy sessions.
- **Disk-backed large-output relay**: `bash` / `webfetch` outputs over an inline budget (default 8 KiB) are written to `~/.dirge/transient/<pid>/<tool>-<ts>.txt` and replaced with a head + ellipsis + tail summary plus a hint to `read` for specifics. Aged cleanup runs on every relay write.
- **Anthropic prompt-cache positioning**: system prompt + tool defs sit at the start of every request (cache-warm prefix); a `prompt_cache_prefix` tracing event emits per-turn with stable hashes so unexpected prefix drift is observable.
- **Dual-client tiering** (`escalation_provider` role): when a tool input fails to repair OR generated code fails the tree-sitter pre-write check, the next model call is routed through a more capable provider. One-shot per failure, capped at 3 per session, surfaced as a dim `↑ escalating to <provider>` status line.
- **Context-depth reminders** (`context_depth_reminder_threshold`): tracks consecutive turns that touch the same file(s); when the streak crosses the threshold (default 8, opt-in), injects a single mid-turn reminder restating the active task + touched files so long runs don't drift.
- **Tool-loop circuit breaker**: per-tool-call repeat counter trips on the 3rd identical `(tool, input)` within a 32-call window — catches non-progressing loops without needing model cooperation.

**NOTE**: Windows support is not tested, but feel free to try and open an issue if you encounter any bugs.

## Performance

_dirge_ is one of the smallest and most performant coding agents on the market.

- Lines of code: ~100k LoC
- Binary size: 25MB
- RAM footprint: ~8MB on an empty session, ~15MB when working (vs ~300MB for opencode or other JS-based coding agents)

### Tool result caching

Most tool calls (`read`, `write`, `edit`, `bash`, `grep`, `find_files`, `list_dir`) are cached per agent turn. Repeated calls with identical arguments within the same turn return cached results, avoiding redundant filesystem I/O. The cache clears automatically before each new prompt, and after `write`/`edit`/`bash` so a re-read sees fresh content.

### Error recovery

Transient API errors (network, rate limits, Anthropic `overloaded_error`) are automatically retried with exponential backoff (1s → 2s → 4s, max 3 retries) plus 0–25% jitter so concurrent agents don't retry in lockstep. Auth and unknown errors surface immediately. Context-length errors are not retried — surface a `/compress` hint instead. Tokens stream live to the chat as they arrive; if a retry fires, the user sees an "(error: …; retrying)" banner and the next attempt's tokens stream in fresh. If any tool calls were already dispatched (side effects applied), the error is surfaced without retrying so a partial-but-applied turn isn't re-run.

## Installation

```bash
# Default — MCP, loop, and git-worktree included
cargo install dirge

# With semantic code tools (tree-sitter)
cargo install dirge --features "semantic,semantic-ts,semantic-python,semantic-bash,semantic-clojure,semantic-go,semantic-ruby,semantic-rust,semantic-java,semantic-c,semantic-cpp"

# With ACP (Agent Communication Protocol) support for editor integration
cargo install dirge --features acp

# All features
cargo install dirge --features "acp,loop,git-worktree,mcp,semantic,semantic-ts,semantic-python,semantic-bash,semantic-clojure,semantic-go,semantic-ruby,semantic-rust,semantic-java,semantic-c,semantic-cpp,plugin"
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

# Verbose mode — debug-level dirge logs + warn-level plugin hook
# errors (useful when authoring a plugin or filing a bug report).
# RUST_LOG env still takes precedence if set.
dirge --verbose

# Pass an API key inline (one-off testing, CI). `--api-key` is
# visible to other processes via the process list (`ps`) and emits
# a startup warning. Prefer one of:
dirge --provider openai --api-key-file /run/secrets/openai_key
pass openai-key | dirge --provider openai --api-key-stdin
# or set the provider's env var (e.g. OPENAI_API_KEY) before launch.
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
| `/tree [id-prefix]` | Show session tree; with prefix, switch the active branch to that leaf |
| `/fork [id-prefix]` | Branch off the chosen message (default: last user message) and restore its text to the editor |
| `/clone <id-prefix>` | Switch the active branch to the entry without restoring text |
| `/loop [prompt]` | Start iterative coding loop |
| `/worktree <name>` | Create a git worktree on branch |
| `/wt-merge [branch]` | Merge worktree branch |
| `/wt-exit` | Exit worktree |
| `/toggle` | Toggle features on/off (currently todo tools) |
| `/regen-prompts` | Restore built-in prompts |
| `/mcp` | List MCP servers and tools |
| `/panel [on\|off\|auto]` | Toggle the right-hand info panel (cwd, MCP, LSP, todos, modified files). `auto` shows it when the terminal is at least 100 cols wide. |
| `/allow <list\|add\|remove\|clear>` | Manage the session permission allowlist (see `/help` for argument shapes) |
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
| Ctrl+C / Ctrl+D / Esc | Interrupt running agent (also clears queued interjections) |
| Type while running | Queues your message; runs after the current turn finishes. The runner also stops at the next tool-result boundary so the message is picked up quickly instead of waiting for the whole multi-turn run. Status line shows `q:N` for pending count. |
| Ctrl+X | Drop the most-recently-queued interjection |
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
| 4-line collapse | Tool result bodies default to the first 4 lines + a dim `↓ N more lines (Ctrl+O to expand)` footer. Configurable via `tool_result_max_lines` (default `4`). Exempt tools — body IS the value — render unchanged: `edit` (colorized diff), `read`, `question`, `task`, `task_status`. |
| Ctrl+O to expand | Re-prints the most-recent collapsed tool result in full as a fresh chamber. Press again to re-emit. The stash resets on every new user prompt and on context-overflow auto-recovery. |
| Hard char cap | On top of the line cap, `tool_result_max_chars` (default `500`) trims a single pathological line so a 10 MB minified blob can't blow the chamber. |
| Colorized edit diffs | `edit` tool results render with `-` (red), `+` (green), `@@` (cyan) coloring (`show_edit_diff: true` in config) |

### Inline ASCII avatar

A 5-cell face lives in the left margin of the input row and reflects what the
agent is currently doing. Single-tick animation alternates between two poses
where applicable.

| State | Frames | Meaning |
|-------|--------|---------|
| **Idle** | `(o o)` / `(- -)` | Nothing happening — neutral blink |
| **Thinking** | `(o .)` / `(. o)` | Reasoning tokens streaming (eyes shifting) |
| **Speaking** | `(o o)` / `(o O)` | Regular tokens streaming (mouth opens) |
| **Reading** | `[@ @]` | `read` / `grep` / `find_files` / `list_dir` / `lsp` / `semantic` tool running |
| **Writing** | `(>_<)` / `(-_-)` | `write` / `edit` / `apply_patch` / `write_todo_list` tool running |
| **Bash** | `[$_$]` | `bash` shell command running |
| **Alert** | `(O_O)` | Permission prompt waiting on you — paints in the perm color |
| **Error** | `(x_x)` | Agent hit an error — paints in the error color |
| **Done** | `(^_^)` | Turn completed cleanly — paints in the accent color |

Unknown / plugin / MCP tools default to the `Reading` face since most are
observational. The avatar is purely informational — no functional dependence.

## Prompts system

Built-in prompts that change the agent's behavior and tone:

| Prompt | Description |
|--------|-------------|
| **`code`** (default) | Coding mode with full tool access, TDD workflow |
| **`plan`** | Planning-only mode — `edit`/`write`/`apply_patch`/`bash`/`webfetch` are denied at the permission layer (via `deny_tools` frontmatter). Plan is delivered as the chat reply; the user saves it to disk if desired. |
| **`review`** | Code review mode — same deny list as plan; findings delivered in chat |
| **`debug`** | Debug mode — finds root cause before proposing fixes |
| **`ask`** | Read-only mode — `edit`/`write`/`apply_patch`/`bash`/`webfetch` denied via deny_tools |
| **`brainstorm`** | Design-only mode — explores ideas and presents designs without code |
| **`frontend-design`** | Frontend design mode — distinctive, production-grade UI |
| **`review-security`** | Security review mode — same deny list as plan/review; finds exploitable vulnerabilities |
| **`simplify`** | Code simplification mode — refines for clarity without changing behavior |
| **`write-prompt`** | Prompt writing mode — creates and optimizes agent prompts |
| **`default`** | Default system prompt — the base built-in prompt |

Each prompt is a markdown file with optional YAML frontmatter declaring its
tool restrictions:

```markdown
---
deny_tools: [edit, write, apply_patch, bash, webfetch]
description: Read-only planning mode
---
You are dirge in plan mode. …
```

The permission checker refuses any denied tool BEFORE the call leaves dirge
— even under `--yolo` mode. Applies symmetrically to MCP tools: an entry
in `deny_tools` matches an MCP-exported tool when the entry equals
**any** of the following:

- the bare tool name as the MCP server registered it (e.g. `edit` matches
  an MCP `edit` tool from any server — convenient blanket deny, but be
  aware that `deny_tools: [edit]` intended for the built-in editor will
  also block an MCP server's `edit` tool)
- the qualified `mcp_tool:<server>:<name>` form (for narrowly denying a
  specific server's tool)
- the umbrella `mcp_tool` (denies every MCP tool from every server)

For surgical control over one MCP tool without affecting the built-in, use
the qualified form.

Custom prompts can be placed in `$XDG_CONFIG_HOME/dirge/prompts/` as `.md` files.

The agent automatically loads `AGENTS.md` or `CLAUDE.md` from the project root, ancestor directories, and `~/.config/dirge/agent/AGENTS.md` as a global fallback. Use `-n` / `--no-context-files` to disable.

## Claude-compatible skills

Place skill directories in `.claude/skills/`, `.opencode/skills/`, or `.dirge/skills/` in your project or home directory. Each skill is a directory containing `SKILL.md` with optional YAML frontmatter:

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

When built with `--features plugin`, dirge embeds the [Janet](https://janet-lang.org) scripting language for harness-driven workflows. Plugins are Janet scripts placed in `~/.config/dirge/plugins/` (global) or `./.dirge/plugins/` (project-local) that define hooks at specific points in the agent lifecycle and call into `harness/*` APIs to log, gate tools, transform prompts, augment the system prompt, transform the message context per call, rewrite finalized messages, supply custom compaction summaries, render custom entries, control the session tree, and more. (The plugin layer is opt-in at build time — `build.sh` enables it; a bare `cargo build` compiles it to no-op stubs.)

A minimal plugin:

```janet
(defn on-prompt [ctx]
  (when (string/find "security" (ctx :prompt))
    (harness/notify "running with security mindset" :info)))
```

**See [`docs/plugins.md`](docs/plugins.md)** for the complete plugin author's guide — hook reference, the full `harness/*` API surface (logging, tool interception, dialogs, custom commands, renderers, providers, session-tree control), worked examples, and debugging tips.

Example plugins in [`plugins/`](plugins/):

| File | Demonstrates |
|------|--------------|
| `workflow.janet` | Architect → implementor → review orchestration via inversion of control |
| `protected_paths.janet` | `harness/block` + `harness/replace-result` (deny + truncate) |
| `hello_cmd.janet` | `harness/register-command` (custom `/cmd`) |
| `notify_example.janet` | `harness/notify` for chat notifications |
| `prefix_lang.janet` | `harness/replace-prompt` for input transform |
| `confirm_destructive.janet` | `harness/confirm` gating bash danger commands |
| `select_persona.janet` | `harness/select` + `/persona` to pick a response style |
| `turn_timing.janet` | `on-turn-start`/`on-turn-end` measuring per-turn elapsed time |
| `bookmark.janet` | `harness/append-entry` + `harness/register-renderer` — typed entries with custom rendering |
| `local_openai.janet` | `harness/register-provider` declaring vLLM/Ollama/LMStudio local endpoints |
| `session_tree.janet` | `harness/set-label` + `harness/new-session` — `/label` and `/fresh` slash commands |
| `turn_timer/` | Multi-file plugin — state, hooks, and a `/timer-stats` command split across three files in a single directory |
| `response_inspector.janet` | `on-response` hook — pattern-match the LLM's reply, post notifications, and return a steering string appended to the next turn's system prompt |

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
| `gopls` | `gopls` | `.go` |
| `jdtls` | `jdtls` | `.java` |
| `clangd` | `clangd` | `.c`, `.cc`, `.cpp`, `.cxx`, `.h`, `.hh`, `.hpp`, `.hxx`, `.m`, `.mm` |
| `ruby-lsp` | `ruby-lsp` | `.rb`, `.rake`, `.gemspec` |
| `bash-language-server` | `bash-language-server start` | `.sh`, `.bash` |

Missing binaries trip the broken-server backoff (1s → 2s → … capped at 10 min)
rather than failing dirge — the rest of the session keeps working. Override the
spawn command per server via the `lsp` config key; see [CONFIG.md](CONFIG.md).

Workspace root resolution is per-server: rust-analyzer walks past nested member crates to the workspace `Cargo.toml` declaring `[workspace]`; typescript stops at the nearest `package.json`/`tsconfig.json` and yields to deno when a `deno.json` is closer; pyright looks for `pyproject.toml`/`setup.py`/etc.; clojure-lsp looks for `deps.edn`/`project.clj`/`shadow-cljs.edn`/`bb.edn`/`.clj-kondo`; gopls follows `go.mod`/`go.work`; jdtls looks for `pom.xml`/`build.gradle`; clangd uses `compile_commands.json`/`CMakeLists.txt`/`Makefile`/`meson.build`; ruby-lsp follows `Gemfile`/`Rakefile`; bash-language-server uses the file's parent.

Disable: `--no-lsp` flag or `{ "lsp": false }` in the config. Per-server overrides (custom command, env, init options) live in the config — see [CONFIG.md](CONFIG.md).

## Semantic code tools

When built with `--features "semantic,semantic-ts,semantic-python"`, dirge gains AST-powered code analysis:

| Tool | Description |
|------|-------------|
| `list_symbols` | List functions, classes, methods, interfaces, and type aliases in a file or project. Filter by kind. |
| `get_symbol_body` | Full source of a named symbol via precise byte-range extraction. |
| `find_definition` | Locate where a symbol is defined across the project. |
| `find_callers` | Find all call sites of a function/method via the tree-sitter symbol index (word-boundary semantics, excludes the definition site). |
| `find_callees` | Extract all function/method calls made within a symbol's body (tree-sitter query). |

Supports TypeScript/TSX, Python, Clojure (`.clj`/`.cljs`/`.cljc`/`.edn`/`.bb`), Go, Ruby (`.rb`/`.rake`/`.gemspec`), Rust, Java, C (`.c`/`.h`), and C++ (`.cpp`/`.cc`/`.cxx`/`.hpp`/`.hh`/`.hxx`). Index is built lazily on first use and cached by file mtime.

| Language | Exports detected from | Maps to dirge SymbolKinds |
|---|---|---|
| TypeScript/TSX | `export` keyword + index re-exports | function/class/interface/method/type alias |
| Python | leading underscore convention; `__dunder__` treated as public | function/class/method |
| Clojure | `defn-` is private; everything else exported | function/variable/class (defrecord/deftype) /interface (defprotocol) /method (defmethod, defprotocol body) |
| Go | uppercase-first-letter convention | function/method (receiver type as `parent_class`) /class (struct) /interface (with methods) /type alias |
| Ruby | not detected (visibility is keyword-scoped) | class/interface (module) /method (instance + `def self.`) /function (top-level) |
| Rust | `pub` / `pub(crate)` / `pub(super)` visibility modifier | function/class (struct/enum) /interface (trait + methods) /method (impl block, attached to receiving type) /type alias /variable (const/static) |
| Java | `public` modifier; package-private + `private` / `protected` stay non-exported | class/interface/method (incl. constructors) /variable (fields) — nested classes recursed |
| C | `static` storage class = non-exported; extern by default | function/class (struct/enum) /type alias (typedef; suppressed when wrapping a named struct to avoid duplicates) |
| C++ | `public:` / `private:` / `protected:` access labels tracked through class bodies | class (class/struct) /method (incl. through templates + namespaces) /function (top-level) — namespaces recursed |

C and C++ both claim `.h`. When extracting a `.h` file, dirge sniffs the
first 32 KiB for C++-only markers (`class `, `namespace `, `template<`, `::`)
and routes the file to the C++ adapter if any match — so a Qt / libstdc++
header with classes is parsed correctly without the user having to rename it.
Pure-C headers fall through to the C adapter as before.

Adding a new language requires writing a Rust `LanguageAdapter` impl (see `src/semantic/adapters/clojure.rs` for a 60-line reference covering the full lifecycle) and gating it behind a new `semantic-<lang>` cargo feature. Tree-sitter Rust bindings don't load grammars dynamically today, so the per-language adapters need to ship in the binary — but users who want their own language can add an adapter in a fork without touching anything outside `src/semantic/`. For runtime-pluggable language intelligence, register an LSP server in `config.json` instead (see the LSP section below) — that's the supported path for languages dirge doesn't bake in.

## Bash permissions

When built with `--features semantic-bash`, dirge uses tree-sitter to parse shell commands and split them by `&&`, `;`, and `|` operators. Each segment is checked individually against permission rules. Complex constructs like command substitution (`$(...)`), subshells (`(...)`), and process substitution (`<(...)`) trigger a full-command permission prompt.

## Permission system

Four modes, from safest to most permissive:

1. **restrictive** (`-R`): every tool action prompts for approval
2. **standard** (default): safe commands auto-approved; writes, `bash`, and MCP tools ask
3. **accept-all** (`--accept-all`): auto-approves inside working directory; external paths prompt. `bash` and `mcp_tool` still ask in this mode — they execute external code with arbitrary effects, so the "trust the agent inside cwd" rationale doesn't apply
4. **yolo** (`--yolo`): auto-approves everything without prompting. Skips the rule eval, the per-tool default-ask for `mcp_tool`/`bash`, AND the doom-loop detector. The per-prompt `deny_tools` frontmatter still applies (that gate runs BEFORE the yolo short-circuit by design — opting into a restrictive prompt should survive `--yolo`). Use only when you know exactly what the agent will do

Session allowlists persist approvals for the session. Doom-loop detection triggers after 3+ identical calls.

## Configuration

See [CONFIG.md](CONFIG.md) for config file location, accepted keys, provider aliases, permission rules, and MCP server configuration.

### UI theme

dirge ships with an 80s-CRT phosphor green palette by default. To opt out, set `"theme": "plain"` in `config.json` for the pre-theme white/cyan look:

```json
{ "theme": "plain" }
```

Errors stay red and warnings stay yellow under every theme — those colors are part of the load-bearing semantic contract.

For custom themes, create `~/.config/dirge/<name>.theme.json` with overrides for any subset of the palette (named colors, hex `#rrggbb`, or 256-color indices), then set `theme: "<name>"` in `config.json`. See [`docs/themes.md`](docs/themes.md) for the full schema and examples.

## Supported providers

- OpenRouter (default)
- OpenAI
- Anthropic
- Gemini
- DeepSeek — OpenAI-compatible, `DEEPSEEK_API_KEY` env var
- GLM (ZhipuAI) — OpenAI-compatible, `GLM_API_KEY` env var
- Ollama
- Custom — any OpenAI-compatible endpoint

Providers are declared once in `$XDG_CONFIG_HOME/dirge/config.json` and referenced by alias from role-assignment keys:

```json
{
  "provider": "deepseek",
  "review_provider": "glm",
  "escalation_provider": "anthropic",
  "subagent_provider": "glm",

  "providers": {
    "deepseek": {
      "model": "deepseek-v4-pro"
    },
    "glm": {
      "model": "glm-4.6"
    },
    "anthropic": {
      "model": "claude-opus-4-5"
    },
    "ollama": {
      "provider_type": "openai",
      "base_url": "http://127.0.0.1:11434/v1",
      "model": "llama3.1"
    }
  }
}
```

Each `providers` entry accepts `provider_type` (optional — defaults to the entry's alias when that alias matches a built-in name), `base_url`, `model`, `api_key_env`, `allow_insecure`, and `stream_chunk_timeout_secs`. The aliases on the left of the map become the values you write in role-assignment keys.

Role assignments:

| Key | Used for | Falls back to |
|-----|----------|---------------|
| `provider` | Default / main loop | (none — required) |
| `review_provider` | Background session-review pass | `provider` |
| `escalation_provider` | One-shot retry after repair-exhaustion / pre-write syntax failure | `provider` (no-op when equal) |
| `summarization_provider` | Context compaction | `provider` |
| `subagent_provider` | `task` tool subagents | `provider` |

When a role's provider equals `provider` (either explicitly or by fallback), no duplicate client is constructed and the feature has zero overhead — escalation routes, for example, simply don't fire because they'd be a no-op anyway.

> **Note**: dirge no longer reads the legacy top-level `model`, `custom_providers`, or `review_model` keys — starting a session with any of those at the root fails fast with a migration hint. Move `model` inside the active provider's entry, `custom_providers.<name>` entries directly into `providers`, and `review_model` into the entry referenced by `review_provider`.

## License

GPL-3.0-only

## Acknowledgements

This project builds on and is deeply indebted to:

- [**zerostack**](https://github.com/gi-dellav/zerostack) by Giuseppe Della Vedova — the original minimal coding agent that dirge was forked from. Provides the core agent architecture, permission system, TUI, and prompt infrastructure.
- [**maki**](https://github.com/tontinton/maki) by Tony Solomonik — a feature-rich Rust coding agent. The Claude-compatible skills system, bash tree-sitter permissions, memory tool, bang commands (`!`/`!!`), `/cd` command, `/btw` query, rewind picker, and task/subagent tool were all ported from maki.
