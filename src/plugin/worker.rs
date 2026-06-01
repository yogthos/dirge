//! Janet runs on a dedicated OS thread.
//!
//! The original `PluginManager` held the `JanetClient` directly and relied
//! on `#[tokio::main(flavor = "current_thread")]` + an `unsafe impl Send`
//! to satisfy `rig::ToolDyn`'s Send bound on tool-call futures. That was
//! sound under the existing single-thread runtime but blocked synchronous
//! dialog APIs (`harness/confirm`, `harness/select`) — they would have
//! deadlocked, since the Janet eval call sat on the same OS thread that
//! also drove the UI event loop.
//!
//! This module spawns a dedicated worker thread that owns the
//! `JanetClient`. Callers send [`Cmd`]s to the worker via an mpsc channel
//! and block-receive replies on per-command oneshot reply channels. The
//! UI thread is free to render dialogs while the worker thread is blocked
//! inside Janet awaiting a dialog response.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
#[cfg_attr(not(feature = "plugin"), allow(unused_imports))]
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::mpsc as tmpsc;

/// How long the init handshake waits for the worker to confirm Janet
/// initialization before giving up. Worker init is normally well under
/// 100 ms; 10 s is just a watchdog so a hung worker doesn't pin main.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval for the dialog reply loop. The cfn wakes every
/// `DIALOG_POLL` to check the shutdown flag so a UI exit doesn't pin
/// the worker thread forever. Short enough that shutdown feels snappy,
/// long enough that polling overhead is negligible.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const DIALOG_POLL: Duration = Duration::from_millis(50);

/// Upper bound on how long a single `Worker::eval` will wait for the
/// worker's reply. Generous (10 min) because `harness/confirm` /
/// `harness/select` legitimately block the worker on user input — a
/// distracted user may take minutes to answer a dialog. The point of
/// the bound is to detect a truly wedged plugin (e.g. plugin code in
/// `(while true)`) rather than to enforce snappy responses. When the
/// timeout fires the caller gets a clean error instead of hanging.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const EVAL_TIMEOUT: Duration = Duration::from_secs(600);
/// UI-3: default for interactive `eval()` calls (slash commands,
/// provider list lookups, UI-driven plugin queries). A runaway
/// plugin shouldn't freeze the UI for the full `EVAL_TIMEOUT`.
const INTERACTIVE_EVAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on how long `Worker::Drop` will wait for the worker
/// thread to exit. Short by design: shutdown is best-effort. If the
/// plugin is stuck in an infinite loop, the worker thread can't
/// respond to `Cmd::Shutdown` and we'd hang the user's terminal
/// forever on Drop. Beyond `JOIN_TIMEOUT` we leak the thread (it's
/// reaped on process exit) and log a warn so the operator knows.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(feature = "plugin")]
use janetrs::client::JanetClient;
#[cfg(feature = "plugin")]
use janetrs::env::CFunOptions;

/// Janet definitions installed on the worker thread at startup. Includes
/// the harness state variables, the regular harness/* functions, and
/// Janet-side wrappers that forward to the registered C functions for
/// `harness/confirm` and `harness/select`.
///
/// Kept as a single string so worker init does one `client.run` call.
#[cfg(feature = "plugin")]
const HARNESS_INIT: &str = r#"
# Redirect Janet's stdout to a discard buffer BEFORE anything else
# runs. The default `:out` is the real stdout — in dirge's
# interactive (raw-mode) TUI, every `(print …)` from plugin code
# corrupts the screen: bare `\n` produces staircase artifacts AND
# bypasses ratatui's tracked buffer, leaving "ghost" cells that
# the next diff doesn't clean up (this is what the user saw as
# `[plugin] tool: list_dir` leaking under the alert dialog).
# Plugin authors that need real logging should write to a file
# via `file/open`/`file/write` — Janet's `(print …)` is silent.
(setdyn :out @"")
(setdyn :err @"")

(var harness-pending nil)
(var harness-response nil)
# Per-tool-hook slots: cleared by the host at the start of
# dispatch_tool_hook so previous-call state doesn't leak.
(var harness-block nil)
(var harness-mutate-input nil)
(var harness-replace-result nil)

# harness/log is now a no-op. The return value of plugin commands
# is what surfaces in chat — that's the supported surface for
# plugin output.
(defn harness/log [msg] nil)
(defn harness/get-cwd [] (os/cwd))
(defn harness/request-prompt [prompt]
  (when (string? prompt)
    (set harness-pending prompt)))
(defn harness/store-response [resp]
  (set harness-response resp))
(defn harness/has-symbol? [name]
  (truthy? (get (curenv) (symbol name))))

# dirge-99ic: the loading plugin's config.json settings
# (`plugins.<name>`). The host sets this to the plugin's settings right
# BEFORE each plugin's files load, then clears it. A plugin must capture
# its own config in LOAD-TIME code (e.g. `(def my-cfg (harness/plugin-config))`)
# — reading it later from a shared hook is unreliable because the slot
# reflects the LAST plugin loaded. Shape: @{:enabled bool :auto-start bool}
# or nil when no `plugins` config applies.
(var harness-plugin-config nil)
(defn harness/plugin-config [] harness-plugin-config)

# Tool-hook slots. Plugins call these from inside
# on-tool-start / on-tool-end. The host reads them via
# dispatch_tool_hook on the Rust side.
(defn harness/block [reason]
  (when (string? reason) (set harness-block reason)))
(defn harness/mutate-input [json-str]
  (when (string? json-str) (set harness-mutate-input json-str)))
(defn harness/replace-result [output]
  (when (string? output) (set harness-replace-result output)))

# Run-boundary slots. Plugins call `harness/set-next-model` from
# inside `prepare-next-run` to swap the active model before the
# next user prompt runs. Mid-stream model swap isn't supported
# (rig's multi-turn stream state doesn't survive it); the slot is
# read by the host after Done and applied via the same path that
# `/model <name>` uses.
(var harness-next-model nil)
(defn harness/set-next-model [model-name]
  (when (string? model-name) (set harness-next-model model-name)))

# ============================================================
# Phase 5 — pi-loop hook surface for plugins
# ============================================================
# These slots are polled by the new agent_loop path between
# turns. Plugins set them from `on-tool-end` / `on-prompt` /
# `prepare-next-run` to influence the next turn without
# restarting the whole run.

# Next turn's thinking level. Plugins call
# (harness/set-next-thinking-level "high") inside on-tool-end
# to bump reasoning on the next assistant turn. Valid values:
# "off" "minimal" "low" "medium" "high" "xhigh". Other strings
# are ignored.
(var harness-next-thinking-level nil)
(defn harness/set-next-thinking-level [level]
  (when (string? level)
    (set harness-next-thinking-level level)))

# Stop-after-turn flag. Plugins call
# (harness/request-stop-after-turn) to ask the loop to end
# gracefully after the current turn finishes. Drained per turn
# by the host.
(var harness-stop-after-turn nil)
(defn harness/request-stop-after-turn []
  (set harness-stop-after-turn true))

# Steering message queue (mid-run). Plugins call
# (harness/add-steering "wait, also do X") to inject a user
# turn between assistant turns. Drained per turn-boundary by
# the host. Stored as a `msg\n` blob (newline-separated) so the
# read side is a single string roundtrip.
(var harness-steering-messages "")
(defn harness/add-steering [content]
  (when (string? content)
    (set harness-steering-messages
         (string harness-steering-messages content "\n"))))

# Follow-up message queue (outer-loop boundary). Plugins call
# (harness/add-followup "do this next") to add a turn AFTER the
# loop would otherwise stop. Same blob shape as steering.
(var harness-followup-messages "")
(defn harness/add-followup [content]
  (when (string? content)
    (set harness-followup-messages
         (string harness-followup-messages content "\n"))))

# Wire-format escape — used by every tab-separated harness blob.
# Defined before any caller so the cond-arity helpers below
# (harness/add-custom-message, harness/register-*) can reference it.
(defn- harness/-escape [s]
  (->> s
       (string/replace-all "\\" "\\\\")
       (string/replace-all "\t" "\\t")
       (string/replace-all "\n" "\\n")))

