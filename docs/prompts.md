# Prompts system

Prompts change the agent's behavior and tone, and can declare tool
restrictions enforced at the permission layer. Switch at runtime with
`/prompt [name]`.

## Built-in prompts

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

## Per-prompt tool restrictions

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

## Custom prompts

Custom prompts can be placed in `$XDG_CONFIG_HOME/dirge/prompts/` as `.md` files.

## Context files

The agent automatically loads `AGENTS.md` or `CLAUDE.md` from the project root,
ancestor directories, and `~/.config/dirge/agent/AGENTS.md` as a global
fallback. Use `-n` / `--no-context-files` to disable.
