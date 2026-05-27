# Storyboard 02 — Permission ask on a `bash` write

## Scenario

The user is in **standard** permission mode. The agent decides to run
`rm /tmp/oldlogs/*.log`. Standard mode's default for `bash` is "ask".
The user is prompted, picks **Allow once**, then later the agent runs
`rm /tmp/oldlogs/server.log` (a single-file variant). Different
arguments → a fresh prompt.

A third invocation tries `rm /etc/hosts`. The `external_directory: { "/etc/**": "deny" }`
rule in `config.json` fires before the user is asked — denied without
prompting.

## What the user sees

```
[$_$] running: bash
  > rm /tmp/oldlogs/*.log

  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  permission required: bash
  command: rm /tmp/oldlogs/*.log
  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  [a]llow once  [A]llow always  [d]eny
```

The choices map to the three `UserDecision` variants:
`AllowOnce`, `AllowAlways(String)`, `Deny`. There is no separate
"stop the agent" choice from this prompt — denying causes the LLM
to receive a tool error which it can react to, and Ctrl-C
elsewhere in the UI is the way to terminate the run.

User presses `a`:

```
  ✓ allowed (once)

  removed 12 files (3.4 MB freed)
```

A few turns later the agent runs `rm /tmp/oldlogs/server.log` — a
**different** input string, so the session allowlist (which records the
exact match from "Allow always") does not apply, and "Allow once" was
already consumed. Same prompt fires again.

Finally the agent tries `rm /etc/hosts`:

```
[$_$] running: bash
  > rm /etc/hosts

  ✗ denied by rule: external_directory "/etc/**"
```

No prompt; the deny rule fires before `ask` is reached.

## Code trace

### Step 1 — Tool dispatch reaches the permission checker

- `BashTool::call` in `src/agent/tools/bash.rs` invokes the bash-segment
  splitter (via tree-sitter when `semantic-bash` is enabled). Each
  segment is independently submitted to `check_bash_segments`
  (`src/agent/tools/bash.rs:362`).
- `check_bash_segments` calls `enforce(...)` (defined in
  `src/agent/tools/mod.rs`) for each segment and for each
  extracted mutation path. `enforce` is the public surface that
  takes the per-tool checker via `Arc<Mutex<PermissionChecker>>`,
  runs `PermissionChecker::check`/`check_path`, and routes the
  Ask outcomes through `handle_ask_inner`
  (`src/agent/tools/mod.rs:195`).

### Step 2 — Checker evaluates rules in defined order

`PermissionChecker::check` (`src/permission/checker.rs:369`) evaluates:

  1. **Prompt deny-list** (`is_prompt_denied(tool)`, line 380) —
     refused immediately if the active prompt's `deny_tools`
     frontmatter includes the tool. Runs BEFORE the yolo short-
     circuit (security contract: `--yolo` does NOT override
     prompt-mode restrictions).
  2. **MCP concrete-name deny** (PERM-7, line 394) — for
     `tool == "mcp_tool"`, parses the `mcp_tool:<server>:<name>`
     input and additionally probes the bare `<name>` against
     `is_prompt_denied`.
  3. **Yolo short-circuit** (line 403) — `mode == Yolo` returns
     `CheckResult::Allowed` regardless of remaining rules.
  4. **Session allowlist** (`is_session_allowed`, line 407) —
     exact-pattern matches from prior "Allow always" grants.
  5. **Per-tool rules** (the `rules.get(tool)` loop at line 416) —
     `bash`/`edit`/`write`/etc. patterns from config.
  6. **`external_directory`** patterns (`match_ext_dir`) — for
     path-shaped inputs the rule applies inside `check_path`
     (`src/permission/checker.rs:548`) regardless of mode. For
     bash, PERM-6's `extract_mutation_paths` walker pulls write
     targets out of the command (e.g. `/etc/hosts` from
     `rm /etc/hosts`) and submits each to `enforce("write",
     Scope::PathResolve(...))` which routes through `check_path`.
     In Accept mode the `match_ext_dir` lookup is ALSO consulted
     inline at line 440 to override the Ask→Allow coercion when an
     external_directory rule says Deny.
  7. **Mode-specific defaults** — Restrictive promotes Allow → Ask;
     Accept demotes Ask → Allow inside cwd EXCEPT for the high-
     risk-tool list (PERM-19); Standard keeps the matched action.
  8. **Doom-loop check** (PERM-1/2, line 446-466) — when the
     resolved action is not Deny, looks up the per-key counter; if
     `>= 2` (i.e. this would be the 3rd identical call), the
     `doom_loop_action` (default Ask) overrides.