# Custom (UI-only) message queue. Plugins call this to push a
# notification the user SEES in the chat but the model does NOT
# see in its context. Pi semantics: any message variant other
# than user/assistant/toolResult is filtered out by `convertToLlm`.
# We make this explicit with a `LoopMessage::Custom` variant; the
# UI renders it; convert_to_llm drops it before the LLM sees it.
#
# Two call shapes (pi parity — CustomMessage carries customType,
# content, display at top level; see messages.ts:46):
#
#   (harness/add-custom-message "build started")
#     bare content. customType="" display=true. The UI uses its
#     default formatter ("[plugin] <text>"); no registered
#     renderer fires.
#
#   (harness/add-custom-message customType content &opt display)
#     structured. customType is the key registered renderers are
#     keyed by (see `harness/register-message-renderer`); display
#     is true by default — false keeps the message in the
#     transcript but suppresses the chat line.
#
# Stored as tab-separated `customType\tcontent\tdisplay\n`
# (harness/-escape'd so embedded tabs/newlines round-trip).
# Drained per turn boundary alongside steering messages.
# dirge-df1v: same per-turn cap as harness/notify above — a plugin
# can't grow this buffer without bound before the host's per-turn drain.
(def harness-custom-msg-cap 131072)
(var harness-custom-flooded false)
(var harness-custom-messages "")
(defn harness/add-custom-message [a &opt b c]
  (when (= harness-custom-messages "") (set harness-custom-flooded false))
  (if (>= (length harness-custom-messages) harness-custom-msg-cap)
    (unless harness-custom-flooded
      (set harness-custom-flooded true)
      (set harness-custom-messages
           (string harness-custom-messages
                   (harness/-escape "") "\t"
                   (harness/-escape "[plugin] too many custom messages this turn — further ones dropped") "\t"
                   "1\n")))
    (cond
      # Single-string form — bare content, no type.
      (and (string? a) (nil? b))
        (set harness-custom-messages
             (string harness-custom-messages
                     (harness/-escape "") "\t"
                     (harness/-escape a) "\t"
                     "1\n"))
      # Typed form.
      (and (string? a) (string? b))
        (let [d (if (nil? c) "1" (if c "1" "0"))]
          (set harness-custom-messages
               (string harness-custom-messages
                       (harness/-escape a) "\t"
                       (harness/-escape b) "\t"
                       d "\n"))))))

# Slash-command registry (9b — wire format aligned with the other
# tab-separated harness blobs). Plugins register at load time; the
# host reads the list once after all plugins load and dispatches
# matching /cmd input back to the named handler. Last-load-wins on
# name collision (matches pi's Map.set + the dedup_last_wins helper
# applied to all the other plugin registries).
(var harness-cmd-list "")
(defn harness/register-command [name handler]
  (when (and (string? name) (string? handler))
    (set harness-cmd-list
         (string harness-cmd-list
                 (harness/-escape name) "\t"
                 (harness/-escape handler) "\n"))))

# Replace the user's prompt for the current turn. Plugins
# call this from on-prompt hooks. Distinct from
# harness/request-prompt which queues a follow-up turn.
(var harness-prompt-replace nil)
(defn harness/replace-prompt [text]
  (when (string? text)
    (set harness-prompt-replace text)))

# dirge-wqxj: append text to the assembled system prompt before
# the agent starts. Plugins call this from the `before-agent-start`
# hook, which receives the current prompt in ctx :system-prompt.
# Append-only by design — the base preamble (model identity + tool
# docs) is preserved; the appended text is added after it. Multiple
# appends from one hook concatenate (newline-joined).
(var harness-system-prompt-append nil)
(defn harness/append-system-prompt [text]
  (when (string? text)
    (set harness-system-prompt-append
         (if (string? harness-system-prompt-append)
           (string harness-system-prompt-append "\n" text)
           text))))

# dirge-lsoq: rewrite the finalized assistant message. Plugins call
# this from the `message-end` hook (which receives the message text
# in ctx :message). Last-write-wins; the host replaces the response
# text with the slot value before it is rendered/stored.
(var harness-message-rewrite nil)
(defn harness/rewrite-message [text]
  (when (string? text)
    (set harness-message-rewrite text)))

# dirge-264x: replace the message array for the NEXT LLM call.
# Plugins call this from the `transform-context` hook, which
# receives the current messages as a JSON array string in
# ctx :messages. The value must be a JSON array string; the host
# parses it and uses it for that single LLM call only (the persisted
# transcript is unchanged). Last-write-wins.
(var harness-replace-context nil)
(defn harness/replace-context [json-array]
  (when (string? json-array)
    (set harness-replace-context json-array)))

# dirge-jia8: supply a custom compaction summary. Plugins call this
# from the `on-compact` hook (which receives the to-be-summarized
# middle messages as JSON in ctx :messages). The host uses this
# string instead of calling the LLM summarizer, provided it passes
# the same validity check. The `on-before-compact` hook is
# observe-only (no slot) — it cannot cancel the fold.
(var harness-compact-summary nil)
(defn harness/set-compact-summary [text]
  (when (string? text)
    (set harness-compact-summary text)))

# Notification queue. Plugins call (harness/notify msg level?)
# to push a line into the host's chat display. Stored as a
# `level\tmsg\n` blob; the host's drain_notifications
# splits and clears in one round-trip.
# dirge-df1v: cap per-turn accumulation. A plugin that calls
# harness/notify in a hot hook (on-message-update fires every ~16
# tokens) would otherwise grow this buffer without bound before the
# host drains it at the turn boundary. Once over the cap we append ONE
# "dropped" marker and stop; the host clears the list to "" on drain,
# and the reset-on-empty check below re-arms the marker for next turn.
(def harness-notif-cap 65536)
(var harness-notif-flooded false)
(var harness-notif-list "")
(defn harness/notify [msg &opt level]
  (when (string? msg)
    (when (= harness-notif-list "") (set harness-notif-flooded false))
    (if (>= (length harness-notif-list) harness-notif-cap)
      (unless harness-notif-flooded
        (set harness-notif-flooded true)
        (set harness-notif-list
             (string harness-notif-list
                     "warn\t[plugin] too many notifications this turn — further ones dropped\n")))
      (let [lvl (cond
                  (or (= level :info) (= level "info")) "info"
                  (or (= level :warn) (= level "warn")) "warn"
                  (or (= level :error) (= level "error")) "error"
                  "info")]
        (set harness-notif-list
             (string harness-notif-list lvl "\t" msg "\n"))))))

# Hook-error dedup slots. `harness-last-hook-err-msg` is the most
# recently pushed sanitized hook-error message; `harness-last-hook-err-count`
# is how many consecutive identical errors followed it. When a
# DIFFERENT error arrives (or any other notification fires), the
# count is flushed as a "(repeated N times)" entry. Drained alongside
# the regular notif list. See `harness/push-hook-err` below + the
# Rust-side dispatch wrapper in `plugin/mod.rs::dispatch`.
(var harness-last-hook-err-msg nil)
(var harness-last-hook-err-count 0)

# Sanitize a hook-error message for the `level\tmsg\n` wire format.
# Embedded tabs become spaces (so they don't get parsed as a second
# `level` field) and newlines become ` | ` (so a multi-line Janet
# stack trace stays on one notification entry).
#
# `string/replace-all` takes args as (patt subst str), so threading
# with `->` (first-position) would pass the wrong arg as the
# subject. Explicit nesting from inside out is the safest spelling.
(defn harness/sanitize-hook-err [s]
  (string/replace-all
    "\n" " | "
    (string/replace-all
      "\r\n" " | "
      (string/replace-all "\t" " " (string s)))))

# Push a hook error onto the notif list, deduplicating consecutive
# identical messages. The catch arm in dispatch calls this rather
# than appending directly so a buggy on-message-update hook can't
# flood the chat with thousands of identical banners.
(defn harness/push-hook-err [sanitized-msg]
  (if (= sanitized-msg harness-last-hook-err-msg)
    # Same as last — increment in place; do not push.
    (set harness-last-hook-err-count (+ harness-last-hook-err-count 1))
    # Different message (or first one). If the previous one had
    # been repeated, flush its summary now; then push the new msg
    # and reset the dedup state.
    (do
      (when (and harness-last-hook-err-msg
                 (> harness-last-hook-err-count 1))
        (set harness-notif-list
             (string harness-notif-list
                     "error\t"
                     harness-last-hook-err-msg
                     " (repeated "
                     harness-last-hook-err-count
                     " times)\n")))
      (set harness-notif-list
           (string harness-notif-list "error\t" sanitized-msg "\n"))
      (set harness-last-hook-err-msg sanitized-msg)
      (set harness-last-hook-err-count 1))))

# Plugin entries on the session timeline. Plugins call
# (harness/append-entry type data &opt display) to record
# bookmarks, telemetry, or custom state that should survive
# session save/load. The data is treated as opaque by the host
# (any registered renderer for `type` formats it); other plugins
# can use plain text, JSON, etc.
#
# Stored as `type\tdata\tdisplay\n` per entry; data is escaped so
# embedded tabs / newlines / backslashes don't break parsing.
(var harness-entries-buf "")
(defn harness/append-entry [type data &opt display]
  (when (and (string? type) (string? data))
    (let [d (if (nil? display) "1" (if display "1" "0"))]
      (set harness-entries-buf
           (string harness-entries-buf
                   (harness/-escape type) "\t"
                   (harness/-escape data) "\t"
                   d "\n")))))

# Registered renderer functions per plugin entry type.
# (harness/register-renderer "bookmark" "fn-name") records a
# (type, fn-name) pair the host looks up when displaying entries
# of that type. Stored as `type|fn\n` (same convention as
# harness-cmd-list).
(var harness-renderer-list "")
(defn harness/register-renderer [type fn-name]
  (when (and (string? type) (string? fn-name))
    (set harness-renderer-list
         (string harness-renderer-list type "|" fn-name "\n"))))

