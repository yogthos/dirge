# Permissions

Every tool call dirge's agent makes — reading a file, running a shell
command, writing an edit, calling an MCP server — passes through ONE
authorization engine before it executes. The engine answers a single
question, **Allow / Ask / Deny**, and records *why*. This page explains
how that decision is made and how to shape it.

## The model: one decision point

dirge uses a **Policy Decision Point (PDP)**. A tool normalizes its
intent into an `AccessRequest` and calls `Engine::authorize`, which
returns a `Decision`. There is no per-tool gate logic scattered around
the codebase — adding a tool or a rule never means re-implementing a
check.

A request is made of one or more **claims**, each a pair of:

- an **operation** — the *kind* of action, and
- a **resource** — the *thing* acted on.

| Operation | Tools |
|-----------|-------|
| `Read` | read, grep, list_dir, glob, find_files, lsp, the semantic readers |
| `Edit` | write, edit, apply_patch (and bash redirect / mutation targets) |
| `Execute` | bash command segments |
| `Network` | webfetch, websearch |
| `Mcp` | MCP server tool calls |
| `Memory` | the memory store |
| `Skill` | skill load/list (read) and create/edit/patch (write) |
| `Agent` | the recursive `task` tool |
| `Meta` | no-effect internal tools (write_todo_list, task_status, question) |
| `Other` | unknown / plugin tools |

Most tool calls are a single claim. A **bash command is one request with
many claims** — an `Execute` claim per command segment plus an `Edit`
claim per redirect target and mutation path (`rm`, `mv`, `touch`, …) —
so `cmd > out.txt && other` is authorized **atomically** and prompts at
most once, never gate-by-gate.

## How a decision is reached

Each claim is evaluated in two stages; the registered policies run in a
fixed, documented precedence.

**Stage A — deciders (first claim wins; may loosen).** The first policy
to claim a resource sets its base effect:

| # | Policy | Effect |
|---|--------|--------|
| 1 | prompt-deny | terminal **Deny** if the active prompt's `deny_tools` forbids the tool (beats even Yolo) |
| 2 | yolo | terminal **Allow** when `--yolo` |
| 3 | session-allow | terminal **Allow** for anything you picked "allow always" for this session |
| 4 | configured-rule | your configured rules, **last match wins** |
| 5 | builtin-allow | the **sane defaults** (below) |
| 6 | external-dir | out-of-project paths → your `external_directory` rules, else Ask |
| 7 | default | the configured fallback (Ask) |

Then **Accept-mode coercion** runs (the one place a mode *loosens*): in
`--accept-all`, a base `Ask` becomes `Allow` for low-risk, in-project
operations — never for shell/MCP/network/agent ops or out-of-tree paths.

**Stage B — modifiers (monotone; may only tighten).** Currently one:
the **loop guard**. It never gates an already-allowed op; it only acts
when an op was *already* going to prompt and the agent keeps retrying
the identical call — after a threshold it hard-**Deny**s to break a true
loop. (Re-reading a file or re-writing in your project never starts
prompting just because it happened more than once.)

**Per request:** the claims' effects combine **most-restrictive-wins**
(`Deny > Ask > Allow`). One `Ask` anywhere → one prompt for the whole
request.

## Sane defaults (builtin-allow)

Out of the box, with no configuration, these never prompt:

- **Reads** anywhere (read, grep, list_dir, the semantic readers).
- **Writes/edits inside the project directory.** Writes *outside* the
  project still prompt.
- **Memory and skill** operations.
- **`/dev/null`** as a write target.
- A curated set of safe **bash** commands (git status/log/diff, cargo,
  test runners, …); see the built-in bash rules.
- No-effect internal tools (todo list, task status, the question tool).

These are code, not config, so they can't drift — and an explicit
config rule (precedence 4) always overrides them.

## Configuration

Permissions are configured under the `permission` key. `rules` is an
**ordered list**; each rule names the operation it governs (`op`), a glob
to `match`, and the `effect`. Reading top-to-bottom, **last match wins**:

```jsonc
{
  "permission": {
    "*": "ask",                                          // default for anything unmatched
    "rules": [
      { "op": "execute", "match": "cargo *",   "effect": "allow" },
      { "op": "execute", "match": "git push *", "effect": "deny"  },
      { "op": "edit",    "match": "/etc/**",   "effect": "deny"  }  // governs write + edit + apply_patch
    ],
    "external_directory": [
      { "match": "/shared/**", "effect": "allow" }
    ]
  }
}
```

