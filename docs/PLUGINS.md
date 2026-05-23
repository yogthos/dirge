# Plugin author's guide

dirge embeds [Janet](https://janet-lang.org) as a plugin language. Plugins
are small Janet scripts that hook into the agent loop, observe and rewrite
inputs and outputs, register custom slash commands, gate tool execution,
and (with the P4 session-tree work) drive branch/fork/navigation from
code.

This guide covers every hook, every `harness/*` API, and the patterns
that make plugins easy to write and easy to debug.

> Requires building with `--features plugin`. The default `cargo install`
> already includes it; verify with `dirge --version`.

---

## Table of contents

1. [Where plugins live](#where-plugins-live)
2. [Anatomy of a plugin](#anatomy-of-a-plugin)
3. [Hook reference](#hook-reference)
4. [Harness API reference](#harness-api-reference)
   1. [Logging and introspection](#logging-and-introspection)
   2. [Prompt control](#prompt-control)
   3. [Tool interception](#tool-interception)
   4. [Notifications and entries](#notifications-and-entries)
   5. [Renderers](#renderers)
   6. [Slash commands](#slash-commands)
   7. [Custom tools](#custom-tools)
   8. [Keyboard shortcuts](#keyboard-shortcuts)
   9. [Message renderers](#message-renderers)
   10. [User dialogs](#user-dialogs)
   11. [Custom LLM providers](#custom-llm-providers)
   12. [Session-tree control](#session-tree-control)
5. [Workflow patterns](#workflow-patterns)
6. [Debugging plugins](#debugging-plugins)
7. [Threading model and pitfalls](#threading-model-and-pitfalls)
8. [Worked examples](#worked-examples)

---

## Where plugins live

dirge auto-loads plugins from these directories, in order:

| Path | Scope |
|------|-------|
| `~/.config/dirge/plugins/` (or `$XDG_CONFIG_HOME/dirge/plugins/`) | Global — applies to every project |
| `./.dirge/plugins/` (relative to cwd at startup) | Project-local — overrides globals |

Project-local plugins are loaded after globals, so if both declare a hook
or slash command with the same name, the project-local one wins.

A plugin is **either**:

- A single `*.janet` file. The file's stem (`my-plugin.janet` → `my-plugin`)
  becomes the plugin's namespace.
- A directory containing one or more `*.janet` files. The directory name
  is the plugin's namespace, and every `*.janet` file inside is loaded
  into the *same* Janet environment in alphabetical order. Files share
  state, harness registrations, helper functions — they're effectively
  concatenated.

```
~/.config/dirge/plugins/
  hello.janet                   ← single-file plugin (namespace: "hello")
  my-workflow/                  ← multi-file plugin (namespace: "my-workflow")
    00-state.janet              ← loaded first (alpha order)
    01-hooks.janet              ← can reference vars from 00-state.janet
    02-commands.janet           ← can reference everything above
```

Conventional alphabetical filenames (`00-state.janet`, `01-hooks.janet`)
let you control load order when one file depends on definitions in
another. Naming is convention only — there's no manifest, no required
entry point, no file-level metadata.

---

## Anatomy of a plugin

A plugin is just Janet code. The harness looks for top-level functions
with specific names and calls them on matching events. Anything else the
file does at load time (registering renderers, providers, slash commands)
takes effect immediately.

Minimal plugin:

```janet
# ~/.config/dirge/plugins/hello.janet

(defn on-prompt [ctx]
  (harness/notify (string "user said: " (ctx :prompt)) :info))
```

That's it. No exports, no setup boilerplate. Load dirge with the plugin
feature, type a message, and the notify line appears in the chat.

### How hooks are discovered

After your plugin is loaded, the harness scans the Janet environment for
hook functions. You can define them in two equivalent styles:

```janet
# Bare style — most natural; the host aliases this to
# `{namespace}-on-prompt` under the hood so it survives other plugins
# defining the same bare name.
(defn on-prompt [ctx] ...)

# Namespaced style — explicit; useful when you want the function name
# to match what shows up in the host's `registered hook:` log line.
(defn my-workflow-on-prompt [ctx] ...)
```

For a multi-file plugin, define the hook in any of the plugin's files —
the host scans the shared environment, so it doesn't matter which file
the function lives in.

Multiple plugins can register the same hook; they each contribute a
distinct result, and dispatch order is load order.

### How `harness/*` calls work

`harness/*` symbols are pre-loaded by the host before your plugin file
runs, so they're always available. Most of them either:

- **Queue an op** on a per-session buffer that the host drains between
  events (notifications, entries, tree mutations).
- **Set a slot** the host reads after the hook returns (tool gating,
  prompt rewrite, pending prompt).
- **Block-call the host** via a registered C function (dialogs).

You don't need to know the implementation — just call them and the host
takes care of the rest.

---

## Hook reference

Every hook takes a single `ctx` table (immutable from the plugin's POV)
and returns either nil or a string. The return value is hook-specific —
some accumulate into a chat-visible response, some get ignored.

| Hook | Fires | `ctx` contents | Return value |
|------|-------|---------------|--------------|
| `on-init` | Once at session start, after config + agent are ready | `{:model "..." :cwd "..." :provider "..."}` | Ignored |
| `on-prompt` | After the user submits a message, before the LLM call | `{:prompt "<text>"}` | Optional string — appended to the system prompt for this turn; `harness/replace-prompt` replaces the user message itself |
| `on-response` | After a single LLM response is received | `{:response "<text>"}` | Ignored (use for logging/notifications) |
| `on-tool-start` | Before any tool call (`bash`, `read`, `write`, MCP tools) | `{:tool "<name>" :args {...}}` | Ignored — use `harness/block` / `harness/mutate-input` |
| `on-tool-end` | After the tool returns its result | `{:tool "<name>" :output "<text>"}` | Ignored — use `harness/replace-result` |
| `on-error` | A tool or LLM call raised an error | `{:error "<msg>"}` | Ignored |
| `on-complete` | The agent has finished its multi-turn response | `{}` | Ignored |
| `on-turn-start` | Start of one LLM call cycle within a single run | `{:index N}` | Ignored |
| `on-message-update` | Every ~16 streamed tokens during the turn | `{:index N :partial "<text-so-far>"}` | Ignored |
| `on-turn-end` | After this turn's tool results return | `{:index N :message "<full-text>"}` | Ignored |

### Subtle distinctions

- **`on-prompt` vs `on-turn-start`**: `on-prompt` fires once per user
  message; `on-turn-start` fires once per LLM call (a single user message
  can spawn multiple turns when the model uses tools).
- **`on-response` vs `on-turn-end`**: `on-response` is the legacy
  one-call-per-agent-completion hook; `on-turn-end` fires for every
  intermediate turn, with `:index` so you can distinguish them.
- **`on-tool-start` runs *after* permission checks**. If the user denied
  the tool, neither `on-tool-start` nor the actual tool runs.
- **Multi-plugin `harness/block`: first-wins.** When two or more plugins
  register `on-tool-start` and one of them calls `(harness/block reason)`,
  dispatch stops there — subsequent plugins do NOT run for this tool call.
  Block reason is the first blocker's; load order matters. Matches pi's
  `runner.ts:806-827` semantics. `harness/mutate-input` and
  `harness/replace-result` keep the chained last-write-wins behavior so
  successive plugins can refine each other's mutations.
- **Subagents (`task` tool) are isolated**: when the LLM calls the `task`
  tool, dirge runs a one-shot LLM query with no tools, no plugin hooks,
  no permission gates, and no plugin state. Hooks registered for
  `on-tool-start` / `on-tool-end` do NOT fire for tools the subagent
  might internally consider — because the subagent has no tool access
  at all. Matches opencode's and pi's subagent design: isolation by
  default. If you need plugin observability into subtask work, route it
  through `harness/log` from the parent's `on-response` or `on-turn-end`
  hooks instead.
- **Multi-file plugin load order is lexicographic.** When a plugin
  directory contains multiple `.janet` files, they're loaded in
  **lexicographic filename order** (e.g. `00-init.janet` before
  `hooks.janet` before `state.janet`). The load is one shared Janet
  worker, so later files see definitions from earlier files. If
  one file depends on a helper defined in another, name the
  defining file to sort first — the conventional `NN-prefix`
  pattern (`00-`, `01-`, …) is the easiest way. Renaming files
  silently changes load order; if hooks register in different
  positions you may see different behavior from the same plugin
  code.
- **`harness/mutate-input` and `harness/replace-result` chain
  across hooks.** When multiple plugins register for the same
  `on-tool-start` / `on-tool-end`, the slot for the mutated value
  is cleared once at the START of dispatch, and EACH hook sees
  whatever the prior hook wrote. Last-write-wins for mutations:
  hook B can read what hook A set in `harness-mutate-input` (via
  the tool args it sees) and decide whether to overwrite, refine,
  or no-op. This is intentional and unlike `harness/block`, which
  stops dispatch entirely after the first writer. If you want
  isolated mutations, gate by checking a sentinel before writing
  (e.g. a custom key in the context table).

---

## Harness API reference

### Logging and introspection

```janet
(harness/log "message")          # prints "[plugin] message" to stderr
(harness/get-cwd)                # returns the agent's working directory
```

Plain debugging aids. `harness/log` shows up in the dirge log file (or
stderr in dev), not in the chat — use `harness/notify` for chat-visible
output.

### Prompt control

```janet
# Queue a follow-up prompt — runs as a fresh turn after the current one.
(harness/request-prompt "now run the tests")

# Replace the *current* user message before the LLM sees it. Call from
# on-prompt. The original user message is discarded entirely.
(harness/replace-prompt "rewritten version of the user's message")
```

- `harness/request-prompt`: think of it as "the plugin pushing a prompt
  onto a queue." Useful for `on-response` hooks that want to chain a
  follow-up turn automatically.
- `harness/replace-prompt`: think of it as "rewrite what the LLM sees
  this turn." Only meaningful from `on-prompt`.

```janet
# Capture the latest LLM response so the next `on-prompt` hook can
# read it from the binding `harness-response`. The host calls
# `(harness/store-response text)` itself after every turn, so you
# normally don't need to invoke this directly — read `harness-response`
# from inside `on-prompt` to inspect the previous assistant message.
# If you DO want to fabricate a "previous response" for your own
# state machine (e.g. seeding a test fixture), you can call it
# explicitly.
(harness/store-response "the previous assistant message text")
```

- `harness/store-response`: sets the `harness-response` binding so the
  next `on-prompt` hook can react to what the LLM said last turn. The
  host wires this automatically; plugins call it only for testing or
  to seed a synthetic prior response.

### Tool interception

These three set slots that the host inspects right after each tool hook.
Most useful inside `on-tool-start` / `on-tool-end`:

```janet
(defn on-tool-start [ctx]
  (when (= (ctx :tool) "bash")
    (let [cmd (get-in ctx [:args "command"])]
      (when (string/find "rm -rf" cmd)
        (harness/block "denied: dangerous deletion")))))

(defn on-tool-start [ctx]
  (when (= (ctx :tool) "write")
    # Force every write under /tmp/safe regardless of what the LLM asked.
    (harness/mutate-input "{\"path\":\"/tmp/safe/out.txt\",\"content\":\"...\"}")))

(defn on-tool-end [ctx]
  (when (= (ctx :tool) "bash")
    (let [out (ctx :output)]
      (when (> (length out) 5000)
        (harness/replace-result (string/slice out 0 5000))))))
```

| API | Effect |
|-----|--------|
| `harness/block reason` | Tool is not executed; the LLM sees `reason` as the tool error. Plugins after this one in the chain still run, but their slot writes are ignored. |
| `harness/mutate-input json-str` | The tool runs with the rewritten args. Pass a JSON string — the host re-parses it into the tool's input shape. |
| `harness/replace-result text` | The actual tool output is discarded; the LLM sees `text` instead. |

If multiple plugins set the same slot, the first non-nil wins (block beats
mutate beats nothing).

### Notifications and entries

```janet
# One-shot chat lines, levels map to colors.
(harness/notify "task complete")              # :info — dim grey
(harness/notify "drift detected" :warn)       # yellow
(harness/notify "broken: see log" :error)     # red

# Typed timeline entries that survive save/load.
(harness/append-entry "bookmark" "milestone-1")           # display=true (default)
(harness/append-entry "telemetry" "{\"cost\":0.02}" false) # persisted, not shown
```

**Notifications** are ephemeral — they render once and aren't stored.

**Entries** are persistent — they become part of the session. Pair them
with a renderer (next section) for custom display, or rely on the default
dim JSON dump.

`display=false` is for plugin state that should round-trip via session
save/load but isn't user-facing (think: a counter, a last-seen timestamp,
a cached scrape).

### Renderers

A renderer turns a persisted entry's opaque `data` string into displayable
lines. Register one at load time:

```janet
(defn render-bookmark [data]
  # data is whatever you passed as the second arg to append-entry.
  (harness/render "cyan" (string "★ " data)))

(harness/register-renderer "bookmark" "render-bookmark")
```

| API | Description |
|-----|-------------|
| `(harness/register-renderer type fn-name)` | Associates a custom_type with a Janet function. Pass the function's name as a string; the host looks it up later. |
| `(harness/render color text)` | Inside a renderer, emits one chat line. Colors: `cyan`, `red`, `yellow`, `green`, `blue`, `magenta`, `white`, `black`, `grey` (alias `darkgrey`), plus `dark*` variants (`darkred`, `darkgreen`, etc.). Keyword forms like `:cyan` are accepted. Unknown names fall back to grey. |

If no renderer is registered for the entry's type, the host dumps the raw
`data` in dim grey.

### Slash commands

```janet
(defn echo-handler [args]
  (string "you said: " args))

(harness/register-command "echo" "echo-handler")
```

Now typing `/echo hello world` in the chat calls `(echo-handler "hello world")`
and displays the return string. Return `nil` to display nothing.

The handler runs synchronously on the Janet worker thread; long-running
handlers will stall the agent until they return.

### Custom tools

Plugins can register tools the LLM calls directly, not just intercept
built-in ones. Mirrors pi's `api.registerTool({...})`.

```janet
(defn echo-tool-handler [args]
  # `args` is the raw JSON string the LLM produced. Plugins parse
  # themselves if they want structured fields.
  (string "echo received args: " args))

(harness/register-tool
  "plugin_echo"                          # LLM-visible tool name
  "Echoes the args back verbatim."       # description shown to the LLM
  "Plugin Echo"                          # UI display label
  "{\"type\":\"object\",\"properties\":{\"msg\":{\"type\":\"string\"}},\"required\":[\"msg\"]}"
  "echo-tool-handler"                    # handler fn name
  :parallel)                             # optional: :parallel or :sequential
```

| Field | Notes |
|-------|-------|
| `name` | What the LLM sees in tool calls. Must not collide with a built-in (`read`, `bash`, etc.); collisions drop the plugin tool with a warning. |
| `description` | Shown to the LLM in the tool list. State *when* to use the tool and the expected arg shape; this is what the model reads. |
| `label` | UI display name (chat banner). Falls back to `name` when empty. |
| `parameters` | JSON-schema string. Parsed once at startup; invalid JSON falls back to `{}` with a `tracing::warn`. |
| `handler` | Janet function name. Called as `(handler args-json-string)`. Returns either a string (used directly) or any value `(string …)` can render. Errors surface to the LLM as tool failure. |
| `execution-mode` | `:parallel` (default; read-only) or `:sequential` (mutating). One sequential tool forces the whole tool batch sequential. Pass `nil` here when you only want to set `prepare-arguments`. |
| `prepare-arguments` | Optional 7th positional — name of a Janet function that runs BEFORE schema validation to normalize the LLM-supplied args. Receives the raw JSON args string; returns a JSON string the loop validates. Errors / invalid JSON / non-string returns fall back to the original args. Mirrors pi's `prepareArguments?` (extensions/types.ts:443). |

```janet
# Optional: normalize args before the loop validates them.
(defn prep-echo [args]
  # Wrap legacy positional input as a structured object the schema
  # expects. Returns a JSON string.
  (string "{\"msg\":" args "}"))

(harness/register-tool
  "plugin_echo" "Echoes arg" "Echo"
  "{\"type\":\"object\",\"properties\":{\"msg\":{\"type\":\"string\"}}}"
  "echo-tool-handler"
  :parallel
  "prep-echo")
```

See [`plugins/example_tool.janet`](../plugins/example_tool.janet).

**Cancellation semantics.** Plugin tools run synchronously on the
Janet worker thread, which can't be preempted mid-evaluation. When
the user hits Ctrl+C/Esc:

- The agent loop sees the abort and returns to the user immediately
  (the dispatcher races the tool future against the cancel signal).
- A plugin tool that hasn't started yet is short-circuited before it
  even acquires the PluginManager lock; its handler does not run.
- A plugin tool that has already entered the Janet handler runs to
  completion in the background — its return value is discarded, but
  the Janet VM holds the PluginManager mutex until the handler
  returns. Subsequent plugin-tool calls (and any other PM consumer)
  block on that lock.

Keep handler bodies bounded. There is no `signal.aborted` equivalent
inside Janet because the worker can't yield mid-call.

### Keyboard shortcuts

Bind a key combination in interactive mode. Mirrors pi's
`api.registerShortcut(KeyId, {handler})`.

```janet
(defn refresh-handler [key]
  (string "F5 (" key ") pressed"))

(harness/register-shortcut "f5"     "refresh-handler" "Refresh chat (demo)")
(harness/register-shortcut "ctrl-s" "save-handler"    "Save (demo)")
```

**Key spec grammar** (case-insensitive):

```
(modifier "-")* key-name
```

| Modifiers | `ctrl`, `control`, `alt`, `meta`, `shift` |
|-----------|-------------------------------------------|
| Key names | A single character, `f1`..`f12`, or: `enter`, `esc`, `tab`, `backspace`, `space`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `delete`, `insert` |

Examples: `"ctrl-x"`, `"alt-shift-f"`, `"f5"`, `"ctrl-alt-enter"`.

Handlers receive the matched key spec as a single string argument so
one handler can serve many bindings. Returning a non-nil string
surfaces as a chat line.

**Reserved keys** that plugins **cannot** override (kill signals + core
UX): Ctrl+C, Ctrl+D, Esc (mid-run cancel), the search and rewind picker
keys, Ctrl+O (expand collapsed tool result), Ctrl+X (drop interjection),
PageUp / PageDown / Home / End. Plugin shortcuts dispatch AFTER those
but BEFORE text input — any unused combination is yours.

Plugins are snapshotted at UI startup; new bindings require restarting
the host to take effect. Bad specs are dropped with a `tracing::warn`
so a typo doesn't break the host.

See [`plugins/example_shortcut.janet`](../plugins/example_shortcut.janet).

### Message renderers

Provide a Janet function the UI invokes when a `LoopMessage::Custom`
event reaches the chat. Custom messages enter the loop via
`harness/add-custom-message` (typically from a `prepare-next-run` or
`on-turn-end` hook) and are filtered out of the LLM context — they're
UI-only. Without a registered renderer the UI falls back to printing
the payload's `content` verbatim.

Mirrors pi's `api.registerMessageRenderer(customType, renderer)`.

```janet
(defn render-status [payload]
  # payload is the wrapper JSON carrying customType, content, display.
  (string "■ status: " payload))

(harness/register-message-renderer "status" "render-status")

(defn prepare-next-run [ctx]
  # Three-arg form: customType + content (+ optional display).
  (harness/add-custom-message "status" "another turn complete"))
```

**`harness/add-custom-message` call shapes** (pi parity — see
pi `CustomMessage` in `core/messages.ts:46`):

| Shape | customType | content | display |
|-------|-----------|---------|---------|
| `(harness/add-custom-message "text")` | `""` | `"text"` | `true` |
| `(harness/add-custom-message "status" "text")` | `"status"` | `"text"` | `true` |
| `(harness/add-custom-message "status" "text" false)` | `"status"` | `"text"` | `false` |

`display=false` keeps the message in the transcript (plugin handlers
can observe it on subsequent turns) but suppresses the chat row. Use
for telemetry or state markers you don't want to clutter the chat.

The renderer's `payload` argument is the **full wrapper JSON** —
`{"role": "custom", "customType": ..., "content": ..., "display": ...}`
— not just the inner content. Plugins parse what they need.

Distinct from [`harness/register-renderer`](#renderers), which handles
session-timeline entries (bookmarks, telemetry) — message renderers
fire live mid-conversation.

See [`plugins/example_message_renderer.janet`](../plugins/example_message_renderer.janet).

### User dialogs

These are the only synchronous APIs that round-trip through the UI. The
Janet worker thread blocks while the dialog is shown; the UI thread
continues to render. The pair is safe to call from any hook.

```janet
# Returns true if the user confirms, false on Cancel/Esc.
(if (harness/confirm "Confirm" "Run the migration?")
  (harness/notify "running..." :info)
  (harness/block "user said no"))

# Returns the selected string, or nil on cancel.
(let [choice (harness/select "Pick a model" ["gpt-4" "claude-4" "deepseek"])]
  (when choice
    (harness/notify (string "switching to " choice) :info)))
```

These dialogs respect the UI's selection picker, so users can use arrow
keys / Esc to cancel. If the host is shutting down while a dialog is
in flight, the dialog returns `false` / `nil` so the plugin can unblock.

### Custom LLM providers

Register an OpenAI-compatible (or any other rig-supported) endpoint as
a first-class provider:

```janet
(harness/register-provider
  "local-openai"                       # name surfaced in /model
  "openai"                              # provider type
  "http://localhost:8000/v1"            # base URL
  "LOCAL_OPENAI_API_KEY")               # env var holding the API key
```

After registration, `/model local-openai/<model-id>` switches to that
provider. Config-declared `custom_providers` in `~/.config/dirge/config.toml`
win on name collision, so users can override plugin defaults.

### Session-tree control

The session is stored as a node-based tree (each message has a parent
link), and these APIs let plugins drive navigation programmatically.
They mirror pi's `ctx.setLabel` / `ctx.fork` / `ctx.navigateTree` /
`ctx.newSession` / `ctx.switchSession`.

Plugins queue ops on a per-session buffer; the host drains and applies
them between UI events. There's no synchronous return value — verify via
`/tree` or subsequent hook context.

```janet
# Label a node so it's easy to find later in /tree.
(harness/set-label entry-id "milestone-1")
(harness/set-label entry-id nil)            # clears the label

# Branch off the chosen entry. Default position is :before — the
# entry's *parent* becomes the new leaf and the entry's text is
# restored to the editor so the user can re-edit and re-submit.
(harness/fork entry-id)

# :at position — the entry itself becomes the leaf. No editor restore.
(harness/fork entry-id :at)

# Move the active leaf to entry-id. Role-aware: user messages go to
# the parent + restore text (same as fork :before); other roles
# become the new leaf directly.
(harness/navigate-tree entry-id)

# Persist the current session and start a fresh one in place.
# Optionally record the prior session id as parent lineage.
(harness/new-session)
(harness/new-session "previous-session-uuid")

# Load a saved session by id prefix. The current session is persisted
# first; the agent's model/provider/cwd are preserved.
(harness/switch-session "abc12345")
```

| API | Slash equivalent | Notes |
|-----|------------------|-------|
| `harness/set-label` | (none) | Read back via `/tree` (labels render in `[brackets]`) |
| `harness/fork` | `/fork [id]` | `:before` (default) is the same as `/fork`; `:at` skips the editor restore |
| `harness/navigate-tree` | `/tree <id>` (non-user msgs) or `/fork <id>` (user msgs) | Picks the right behavior based on the target's role |
| `harness/new-session` | `/clear` (rough equivalent) | Stronger than `/clear` — assigns a new session id and persists the old one |
| `harness/switch-session` | `/sessions <prefix>` | Same id-prefix resolution semantics |

**Where do entry ids come from?** Hook contexts. The host plans to thread
`:id` through `on-message-update` / `on-turn-end` so plugins can stash
them. For now, a plugin that wants to label "the most recent entry" can
track that itself across hooks (see `plugins/session_tree.janet`).

---

## Workflow patterns

### Inversion of control

The harness drives the LLM through scripted phases rather than waiting for
the model to decide what to do. The built-in `workflow.janet` plugin
demonstrates this:

1. `on-prompt` detects a feature-request prompt and sets a phase var.
2. `on-response` checks the phase var — when the model says "done with
   the plan," the plugin calls `harness/request-prompt` to start the
   implementation phase.
3. After implementation, the plugin queues another prompt to start the
   review phase.

Net effect: the user types one prompt, and the harness shepherds three
turns through architect → implementor → review.

### Gate-then-augment

`on-tool-start` is the natural place to combine `harness/confirm` with
`harness/block`:

```janet
(defn on-tool-start [ctx]
  (when (and (= (ctx :tool) "bash")
             (string/find "rm" (get-in ctx [:args "command"])))
    (unless (harness/confirm "Confirm" "Run dangerous bash?")
      (harness/block "user denied dangerous bash"))))
```

The plugin pauses the tool call, asks the user, and either allows or
blocks. The agent sees nothing about the dialog — just the block (or
the tool's normal result).

### Cross-hook state

The Janet env is shared across hooks in the same plugin file. Use a
plain `var`:

```janet
(var last-tool-name nil)

(defn on-tool-start [ctx]
  (set last-tool-name (ctx :tool)))

(defn on-tool-end [ctx]
  (harness/notify (string last-tool-name " finished") :info))
```

State persists for the life of the session — survives between turns.

---

## Debugging plugins

- **`harness/log`** writes to dirge's log file (not the chat). Use it
  freely.
- **Janet errors** in a hook are caught (so a broken hook can never
  crash the host or block other plugins from dispatching), and the
  error surfaces in TWO places:

  1. **Chat banner** — a red `[plugin] hook <hook>.<fn> errored:
     <message>` notification appears at the next loop tick (drained
     out of `harness-notif-list` like a regular `harness/notify`
     `:error` call). The user sees it inline with the chat without
     having to know about logs.
  2. **Structured log** — a `tracing::warn!` with target
     `dirge::plugin` and fields `hook` / `function` / `error`.
     Visible via `dirge --verbose` (or
     `RUST_LOG=dirge::plugin=warn`). Includes the Janet stack line
     so you can jump straight to the offending form.

  The hook's return value is treated as `nil` (no effect on the
  host) and dispatch continues to the next plugin.

  **Why "log + notify + continue" rather than "abort the turn"**: a
  single misbehaving plugin shouldn't be able to wedge the user out
  of their session. The two-surface approach is modeled on pi's
  `ExtensionError` flow (see
  `packages/coding-agent/src/core/extensions/runner.ts` —
  `emitError` calls registered listeners, hosts wire
  `onError: (e) => ctx.ui.notify("Compaction failed: ...", "error")`).
  opencode logs only (`log.error("plugin config hook failed", …)`
  in `packages/opencode/src/plugin/index.ts`); dirge does both so
  plugin authors get the visible signal without having to enable
  logs.

  **Quick debugging recipe**:
  - Notice the red `[plugin] hook X.Y errored: ...` in the chat?
    Your plugin's `Y` hook threw. The message is the Janet error.
  - Run with `dirge --verbose` for the full Rust-side log including
    `dirge::plugin` warn events plus dirge's own debug-level traces.
  - Use `(harness/log "...")` from inside your hook to add ad-hoc
    breadcrumbs — these appear in the same `dirge::plugin` log
    stream when verbose mode is on.
- **Hook didn't fire?** Double-check the function name matches the
  hook reference exactly — `on_prompt` (underscore) is a different
  symbol than `on-prompt`.
- **`harness/notify`** is the easiest "did this code run?" probe — it
  lights up the chat without dumping data into the LLM context.
- **No plugin feature?** Run `dirge --version`; if "plugin" isn't in the
  feature list, rebuild with `cargo install --features plugin ...`.

### Reading existing plugin state

For non-trivial debugging, drop a slash command:

```janet
(defn dump-handler [_args]
  (string "last-tool-name=" last-tool-name "\n"
          "phase=" current-phase))

(harness/register-command "dump-state" "dump-handler")
```

Now `/dump-state` shows your plugin's internal state in the chat.

---

## Threading model and pitfalls

Janet runs on a **dedicated worker thread**. The agent and UI run on
separate threads. Implications:

- Hooks are serialized — only one runs at a time. You can't accidentally
  introduce race conditions inside Janet code.
- `harness/confirm` / `harness/select` are safe — the worker blocks
  while the UI thread renders. They are the only APIs that block the
  worker on user input.
- Long-running Janet code blocks every subsequent tool call. If you need
  to compute something expensive, do it asynchronously by having the
  host fire a follow-up turn (`harness/request-prompt`) once the work is
  ready.
- **Hot reloading** — there isn't any. Edit a plugin file, restart
  dirge to pick up changes.

Common gotchas:

- **Janet table key types matter**. `(get ctx :tool)` works; `(get ctx
  "tool")` does not (the host passes keywords). If unsure, do
  `(harness/notify (string ctx))` to dump the structure.
- **Strings vs keywords for harness args**. Most APIs accept both
  (e.g. `:info` and `"info"` for `harness/notify` levels). When in doubt,
  use the keyword form — that's what the host serializes back internally.
- **`harness/block` only takes effect inside hooks**. Calling it from a
  slash-command handler does nothing.

---

## Worked examples

The [`plugins/`](../plugins/) directory has a working example of each
feature. Read these in order to build intuition:

1. **`hello_cmd.janet`** — simplest plugin: one slash command.
2. **`notify_example.janet`** — `harness/notify` from a hook.
3. **`prefix_lang.janet`** — `harness/replace-prompt` for input rewrites.
4. **`protected_paths.janet`** — `harness/block` gating bash + writes.
5. **`confirm_destructive.janet`** — adds `harness/confirm` to the gate.
6. **`select_persona.janet`** — `harness/select` + a slash command.
7. **`bookmark.janet`** — `harness/append-entry` + custom renderer.
8. **`turn_timing.janet`** — `on-turn-start` / `on-turn-end` for telemetry.
9. **`local_openai.janet`** — `harness/register-provider` for a local LLM.
10. **`session_tree.janet`** — `harness/set-label` + `harness/new-session`
    with cross-hook state.
11. **`turn_timer/`** — a *multi-file* plugin. `00-state.janet` defines
    vars, `01-hooks.janet` defines `on-turn-start` / `on-turn-end`, and
    `02-commands.janet` registers a `/timer-stats` slash command — all
    in the same Janet env.
12. **`workflow.janet`** — the full inversion-of-control pattern. Read
    this last; it ties many APIs together.

Each is heavily commented. The reading order above goes from "one API"
to "everything at once."