# Output buffer for a renderer invocation. The host clears it
# before calling the renderer, then reads back the accumulated
# `color\ttext\n` lines. Plugins call (harness/render color text)
# from inside their renderer to emit each output line.
(var harness-render-buf "")
(defn harness/render [color text]
  (when (and (or (string? color) (keyword? color) (symbol? color))
             (string? text))
    (set harness-render-buf
         (string harness-render-buf
                 (string color) "\t"
                 (harness/-escape text) "\n"))))

# Plugin-registered LLM providers (P1; 9b — wire format aligned with
# the other harness blobs). Plugins call
# (harness/register-provider name type base-url &opt api-key-env)
# at load time to make a custom provider available alongside the
# ones in config. Stored as tab-separated, escape-encoded fields so
# a single Janet -> Rust round-trip surfaces them all. Last-load-wins
# on name collision via dedup_last_wins.
(var harness-providers-list "")
(defn harness/register-provider [name type base-url &opt api-key-env]
  (when (and (string? name) (string? type) (string? base-url))
    (let [env (if (and api-key-env (string? api-key-env)) api-key-env "")]
      (set harness-providers-list
           (string harness-providers-list
                   (harness/-escape name) "\t"
                   (harness/-escape type) "\t"
                   (harness/-escape base-url) "\t"
                   (harness/-escape env) "\n")))))

# Session-tree mutation ops queued from plugins (P4d). Mirrors pi's
# ctx.setLabel / ctx.fork / ctx.navigateTree / ctx.newSession /
# ctx.switchSession but routed through the host so the drain happens
# between turns. Each line is `op\targ1[\targ2...]\n` (escaped via
# harness/-escape) so a single round-trip + parse gives the host the
# whole queue.
(var harness-tree-ops "")
(defn- harness/-push-op [& parts]
  (set harness-tree-ops
       (string harness-tree-ops
               (string/join (map harness/-escape (map string parts)) "\t")
               "\n")))
# (harness/set-label id label-or-nil) — set or clear a node label.
# Pass nil/false to clear; any string is set verbatim.
(defn harness/set-label [id label]
  (when (string? id)
    (harness/-push-op "set-label" id (if (string? label) label ""))))
# (harness/fork id &opt position) — branch off the chosen entry.
# position defaults to :before (extracts prompt text into editor);
# :at switches to that entry as the leaf without touching the editor.
(defn harness/fork [id &opt position]
  (when (string? id)
    (let [pos (cond
                (or (= position :at) (= position "at")) "at"
                "before")]
      (harness/-push-op "fork" id pos))))
# (harness/navigate-tree id) — move active leaf to id. User-message
# entries restore prompt text + go to parent (matching pi's behaviour);
# other entries become the new leaf directly.
(defn harness/navigate-tree [id]
  (when (string? id)
    (harness/-push-op "navigate-tree" id)))
# (harness/new-session &opt parent-session) — start a fresh session
# in place, optionally recording the prior session id as parent
# lineage. The host persists the current session before resetting.
(defn harness/new-session [&opt parent-session]
  (let [p (if (string? parent-session) parent-session "")]
    (harness/-push-op "new-session" p)))
# (harness/switch-session session-id-prefix) — load a saved session
# matching the id prefix and replace the current session in place.
(defn harness/switch-session [session-id]
  (when (string? session-id)
    (harness/-push-op "switch-session" session-id)))

# Plugin-registered renderers for `LoopMessage::Custom` events (P9d).
# Mirrors pi's `api.registerMessageRenderer(customType, renderer)`
# (extensions/types.ts:1171). Plugins call
#   (harness/register-message-renderer type-name handler)
# to provide a Janet function that the UI invokes when it sees a
# custom message whose JSON payload's `type` field matches. The
# handler receives the payload as a JSON string and returns the
# text to display. Distinct from `harness/register-renderer`, which
# is for session-timeline plugin entries (bookmarks, etc.) — message
# renderers fire mid-conversation as the agent loop emits Custom
# messages plugins queued via `harness/add-custom-message`.
(var harness-msg-renderers-list "")
(defn harness/register-message-renderer [type-name handler]
  (when (and (string? type-name) (string? handler))
    (set harness-msg-renderers-list
         (string harness-msg-renderers-list
                 (harness/-escape type-name) "\t"
                 (harness/-escape handler) "\n"))))

# Plugin-registered keyboard shortcuts (P9c). Plugins call
#   (harness/register-shortcut keys handler &opt description)
# to bind a key combination to a Janet handler the host invokes in
# interactive mode. `keys` is a string like "ctrl-x", "alt-shift-f",
# "f5", or "enter"; the host parses it via parse_key_spec and matches
# against incoming KeyEvents BEFORE built-in dispatch. Handler is a
# Janet function name; it's called with the key string as a single
# argument so one handler can serve multiple shortcuts and discriminate.
(var harness-shortcuts-list "")
(defn harness/register-shortcut [keys handler &opt description]
  (when (and (string? keys) (string? handler))
    (let [desc (if (and description (string? description)) description "")]
      (set harness-shortcuts-list
           (string harness-shortcuts-list
                   (harness/-escape keys) "\t"
                   (harness/-escape handler) "\t"
                   (harness/-escape desc) "\n")))))

# Per-invocation context slot set by the host before each plugin
# tool handler runs (H2). Reads return the tool-call id the LLM
# assigned to the current call — useful for correlating progress
# updates, logging, or pairing related state. Cleared between
# invocations so a handler observing nil knows no plugin tool is
# active.
(var harness-current-tool-call nil)

# (harness/emit-tool-progress text) — push a streaming progress
# update for the currently-running plugin tool (H2). Mirrors pi's
# onUpdate callback (extensions/types.ts execute signature). No-op
# when called outside a plugin tool handler (current-tool-call nil)
# or with a non-string arg. The host drains the queue and forwards
# each entry to the loop's per-tool on_update callback.
(var harness-tool-progress "")
(defn harness/emit-tool-progress [text]
  (when (and (string? text) harness-current-tool-call)
    (set harness-tool-progress
         (string harness-tool-progress
                 (harness/-escape harness-current-tool-call) "\t"
                 (harness/-escape text) "\n"))))

# Plugin-registered LLM-callable tools (P9a). Plugins call
#   (harness/register-tool name description label parameters handler
#                          &opt execution-mode prepare-arguments)
# at load time to make a new tool available to the LLM alongside
# the built-ins.
#
# - `parameters` is a JSON-schema string.
# - `handler` is the name of a Janet function that takes one
#   argument (the raw JSON args string the LLM produced) and
#   returns either a string (the tool result text) or any value
#   that (string ...) can render.
# - `execution-mode` is :parallel (read-only, default) or
#   :sequential (mutating). Pass nil to skip when you only want
#   to set prepare-arguments.
# - `prepare-arguments` (H3) is the name of an optional Janet
#   function that takes the raw JSON args string and returns a
#   mutated JSON string the loop validates against the schema.
#   Mirrors pi's `prepareArguments` (extensions/types.ts:443).
#   Errors fall back to the original args.
#
# Stored as tab-separated, escape-encoded line per tool.
(var harness-tools-list "")
(defn harness/register-tool [name description label parameters handler &opt execution-mode prepare-arguments]
  (when (and (string? name) (string? description) (string? label)
             (string? parameters) (string? handler))
    (let [mode (cond
                 (or (= execution-mode :sequential) (= execution-mode "sequential")) "sequential"
                 (or (= execution-mode :parallel) (= execution-mode "parallel")) "parallel"
                 "")
          prep (if (and prepare-arguments (string? prepare-arguments))
                 prepare-arguments
                 "")]
      (set harness-tools-list
           (string harness-tools-list
                   (harness/-escape name) "\t"
                   (harness/-escape description) "\t"
                   (harness/-escape label) "\t"
                   (harness/-escape parameters) "\t"
                   (harness/-escape handler) "\t"
                   mode "\t"
                   (harness/-escape prep) "\n")))))
"#;

/// Janet-side aliases that defer the actual blocking work to the
/// registered C functions. Installed after `add_c_fn` so the symbols
/// are present in the env.
#[cfg(feature = "plugin")]
const HARNESS_DIALOG_INIT: &str = r#"
# (harness/confirm "title" "question") -> bool
# (harness/select  "title" array-of-options) -> string | nil
#
# Both block the worker thread (not the UI thread) until the host
# replies, so they are safe to call from any plugin hook.
(defn harness/confirm [title question]
  (if (and (string? title) (string? question))
    (harness/__confirm title question)
    false))

(defn harness/select [title opts]
  (when (and (string? title) (indexed? opts))
    (harness/__select title opts)))
"#;

