# dirge roadmap

theme color styles as json

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

### Phase 6 — Cost tracking 📋 - not planned

Per-provider pricing tables + actual usage from rig's
`FinalResponse.usage`. Removes the `TODO(cost-tracking)` markers
in `session/mod.rs`, `ui/status.rs`, and `ui/mod.rs`. Pattern:
pi's in-tree `pricing.ts`.

- **Size**: MEDIUM (2 days).
- **Includes**: new `/cost` slash command for session-to-date
  cost + per-model breakdown.
- **Skipped originally per user request**; re-evaluate once cost-
  attribution becomes a felt need.

- investigate if tree-sitter can be loaded via plugins
add tree-sitter for Clojure https://github.com/sogaiu/tree-sitter-clojure/

## Gap-closure plan (new)

Five tracks, each independent. Pick by priority + effort.

### Track A — Tool surface 🟢 enhancements

Smaller additions that broaden what tools the LLM can reach for.

| Phase | Description | Size | Pattern | planned?
|---|---|---|---|
| 📋 A1 | **Cross-platform shell** (PowerShell + cmd.exe in addition to bash). `Sandbox::wrap_command` dispatches by `cfg!(windows)` plus a `--shell` override. | SMALL | opencode `packages/opencode/src/tool/shell.ts:1-68` | no
| 📋 A2 | **`repo_clone` + `repo_overview` tools.** Cache repos under `~/.dirge/repos/`, infer package manager + ecosystem, expose to the LLM as a single call instead of orchestrating bash+git manually. | MEDIUM | opencode `packages/opencode/src/tool/repo_clone.ts` + `repo_overview.ts` | yes
| 📋 A3 | **Multi-file atomic edit** mode on `WriteTool` / `EditTool`. Apply 1 → N files; if any fail validation, none commit. | SMALL | pi `packages/coding-agent/src/core/tools/file-mutation-queue.ts` | yes
| 📋 A4 | **Dry-run flag** on `BashTool` + `WriteTool` + `EditTool`. Returns the would-be effect without side effects. Hooks into permission `Ask` so the user sees the diff before deciding. | SMALL | pi tool conventions; opencode preview | yes
| 📋 A5 | **Structured tool result metadata.** Extend `ToolResult` to carry a typed `metadata: serde_json::Value` alongside the text. Lets renderers (e.g. plugin custom renderers, future panels) display rich info without re-parsing. | SMALL | opencode `packages/opencode/src/tool/tool.ts` (`Metadata` type) | yes
| 📋 A6 | **File-watcher event bus.** After write/edit/apply_patch, emit a structured event other code paths (plugins, future panels, LSP-aware tools) can subscribe to. Today `modified.rs` records state but has no notification. | SMALL | opencode `FileWatcher.Event.Updated` | yes
| 💡 A7 | **Image generation tool** (Anthropic vision / OpenAI DALL·E wrapper). | SMALL | opencode `packages/core/src/github-copilot/.../image-generation.ts` — IDEAS QUEUE; not core to coding | yes

### Track B — Permission system 🟢 hardening

Make the permission layer more expressive without expanding its
surface for the agent.

| Phase | Description | Size | Pattern | planned?
|---|---|---|---|
| 📋 B1 | **Semantic command arity for permission rules.** Use the already-feature-gated `semantic-bash` tree-sitter parser to normalize bash commands (`git checkout main` → `git checkout`) before matching. Lets rules be written as `"git checkout *"` instead of fragile globs. | MEDIUM | opencode `packages/opencode/src/permission/arity.ts:1-80` | yes
| 📋 B2 | **Scoped permission grants** (session / project / global) with optional TTL. Extends `UserDecision::AllowAlways` to `AllowScoped { scope, ttl }`. Today's "allow always" is session-scoped only. | MEDIUM | opencode `packages/opencode/src/permission/index.ts:32-61` | yes
| 📋 B3 | **Project-scoped allowlists.** Store under `~/.dirge/projects/<hash>/allowlist.json` keyed by canonicalized cwd; merge with session allowlist at check time. | SMALL | opencode project-scope distinction | yes
| 📋 B4 | **Audit log.** Append every `allow/ask/deny` decision with timestamp + normalized command to `~/.dirge/audit.log`. Useful for postmortems and compliance contexts. | SMALL | opencode logs decisions | yes

### Track C — Plugin system 🟡 expansion

Bring the Janet hook surface closer to pi/opencode's plugin APIs
without reinventing the host.

