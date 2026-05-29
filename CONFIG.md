# Configuration

dirge reads an optional JSON config file named `config.json` from its config
folder:

- If `DIRGE_CONFIG_DIR` is set: `$DIRGE_CONFIG_DIR/config.json`
- Otherwise: the platform config directory joined with `dirge/config.json`
  (for example `$XDG_CONFIG_HOME/dirge/config.json` on Linux)
- Fallback: `$HOME/.config/dirge/config.json`

All config keys are optional. CLI flags and their environment-backed values
(such as `DIRGE_PROVIDER` and `DIRGE_MODEL`) take precedence where both exist.

Example:

```json
{
  "provider": "openrouter",
  "max_tokens": 8192,
  "temperature": 0.7,
  "context_window": 128000,
  "reserve_tokens": 16384,
  "keep_recent_tokens": 20000,
  "compact_enabled": true,
  "default_prompt": "code",
  "default_permission_mode": "standard",
  "show_tool_details": true,
  "show_edit_diff": true,
  "tool_result_max_chars": 500,
  "tool_result_max_lines": 4,
  "providers": {
    "openrouter": {
      "model": "deepseek/deepseek-v4-flash"
    },
    "local-vllm": {
      "provider_type": "openai",
      "base_url": "http://localhost:8000/v1",
      "api_key_env": "VLLM_API_KEY"
    }
  },
  "permission": {
    "*": "ask",
    "rules": [
      { "op": "edit",    "match": "**/*.rs",   "effect": "allow" },
      { "op": "edit",    "match": "**",        "effect": "ask"   },
      { "op": "execute", "match": "cargo test", "effect": "allow" },
      { "op": "execute", "match": "rm **",     "effect": "deny"  }
    ],
    "external_directory": [
      { "match": "/tmp/**", "effect": "allow" },
      { "match": "/**",     "effect": "ask"   }
    ],
    "doom_loop": "ask"
  }
}
```

Accepted top-level keys:

| Key                       | Type    | Description                                                                                                                                                                 |
| ------------------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `provider`                | string  | Active provider alias. Built-ins are `openrouter`, `openai`, `anthropic`, `gemini`/`google`, `deepseek`, `glm`/`zhipu`, and `ollama`; any alias declared in `providers` is also accepted. Default: `openrouter`. See [Providers and roles](#providers-and-roles). |
| `providers`               | object  | Map of provider alias → entry. The active model lives in `providers.<active-provider>.model`. Each role key below points at one of these aliases. See [Providers and roles](#providers-and-roles). |
| `review_provider`         | string  | Provider alias for the background session-review pass. Falls back to `provider`. |
| `escalation_provider`     | string  | Provider alias for the one-shot retry after repair-exhaustion / pre-write syntax failure. Falls back to `provider` (no-op when equal). |
| `summarization_provider`  | string  | Provider alias for context compaction. Falls back to `provider`. |
| `subagent_provider`       | string  | Provider alias for `task` tool subagents. Falls back to `provider`. |
| `max_tokens`              | integer | Maximum response tokens. Default: `8192`.                                                                                                                                   |
| `max_agent_turns`         | integer | Maximum agent turns per response. Default: `100`.                                                                                                                           |
| `temperature`             | number  | Model sampling temperature in `0.0`–`2.0`. `--temperature` CLI flag overrides this. Values outside the range are clamped with a stderr warning.                            |
| `no_tools`                | boolean | Disable all tools. Default: `false`.                                                                                                                                        |
| `no_context_files`        | boolean | Disable loading global/project `AGENTS.md` and `CLAUDE.md` context files. Default: `false`.                                                                                 |
| `context_window`          | integer | Session context-window size used for status and auto-compaction. Default: `128000`.                                                                                         |
| `reserve_tokens`          | integer | Tokens to reserve before compaction is triggered. Default: `16384`.                                                                                                         |
| `keep_recent_tokens`      | integer | Approximate recent-token budget kept verbatim during compaction. Default: `20000`.                                                                                          |
| `compact_enabled`         | boolean | Enable automatic conversation compaction. Default: `true`.                                                                                                                  |
| `permission`              | object  | Permission rules; see the permission config notes below.                                                                                                                    |
| `restrictive`             | boolean | Select restrictive permission mode. Overridden by `accept_all`/`yolo` if those are also true.                                                                               |
| `accept_all`              | boolean | Select accept mode, equivalent to `--accept-all`. Overridden by `yolo` if true.                                                                                             |
| `yolo`                    | boolean | Select yolo mode, auto-approving all operations.                                                                                                                            |
| `sandbox`                 | boolean | Run bash commands in the bubblewrap sandbox. Default: `false`.                                                                                                              |
| `default_permission_mode` | string  | Permission mode when no mode boolean/CLI flag is set. Use `standard`, `restrictive`, `accept`, or `yolo`.                                                                   |
| `show_tool_details`       | boolean | Show tool-result output in the TUI. Default: `true`.                                                                                                                         |
| `show_edit_diff`          | boolean | Show colorized diff output for `edit` tool results (`-` red, `+` green, `@@` cyan). Default: `true`.                                                                        |
| `tool_result_max_chars`   | integer | Hard ceiling on characters per tool result. Default: `500`. Combined with `tool_result_max_lines` (lines applied first; chars trim what's left).                                |
| `tool_result_max_lines`   | integer | Body lines shown inside a tool chamber before collapsing to `↓ N more lines (Ctrl+O to expand)`. Default: `4`. Press `Ctrl+O` to re-print the most recent collapsed result in full. `edit`, `apply_patch`, `question`, `task`, and `task_status` are exempt (their body IS the value). |
| `default_prompt`          | string  | Prompt name to activate on startup. Default: `code`.                                                                                                                        |
| `theme`                   | string  | UI color theme. `phosphor` (default — 80s CRT green-on-black), `plain` (pre-theme white/cyan), or any `<name>.theme.json` file in the config dir. See [docs/themes.md](docs/themes.md). |
| `tools`                   | object  | Optional per-tool enable map. Currently honors `tools.websearch` and `tools.webfetch` (both `bool`, default `true`); set either to `false` to drop the tool from the registered set even when its env vars are present. |
| `mcp_servers`             | object  | MCP server map when compiled with the `mcp` feature. When omitted, defaults to a single Exa Web Search server; see below.                                                   |
| `acp_servers`             | object  | ACP server config map when compiled with the `acp` feature. See the ACP section below.                                                                                       |

## Providers and roles

Providers are declared once in the `providers` map and referenced by alias from
the role-assignment keys — so each role can run on a different model:

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

Each `providers` entry accepts:

| Field | Description |
|-------|-------------|
| `provider_type` | Built-in provider type to speak. Optional — defaults to the entry's alias when that alias matches a built-in name. |
| `base_url` | Endpoint base URL (for custom / self-hosted endpoints). |
| `model` | Model name for this provider. |
| `api_key` | Literal key or `${ENV_VAR}` interpolation. Takes precedence over `api_key_env`. |
| `api_key_env` | Name of the env var holding the API key. |
| `allow_insecure` | Allow `http://` URLs (plaintext). Default `false`; only enable for local-only proxies. |
| `stream_chunk_timeout_secs` | Per-provider streaming chunk timeout override. |
| `options` | Free-form per-provider model options; currently honors `temperature`. |

The aliases on the left of the map become the values you write in
role-assignment keys.

### Role assignments

| Key | Used for | Falls back to |
|-----|----------|---------------|
| `provider` | Default / main loop | (none — required) |
| `review_provider` | Background session-review pass | `provider` |
| `escalation_provider` | One-shot retry after repair-exhaustion / pre-write syntax failure | `provider` (no-op when equal) |
| `summarization_provider` | Context compaction | `provider` |
| `subagent_provider` | `task` tool subagents | `provider` |

When a role's provider equals `provider` (either explicitly or by fallback), no
duplicate client is constructed and the feature has zero overhead — escalation
routes, for example, simply don't fire because they'd be a no-op anyway.

> **Migration note**: dirge no longer reads the legacy top-level `model`,
> `custom_providers`, or `review_model` keys — starting a session with any of
> those at the root fails fast with a migration hint. Move `model` inside the
> active provider's entry, `custom_providers.<name>` entries directly into
> `providers`, and `review_model` into the entry referenced by
> `review_provider`.

## Permissions

Permission actions are lowercase strings: `allow`, `ask`, or `deny`. `rules`
is an **ordered list** read top-to-bottom; **last match wins**. Each rule has:

- `op` — the operation class it governs (NOT a tool name). One of:
  `read`, `edit`, `execute`, `network`, `mcp`, `memory`, `skill`,
  `agent`, `meta`, or `*` (any). `edit` covers write/edit/apply_patch —
  they're one operation, so one rule governs all three.
- `match` — a glob. Read/edit use path-style globs (`*` is one path
  segment, `**` spans directories); execute/network/mcp use shell-style
  (`*` matches anything including `/`, trailing ` *` makes args optional).
  The `*` (any) op uses shell-style too, since it can match commands and
  MCP keys as well as paths. MCP patterns match the full key
  `mcp_tool:{server}:{tool}`.
- `effect` — `allow`, `ask`, or `deny`.
- `tool` *(optional)* — narrow the rule to a single concrete tool name
  (e.g. `"grep"`) instead of the whole op.

Use `"*"` for the default action, `external_directory` (also a `rules`
list, op defaults to `*`) for absolute-path rules outside the working
directory, and `doom_loop` for the retry-loop hard-deny (set to `allow`
to disable it). dirge always installs its built-in safe bash allow/deny
rules and a read-only/memory/skill/in-cwd-write allow set beneath your
rules; your `rules` override them.

MCP tools default to `ask` for ALL servers — they execute external code
(the server's implementation, plus whatever filesystem / network / API
effects it has), and silent default-allow let entire query sequences run
before any prompt fired. To re-enable silent allow for a trusted server:

```json
{
  "permission": {
    "rules": [
      { "op": "mcp", "match": "mcp_tool:lattice:*", "effect": "allow" }
    ]
  }
}
```

Or accept once at the alert and pick "allow always" for the same
session-allowlist effect.

### Mode semantics

- **`standard`** (default): every rule in `permission` is consulted; tools without
  matching rules fall back to `*` (default `allow`).
- **`restrictive`**: like `standard`, but any tool whose rule resolves to `allow`
  via the `*` fallback (no explicit allow rule matched) is converted to `ask`.
  Explicit `allow` rules still allow. Explicit `deny` rules still deny.
- **`accept`** (equivalent to `--accept-all`): auto-allows tools whose targets
  resolve inside the working directory; tools touching paths outside still
  consult `external_directory` rules.
- **`yolo`** (equivalent to `--yolo`): bypasses every check. Use with caution.

CLI precedence (high → low): `--yolo` > `--accept-all` > `--restrictive` >
`default_permission_mode` config > `standard`.

When compiled with MCP support, `mcp_servers` accepts command-based and URL-based
servers:

```json
{
  "mcp_servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
      "env": {}
    },
    "semantic-index": {
      "command": "my-indexer",
      "args": ["--repo", "/work/other-project"],
      "allow_external_paths": true
    },
    "remote-search": {
      "url": "https://example.com/mcp",
      "headers": {
        "authorization": "Bearer token"
      }
    }
  }
}
```

If `mcp_servers` is omitted (`null`) and the `mcp` feature is enabled, dirge
adds a default Exa Web Search MCP server at `https://mcp.exa.ai/mcp` with the
`x-api-key` header set to `EXA_API_KEY` when that environment variable is set.
Set `"mcp_servers": {}` to disable all MCP servers.

### Per-server external-path opt-in (`allow_external_paths`)

By default an MCP tool call whose JSON arguments name a path resolving outside
the working directory is refused with a clear error — matching the trust model
of dirge's built-in file tools (`read` / `write` / `edit` anchored to cwd).
The check scans top-level args fields named `path`, `file_path`, `file`,
`directory`, `dir`, `cwd`, and the `paths` array.

Some MCP servers legitimately need broader scope: a semantic indexer pointed
at a sibling repo, a project-wide search tool, a backup utility. Set
`"allow_external_paths": true` on that one server's config (both `Command` and
`Url` variants accept it; default `false`) to skip the cwd guard for tools
from THAT server only.

The flag is path-scoped and narrow:

- It only bypasses the cwd-external-path check.
- It does NOT bypass `mcp_tool` deny rules, prompt `deny_tools` frontmatter,
  doom-loop detection, the sandbox, or `--yolo`/`--restrictive` mode logic —
  every other gate runs unchanged.
- It applies per-server: enabling it on `semantic-index` does not affect
  `filesystem` or any other server in the same config.

Pair it with a tight `mcp_tool` rule for layered control, e.g.:

```json
{
  "mcp_servers": {
    "semantic-index": {
      "command": "indexer",
      "allow_external_paths": true
    }
  },
  "permission": {
    "rules": [
      { "op": "mcp", "match": "mcp_tool:semantic-index:*",          "effect": "allow" },
      { "op": "mcp", "match": "mcp_tool:semantic-index:write_file", "effect": "deny"  }
    ]
  }
}
```

### MCP tools and prompt deny-lists

Per-prompt `deny_tools` frontmatter (see "Prompt restrictions" below) applies
to MCP tools too. The deny gate matches against three names for each MCP tool
call:

- the raw tool name as exported by the MCP server (e.g. `edit`, `write_file`),
- the qualified `mcp_tool:<server>:<name>`,
- the umbrella `mcp_tool` (denies every MCP tool from every server).

So a plan-mode prompt that ships `deny_tools: [edit, write, apply_patch, bash]`
also blocks any MCP server that exports a tool named `edit` / `write` /
`apply_patch` / `bash`. Use `mcp_tool` as a blanket deny when in doubt about
what an MCP server might expose.

## Plugin trust boundary

The Janet plugin system runs INSIDE the trust boundary. Plugin hooks
(`on-tool-start`, `on-tool-end`) can mutate tool inputs, block tool calls,
and replace tool outputs with arbitrary text. They cannot, however, bypass
the permission checker (`check_perm*` runs inside the inner tool, after the
plugin pre-hook). If you load third-party plugins, treat them with the same
care you'd give to executing third-party code in your shell — the plugin's
trust level effectively equals the user's. There is no sandboxing.

## Streaming timeouts

dirge applies a per-chunk read deadline to streaming LLM responses so a
silently-dropped TCP connection (which reqwest can't always detect) doesn't
freeze the agent. The default is 5 minutes (`300s`) — well above any
legitimate reasoning gap from Claude 3.7 extended thinking, GPT-5 thinking,
or large-tool-output processing. Bump it if you see false-positive
`stream chunk timed out` errors in the middle of a turn.

Resolution order (first hit wins):

1. `providers.<name>.stream_chunk_timeout_secs` — per-provider override
2. top-level `stream_chunk_timeout_secs` — applies to every provider
3. `300s` default

Provider name matching is case-insensitive (`anthropic` matches
`--provider Anthropic`).

```json
{
  "stream_chunk_timeout_secs": 300,
  "providers": {
    "anthropic": { "stream_chunk_timeout_secs": 900 },
    "ollama":    { "stream_chunk_timeout_secs": 60 },
    "my-vllm": {
      "provider_type": "openai",
      "base_url": "http://localhost:8000/v1",
      "api_key_env": "VLLM_API_KEY",
      "stream_chunk_timeout_secs": 1200
    }
  }
}
```

## Environment variables

| Variable | Purpose |
|----------|---------|
| `EXA_API_KEY` | API key for the built-in `websearch` tool and the default Exa MCP server. Without this the `websearch` tool emits a startup warning and is not registered. |
| `DIRGE_WEBFETCH_ALLOW_PRIVATE` | Set to `1` (or any non-empty value) to allow `webfetch` to call private / loopback IPs. By default `webfetch` enforces SSRF protection — it refuses `localhost`, `127.x`, `10.x`, `172.16-31.x`, `192.168.x`, and link-local addresses. Override only in trusted local-dev contexts; never set this in production environments that touch attacker-influenced URLs. |
| `WEBSEARCH_ENABLED` / `WEBFETCH_ENABLED` | Force-enable the corresponding tool when not enabled via `tools.*` config. Useful in container builds where you set the toggle once via env rather than per-config-file. |

## LSP configuration

When compiled with the `lsp` feature (default-on), dirge spawns language
servers on demand to surface compile errors in tool output. The `lsp` config
key accepts three forms:

```json
// Default-on, built-in commands for rust/typescript/pyright/clojure-lsp.
{ "lsp": true }

// Off entirely. Same as the --no-lsp CLI flag.
{ "lsp": false }

// Default-on with per-server overrides.
{
  "lsp": {
    "rust": {
      "command": ["rust-analyzer"],
      "env": { "RA_LOG": "rust_analyzer=debug" },
      "initialization": { "cargo": { "buildScripts": { "enable": true } } }
    },
    "typescript": { "disabled": true }
  }
}
```

Per-server fields (all optional):

| Field            | Type             | Description |
| ---------------- | ---------------- | ----------- |
| `command`        | string[]         | argv to launch the server. Replaces the built-in default. |
| `extensions`     | string[]         | *Reserved.* Currently ignored — see "Known limitations" below. |
| `env`            | object           | extra env vars for the child process. |
| `initialization` | object           | sent as `initializationOptions` in the LSP `initialize` request. |
| `disabled`       | boolean          | `true` removes the server entirely. |

CLI flag: `--no-lsp` (overrides the config; same effect as `lsp: false`).

### Built-in server commands

| Server id     | Default command                              |
| ------------- | -------------------------------------------- |
| `rust`        | `rust-analyzer`                              |
| `typescript`  | `typescript-language-server --stdio`         |
| `pyright`     | `pyright-langserver --stdio`                 |
| `clojure-lsp` | `clojure-lsp`                                |

Servers are spawned lazily on first file touch and cached per `(workspace_root, server_id)` pair. Concurrent agent tool calls for the same file deduplicate so dirge never races two `rust-analyzer` processes against one workspace.

### Known limitations

- The `extensions` override is currently ignored. The claimed-extensions list lives in the static `builtin_servers()` registry at `src/lsp/server.rs`. Adding new extensions today requires editing that file. Follow-up.
- v1 has four built-in servers. Additional servers can be added by extending `builtin_servers()` + `ProcessSpawner::default_commands()` in source.

## ACP (Agent Communication Protocol) configuration

When compiled with the `acp` feature, dirge can act as an ACP agent server.
The following config keys are available:

| Key           | Type    | Description                                            |
| ------------- | ------- | ------------------------------------------------------ |
| `acp_servers` | object  | Named ACP server configurations (see below)            |

dirge's ACP runs over stdio only; the `acp_host` / `acp_port`
keys that earlier docs mentioned have been removed from the CLI
and config in favor of editors driving the agent via stdio.

ACP server configs (in `acp_servers`) support two transport types:

```json
{
  "acp_servers": {
    "tcp-server": {
      "host": "127.0.0.1",
      "port": 7243,
      "api_key": "optional-key"
    }
  }
}
```

When `--acp` is passed without `--acp-host`, dirge runs in stdio mode
(the editor spawns it as a subprocess). With `--acp-host`, it listens on TCP.