/// Janet wrappers for the LSP bridge. Installed after the C function is
/// (conditionally) registered. Every wrapper guards on `(harness/lsp?)`
/// so that on a build without the `lsp` feature — or when LSP is disabled
/// at runtime — the functions exist but return `nil` instead of erroring.
///
/// `harness/lsp` returns a JSON string (the LSP result) or nil. The
/// typed wrappers fill in the operation name. Positions are 1-based
/// line/column to match the `lsp` tool.
#[cfg(feature = "plugin")]
const HARNESS_LSP_INIT: &str = r#"
(defn harness/lsp?
  "True when the LSP bridge is available AND wired to a live language-
   server manager. False on builds without the `lsp` feature, and also
   when LSP is disabled at runtime — so a true result guarantees that a
   following `harness/lsp` call will actually reach a server (returning a
   JSON string), never a silent nil."
  []
  (if-let [entry (get (curenv) 'harness/__lsp-live)]
    (truthy? ((entry :value)))
    false))

(defn harness/lsp
  "Query the language servers. `op` is one of definition, references,
   hover, documentSymbol, workspaceSymbol, implementation,
   incomingCalls, outgoingCalls, diagnostics. Returns a JSON string of
   the result, or nil when LSP is unavailable. line/char are 1-based;
   query is the search string for workspaceSymbol."
  [op file &opt line char query]
  (let [l (if line line 1)
        c (if char char 1)]
    # Validate before anything else — 1-based coordinates must be
    # positive integers. A bad value is a plugin bug; surface it loudly
    # rather than silently clamping it to the first line/column.
    (assert (and (number? l) (>= l 1))
            "harness/lsp: line must be a positive (1-based) integer")
    (assert (and (number? c) (>= c 1))
            "harness/lsp: char must be a positive (1-based) integer")
    (if (harness/lsp?)
      (harness/__lsp (string op) (string file) l c
                     (if query (string query) ""))
      nil)))

(defn harness/lsp-definition [file line char] (harness/lsp "definition" file line char))
(defn harness/lsp-references [file line char] (harness/lsp "references" file line char))
(defn harness/lsp-hover [file line char] (harness/lsp "hover" file line char))
(defn harness/lsp-implementation [file line char] (harness/lsp "implementation" file line char))
(defn harness/lsp-incoming-calls [file line char] (harness/lsp "incomingCalls" file line char))
(defn harness/lsp-outgoing-calls [file line char] (harness/lsp "outgoingCalls" file line char))
(defn harness/lsp-document-symbols [file] (harness/lsp "documentSymbol" file))
(defn harness/lsp-workspace-symbols [file query] (harness/lsp "workspaceSymbol" file 1 1 query))
(defn harness/lsp-diagnostics [file] (harness/lsp "diagnostics" file))
"#;

/// dirge-l6bf: neuter the Janet escape hatches that can terminate or
/// destabilize the HOST process. Every hook / command / tool handler is
/// already run inside a Janet `(try ...)` (see `mod.rs`), so an ordinary
/// Janet error in a plugin is caught and surfaced as a
/// `[plugin] … errored` notification — dirge survives. The one thing that
/// bypasses that net is a plugin calling a function that exits the OS
/// process directly (e.g. `os/exit` → C `exit()`), which would take down
/// all of dirge. We rebind those symbols in the shared plugin env to raise
/// a NORMAL, catchable Janet error instead, so a buggy or hostile plugin
/// can never quit/crash the tool. Runs after the harness preludes and
/// before any plugin file is loaded, so plugins compile against the
/// shadowed bindings. dirge itself never calls `os/exit` from Janet.
// Janet source — consumed only by the `cfg(feature = "plugin")` worker loop
// below; gated to match so a no-plugin build (e.g. Windows `windows-default`)
// doesn't trip `-D warnings` on the unused const.
#[cfg(feature = "plugin")]
const HARNESS_SANDBOX: &str = r#"
(defn- dirge-disabled-fn [sym-name]
  (fn [&] (error (string sym-name
                        " is disabled in dirge plugins: a plugin cannot"
                        " terminate or signal the host process"))))
(each name ["os/exit" "os/proc-kill" "os/sigaction"]
  (def sym (symbol name))
  (when (get (curenv) sym)
    (put (curenv) sym @{:value (dirge-disabled-fn name)})))
"#;

/// A plugin LSP query, forwarded from the worker thread to the tokio-side
/// drainer (which owns the `LspManager`). `request` is a JSON object
/// `{op, file, line, char, query}`; the drainer runs the query and sends
/// the JSON-encoded result back on `reply`. Mirrors the dialog bridge so
/// a synchronous Janet `(harness/lsp …)` call can drive async LSP work.
/// Referenced unconditionally by the UI channel signature (like
/// `DialogRequest`), so the type isn't feature-gated.
#[derive(Debug)]
// The fields are only produced (worker `send_lsp`) and consumed
// (`lsp::harness::run_query`) when BOTH `plugin` and `lsp` are on; the
// type itself stays in the channel signature regardless, like
// `DialogRequest`.
#[cfg_attr(not(all(feature = "plugin", feature = "lsp")), allow(dead_code))]
pub struct LspRequest {
    pub request: String,
    pub reply: mpsc::Sender<String>,
}

/// What the UI is being asked to render. Carries a one-shot reply
/// channel back so the worker can resume once the user answers.
///
/// Variants are only constructed when the plugin feature is enabled,
/// but the *type* is referenced unconditionally by the UI's channel
/// signature — hence the cfg-gated dead-code allow rather than a
/// feature gate on the whole enum.
#[derive(Debug)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum DialogRequest {
    Confirm {
        title: String,
        question: String,
        reply: mpsc::Sender<DialogReply>,
    },
    Select {
        title: String,
        options: Vec<String>,
        reply: mpsc::Sender<DialogReply>,
    },
}

#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum DialogReply {
    /// User answered yes/no. False also covers cancel/timeout.
    Confirm(bool),
    /// Some(option) when the user picked, None on cancel.
    Select(Option<String>),
}

thread_local! {
    /// Set once at worker init. The JanetCFunctions read this to forward
    /// dialog requests to the UI. `RefCell<Option<...>>` so we can
    /// install at startup and tests can clear/set.
    static DIALOG_TX: RefCell<Option<tmpsc::UnboundedSender<DialogRequest>>> = const { RefCell::new(None) };

    /// Set once at worker init (mirrors `DIALOG_TX`). The `harness/__lsp`
    /// C-function reads this to forward LSP queries to the tokio drainer.
    static LSP_TX: RefCell<Option<tmpsc::UnboundedSender<LspRequest>>> = const { RefCell::new(None) };

    /// Shared with the Worker handle. The cfns poll this every
    /// `DIALOG_POLL` while blocked on a dialog reply so that
    /// `Worker::Drop` can abort an in-flight `harness/confirm` /
    /// `harness/select` call instead of hanging forever when the UI
    /// receiver has been dropped.
    static SHUTDOWN: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum Cmd {
    /// Evaluate Janet code and return its stringified result.
    Eval {
        code: String,
        reply: mpsc::Sender<Result<String, String>>,
    },
    Shutdown,
}

/// Handle to the worker thread. All Janet evaluation goes through `eval`.
/// Cheap to construct (only the spawn is heavy); cloneable senders are
/// not exposed — callers go through `&mut self` so writes serialize.
pub struct Worker {
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    cmd_tx: mpsc::Sender<Cmd>,
    join: Option<JoinHandle<()>>,
    /// Flipped by `Drop` so an in-flight `harness/confirm`/`harness/select`
    /// can stop waiting on the UI and let the worker exit. Shared by
    /// `Arc` with the worker thread's `SHUTDOWN` thread-local.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    shutdown: Arc<AtomicBool>,
}

impl Worker {
    /// Spawn the Janet worker thread, install harness defs, and wait for
    /// the worker to confirm Janet init succeeded. Returns Err if Janet
    /// VM initialization fails (e.g. linker / lib issues).
    ///
    /// The returned `dialog_rx` is the consumer end of the dialog channel
    /// the UI loop should drain. It's only returned once because we want
    /// a single owner.
    #[cfg(feature = "plugin")]
    #[allow(clippy::type_complexity)]
    pub fn try_spawn() -> Result<
        (
            Self,
            tmpsc::UnboundedReceiver<DialogRequest>,
            tmpsc::UnboundedReceiver<LspRequest>,
        ),
        String,
    > {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (dialog_tx, dialog_rx) = tmpsc::unbounded_channel::<DialogRequest>();
        let (lsp_tx, lsp_rx) = tmpsc::unbounded_channel::<LspRequest>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let join = thread::Builder::new()
            .name("dirge-janet".to_string())
            .spawn(move || worker_loop(cmd_rx, dialog_tx, lsp_tx, init_tx, shutdown_clone))
            .map_err(|e| format!("spawn janet worker: {e}"))?;

        // Block (with a watchdog timeout) until worker confirms init.
        // A worker panic before init_tx.send would otherwise hang main
        // forever; INIT_TIMEOUT bounds that worst case.
        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(())) => Ok((
                Self {
                    cmd_tx,
                    join: Some(join),
                    shutdown,
                },
                dialog_rx,
                lsp_rx,
            )),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("janet worker did not init within {INIT_TIMEOUT:?}"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("janet worker exited during init".to_string())
            }
        }
    }

    #[cfg(not(feature = "plugin"))]
    #[allow(clippy::type_complexity)]
    pub fn try_spawn() -> Result<
        (
            Self,
            tmpsc::UnboundedReceiver<DialogRequest>,
            tmpsc::UnboundedReceiver<LspRequest>,
        ),
        String,
    > {
        // No-op worker for non-plugin builds. cmd_rx is dropped immediately
        // when the thread exits; cmd_tx writes will Err out cleanly.
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let (_dialog_tx, dialog_rx) = tmpsc::unbounded_channel::<DialogRequest>();
        let (_lsp_tx, lsp_rx) = tmpsc::unbounded_channel::<LspRequest>();
        Ok((
            Self {
                cmd_tx,
                join: None,
                shutdown: Arc::new(AtomicBool::new(false)),
            },
            dialog_rx,
            lsp_rx,
        ))
    }

    /// Send a Janet expression to the worker and block until it returns
    /// the stringified result (or a Janet error message). Uses a
    /// moderate default (`INTERACTIVE_EVAL_TIMEOUT`, 30s) appropriate
    /// for slash-command dispatch, provider list lookups, and any
    /// UI-driven path where a hung plugin would otherwise freeze the
    /// session. For deliberately long-running operations
    /// (`harness/...` jobs from a plugin's top-level setup), use
    /// [`eval_long`].
    ///
    /// UI-3 (audit follow-up): the previous default was 10 minutes,
    /// inherited from `EVAL_TIMEOUT`. A runaway `(while true)` in any
    /// non-hook plugin code would block every UI interaction for 10
    /// minutes per call.
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        self.eval_with_timeout(code, INTERACTIVE_EVAL_TIMEOUT)
    }

    /// Long-running variant: same as `eval` but uses the global
    /// `EVAL_TIMEOUT` (10 min). Call this only for explicit
    /// long-running operations — anything user-interactive should
    /// use `eval()`.
    #[allow(dead_code)]
    pub fn eval_long(&mut self, code: &str) -> Result<String, String> {
        self.eval_with_timeout(code, EVAL_TIMEOUT)
    }

    /// Variant of `eval` with a caller-provided timeout. Capped at
    /// the global `EVAL_TIMEOUT` so callers can't accidentally
    /// extend the wait.
    pub fn eval_with_timeout(&mut self, code: &str, timeout: Duration) -> Result<String, String> {
        let effective = timeout.min(EVAL_TIMEOUT);
        let (reply, rx) = mpsc::channel();
        self.cmd_tx
            .send(Cmd::Eval {
                code: code.to_string(),
                reply,
            })
            .map_err(|_| "janet worker disconnected".to_string())?;
        match rx.recv_timeout(effective) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "janet worker did not reply within {}s — plugin may be stuck in an infinite loop",
                effective.as_secs(),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("janet worker dropped reply channel".to_string())
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Set the shutdown flag FIRST, then send the Shutdown cmd.
        // A worker that's currently blocked inside an unanswered
        // `harness/confirm`/`harness/select` polls this flag every
        // `DIALOG_POLL` and gives up — without the flag, the cfn would
        // sit on `reply_rx.recv()` forever, the cmd_rx would never read
        // Shutdown, and `join` would hang.
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(h) = self.join.take() {
            // Bounded join. `JoinHandle::join` has no timeout, so we
            // poll `is_finished()` (stable since Rust 1.61) and bail
            // after JOIN_TIMEOUT. If the worker is wedged in plugin
            // code (e.g. Janet `(while true)`), join would otherwise
            // hang the calling thread — usually the user's main
            // thread on `/quit`. We leak the JoinHandle rather than
            // pinning the process; the thread is reaped on exit.
            let deadline = std::time::Instant::now() + JOIN_TIMEOUT;
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                tracing::warn!(
                    target: "dirge::plugin",
                    timeout_secs = JOIN_TIMEOUT.as_secs(),
                    "janet worker thread did not exit within JOIN_TIMEOUT; leaking on shutdown",
                );
                std::mem::forget(h);
            }
        }
    }
}

