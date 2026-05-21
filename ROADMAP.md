# dirge roadmap

Forward-looking work organized into phases. Each phase is a self-
contained PR/series with TDD coverage, scoped to ship independently.
Gaps below were identified by comparing dirge to
[pi](https://github.com/tontinton/pi) and
[opencode](https://github.com/sst/opencode).

Status legend: ✅ shipped · 🚧 in progress · 📋 planned · 💡 ideas
queue (re-evaluate before scheduling).

## Recently shipped

- ✅ **Phase 1** (PR #69): first-wins `harness/block` semantics
  matching pi's `runner.ts:806-827`; subagent isolation
  documented; README streaming-buffered claim corrected.
- ✅ **Phase 2** (PR #70): sibling-branch pruning on compress +
  rewind with chat notification (opencode's
  `session/compaction.ts:386-396` drop-with-truncation).
- ✅ **Phase 3** (PR #71): structured tool-call persistence with
  interrupted-state pairing (opencode's `ToolPart` state
  machine, `message-v2.ts:310-320, 630-899`).
- ✅ **Phase 4** (PR #72): branch summary metadata captured on
  prune and surfaced in `/tree` (pi's `branch-summarization.ts`,
  metadata-only MVP).
- ✅ **Phase 5** (PR #73): `/allow` CRUD slash command.

## Carried over from prior plan

### Phase 4b — LLM-generated branch summaries 📋

Extends Phase 4. `BranchSummary.summary: Option<String>` populated
by a background LLM call instead of the truncated content preview.
Pattern: pi's `branch-summarization.ts:283-355`.

- **Size**: MEDIUM (3–4 days).
- **Triggers**: when users complain that the preview is too
  cryptic to identify branches.
- **Risk**: requires routing the LLM client into the currently-
  sync `Session::compress_reporting`; async error path needed.

### Phase 6 — Cost tracking 📋

Per-provider pricing tables + actual usage from rig's
`FinalResponse.usage`. Removes the `TODO(cost-tracking)` markers
in `session/mod.rs`, `ui/status.rs`, and `ui/mod.rs`. Pattern:
pi's in-tree `pricing.ts`.

- **Size**: MEDIUM (2 days).
- **Includes**: new `/cost` slash command for session-to-date
  cost + per-model breakdown.
- **Skipped originally per user request**; re-evaluate once cost-
  attribution becomes a felt need.

## Gap-closure plan (new)

Five tracks, each independent. Pick by priority + effort.

### Track A — Tool surface 🟢 enhancements

Smaller additions that broaden what tools the LLM can reach for.

| Phase | Description | Size | Pattern |
|---|---|---|---|
| 📋 A1 | **Cross-platform shell** (PowerShell + cmd.exe in addition to bash). `Sandbox::wrap_command` dispatches by `cfg!(windows)` plus a `--shell` override. | SMALL | opencode `packages/opencode/src/tool/shell.ts:1-68` |
| 📋 A2 | **`repo_clone` + `repo_overview` tools.** Cache repos under `~/.dirge/repos/`, infer package manager + ecosystem, expose to the LLM as a single call instead of orchestrating bash+git manually. | MEDIUM | opencode `packages/opencode/src/tool/repo_clone.ts` + `repo_overview.ts` |
| 📋 A3 | **Multi-file atomic edit** mode on `WriteTool` / `EditTool`. Apply 1 → N files; if any fail validation, none commit. | SMALL | pi `packages/coding-agent/src/core/tools/file-mutation-queue.ts` |
| 📋 A4 | **Dry-run flag** on `BashTool` + `WriteTool` + `EditTool`. Returns the would-be effect without side effects. Hooks into permission `Ask` so the user sees the diff before deciding. | SMALL | pi tool conventions; opencode preview |
| 📋 A5 | **Structured tool result metadata.** Extend `ToolResult` to carry a typed `metadata: serde_json::Value` alongside the text. Lets renderers (e.g. plugin custom renderers, future panels) display rich info without re-parsing. | SMALL | opencode `packages/opencode/src/tool/tool.ts` (`Metadata` type) |
| 📋 A6 | **File-watcher event bus.** After write/edit/apply_patch, emit a structured event other code paths (plugins, future panels, LSP-aware tools) can subscribe to. Today `modified.rs` records state but has no notification. | SMALL | opencode `FileWatcher.Event.Updated` |
| 💡 A7 | **Image generation tool** (Anthropic vision / OpenAI DALL·E wrapper). | SMALL | opencode `packages/core/src/github-copilot/.../image-generation.ts` — IDEAS QUEUE; not core to coding |

### Track B — Permission system 🟢 hardening

Make the permission layer more expressive without expanding its
surface for the agent.

| Phase | Description | Size | Pattern |
|---|---|---|---|
| 📋 B1 | **Semantic command arity for permission rules.** Use the already-feature-gated `semantic-bash` tree-sitter parser to normalize bash commands (`git checkout main` → `git checkout`) before matching. Lets rules be written as `"git checkout *"` instead of fragile globs. | MEDIUM | opencode `packages/opencode/src/permission/arity.ts:1-80` |
| 📋 B2 | **Scoped permission grants** (session / project / global) with optional TTL. Extends `UserDecision::AllowAlways` to `AllowScoped { scope, ttl }`. Today's "allow always" is session-scoped only. | MEDIUM | opencode `packages/opencode/src/permission/index.ts:32-61` |
| 📋 B3 | **Project-scoped allowlists.** Store under `~/.dirge/projects/<hash>/allowlist.json` keyed by canonicalized cwd; merge with session allowlist at check time. | SMALL | opencode project-scope distinction |
| 📋 B4 | **Audit log.** Append every `allow/ask/deny` decision with timestamp + normalized command to `~/.dirge/audit.log`. Useful for postmortems and compliance contexts. | SMALL | opencode logs decisions |

### Track C — Plugin system 🟡 expansion

Bring the Janet hook surface closer to pi/opencode's plugin APIs
without reinventing the host.

| Phase | Description | Size | Pattern |
|---|---|---|---|
| 📋 C1 | **`on-provider-request` hook.** Plugin can mutate `{model, temperature, top_p, max_tokens, extra_headers}` before each LLM call. Returns Janet table; host merges into rig's request. | MEDIUM | opencode `packages/plugin/src/index.ts:246` (`chat.params`, `chat.headers`); pi `before_provider_request` |
| 📋 C2 | **Session-tree mutation hooks.** `on-session-fork`, `on-session-compact`, `on-session-switch` fire alongside the existing data-mutation. Lets plugins coordinate state on branch ops without polling. | SMALL | pi `packages/coding-agent/src/core/extensions/types.ts:522` |
| 📋 C3 | **Plugin manifest (`plugin.toml`).** Optional metadata file alongside `.janet` files: name, version, dirge-version constraint, declared hooks, declared commands. Enables future package manager + dependency resolution. | MEDIUM | pi npm + opencode plugin packaging |
| 📋 C4 | **VSCode extension** wrapping the existing ACP server. Terminal-launch from a file, context injection, status bar. Distribution via the VSCode marketplace. | LARGE | opencode `sdks/vscode/src/extension.ts:8` |
| 📋 C5 | **Zed agent integration** via the ACP transport. dirge already speaks ACP over stdio; add the manifest + packaging so Zed users can drop dirge in as an agent backend. | MEDIUM | opencode `packages/extensions/zed/extension.toml` |
| 💡 C6 | **JetBrains plugin** for IntelliJ/PyCharm/etc. Larger lift; defer to after VSCode + Zed land. | LARGE | not yet implemented in opencode either |
| 💡 C7 | **Plugin package registry / distribution.** Out of scope until C3 ships and adoption justifies it. | LARGE | — |

### Track D — UI/UX 🟡 polish

User-facing features that make the TUI feel modern.

| Phase | Description | Size | Pattern |
|---|---|---|---|
| 📋 D1 | **Slash command popover with autocomplete.** Show `/` triggers a floating panel listing matching commands with descriptions + keybinds. Replaces today's "type the whole command" experience. | MEDIUM | opencode `packages/app/src/components/prompt-input/slash-popover.tsx` |
| 📋 D2 | **Command palette (`Ctrl+K`).** Fuzzy search across all commands. Reuses the slash command registry built for D1. | SMALL after D1 | opencode `packages/app/src/context/command.tsx:257-276` |
| 📋 D3 | **Syntax highlighting in code blocks.** tree-sitter-driven (we already vendor parsers for Rust/Python/TS/Bash via the semantic feature) → emit colored spans in `markdown_to_styled`. | MEDIUM | opencode uses Shiki; dirge would use tree-sitter |
| 📋 D4 | **Vim keybinding mode.** Optional via `--vim` flag or config. Normal/insert modes, hjkl navigation, word objects. | SMALL | pi `packages/tui/src/keybindings.ts:54-134` declarative registry |
| 📋 D5 | **Kitty image protocol** for inline image rendering in markdown. Fallback to a text placeholder on non-Kitty terminals. | MEDIUM | pi `packages/tui/src/components/image.ts` |
| 📋 D6 | **`/metrics` slash command.** Per-message token count, latency, model used, cost (after Phase 6). Cumulative session view. Sparkline if the terminal supports it. | LARGE | opencode `packages/app/src/components/session-context-usage.tsx` |
| 📋 D7 | **Review / diff mode.** A read-only sidebar showing diff of each turn's changes; arrow keys move between turns; `r` rewinds to a turn. Wraps the existing rewind picker with structured display. | LARGE | opencode `session-revert-dock.tsx` |
| 📋 D8 | **Bookmarks UI built on `set-label`.** Visible marker on labeled tree nodes in `/tree`; `/bookmarks` lists them with timestamps; jump-to-bookmark navigates the leaf. | SMALL | builds on existing `harness/set-label` |
| 💡 D9 | **Mermaid diagram rendering.** Too hard in TUI; defer to a hypothetical web playground. | LARGE | — |

### Track E — Conversation features 🟢 incremental

Smaller workflow additions.

| Phase | Description | Size | Pattern |
|---|---|---|---|
| 📋 E1 | **Message editing with partial regeneration.** Edit a prior user message in-place; the session forks at that point and the agent regenerates the rest. | MEDIUM | opencode message-edit UX |
| 📋 E2 | **Image attachment input.** Drop / paste an image, encode as base64, pass to the LLM as multimodal content. Size limits + auto-resize. | MEDIUM | opencode attachments + pi compression |
| 📋 E3 | **Reasoning token counter** as a separate display from output tokens (Anthropic + OpenAI o1 expose them). Helps users understand cost composition. | SMALL | opencode `session-message.ts:144` `tokens.reasoning` field |
| 📋 E4 | **Session export / import** with versioned JSONL schema. Backup, share, transfer between machines. | MEDIUM | pi `packages/agent/src/harness/session/jsonl-storage.ts` |

## Sequencing recommendation

Pick from each track in priority order:

1. **D1 + D2** (slash popover + Ctrl+K palette) — biggest day-1
   UX win; shows users what's possible.
2. **D8** (bookmarks UI) — small, makes the existing label
   infrastructure visible.
3. **Phase 6** (cost tracking) + **D6** (`/metrics`) — pair these
   since `/metrics` needs the cost data.
4. **B1 + B2 + B3 + B4** (permission hardening batch) — one PR
   per fix, ship as a series.
5. **C1 + C2** (provider-request hook + tree mutation hooks) —
   broadens plugin authoring without packaging changes.
6. **A2 + A4** (repo tools + dry-run) — most impactful tool
   additions.
7. **D3** (syntax highlighting) and **D5** (Kitty images) — nice-
   to-haves once the rest lands.
8. **C4 + C5** (VSCode + Zed extensions) — packaging work, large
   surface area.

Phases 4b, D7 (review/diff), and the ideas queue stay deferred
until usage data justifies them.

## Ideas queue (re-evaluate before scheduling)

- A7 image generation tool
- C6 JetBrains plugin
- C7 plugin registry
- D9 mermaid rendering
- Multi-session views (neither ref project has this)
- Semantic search over conversation history
- Real-time token-rate display (TUI animation)
- Parallel LLM request batching with dedup
