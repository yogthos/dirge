# dirge

A minimal, fast coding agent written in Rust — inspired by [pi](https://pi.dev/docs/latest/usage), [opencode](https://opencode.ai/), and [maki](https://github.com/tontinton/maki).

## Why dirge

What sets dirge apart from other agentic editors:

- **Tiny and fast.** ~8 MB RAM idle, ~15 MB working, 25 MB binary — versus ~300 MB for JS-based agents. Native Rust, no runtime.
- **Built to keep weaker/cheaper models on the rails.** A [robust agent loop](docs/features.md#robust-agent-loop) repairs malformed tool calls, validates every write through tree-sitter *before* it touches disk, escalates to a stronger model on repeated failure, and trips circuit breakers on non-progressing loops.
- **One explainable permission engine.** All authorization flows through a single Policy Decision Point with four modes, op-based rules, session allowlists, and a `/why` command that traces exactly which policy decided and why. See [docs/permissions.md](docs/permissions.md).
- **Role-based multi-provider routing.** Point the main loop, review, escalation, summarization, and subagent roles at different models — mix DeepSeek, GLM, Anthropic, OpenAI, Ollama, and any OpenAI-compatible endpoint in one session.
- **Self-improving project memory.** Persistent per-project memory plus a post-session orchestrator that extracts learnings, curates memory + skills, and promotes patterns recurring across sessions.
- **Code intelligence baked in.** Tree-sitter [semantic tools](docs/semantic.md) and [LSP diagnostics](docs/lsp.md) for 10+ languages, surfaced inline so the agent fixes compile errors on the same turn.
- **Extensible at runtime.** A [Janet plugin system](docs/plugins.md) hooks the full lifecycle, and [Claude-compatible skills](docs/skills.md) load instructions on demand.

See the full [feature catalog](docs/features.md) for everything else.

## Installation

> The crate is published as **`dirge-agent`** (the short `dirge` name was
> already taken on crates.io). The installed command is still `dirge`.

```bash
# Batteries included — MCP, LSP, ACP, plugins, and every tree-sitter
# language are on by default.
cargo install dirge-agent
```

Want a leaner binary? Opt out of the defaults and pick only what you need:

```bash
# Minimal: just the core agent + MCP, no semantic tools / plugins / ACP
cargo install dirge-agent --no-default-features --features "loop,git-worktree,mcp,lsp"

# Core + only the languages you use
cargo install dirge-agent --no-default-features \
  --features "loop,git-worktree,mcp,lsp,semantic-rust,semantic-python"
```

Prebuilt binaries for Linux (glibc + static musl), macOS (Intel + Apple
Silicon), and Windows are attached to each [GitHub Release](https://github.com/dirge-code/dirge/releases).

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

# Verbose mode — debug-level dirge logs + warn-level plugin hook errors
dirge --verbose
```

Avoid `--api-key <key>` outside one-off testing — it's visible to other
processes via `ps` and emits a startup warning. Prefer a key file, stdin, or
the provider's env var:

```bash
dirge --provider openai --api-key-file /run/secrets/openai_key
pass openai-key | dirge --provider openai --api-key-stdin
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
| `/panel [on\|off\|auto]` | Toggle the right-hand info panel (cwd, MCP, LSP, todos, modified files). `auto` shows it at ≥100 cols. |
| `/allow [list\|add\|remove\|clear]` | Manage the session permission allowlist; bare `/allow` lists it. See [docs/permissions.md](docs/permissions.md#allow-always-and-the-session-allowlist) |
| `/why <tool> [input]` | Dry-run a permission decision and print the full policy trace |
| `/retry` | Retry last prompt |
| `/quit` | Exit dirge |
| `/help` | Show all commands |

For key bindings, the inline avatar, and tool-output display, see [docs/tui.md](docs/tui.md).

## Supported providers

OpenRouter (default), OpenAI, Anthropic, Gemini, DeepSeek, GLM (ZhipuAI),
Ollama, and any custom OpenAI-compatible endpoint.

Providers are declared once in `$XDG_CONFIG_HOME/dirge/config.json` and
referenced by alias from role-assignment keys (`provider`, `review_provider`,
`escalation_provider`, `summarization_provider`, `subagent_provider`) — so each
role can run on a different model. See [CONFIG.md](CONFIG.md) for the schema,
provider aliases, role-assignment table, permission rules, and MCP setup.

## Documentation

| Document | Topic |
|---|---|
| [CONFIG.md](CONFIG.md) | Config file location, keys, provider aliases, permission rules, MCP servers |
| [docs/features.md](docs/features.md) | Full feature catalog, robust agent loop, performance |
| [docs/permissions.md](docs/permissions.md) | Authorization engine, security modes, `/why` |
| [docs/prompts.md](docs/prompts.md) | Prompts system, per-prompt tool restrictions, context files |
| [docs/skills.md](docs/skills.md) | Claude-compatible skills |
| [docs/semantic.md](docs/semantic.md) | Tree-sitter semantic code tools |
| [docs/lsp.md](docs/lsp.md) | LSP integration and built-in server set |
| [docs/tui.md](docs/tui.md) | Key bindings, avatar, tool-output display, themes |
| [docs/plugins.md](docs/plugins.md) | Janet plugin authoring — hooks, `harness/*` API, examples |
| [docs/agent-loop.md](docs/agent-loop.md) | Multi-turn execution loop architecture |
| [docs/tool-input-repair.md](docs/tool-input-repair.md) | Repair layer for malformed tool calls |
| [docs/themes.md](docs/themes.md) | Built-in palettes and custom theme schema |

## License

GPL-3.0-only

## Acknowledgements

This project builds on and is deeply indebted to:

- [**zerostack**](https://github.com/gi-dellav/zerostack) by Giuseppe Della Vedova — the original minimal coding agent that dirge was forked from. Provides the core agent architecture, permission system, TUI, and prompt infrastructure.
- [**maki**](https://github.com/tontinton/maki) by Tony Solomonik — a feature-rich Rust coding agent. The Claude-compatible skills system, bash tree-sitter permissions, memory tool, bang commands (`!`/`!!`), `/cd` command, `/btw` query, rewind picker, and task/subagent tool were all ported from maki.