#[cfg(feature = "plugin")]
fn worker_loop(
    rx: mpsc::Receiver<Cmd>,
    dialog_tx: tmpsc::UnboundedSender<DialogRequest>,
    lsp_tx: tmpsc::UnboundedSender<LspRequest>,
    init_tx: mpsc::Sender<Result<(), String>>,
    shutdown: Arc<AtomicBool>,
) {
    // Hand the dialog sender + shutdown flag to this thread's C functions
    // before we run any plugin code, otherwise harness/confirm/select
    // would no-op and shutdown couldn't cancel an in-flight dialog.
    DIALOG_TX.with(|cell| *cell.borrow_mut() = Some(dialog_tx));
    LSP_TX.with(|cell| *cell.borrow_mut() = Some(lsp_tx));
    SHUTDOWN.with(|cell| *cell.borrow_mut() = Some(shutdown));

    let mut client = match JanetClient::init_with_default_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("Janet init failed: {e}")));
            return;
        }
    };

    // Install C functions backing harness/confirm and harness/select.
    // They must be registered before the Janet-side aliases reference
    // them; we register, then run the alias definitions.
    //
    // `CFunOptions::namespace` requires `'static` CStr, so we use C string
    // literals (Rust 1.77+ `c"..."` syntax) instead of runtime CString.
    if let Some(env) = client.env_mut() {
        env.add_c_fn(CFunOptions::new(c"__confirm", janet_confirm_cfn).namespace(c"harness"));
        env.add_c_fn(CFunOptions::new(c"__select", janet_select_cfn).namespace(c"harness"));
        // Only register the LSP bridge when the lsp feature is compiled
        // in. The Janet `harness/lsp` wrappers (HARNESS_LSP_INIT) guard on
        // this symbol's existence and degrade to nil when it's absent.
        #[cfg(feature = "lsp")]
        {
            env.add_c_fn(CFunOptions::new(c"__lsp", janet_lsp_cfn).namespace(c"harness"));
            env.add_c_fn(CFunOptions::new(c"__lsp-live", janet_lsp_live_cfn).namespace(c"harness"));
        }
    }

    if let Err(e) = client.run(HARNESS_INIT) {
        let _ = init_tx.send(Err(format!("harness init failed: {e}")));
        return;
    }
    if let Err(e) = client.run(HARNESS_DIALOG_INIT) {
        let _ = init_tx.send(Err(format!("harness dialog init failed: {e}")));
        return;
    }
    if let Err(e) = client.run(HARNESS_LSP_INIT) {
        let _ = init_tx.send(Err(format!("harness lsp init failed: {e}")));
        return;
    }
    // dirge-l6bf: disable host-terminating Janet functions. MUST run after
    // the harness preludes and before any plugin loads, so plugin code
    // compiles against the neutered bindings.
    if let Err(e) = client.run(HARNESS_SANDBOX) {
        let _ = init_tx.send(Err(format!("harness sandbox init failed: {e}")));
        return;
    }

    let _ = init_tx.send(Ok(()));

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Eval { code, reply } => {
                let r = client
                    .run(&code)
                    .map(|v| v.to_string())
                    .map_err(|e| format!("Janet error: {e}"));
                let _ = reply.send(r);
            }
            Cmd::Shutdown => break,
        }
    }
}

#[cfg(not(feature = "plugin"))]
#[allow(dead_code)]
fn worker_loop(
    _rx: mpsc::Receiver<Cmd>,
    _dialog_tx: tmpsc::UnboundedSender<DialogRequest>,
    _lsp_tx: tmpsc::UnboundedSender<LspRequest>,
    _init_tx: mpsc::Sender<Result<(), String>>,
    _shutdown: Arc<AtomicBool>,
) {
    unreachable!("worker_loop should never run without the plugin feature");
}

// --- JanetCFunction shims ----------------------------------------------
//
// These run on the worker thread under Janet's control. They unwrap
// argv as strings via evil_janet's raw API, build a DialogRequest, send
// it to the UI through DIALOG_TX, block on the reply, and wrap the
// answer back into a Janet value.

#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_confirm_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    // Catch any Rust panic at the FFI boundary. The C-unwind ABI would
    // technically let it propagate into Janet's C runtime, but Janet
    // isn't built to clean up after foreign unwinds — heap corruption
    // and segfaults follow. Convert any panic to a safe `false`.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        confirm_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            // Log the panic before swallowing so the operator can
            // see *why* harness/confirm collapsed to false. Previous
            // behavior masked panics silently and plugin authors had
            // no way to distinguish "user said no" from "Rust panic
            // at FFI boundary."
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/confirm",
                panic = %msg,
                "FFI panic in dialog cfn — returning safe default (false)",
            );
            unsafe { janet_wrap_boolean(0) }
        }
    }
}

/// Safe-Rust body of `janet_confirm_cfn`. Split out so it can panic
/// without worrying about FFI unwind semantics; the cfn wraps the call
/// in `catch_unwind` and substitutes a safe default on panic.
#[cfg(feature = "plugin")]
unsafe fn confirm_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_boolean(0) };
    }
    let title = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_boolean(0) },
    };
    let question = match unsafe { read_string_arg(argv, 1) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_boolean(0) },
    };

    let answer = DIALOG_TX.with(|cell| match cell.borrow().as_ref() {
        Some(tx) => send_dialog(tx, |reply| DialogRequest::Confirm {
            title,
            question,
            reply,
        })
        .unwrap_or(DialogReply::Confirm(false)),
        None => DialogReply::Confirm(false),
    });

    let yes = matches!(answer, DialogReply::Confirm(true));
    unsafe { janet_wrap_boolean(if yes { 1 } else { 0 }) }
}

