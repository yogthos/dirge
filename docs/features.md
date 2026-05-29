# Feature catalog

The full inventory of dirge's capabilities. For a short overview and the
headline differentiators, see the top-level [README](../README.md).

## Core

- **Multi-provider**: OpenRouter, OpenAI, Anthropic, Gemini, DeepSeek, GLM, Ollama, plus custom OpenAI-compatible endpoints. A single `providers` map declares aliases; top-level role keys (`provider`, `review_provider`, `escalation_provider`, `summarization_provider`, `subagent_provider`) point each role at one of those aliases. See [CONFIG.md](../CONFIG.md).
- **Standard tools**: read, write, edit, bash, grep, find_files, glob, list_dir, write_todo_list, apply_patch, repo_overview, session_search, webfetch, websearch, question, memory, skill, task, task_status, tool_search.
- **Line-numbered read output**: `read` prefixes each line with right-aligned line numbers (`123: content`).
- **Environment-aware**: system prompt includes OS, shell, working directory, and git branch for context.
- **Semantic code tools** (tree-sitter): list_symbols, get_symbol_body, find_definition, find_callers, find_callees — TypeScript/TSX, Python, Clojure (clj/cljs/cljc/edn/bb), Go, Ruby, Rust, Java, C, C++. See [semantic.md](semantic.md).
- **Claude-compatible skills**: discover skills from `.claude/skills/`, `.opencode/skills/`, `.dirge/skills/`. The agent calls the `skill` tool to load instructions on demand. See [skills.md](skills.md).
- **Bash permissions** (tree-sitter): parses shell commands to split `&&`/`;`/`|` into individual segments, detects command substitution and complex constructs. See [permissions.md](permissions.md).
- **Permission system**: a single decision engine (one Policy Decision Point) with four configurable modes, op-based rules (read/edit/execute/network/mcp/…), session allowlists, external directory policies, and a `/why` decision-trace command. See [permissions.md](permissions.md).
- **Session management & compaction**: save/load/resume sessions with lineage-aware session search. `/compress` runs an LLM-summarization compaction pass (via the auxiliary `summarization_provider`) that folds the conversation middle into a structured summary. Automatic turn-boundary compaction keeps the runtime within the context window without an LLM call: it caps oversized tool results (per-result token cap) and proactively folds when the context-ratio crosses a high threshold, on top of tool-output pruning. See [agent-loop.md](agent-loop.md).
- **Terminal UI**: crossterm-based, markdown rendering, soft-wrapping input box, mouse selection/copy, scrollback, reasoning visibility toggle. See [tui.md](tui.md).
- **Info panel**: optional right-hand sidebar showing cwd, MCP/LSP server status, pending todos, and recently-modified files. Auto-shown at ≥100 cols; toggle via `/panel`.
- **Mid-execution interjection**: type while the agent is running to queue a follow-up message — the runner stops at the next tool-result boundary so it's picked up promptly instead of waiting for the whole multi-turn run. `Ctrl+X` drops queued messages, `Ctrl+C` cancels both the run and the queue.
- **Prompts system**: switch between system prompt modes at runtime (`code`, `plan`, `review`, `debug`, etc.). See [prompts.md](prompts.md).
- **Per-prompt tool restrictions**: each prompt (`prompts/<name>.md`) can declare a `deny_tools` frontmatter list. The permission checker refuses those tools while the prompt is active — a real security boundary, not a prose gate. `plan`, `review`, and `review-security` ship with `edit`, `write`, `apply_patch`, `bash`, and `webfetch` denied. See [prompts.md](prompts.md).
- **Subagent support**: `task` tool spawns a subagent for research or general analysis subtasks.
- **Memory & self-improvement**: persistent per-project memory at `.dirge/memory/` (`MEMORY.md` facts + `PITFALLS.md`), injected into the system prompt as a frozen snapshot. The agent edits it via the `memory` tool. After an idle session, a unified post-session orchestrator runs (in order, fire-and-forget): a background review that extracts learnings into memory + skills, a skills curator and a memory curator (stale-detection + lifecycle + LLM consolidation, with audit reports under each store's `.curator_reports/`), and a cross-session pass that promotes sub-threshold patterns recurring across past sessions.
- **MCP support**: connect MCP servers for extended tooling (optional compile-time feature).
- **Git worktrees**: `/worktree` to create branch-per-task worktrees, `/wt-merge`, `/wt-exit`.
- **Loop system**: iterative coding loop for long-horizon tasks.
- **ACP support** (gated): Agent Communication Protocol server for editor integration. ACP locks the active prompt at launch — use `--prompt <name>` on startup to opt into a restricted mode (the protocol has no mid-session prompt-switch message).
- **Plugin system** (Janet, on a dedicated worker thread): hooks across the full session/agent/tool lifecycle. Plugins can intercept tool calls (block/mutate/replace), register slash commands / tools / keyboard shortcuts / LLM providers, augment the system prompt (`before-agent-start`), transform the message context before each LLM call (`transform-context`), rewrite finalized assistant messages (`message-end`), supply custom compaction summaries (`on-compact`), post notifications, and prompt the user with blocking `confirm`/`select` dialogs. See [plugins.md](plugins.md).

## Robust agent loop

Hardening against the failure modes that plague long sessions and weaker models. See [agent-loop.md](agent-loop.md) for the architecture and [tool-input-repair.md](tool-input-repair.md) for the repair layer.

- **Tool-input repair layer**: catches and fixes common malformed tool calls before they hit the tool — strips `null` optional fields, parses JSON-string arrays, unwraps markdown links in path fields, applies relational defaults declared in the tool's schema. Failed repairs emit a structured `tool_input_invalid` log with the original args.
- **Schema-aware contract hints** (`dirge-hints`): per-tool schemas can declare `semantic: "absolute_path"`, `relational: [{requires, defaults}]`, etc. The repair layer reads these to drive automatic defaults + agent-facing `Note:` text — removing per-tool hardcoded heuristics.
- **Tree-sitter pre-write validation**: every `write` / `edit` / `apply_patch` is parsed through the matching tree-sitter grammar before bytes hit disk. Syntactically-broken code is rejected with line/column-precise errors so the model corrects it on the same turn. Languages: Rust, TS/TSX, Python, Go, Ruby, Java, C, C++, Clojure, Bash (each gated on its `semantic-<lang>` feature).
- **Dynamic `tool_search`** (opt-in via `dynamic_tool_search: true`): ships only `tool_search` + a small always-on set in each request; the model calls `tool_search(query)` to discover and load more tools on demand. ~30% token savings on MCP-heavy sessions.
- **Disk-backed large-output relay**: `bash` / `webfetch` outputs over an inline budget (default 8 KiB) are written to `~/.dirge/transient/<pid>/<tool>-<ts>.txt` and replaced with a head + ellipsis + tail summary plus a hint to `read` for specifics. Aged cleanup runs on every relay write.
- **Anthropic prompt-cache positioning**: system prompt + tool defs sit at the start of every request (cache-warm prefix); a `prompt_cache_prefix` tracing event emits per-turn with stable hashes so unexpected prefix drift is observable.
- **Dual-client tiering** (`escalation_provider` role): when a tool input fails to repair OR generated code fails the tree-sitter pre-write check, the next model call is routed through a more capable provider. One-shot per failure, capped at 3 per session, surfaced as a dim `↑ escalating to <provider>` status line.
- **Context-depth reminders** (`context_depth_reminder_threshold`): tracks consecutive turns that touch the same file(s); when the streak crosses the threshold (default 8, opt-in), injects a single mid-turn reminder restating the active task + touched files so long runs don't drift.
- **Tool-loop circuit breaker**: per-tool-call repeat counter trips on the 3rd identical `(tool, input)` within a 32-call window — catches non-progressing loops without needing model cooperation.

> **NOTE**: Windows support is not tested, but feel free to try and open an issue if you encounter any bugs.

## Performance

dirge is one of the smallest and most performant coding agents on the market.

- Lines of code: ~100k LoC
- Binary size: 25 MB
- RAM footprint: ~8 MB on an empty session, ~15 MB when working (vs ~300 MB for opencode or other JS-based coding agents)

### Tool result caching

Most tool calls (`read`, `write`, `edit`, `bash`, `grep`, `find_files`, `list_dir`) are cached per agent turn. Repeated calls with identical arguments within the same turn return cached results, avoiding redundant filesystem I/O. The cache clears automatically before each new prompt, and after `write`/`edit`/`bash` so a re-read sees fresh content.

### Error recovery

Transient API errors (network, rate limits, Anthropic `overloaded_error`) are automatically retried with exponential backoff (1s → 2s → 4s, max 3 retries) plus 0–25% jitter so concurrent agents don't retry in lockstep. Auth and unknown errors surface immediately. Context-length errors are not retried — surface a `/compress` hint instead. Tokens stream live to the chat as they arrive; if a retry fires, the user sees an "(error: …; retrying)" banner and the next attempt's tokens stream in fresh. If any tool calls were already dispatched (side effects applied), the error is surfaced without retrying so a partial-but-applied turn isn't re-run.