| Phase | Description | Size | Pattern | planned?
|---|---|---|---|
| 📋 C1 | **`on-provider-request` hook.** Plugin can mutate `{model, temperature, top_p, max_tokens, extra_headers}` before each LLM call. Returns Janet table; host merges into rig's request. | MEDIUM | opencode `packages/plugin/src/index.ts:246` (`chat.params`, `chat.headers`); pi `before_provider_request` | yes
| 📋 C2 | **Session-tree mutation hooks.** `on-session-fork`, `on-session-compact`, `on-session-switch` fire alongside the existing data-mutation. Lets plugins coordinate state on branch ops without polling. | SMALL | pi `packages/coding-agent/src/core/extensions/types.ts:522` | yes
| 📋 C3 | **Plugin manifest (`plugin.toml`).** Optional metadata file alongside `.janet` files: name, version, dirge-version constraint, declared hooks, declared commands. Enables future package manager + dependency resolution. | MEDIUM | pi npm + opencode plugin packaging | yes
| 📋 C4 | **VSCode extension** wrapping the existing ACP server. Terminal-launch from a file, context injection, status bar. Distribution via the VSCode marketplace. | LARGE | opencode `sdks/vscode/src/extension.ts:8` | no
| 📋 C5 | **Zed agent integration** via the ACP transport. dirge already speaks ACP over stdio; add the manifest + packaging so Zed users can drop dirge in as an agent backend. | MEDIUM | opencode `packages/extensions/zed/extension.toml` | no
| 💡 C6 | **JetBrains plugin** for IntelliJ/PyCharm/etc. Larger lift; defer to after VSCode + Zed land. | LARGE | not yet implemented in opencode either | no
| 💡 C7 | **Plugin package registry / distribution.** Out of scope until C3 ships and adoption justifies it. | LARGE | — | maybe

### Track D — UI/UX 🟡 polish

User-facing features that make the TUI feel modern.

| Phase | Description | Size | Pattern | planned?
|---|---|---|---|
| 📋 D1 | **Slash command popover with autocomplete.** Show `/` triggers a floating panel listing matching commands with descriptions + keybinds. Replaces today's "type the whole command" experience. | MEDIUM | opencode `packages/app/src/components/prompt-input/slash-popover.tsx` | yes
| 📋 D2 | **Command palette (`Ctrl+K`).** Fuzzy search across all commands. Reuses the slash command registry built for D1. | SMALL after D1 | opencode `packages/app/src/context/command.tsx:257-276` | yes
| 📋 D3 | **Syntax highlighting in code blocks.** tree-sitter-driven (we already vendor parsers for Rust/Python/TS/Bash via the semantic feature) → emit colored spans in `markdown_to_styled`. | MEDIUM | opencode uses Shiki; dirge would use tree-sitter | yes
| 📋 D4 | **Vim keybinding mode.** Optional via `--vim` flag or config. Normal/insert modes, hjkl navigation, word objects. | SMALL | pi `packages/tui/src/keybindings.ts:54-134` declarative registry | no
| 📋 D5 | **Kitty image protocol** for inline image rendering in markdown. Fallback to a text placeholder on non-Kitty terminals. | MEDIUM | pi `packages/tui/src/components/image.ts` | no
| 📋 D6 | **`/metrics` slash command.** Per-message token count, latency, model used, cost (after Phase 6). Cumulative session view. Sparkline if the terminal supports it. | LARGE | opencode `packages/app/src/components/session-context-usage.tsx` | no
| 📋 D7 | **Review / diff mode.** A read-only sidebar showing diff of each turn's changes; arrow keys move between turns; `r` rewinds to a turn. Wraps the existing rewind picker with structured display. | LARGE | opencode `session-revert-dock.tsx` | no
| 📋 D8 | **Bookmarks UI built on `set-label`.** Visible marker on labeled tree nodes in `/tree`; `/bookmarks` lists them with timestamps; jump-to-bookmark navigates the leaf. | SMALL | builds on existing `harness/set-label` | yes - can use empty space on the left to render
| 💡 D9 | **Mermaid diagram rendering.** Too hard in TUI; defer to a hypothetical web playground. | LARGE | — | no

### Track E — Conversation features 🟢 incremental

Smaller workflow additions.

| Phase | Description | Size | Pattern |
|---|---|---|---|
| 📋 E1 | **Message editing with partial regeneration.** Edit a prior user message in-place; the session forks at that point and the agent regenerates the rest. | MEDIUM | opencode message-edit UX | no
| 📋 E2 | **Image attachment input.** Drop / paste an image, encode as base64, pass to the LLM as multimodal content. Size limits + auto-resize. | MEDIUM | opencode attachments + pi compression | yes
| 📋 E3 | **Reasoning token counter** as a separate display from output tokens (Anthropic + OpenAI o1 expose them). Helps users understand cost composition. | SMALL | opencode `session-message.ts:144` `tokens.reasoning` field | no
| 📋 E4 | **Session export / import** with versioned JSONL schema. Backup, share, transfer between machines. | MEDIUM | pi `packages/agent/src/harness/session/jsonl-storage.ts` | yes

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