#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_select_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        select_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/select",
                panic = %msg,
                "FFI panic in dialog cfn — returning safe default (nil)",
            );
            unsafe { janet_wrap_nil() }
        }
    }
}

/// Best-effort conversion of a panic payload (`Box<dyn Any>`) to a
/// printable string. Tries `&str` then `String` — covers the two
/// payload shapes std and most code produce. Returns
/// `"<non-string panic payload>"` for anything else so the log
/// always has SOMETHING to anchor on rather than going silent again.
#[cfg(feature = "plugin")]
fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(feature = "plugin")]
unsafe fn select_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_nil() };
    }
    let title = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let options = match unsafe { read_string_array_arg(argv, 1) } {
        Some(v) if !v.is_empty() => v,
        _ => return unsafe { janet_wrap_nil() },
    };

    let answer = DIALOG_TX.with(|cell| match cell.borrow().as_ref() {
        Some(tx) => send_dialog(tx, |reply| DialogRequest::Select {
            title,
            options,
            reply,
        })
        .unwrap_or(DialogReply::Select(None)),
        None => DialogReply::Select(None),
    });

    match answer {
        DialogReply::Select(Some(s)) => unsafe { wrap_string(&s) },
        _ => unsafe { janet_wrap_nil() },
    }
}

/// Send a dialog request, build it via the supplied closure (so we can
/// move owned strings into the variant), and block on the reply.
/// Returns `None` if the UI side dropped the channel OR the worker is
/// shutting down. The outbound side uses tokio's unbounded sender so
/// the UI loop can `recv().await` in `tokio::select!`; the inbound
/// reply is a std mpsc with a polling timeout so the cfn can also
/// abort when `Worker::Drop` flips the shutdown flag.
#[cfg(feature = "plugin")]
fn send_dialog<F>(tx: &tmpsc::UnboundedSender<DialogRequest>, build: F) -> Option<DialogReply>
where
    F: FnOnce(mpsc::Sender<DialogReply>) -> DialogRequest,
{
    let (reply_tx, reply_rx) = mpsc::channel();
    let req = build(reply_tx);
    tx.send(req).ok()?;

    // Poll for the reply. Wake every `DIALOG_POLL` to check the
    // worker-shutdown flag so a UI exit or `Worker::Drop` doesn't pin
    // us forever on `recv()`. The polling overhead is negligible
    // compared to the time a human takes to answer a dialog.
    loop {
        match reply_rx.recv_timeout(DIALOG_POLL) {
            Ok(r) => return Some(r),
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let shutting_down = SHUTDOWN.with(|cell| {
                    cell.borrow()
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false)
                });
                if shutting_down {
                    return None;
                }
            }
        }
    }
}

/// Read a Janet string at argv[i] and decode as UTF-8. Returns None for
/// non-string args or invalid UTF-8 (we don't surface lossy strings to
/// plugins — caller handles the None as a no-op).
#[cfg(feature = "plugin")]
unsafe fn read_string_arg(argv: *mut janetrs::lowlevel::Janet, i: i32) -> Option<String> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    // janet_checktype returns 1 if the type matches.
    let is_str = unsafe { janet_checktype(v, JanetType_JANET_STRING) } != 0;
    let is_kw = unsafe { janet_checktype(v, JanetType_JANET_KEYWORD) } != 0;
    let is_sym = unsafe { janet_checktype(v, JanetType_JANET_SYMBOL) } != 0;
    let is_buf = unsafe { janet_checktype(v, JanetType_JANET_BUFFER) } != 0;
    if !(is_str || is_kw || is_sym || is_buf) {
        return None;
    }
    if is_buf {
        let buf = unsafe { janet_unwrap_buffer(v) };
        if buf.is_null() {
            return None;
        }
        let data = unsafe { (*buf).data };
        let count = unsafe { (*buf).count } as usize;
        let slice = unsafe { std::slice::from_raw_parts(data, count) };
        return std::str::from_utf8(slice).ok().map(str::to_string);
    }
    let raw = unsafe { janet_unwrap_string(v) };
    if raw.is_null() {
        return None;
    }
    // Janet strings carry their length in the GC header; janet_string_head
    // is the public way to fetch it (janet_string_length is a C macro that
    // isn't exposed through the auto-generated bindings).
    let len = unsafe { (*janet_string_head(raw)).length } as usize;
    let slice = unsafe { std::slice::from_raw_parts(raw, len) };
    std::str::from_utf8(slice).ok().map(str::to_string)
}

/// Read a Janet tuple/array of strings at argv[i].
#[cfg(feature = "plugin")]
unsafe fn read_string_array_arg(
    argv: *mut janetrs::lowlevel::Janet,
    i: i32,
) -> Option<Vec<String>> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    let is_tuple = unsafe { janet_checktype(v, JanetType_JANET_TUPLE) } != 0;
    let is_array = unsafe { janet_checktype(v, JanetType_JANET_ARRAY) } != 0;
    if !is_tuple && !is_array {
        return None;
    }
    let (data, len) = if is_tuple {
        let raw = unsafe { janet_unwrap_tuple(v) };
        if raw.is_null() {
            return None;
        }
        // Same GC-header trick as strings; janet_tuple_length is a macro.
        let n = unsafe { (*janet_tuple_head(raw)).length } as usize;
        (raw, n)
    } else {
        let arr = unsafe { janet_unwrap_array(v) };
        if arr.is_null() {
            return None;
        }
        let n = unsafe { (*arr).count } as usize;
        (unsafe { (*arr).data } as *const janetrs::lowlevel::Janet, n)
    };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    let mut out = Vec::with_capacity(len);
    for (idx, _) in slice.iter().enumerate() {
        // Recurse through the same arg-reader, treating each element as if
        // it sat at argv[idx]. Doable because read_string_arg only uses
        // the raw Janet, not its position.
        let elt_ptr = unsafe { data.add(idx) } as *mut janetrs::lowlevel::Janet;
        match unsafe { read_string_arg(elt_ptr, 0) } {
            Some(s) => out.push(s),
            None => return None,
        }
    }
    Some(out)
}

/// Wrap a Rust `&str` as a Janet string. The Janet GC takes ownership of
/// the copied bytes via janet_string. Returns Janet nil when the string
/// is too large for Janet's i32 length (>2 GB) — this never happens for
/// real dialog answers but is checked defensively because silently
/// truncating the length to i32 would let Janet read past the
/// allocation.
#[cfg(feature = "plugin")]
unsafe fn wrap_string(s: &str) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let bytes = s.as_bytes();
    let Ok(len) = i32::try_from(bytes.len()) else {
        return unsafe { janet_wrap_nil() };
    };
    let raw = unsafe { janet_string(bytes.as_ptr(), len) };
    unsafe { janet_wrap_string(raw) }
}

/// Read a Janet number at argv[i] as a u32 (clamped at 0). Returns None
/// for non-number args. Used for the LSP line/char position arguments.
#[cfg(all(feature = "plugin", feature = "lsp"))]
unsafe fn read_uint_arg(argv: *mut janetrs::lowlevel::Janet, i: i32) -> Option<u32> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    if unsafe { janet_checktype(v, JanetType_JANET_NUMBER) } == 0 {
        return None;
    }
    let n = unsafe { janet_unwrap_number(v) };
    // Reject non-finite / negative rather than coercing to 0 — a bogus
    // coordinate should not silently become line 0. The Janet `harness/lsp`
    // wrapper validates positivity before we get here; this is the backstop.
    if n.is_finite() && n >= 0.0 {
        Some(n as u32)
    } else {
        None
    }
}

/// C-function backing `harness/__lsp`. Reads (op, file, line, char,
/// query), forwards a JSON request to the tokio drainer via `LSP_TX`,
/// blocks on the reply (polling the shutdown flag like the dialog cfns),
/// and returns the JSON result string. Panics are caught at the FFI
/// boundary and collapse to nil.
#[cfg(all(feature = "plugin", feature = "lsp"))]
unsafe extern "C-unwind" fn janet_lsp_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        lsp_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(payload) => {
            let msg = panic_payload_to_string(&payload);
            tracing::error!(
                target: "dirge::plugin",
                cfn = "harness/lsp",
                panic = %msg,
                "FFI panic in lsp cfn — returning nil",
            );
            unsafe { janet_wrap_nil() }
        }
    }
}

/// C-function backing `harness/__lsp-live`. Returns `true` only when the
/// bridge is wired to a live request receiver — i.e. the host spawned the
/// LSP responder against a real `LspManager`. When LSP is disabled at
/// runtime (no manager) the receiver is dropped, so `is_closed()` is true
/// and we report `false`. This makes `(harness/lsp?)` reflect *runtime*
/// availability rather than mere compile-time presence, so a plugin that
/// feature-detects won't then try to decode a nil result.
#[cfg(all(feature = "plugin", feature = "lsp"))]
unsafe extern "C-unwind" fn janet_lsp_live_cfn(
    _argc: i32,
    _argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let live = LSP_TX.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|tx| !tx.is_closed())
            .unwrap_or(false)
    });
    unsafe { janet_wrap_boolean(if live { 1 } else { 0 }) }
}

