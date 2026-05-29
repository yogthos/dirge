# Claude-compatible skills

Skills are on-demand instruction bundles the agent can load mid-session via the
`skill` tool. dirge reads the same format as Claude and opencode.

Place skill directories in `.claude/skills/`, `.opencode/skills/`, or
`.dirge/skills/` in your project or home directory. Each skill is a directory
containing `SKILL.md` with optional YAML frontmatter:

```markdown
---
name: my-skill
description: A helpful skill
---
# Instructions
Detailed skill content here.
```

Skills are auto-discovered at agent startup and listed in the `skill` tool
description. The agent can call `skill "my-skill"` to load the full content on
demand. Project skills override global skills by name.