## Track F — Code quality audit findings 🔴 ship before more features

Identified by parallel implementation-quality surveys of five dirge
subsystems against pi + opencode. Each is a real gap in robustness
/ correctness / security relative to one or both reference projects
(not a feature gap — those went into Tracks A–E above). Verify any
claim against the cited dirge `file:line` before acting; some are
nuanced and the auditor's framing may have been sharper than the
actual issue.

### F-CRITICAL — fix before next release

| Phase | Description | Size | Source |
|---|---|---|---|
| ✅ F1 | **ACP permission asks silently dropped.** `extras/acp/mod.rs:287` builds `(ask_tx, _ask_rx)` and immediately drops `ask_rx`. Tools needing `Ask` confirmation in ACP mode time out at 30s with no notification. Tools requiring permission cannot run from a Zed/editor client. | MEDIUM | opencode permission dispatch via SessionBus |
| ✅ F2 | **Hidden files exposed by `find_files` / `glob` / `list_dir`.** All three set `.hidden(false)` on the ignore walker. Agents reading the filesystem see `.env`, `.git/`, `.DS_Store` and silently pick them up in greps/listings — security + privacy leak. | SMALL | pi + opencode default to hidden-skip; opt-in only |

### F-HIGH — real correctness gaps

| Phase | Description | Size | Source |
|---|---|---|---|
| ✅ F3 | **Compress cut-point can split a tool_use/tool_result pair.** `handle_compress` (`ui/slash.rs:93-101`) does a reverse token-budget scan but doesn't check whether the cut-boundary message has `Interrupted` / pending tool calls. After compress the LLM may see an orphan `tool_use` block. | MEDIUM | opencode `splitTurn` (`compaction.ts:161-184`) respects turn boundaries |
| ✅ F4 | **`read` capped at 10MB with no streaming.** `read.rs:98` does `read_to_string()` whole-file; refuses anything bigger. Large logs / generated files fail. | MEDIUM | opencode `read.ts:119-150` streams + early-terminates; pi `read.ts:215-328` smart-truncates |
| ✅ F5 | **ACP parallel tool calls lose id correlation.** `acp/mod.rs:229-231` uses a single `last_tool_call_id`. Two parallel tool calls + two results → only the second pairs correctly; the first becomes an orphan with an empty id. | SMALL | opencode tracks callID on each ToolPart |
| ✅ F6 | **Bash has no process-group cleanup on timeout/abort.** `bash.rs:76-87` spawns via tokio `.output()` without `setpgid`. Timeout kills only the parent; subprocess tree orphans. | MEDIUM | pi `bash.ts:76-81` uses `detached: true` + `killProcessTree(pid)` |
| ✅ F7 | **Permission `check_path` doesn't canonicalize symlinks.** `resolve_absolute` (`checker.rs:370-376`) joins paths but doesn't follow symlinks. Symlink to `/etc` bypasses `/etc/**` deny. | SMALL | symlink-aware canonicalize at check time |
| ✅ F8 | **No session schema versioning / corruption recovery.** `storage.rs::load_session` deserializes raw JSON; one truncated byte breaks the whole session. No migration path for schema bumps. | MEDIUM | opencode versions message metadata; pi has explicit migration fns |
| 🚫 F9 | **Mid-stream decode failure suppresses retry when `had_tool_calls=true`.** `runner.rs:418-427` skips retry if any tool ran. But "tool dispatched, result pending, stream died mid-decode" is the case we DO want to retry — the result event never arrived. | MEDIUM | pi distinguishes "tool dispatched + result received" from "tool dispatched + still pending" |
| ✅ F10 | **Bash fallback splitter (no `semantic-bash` feature) doesn't respect quotes.** `bash.rs:145-163` splits `;`/`&&`/`||` literally. `echo "; rm -rf /"` splits inside the quoted string. Default builds have `semantic-bash` enabled so this only affects `--no-default-features`. | SMALL (mandate semantic-bash) | opencode arity tokenizer handles quotes |

### F-MEDIUM — robustness + edge cases