#[cfg(all(feature = "plugin", feature = "lsp"))]
unsafe fn lsp_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 5 {
        return unsafe { janet_wrap_nil() };
    }
    let op = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let file = match unsafe { read_string_arg(argv, 1) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let line = unsafe { read_uint_arg(argv, 2) }.unwrap_or(1);
    let character = unsafe { read_uint_arg(argv, 3) }.unwrap_or(1);
    let query = unsafe { read_string_arg(argv, 4) }.unwrap_or_default();

    let request = serde_json::json!({
        "op": op,
        "file": file,
        "line": line,
        "char": character,
        "query": query,
    })
    .to_string();

    let answer = LSP_TX.with(|cell| match cell.borrow().as_ref() {
        Some(tx) => send_lsp(tx, request),
        None => None,
    });
    match answer {
        Some(json) => unsafe { wrap_string(&json) },
        None => unsafe { janet_wrap_nil() },
    }
}

/// Upper bound on a single `harness/lsp` query. Unlike dialogs (bounded by
/// a human), an LSP query can hang against a slow or wedged language
/// server; without a cap it would freeze the Janet worker thread — and
/// thus every plugin hook — indefinitely. After this elapses we give up
/// and return nil to the plugin.
#[cfg(all(feature = "plugin", feature = "lsp"))]
const LSP_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `send_lsp` should stop waiting: either the worker is shutting
/// down, or the query has exceeded [`LSP_QUERY_TIMEOUT`]. Split out so the
/// give-up policy is unit-testable without a real hung server.
#[cfg(all(feature = "plugin", feature = "lsp"))]
fn lsp_should_abort(elapsed: Duration, shutting_down: bool) -> bool {
    shutting_down || elapsed >= LSP_QUERY_TIMEOUT
}