For `rm /etc/hosts`: rule 6 fires → `CheckResult::Denied(...)`. No
prompt; the agent gets `ToolError::Msg("...denied by rule...")` and
surfaces it to the LLM as a tool error.

### Step 3 — Prompt fires for `rm /tmp/oldlogs/*.log`

- For the first `rm /tmp/oldlogs/*.log`, rules 1-4 all resolve to
  Ask. `enforce` calls `handle_ask_inner`
  (`src/agent/tools/mod.rs:195`) which sends an `AskRequest`
  (`src/permission/ask.rs`) into the UI's question channel.
- The UI's question handler renders the prompt box. Keys `a` / `A` / `d`
  map to `UserDecision::AllowOnce` / `UserDecision::AllowAlways(pattern)` /
  `UserDecision::Deny` (`src/permission/ask.rs`).

### Step 4 — "Allow once" returned

- `UserDecision::AllowOnce` → `enforce` returns `Ok(resolved_path)`.
- The session allowlist is NOT updated. Same exact input on a
  subsequent call would prompt again.
- The doom-loop counter (`recent_calls` deque + `repeat_counts`
  HashMap from PERM-1, see `src/permission/checker.rs:808`)
  increments. After 3+ identical calls in the 32-call window the
  doom-loop policy fires (`doom_loop_action`, threshold `>= 2` at
  line 835 — i.e. the COUNT of prior identical calls, so this
  trips on the 3rd identical call matching the README's
  "3+ identical" claim).

### Step 5 — `rm /tmp/oldlogs/server.log` (different input)

- The doom-loop key is `format!("{}\x00{}", tool, input)`. With a
  different `input` (`*.log` vs `server.log`), this is a different
  HashMap entry — counter starts at 0. No doom-loop trip.
- The session allowlist is keyed by the exact pattern, not by glob
  expansion — `*.log` doesn't allowlist `server.log`. Fresh prompt.

## Cross-references

- **PERM-1** (per-key counter via `repeat_counts` HashMap):
  `src/permission/checker.rs:808` (`track_doom_loop`),
  `repeat_counts` field declared at line 50.
- **PERM-2** (check-before-track ordering): `src/permission/checker.rs:441-466`
- **PERM-3** (re-canonicalize cwd): `src/permission/checker.rs:760-806`
  (`is_external_path` — comment at lines 778-784 explains the
  refresh-on-each-check semantics).
- **PERM-6** (mutation-paths extraction on bash): main call sites
  at `src/agent/tools/bash.rs:393-394` (complex path) and
  `src/agent/tools/bash.rs:429-431` (segment path).
- **PERM-7** (prompt-deny probe in checker):
  `src/permission/checker.rs:385-402` (the MCP concrete-name
  branch inside `check`).
- **PERM-19** (Accept mode still asks for bash/webfetch/task/memory/skill/apply_patch): `src/permission/engine.rs:18-31`

## Edge cases verified

- **`accept-all` mode**: rule 2 (external_directory) still fires —
  Accept doesn't override deny rules, only coerces Ask → Allow inside
  cwd.
- **`yolo` mode**: skips all rules EXCEPT prompt deny-list and the
  active-prompt `deny_tools` frontmatter. `/etc/hosts` rm via yolo
  WOULD execute unless the user is in `plan`/`ask`/`review` mode
  (which deny bash entirely via deny_tools).
- **Symlink swap between check and open**: closed by H12 + PERM-3 —
  `check_perm_path_resolve` canonicalizes the path; the tool uses
  the canonical form for the open. PERM-3 additionally
  re-canonicalizes the cwd on each check so a symlink rewrite of
  the working_dir doesn't misclassify in-tree paths as external.