| Phase | Description | Size | Source |
|---|---|---|---|
| 🚫 F11 | **`edit` allows in-call overlapping ranges.** Multi-region replace doesn't check that two `old_text` substrings don't overlap. Order-of-application matters; user is expected to know. | SMALL | pi `edit.ts:31-50` rejects overlaps in schema |
| ✅ F12 | **Bash output is sequential, not interleaved.** `bash.rs:89-102` reads stdout then stderr and concatenates. Time-order lost. | SMALL | pi streams both via `onData()` |
| ✅ F13 | **Compress token math can report negative net savings.** When new summary > replaced messages, `compress_reporting` still reports "saved N tokens" misleadingly. | SMALL | opencode validates `tail+summary` fits budget before issuing call |
| ✅ F14 | **`Retry-After` provider header ignored.** `recovery.rs:38-48` uses fixed exponential backoff regardless of what the API asked. Anthropic + OpenAI send retry-after. | SMALL | opencode reads `retry-after-ms` header |
| 🚫 F15 | **`capture_partial_on_abort` doesn't distinguish failed vs interrupted tools.** Tool entries that errored stay `Interrupted` instead of `Failed`. `convert_history` emits different text per state, so the LLM sees "interrupted" when it should see the error. | SMALL | opencode `finalizeInterruptedAssistant` preserves tool state |
| ✅ F16 | **Plugin load order is lexicographic, undocumented.** Multi-file plugin directories sort by filename. Author renames trigger silent order changes. | SMALL | document + recommend numeric prefixes (`00-`); future C3 manifest |
| ✅ F17 | **Plugin context mutability across hooks not documented.** `dispatch_tool_hook` clears slots before the loop, so hook B sees hook A's mutations. Intentional but undocumented. | SMALL | doc comment |
| ✅ F18 | **No relative-path normalization in `is_external_path`.** `checker.rs:334-341` returns `false` for relative paths. In `Accept` mode, `../../etc/passwd` bypasses external_directory rules. | SMALL | normalize via `resolve_absolute` first |
| ✅ F19 | **`read` doesn't strip UTF-8 BOM.** Old Windows files render as invisible-byte-prefix in the LLM context. | SMALL | opencode `Bom.readFile()` detects + strips |
| ✅ F20 | **Unbounded interjection channel.** `mpsc::UnboundedSender<()>` accumulates if user types ahead of the runner. Only the first wakeup matters; channel grows. | SMALL | switch to bounded(64) with `try_send` |

### F-SKIP — verified false positives, design choices, or N/A

Came back from the surveys but NOT real issues after verification.
Documented here so future audits don't re-raise:

- **🚫 F9 — Mid-stream decode retry**: rig dispatches tools
  synchronously inside its stream loop. By the time we observe
  `ToolCall`, side effects are applied; retry would re-execute.
  `had_tool_calls=true → no retry` is the correct safe behavior.
- **🚫 F11 — Edit overlap detection**: dirge's edit tool is
  single-region (one `old_text` / `new_text` per call). The
  overlap concern applies to the future multi-file atomic edit
  (Track A3), not the current tool.
- **🚫 F15 — Failed vs Interrupted tool state**: rig's
  `ToolResult` has no `is_error` field; dirge can't reliably
  distinguish "tool ran and returned error text" from "tool ran
  successfully" without heuristic string-sniffing. Error text
  still reaches the LLM via `Completed{result=error_text}`.
- **System message position violation**: dirge already prepends the
  compaction summary as the single System message at index 0;
  `convert_history` loops from `first_kept` (typically 1).
- **Permission external-directory double-check**: the secondary
  check at `checker.rs:226-231` only fires when `matched.is_empty()
  && action == Action::Allow`. Explicit allow rules are NOT
  overridden silently — it's defense-in-depth, not a bug.
- **Tree cycle detection**: `switch_to_leaf`'s parent walk could
  loop on a corrupted tree, but `tree.entries` is host-controlled
  and corruption would require external mutation.
- **Write not atomic**: for single-file writes the value of
  temp+rename is marginal; defer.
- **Cache string keys**: tool-prefixed keys (`read:path:offset`)
  have no collision risk in practice.
- **Empty / consecutive assistant messages**: rig filters before
  `convert_history` sees them.

### Status

All actionable Track F items shipped:

- F1 / F2 (CRITICAL): PRs #76, #77.
- F3 / F4 / F5 / F6 / F7 / F8 / F10 (HIGH): PRs #78–#84.
- F12 / F13 / F14 / F16 / F17 / F18 / F19 / F20 (MEDIUM):
  PRs #85, #86, #87, #89, #90, #91.
- F9 / F11 / F15: verified false positives / N/A — see F-SKIP.

## Ideas queue (re-evaluate before scheduling)

- A7 image generation tool
- C6 JetBrains plugin
- C7 plugin registry
- D9 mermaid rendering
- Multi-session views (neither ref project has this)
- Semantic search over conversation history
- Real-time token-rate display (TUI animation)
- Parallel LLM request batching with dedup