- **`op` is the operation class, not a tool name.** Values: `read`,
  `edit`, `execute`, `network`, `mcp`, `memory`, `skill`, `agent`,
  `meta`, or `*` (any). `edit` covers write/edit/apply_patch — they're
  one operation, so one rule governs all three.
- **Narrow to a tool** with an optional `"tool": "<name>"` field when a
  rule should apply to a single concrete tool rather than the whole op.
- **Glob semantics** are inferred from the op: read/edit use path-style
  globs (`*` is one path segment, `**` spans directories);
  execute/network/mcp use shell-style (`*` matches anything including
  `/`, trailing ` *` makes args optional). The `*` (any) op uses
  shell-style too, since it can match commands and MCP keys as well as
  paths.
- **Last-match-wins** across the ordered `rules` list — put general
  rules first, specific overrides last.
- `external_directory` is itself a `rules` list (op defaults to `*`)
  governing access to paths outside the project root.
- Set `"doom_loop": "allow"` to disable the retry-loop hard-deny.

### Security modes

Set with `--standard` (default), `--accept-all`, `--restrictive`, or
`--yolo` (or `default_permission_mode` in config):

| Mode | Behavior |
|------|----------|
| **Standard** | builtin defaults + your rules; unmatched mutating ops Ask |
| **Accept** | coerces in-project `Ask`s to Allow; shell/MCP/network still Ask |
| **Restrictive** | every write/edit (incl. in-project, memory/skill writes) Asks |
| **Yolo** | allow everything — except a prompt's `deny_tools`, which still wins |

### LLM auto-approval (`approval_provider`)

Instead of pausing for **you** on every `Ask`, dirge can route the decision
to a separate model that judges whether the operation is safe and
reasonable, and answers `ALLOW` or `DENY` automatically. Opt in by setting
`approval_provider` to a provider alias:

```json
{
  "approval_provider": "deepseek-flash",
  "providers": {
    "deepseek-flash": {
      "provider_type": "deepseek",
      "model": "deepseek-v4-flash",
      "api_key": "${DEEPSEEK_API_KEY}"
    }
  }
}
```

How it works:

- It only intercepts the **Ask** outcome. Hard **Deny** rules (e.g.
  `rm -rf /**`) and builtin **Allow**s are unaffected — the evaluator can
  never override a deny, and never sees an already-allowed action.
- The evaluator is given the command, the working directory, and a
  per-resource danger summary (each path tagged *inside* / *outside* the
  project), plus a fixed safety rubric. It is told to **deny when in
  doubt**. The rubric flags, among others: deleting or modifying files
  outside the project or a temp dir; committing/pushing in a repo outside
  the project (or any `git push`); fetching-and-running remote code;
  privilege escalation (`sudo`), disk/device ops (`dd`/`mkfs`); and reading
  or transmitting credentials.
- **Fail-safe.** An unparseable verdict counts as DENY. If the evaluator
  call itself errors (network, bad key), dirge falls back to prompting
  **you** — it never silently allows on failure.
- It's a side-LLM call per prompt, so there's latency and token cost; it's
  off unless `approval_provider` is set. Like `critic_provider`, it has no
  fallback to your default provider — set it explicitly.

This is orthogonal to the security mode: in `--yolo` nothing prompts so the
evaluator is never consulted; in the other modes it stands in for the human
on each `Ask`.

## "Allow always" and the session allowlist

Choosing **(a) allow always** at a prompt adds a session-scoped grant
(op-scoped, so one "allow always" on an edit covers write/edit/apply_patch).
Manage it with the `/allow` command:

| Command | Effect |
|---------|--------|
| `/allow` or `/allow list` | List the current grants, each with a `[n]` index |
| `/allow add <tool> <pattern>` | Add a grant manually, e.g. `/allow add bash 'cargo *'` |
| `/allow remove <n>` | Drop the grant at index `n` (from `/allow list`) |
| `/allow clear` | Drop all grants |

Bare `/allow` is shorthand for `/allow list`. Grants are dropped when you
change the working directory (no privilege carry-over between projects).

## `/why` — explain a decision

`/why <tool> [input]` dry-runs a decision and prints the full trace: the
final effect, the deciding policy and its reason, and every applicable
policy's vote. Use it to understand exactly what governs an action.

```
/why bash cargo test
why: bash "cargo test"
  → Allow  (rule "bash:cargo *" → Allow (configured-rule))
  · prompt-deny      (n/a)  not applicable
  · configured-rule  Allow  rule "bash:cargo *" → Allow
  · …

/why write /etc/hosts
why: write "/etc/hosts"
  → Ask  (outside the working directory (external-dir))
```