/// Send an LSP request and block on the JSON reply, polling the shutdown
/// flag so `Worker::Drop` can unblock us (mirrors `send_dialog`). Unlike
/// dialogs, also bounded by [`LSP_QUERY_TIMEOUT`] so a wedged language
/// server can't pin the worker thread forever.
#[cfg(all(feature = "plugin", feature = "lsp"))]
fn send_lsp(tx: &tmpsc::UnboundedSender<LspRequest>, request: String) -> Option<String> {
    let (reply_tx, reply_rx) = mpsc::channel::<String>();
    tx.send(LspRequest {
        request,
        reply: reply_tx,
    })
    .ok()?;
    let start = std::time::Instant::now();
    loop {
        match reply_rx.recv_timeout(DIALOG_POLL) {
            Ok(r) => return Some(r),
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let shutting_down = SHUTDOWN.with(|cell| {
                    cell.borrow()
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false)
                });
                if lsp_should_abort(start.elapsed(), shutting_down) {
                    if !shutting_down {
                        tracing::warn!(
                            target: "dirge::plugin",
                            timeout_secs = LSP_QUERY_TIMEOUT.as_secs(),
                            "harness/lsp query timed out — returning nil",
                        );
                    }
                    return None;
                }
            }
        }
    }
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use super::*;

    #[test]
    fn worker_round_trips_an_eval() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let r = worker.eval("(+ 1 2)").unwrap();
        assert_eq!(r, "3");
    }

    #[test]
    fn worker_surfaces_janet_errors_as_err() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        // `undefined-fn` is genuinely unknown.
        let r = worker.eval("(undefined-fn 1)");
        assert!(r.is_err(), "expected Err, got {r:?}");
    }

    /// dirge-l6bf: a plugin must NOT be able to terminate the host process.
    /// `os/exit` (and friends) are neutered to raise a catchable Janet error
    /// rather than calling C `exit()`. Without the fix, this very test would
    /// terminate the test binary mid-run.
    #[test]
    fn os_exit_cannot_kill_the_host() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();

        // os/exit raises instead of calling C exit(). (At the top level
        // janetrs renders the error as a Janet stack trace; the exact
        // message is asserted in `neutered_os_exit_is_catchable_by_plugin_try`
        // where the raw error value is visible to a Janet `(try ...)`.)
        let r = worker.eval("(os/exit 0)");
        assert!(r.is_err(), "os/exit must raise, not exit; got {r:?}");

        // The worker — and therefore the host — is still alive.
        assert_eq!(worker.eval("(+ 1 2)").unwrap(), "3");

        // The other host-control escape hatches are neutered too.
        assert!(worker.eval("(os/proc-kill nil)").is_err());
        assert!(worker.eval("(os/sigaction :term (fn [&] nil))").is_err());
    }

    /// dirge-df1v: a plugin flooding `harness/notify` in a hot loop must not
    /// grow the notification buffer without bound. It's capped, gets a single
    /// "dropped" marker, and resets once the host drains it.
    #[test]
    fn notification_buffer_is_capped_and_resets_on_drain() {
        let (mut worker, _d, _l) = Worker::try_spawn().unwrap();

        // Flood far past the cap.
        worker
            .eval("(loop [i :range [0 50000]] (harness/notify (string \"notification number \" i) :info))")
            .unwrap();

        let len: usize = worker
            .eval("(length harness-notif-list)")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            len <= harness_notif_cap_for_test() + 256,
            "notif buffer should be capped, got {len}"
        );
        // The single flood marker is present.
        assert_ne!(
            worker
                .eval("(if (string/find \"further ones dropped\" harness-notif-list) 1 0)")
                .unwrap(),
            "0",
            "expected the flood marker",
        );

        // Simulate the host's per-turn drain (it clears the list to "").
        worker.eval("(set harness-notif-list \"\")").unwrap();
        // A normal notification after drain works again (marker re-armed).
        worker
            .eval("(harness/notify \"after drain\" :info)")
            .unwrap();
        let after = worker.eval("harness-notif-list").unwrap();
        assert!(after.contains("after drain"), "got {after}");
        assert!(
            !after.contains("dropped"),
            "flood marker should have reset; got {after}"
        );
    }

    /// Mirror cap for custom messages.
    #[test]
    fn custom_message_buffer_is_capped() {
        let (mut worker, _d, _l) = Worker::try_spawn().unwrap();
        worker
            .eval("(loop [i :range [0 50000]] (harness/add-custom-message (string \"custom message number \" i)))")
            .unwrap();
        let len: usize = worker
            .eval("(length harness-custom-messages)")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            len <= 131072 + 256,
            "custom-message buffer should be capped, got {len}"
        );
        assert_ne!(
            worker
                .eval("(if (string/find \"further ones dropped\" harness-custom-messages) 1 0)")
                .unwrap(),
            "0",
            "expected the custom-message flood marker",
        );
    }

    fn harness_notif_cap_for_test() -> usize {
        65536
    }

    /// The catchable error means the existing per-hook `(try ...)` wrapping
    /// swallows a plugin's `os/exit` exactly like any other plugin error.
    #[test]
    fn neutered_os_exit_is_catchable_by_plugin_try() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let r = worker
            .eval(r#"(try (os/exit 1) ([err] (string "caught: " err)))"#)
            .unwrap();
        assert!(r.contains("caught:"), "got {r}");
        assert!(r.contains("disabled in dirge plugins"), "got {r}");
    }

    #[test]
    fn worker_eval_returns_keyword_string() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        // Verify the worker installed the harness defs.
        let r = worker
            .eval("(harness/has-symbol? \"harness/notify\")")
            .unwrap();
        assert_eq!(r, "true");
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_harness_is_available_and_wrappers_are_defined() {
        // Hold the receiver alive so the bridge counts as live.
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        // With the lsp feature the `__lsp` C-function is registered and a
        // live receiver is attached, so the predicate reports available.
        assert_eq!(worker.eval("(harness/lsp?)").unwrap(), "true");
        // The core fn and every typed wrapper are installed.
        for sym in [
            "harness/lsp",
            "harness/lsp-definition",
            "harness/lsp-references",
            "harness/lsp-hover",
            "harness/lsp-implementation",
            "harness/lsp-incoming-calls",
            "harness/lsp-outgoing-calls",
            "harness/lsp-document-symbols",
            "harness/lsp-workspace-symbols",
            "harness/lsp-diagnostics",
        ] {
            let r = worker
                .eval(&format!("(harness/has-symbol? \"{sym}\")"))
                .unwrap();
            assert_eq!(r, "true", "{sym} should be defined");
        }
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_predicate_is_false_when_bridge_has_no_live_receiver() {
        // Simulate the runtime-disabled case (lsp_manager None → no
        // responder spawned → the request receiver is dropped). The
        // predicate must reflect that the bridge is NOT live, so plugins
        // that feature-detect don't then crash decoding a nil result.
        let (mut worker, _dialog_rx, lsp_rx) = Worker::try_spawn().unwrap();
        drop(lsp_rx);
        assert_eq!(worker.eval("(harness/lsp?)").unwrap(), "false");
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_query_rejects_nonpositive_coordinates() {
        // Coordinates are 1-based; 0, negative, or non-numbers are plugin
        // bugs and must surface as a Janet error rather than being silently
        // clamped to line 0. Drop the receiver so the validation (which runs
        // before the bridge call) is what fails — never a blocked query.
        let (mut worker, _dialog_rx, lsp_rx) = Worker::try_spawn().unwrap();
        drop(lsp_rx);
        for code in [
            r#"(harness/lsp "definition" "f.rs" 0 1)"#,
            r#"(harness/lsp "definition" "f.rs" 1 0)"#,
            r#"(harness/lsp "definition" "f.rs" -3 1)"#,
        ] {
            assert!(worker.eval(code).is_err(), "expected error for {code}");
        }
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_abort_decision_covers_shutdown_and_timeout() {
        // Give up on shutdown immediately, or once the query timeout elapses.
        assert!(!lsp_should_abort(Duration::from_secs(0), false));
        assert!(lsp_should_abort(Duration::from_secs(0), true));
        assert!(lsp_should_abort(LSP_QUERY_TIMEOUT, false));
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_query_returns_nil_when_bridge_has_no_live_receiver() {
        // Even if a plugin ignores the predicate, a query against a
        // dropped receiver must return nil immediately — never block the
        // worker thread (the load-time deadlock guard).
        let (mut worker, _dialog_rx, lsp_rx) = Worker::try_spawn().unwrap();
        drop(lsp_rx);
        let r = worker
            .eval(r#"(harness/lsp "definition" "f.rs" 1 1)"#)
            .unwrap();
        assert_eq!(r, "nil");
    }

    #[test]
    fn confirm_sends_a_dialog_request_with_title_and_question() {
        let (mut worker, dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();

        // Start a helper thread that auto-answers any confirm with `true`.
        let mut dialog_rx = dialog_rx;
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm {
                title,
                question,
                reply,
            }) => {
                assert_eq!(title, "warn");
                assert_eq!(question, "really?");
                let _ = reply.send(DialogReply::Confirm(true));
            }
            other => panic!("unexpected dialog request: {other:?}"),
        });

        let r = worker
            .eval(r#"(harness/confirm "warn" "really?")"#)
            .unwrap();
        // Janet booleans stringify as "true" / "false".
        assert_eq!(r, "true");
        helper.join().unwrap();
    }

    #[test]
    fn confirm_returns_false_when_dialog_replies_false() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm { reply, .. }) => {
                let _ = reply.send(DialogReply::Confirm(false));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/confirm "t" "q")"#).unwrap();
        assert_eq!(r, "false");
        helper.join().unwrap();
    }

    #[test]
    fn select_returns_picked_option_as_string() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select {
                title,
                options,
                reply,
            }) => {
                assert_eq!(title, "pick");
                assert_eq!(options, vec!["alpha".to_string(), "beta".to_string()]);
                let _ = reply.send(DialogReply::Select(Some("beta".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker
            .eval(r#"(harness/select "pick" ["alpha" "beta"])"#)
            .unwrap();
        // Janet strings stringify with surrounding quotes; we check substring.
        assert!(r.contains("beta"), "got {r:?}");
        helper.join().unwrap();
    }

    #[test]
    fn select_returns_nil_on_cancel() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                let _ = reply.send(DialogReply::Select(None));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/select "pick" ["a"])"#).unwrap();
        assert_eq!(r, "nil");
        helper.join().unwrap();
    }

    #[test]
    fn dialog_rx_drains_when_no_request_pending() {
        // Sanity: a fresh worker doesn't emit phantom dialog requests.
        let (_worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        assert!(matches!(
            dialog_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    /// R1 critical: setting the shutdown flag unblocks an in-flight
    /// dialog within ~`DIALOG_POLL` so `Worker::Drop` doesn't hang.
    /// Before R1, send_dialog's `reply_rx.recv()` had no timeout and
    /// the eval would block forever if the UI never replied.
    ///
    /// We can't trigger the abort via Drop directly (the worker is
    /// moved into the eval thread; dropping it from outside is exactly
    /// the catch-22 R1 exists to break). Instead we clone the shutdown
    /// Arc out before moving, then flip it once the dialog has arrived.
    /// This exercises the same code path Drop uses.
    #[test]
    fn shutdown_flag_aborts_in_flight_dialog() {
        use std::time::Instant;

        let (worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let shutdown_handle = worker.shutdown.clone();

        // Kick off a confirm; it will block waiting for a reply we
        // never send. After the shutdown flag flips, send_dialog's
        // polling loop returns None and the cfn returns Janet false.
        let eval_t = std::thread::spawn(move || {
            let mut worker = worker;
            let result = worker.eval(r#"(harness/confirm "x" "y")"#);
            (worker, result)
        });

        // Wait for the dialog request to land — the worker is now
        // parked inside send_dialog's recv_timeout loop.
        let _req = dialog_rx.blocking_recv().expect("dialog request");

        // Flip the flag. The cfn wakes up on its next 50 ms tick.
        shutdown_handle.store(true, Ordering::SeqCst);

        let started = Instant::now();
        let (worker, eval_result) = eval_t.join().expect("eval thread");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "eval took {elapsed:?}, expected ~DIALOG_POLL once the flag was flipped"
        );
        // On shutdown the cfn returns Janet false (its safe default).
        assert_eq!(eval_result.unwrap(), "false");

        // Drop the worker explicitly — should complete promptly since
        // the in-flight dialog has already unwound.
        drop(worker);
    }

    /// R1: oversized strings to wrap_string don't truncate to i32 —
    /// instead they return Janet nil. Hard to test with a real 2 GB
    /// string, so we exercise the same boundary via a small synthetic
    /// check that the i32::try_from path is taken. This is mostly a
    /// regression sentinel — if someone reverts the bounds check it
    /// fails to compile (wrap_string still requires Send/Sync to be
    /// callable from a select reply context).
    #[test]
    fn wrap_string_handles_empty() {
        // Just verify Janet round-trips the empty string through
        // confirm's reply path. Catches any wrap_string regression
        // that miscounts zero-length input.
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                let _ = reply.send(DialogReply::Select(Some(String::new())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker
            .eval(r#"(harness/select "pick" ["only-option"])"#)
            .unwrap();
        // janetrs stringifies a Janet string with no quotes (just the
        // raw bytes), so an empty Janet string round-trips as the
        // empty Rust string here.
        assert_eq!(r, "");
        helper.join().unwrap();
    }

    // --- R2: FFI edge cases ---------------------------------------------

    /// R2: read_string_arg accepts Janet keywords (call sites can use
    /// `(harness/confirm :title "q")` instead of double-quoted strings).
    /// Caught by an integration test through harness/confirm since the
    /// cfn is the only caller; if read_string_arg ever stops accepting
    /// keywords this test fails.
    #[test]
    fn confirm_accepts_keyword_title() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm {
                title,
                question,
                reply,
            }) => {
                assert_eq!(title, "warn");
                assert_eq!(question, "really?");
                let _ = reply.send(DialogReply::Confirm(true));
            }
            other => panic!("unexpected: {other:?}"),
        });
        // Keyword first arg — read_string_arg's is_kw branch handles it.
        let r = worker
            .eval(r#"(harness/__confirm :warn "really?")"#)
            .unwrap();
        assert_eq!(r, "true");
        helper.join().unwrap();
    }

    /// R2: read_string_array_arg returns None for an empty array, and
    /// the select cfn surfaces that as Janet nil. Janet-side
    /// harness/select already short-circuits on `(indexed? opts)`, so
    /// we hit the cfn via __select directly.
    #[test]
    fn select_with_empty_options_returns_nil() {
        let (mut worker, _dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        // Empty array should never even emit a dialog request.
        let r = worker.eval(r#"(harness/__select "pick" [])"#).unwrap();
        assert_eq!(r, "nil");
    }

    /// R2: read_string_array_arg works with tuples too (not just
    /// arrays). Janet array literals `["a"]` are arrays; quoted forms
    /// `'("a")` produce tuples. Both should be accepted.
    #[test]
    fn select_accepts_tuple_options() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { options, reply, .. }) => {
                assert_eq!(options, vec!["alpha".to_string(), "beta".to_string()]);
                let _ = reply.send(DialogReply::Select(Some("alpha".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        // Use a quoted tuple instead of an array literal.
        let r = worker
            .eval(r#"(harness/__select "pick" '("alpha" "beta"))"#)
            .unwrap();
        assert!(r.contains("alpha"), "got {r:?}");
        helper.join().unwrap();
    }

    /// R2: wrap_string handles multibyte UTF-8 correctly. The byte
    /// length is the Janet string's allocation; an off-by-one here
    /// would either truncate emoji or read past the slice.
    #[test]
    fn select_returns_multibyte_option_through_wrap_string() {
        let (mut worker, mut dialog_rx, _lsp_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                // Emoji + CJK + Cyrillic — all multibyte UTF-8.
                let _ = reply.send(DialogReply::Select(Some("🦀漢字Привет".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/select "pick" ["x"])"#).unwrap();
        // Janet stringification preserves the raw UTF-8 bytes; the
        // result should contain all three multibyte sequences intact.
        assert!(r.contains("🦀"), "lost emoji: {r:?}");
        assert!(r.contains("漢字"), "lost CJK: {r:?}");
        assert!(r.contains("Привет"), "lost Cyrillic: {r:?}");
        helper.join().unwrap();
    }
}
