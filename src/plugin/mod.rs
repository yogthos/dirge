// Tests exercise plugin-feature internals (Janet worker, harness
// API, hook dispatch). Without the feature, the JanetClient and
// related symbols don't exist, so the tests can't compile. Gate
// the whole test module on the feature to keep `cargo test` (no
// args) green for users who don't have the plugin toolchain set
// up.
//
// The tests live in the sibling `mod_tests.rs` file; the
// `#[path = "..."]` attribute below pulls that file in AS the
// `tests` child module so `use super::*` references inside the
// tests continue to resolve against this module's items.
#[cfg(all(test, feature = "plugin"))]
#[path = "mod_tests.rs"]
mod tests;

use std::collections::HashMap;

use worker::Worker;
pub use worker::{DialogReply, DialogRequest, LspRequest};

#[cfg(feature = "plugin")]
pub mod extension;
pub mod hook;
pub mod loader;
pub mod worker;

/// Spawn a background task that drains plugin dialog requests in
/// headless modes (`--print`, `--loop`, ACP) and auto-replies based
/// on `mode`. Without this drain, a plugin that calls
/// `harness/confirm` or `harness/select` in a non-interactive run
/// blocks forever because the dialog channel has no UI consumer.
///
/// The drainer runs until `dialog_rx` is closed (i.e. the
/// `PluginManager` / `Worker` is dropped). Reply sends are
/// best-effort: if the worker has moved on, the send is dropped.
#[cfg(feature = "plugin")]
pub fn spawn_headless_dialog_responder(
    mut dialog_rx: tokio::sync::mpsc::UnboundedReceiver<DialogRequest>,
    mode: crate::cli::AutoConfirmMode,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = dialog_rx.recv().await {
            match req {
                DialogRequest::Confirm { reply, .. } => {
                    let answer = matches!(mode, crate::cli::AutoConfirmMode::Yes);
                    let _ = reply.send(DialogReply::Confirm(answer));
                }
                DialogRequest::Select { options, reply, .. } => {
                    let picked = match mode {
                        crate::cli::AutoConfirmMode::Yes => options.into_iter().next(),
                        crate::cli::AutoConfirmMode::No => None,
                    };
                    let _ = reply.send(DialogReply::Select(picked));
                }
            }
        }
    })
}

/// Drain `harness/lsp` requests from the plugin worker and answer them
/// against the `LspManager`. Each request carries a JSON query and a
/// one-shot reply channel back to the (blocked) worker thread; we run the
/// async LSP query and send the JSON result. Runs until the channel
/// closes (worker shutdown). Spawned once at startup when both the
/// `plugin` and `lsp` features are active and a manager exists.
#[cfg(all(feature = "plugin", feature = "lsp"))]
pub fn spawn_lsp_responder(
    mut lsp_rx: tokio::sync::mpsc::UnboundedReceiver<LspRequest>,
    manager: std::sync::Arc<crate::lsp::manager::LspManager>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = lsp_rx.recv().await {
            let json = crate::lsp::harness::run_query(&manager, &req.request).await;
            let _ = req.reply.send(json);
        }
    })
}

/// Escape a Rust string so it can be safely embedded inside a Janet
/// double-quoted string literal. Janet's parser accepts the standard
/// `\"`, `\\`, `\n`, `\r`, `\t` escapes, so we normalise all of those
/// plus any remaining ASCII control characters via `\xNN`.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub fn escape_janet_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\x{:02X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// What the host should do after an agent turn completes.
/// Plugin followups must outrank loop iterations so a queued
/// `harness/request-prompt` never gets silently overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostDoneAction {
    Followup(String),
    LoopIter,
    LoopStop,
    Idle,
}

pub fn decide_post_done_action(
    followup: Option<String>,
    loop_active: bool,
    loop_should_stop: bool,
) -> PostDoneAction {
    if let Some(text) = followup {
        return PostDoneAction::Followup(text);
    }
    if !loop_active {
        return PostDoneAction::Idle;
    }
    if loop_should_stop {
        PostDoneAction::LoopStop
    } else {
        PostDoneAction::LoopIter
    }
}

/// Filter a list of candidate plugin dirs down to those that exist.
/// Used at startup to silently skip default search paths that aren't
/// present rather than spamming "plugin dir not found" warnings.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub fn filter_existing_dirs(candidates: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    loader::filter_existing_dirs(candidates)
}

/// One loaded plugin's stem (used for hook-name namespacing) and the
/// source path(s) that contributed code. For single-file plugins this
/// is one path; for directory plugins it's every `.janet` file inside
/// in load order.
pub use loader::LoadedPlugin;

/// Discover, evaluate, and register a plugin from `path`.
///
/// `path` may be:
/// - A `*.janet` file — single-file plugin; stem = file stem.
/// - A directory — multi-file plugin; stem = directory name. All
///   `*.janet` files inside are loaded in alphabetical order into the
///   shared Janet env, so split files share state and `harness/*`
///   registrations.
///
/// After eval, any bare hook fns (`on-prompt`, `on-tool-start`, etc.)
/// get a `{stem}-{hook}` alias so they survive subsequent plugin loads
/// that would otherwise overwrite the bare symbol in the shared Janet
/// env. Then `{stem}-{hook}` is what we register for dispatch — that
/// way two plugins both defining `on-tool-start` no longer collide.
///
/// Returns the [`LoadedPlugin`] descriptor (stem + which files were
/// read + which hooks fired). Errors short-circuit: a malformed first
/// file aborts the whole plugin load.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub fn load_plugin(
    mgr: &mut PluginManager,
    path: &std::path::Path,
) -> Result<LoadedPlugin, String> {
    loader::load_plugin(mgr, path)
}

pub struct PluginManager {
    hooks: HashMap<String, Vec<String>>,
    /// All Janet evaluation goes through this handle to the worker
    /// thread. The handle is naturally `Send + Sync` (only an mpsc
    /// Sender + JoinHandle inside) so no unsafe impl is needed — the
    /// previous `unsafe impl Send for PluginManager` is gone now that
    /// Janet lives on its own OS thread.
    worker: Worker,
    /// One-shot consumer end of the dialog channel. Taken out by
    /// `take_dialog_rx` on first call so the UI can register it in its
    /// `tokio::select!`. After that, the field is `None`.
    dialog_rx: Option<tokio::sync::mpsc::UnboundedReceiver<DialogRequest>>,
    /// One-shot consumer end of the LSP-request channel (the
    /// `harness/lsp` bridge). Taken by `take_lsp_rx` so the host spawns a
    /// drainer that owns the `LspManager`. `None` after the first take.
    /// Only consumed when the `lsp` feature is also on; held regardless so
    /// the worker handshake stays uniform.
    #[cfg_attr(not(feature = "lsp"), allow(dead_code))]
    lsp_rx: Option<tokio::sync::mpsc::UnboundedReceiver<LspRequest>>,
}

#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
impl PluginManager {
    /// Spawn the Janet worker thread and wait for it to install the
    /// harness API. Returns Err if Janet VM init fails so the host can
    /// fall back to a no-plugin path rather than panicking.
    pub fn try_new() -> Result<Self, String> {
        let (worker, dialog_rx, lsp_rx) = Worker::try_spawn()?;
        Ok(PluginManager {
            hooks: HashMap::new(),
            worker,
            dialog_rx: Some(dialog_rx),
            lsp_rx: Some(lsp_rx),
        })
    }

    /// Consume the dialog-request consumer end so the UI loop can wire it
    /// into its `tokio::select!`. Only succeeds once; subsequent calls
    /// return `None` because the Receiver has a single owner.
    pub fn take_dialog_rx(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<DialogRequest>> {
        self.dialog_rx.take()
    }

    /// Consume the LSP-request consumer end so the host can spawn a drainer
    /// that owns the `LspManager` and answers `harness/lsp` queries. Only
    /// succeeds once (single owner). Only called when the `lsp` feature is
    /// also on; held regardless to keep the worker handshake uniform.
    #[cfg_attr(not(feature = "lsp"), allow(dead_code))]
    pub fn take_lsp_rx(&mut self) -> Option<tokio::sync::mpsc::UnboundedReceiver<LspRequest>> {
        self.lsp_rx.take()
    }

    pub fn load_file(&mut self, path: &std::path::Path) -> Result<(), String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read plugin: {e}"))?;
        self.eval(&content)?;
        Ok(())
    }

    pub fn register(&mut self, hook: &str, script: &str) {
        self.hooks
            .entry(hook.to_string())
            .or_default()
            .push(script.to_string());
    }

    pub fn take_pending_prompt(&mut self) -> Option<String> {
        self.take_string_slot("harness-pending")
    }

    pub fn store_response(&mut self, response: &str) {
        let escaped = escape_janet_string(response);
        let _ = self
            .worker
            .eval(&format!(r#"(set harness-response "{}")"#, escaped));
    }

    /// Check whether a top-level symbol is bound in the Janet env
    /// without triggering Janet's compile-error stderr output.
    pub fn has_symbol(&mut self, name: &str) -> bool {
        let escaped = escape_janet_string(name);
        let code = format!(r#"(harness/has-symbol? "{}")"#, escaped);
        self.worker
            .eval(&code)
            .map(|s| s == "true")
            .unwrap_or(false)
    }

    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        self.worker.eval(code)
    }

    /// dirge-99ic: stash the loading plugin's `config.json` settings in
    /// `harness-plugin-config` so its load-time code can read them via
    /// `harness/plugin-config`. Call right before `load_plugin`, then
    /// [`clear_loading_plugin_config`](Self::clear_loading_plugin_config)
    /// after, so one plugin's config never leaks into the next.
    pub fn set_loading_plugin_config(&mut self, enabled: bool, auto_start: bool) {
        let _ = self.worker.eval(&format!(
            "(set harness-plugin-config @{{:enabled {enabled} :auto-start {auto_start}}})"
        ));
    }

    /// Reset the plugin-config slot to nil after a plugin finishes loading.
    pub fn clear_loading_plugin_config(&mut self) {
        let _ = self.worker.eval("(set harness-plugin-config nil)");
    }

    pub fn dispatch(&mut self, hook: &str, context_janet: &str) -> Result<Vec<String>, String> {
        let names = match self.hooks.get(hook) {
            Some(names) => names.clone(),
            None => return Ok(Vec::new()),
        };

        let mut results = Vec::new();
        for name in &names {
            // Wrap the call so plugin runtime errors don't print
            // Janet stack traces to stderr OR vanish silently. On
            // error, the catch arm does TWO things:
            //   1. Push an entry onto `harness-notif-list` with
            //      `error` level so the next drain surfaces a
            //      chat-visible "[plugin] hook X.Y errored: ..."
            //      banner (pi-style — see
            //      `packages/coding-agent/src/core/extensions/runner.ts`
            //      which emits structured `ExtensionError` events
            //      the host renders as `ctx.ui.notify("...","error")`).
            //   2. Return `DIRGE_HOOK_ERR:<msg>` so the Rust side
            //      can also log a structured `tracing::warn!` —
            //      surfaces via `--verbose` or `RUST_LOG=warn`.
            //
            // Prior behavior (now retired): pure
            // `(try ... ([err fib] nil))` which swallowed errors
            // entirely. Plugin authors got no feedback on broken
            // hooks. opencode logged but didn't notify; pi notified.
            // dirge now does both.
            let hook_escaped = escape_janet_string(hook);
            let name_escaped = escape_janet_string(name);
            let code = format!(
                r#"(try (do (def ctx {ctx}) ({fname} ctx))
                       ([err fib]
                         (do
                           (def sanitized
                             (harness/sanitize-hook-err
                               (string "[plugin] hook "
                                       "\"{hook_escaped}\""
                                       "."
                                       "\"{name_escaped}\""
                                       " errored: "
                                       err)))
                           (harness/push-hook-err sanitized)
                           (string "DIRGE_HOOK_ERR:" err))))"#,
                ctx = context_janet,
                fname = name,
            );
            if let Ok(s) = self.eval(&code) {
                if let Some(msg) = s.strip_prefix("DIRGE_HOOK_ERR:") {
                    tracing::warn!(
                        target: "dirge::plugin",
                        hook = %hook,
                        function = %name,
                        error = %msg,
                        "plugin hook errored — continuing dispatch",
                    );
                    continue;
                }
                // Janet nil -> skip
                if s != "nil" && !s.is_empty() {
                    results.push(s);
                }
            }
        }

        Ok(results)
    }

    /// Read and clear the `harness-block` slot. Returns the reason a plugin
    /// gave when calling `(harness/block "...")` from inside a tool hook,
    /// or `None` if no plugin set it.
    pub fn take_pending_block(&mut self) -> Option<String> {
        self.take_string_slot("harness-block")
    }

    /// Read and clear the `harness-mutate-input` slot. The returned string,
    /// when present, is a JSON encoding of the new tool args that the host
    /// should re-deserialize before invoking the tool.
    pub fn take_pending_mutate_input(&mut self) -> Option<String> {
        self.take_string_slot("harness-mutate-input")
    }

    /// Read and clear the `harness-replace-result` slot. The returned
    /// string, when present, is the tool output the LLM should see instead
    /// of the real one.
    pub fn take_pending_replace_result(&mut self) -> Option<String> {
        self.take_string_slot("harness-replace-result")
    }

    /// Read and clear the `harness-next-model` slot. Set by
    /// plugins from `prepare-next-run` to swap the active model
    /// before the next user prompt runs. Mid-stream model swap
    /// isn't supported — the host reads this only AFTER Done and
    /// applies it via the same path that `/model <name>` uses.
    pub fn take_pending_next_model(&mut self) -> Option<String> {
        self.take_string_slot("harness-next-model")
    }

    // ============================================================
    // Phase 5 — pi-loop hook slots
    // ============================================================

    /// Read and clear the `harness-next-thinking-level` slot.
    /// Set by plugins via `harness/set-next-thinking-level` to
    /// request a reasoning-level change for the next turn. The
    /// new agent_loop path consults this from its
    /// `prepareNextTurn` hook.
    ///
    /// Returns the raw string ("low" | "medium" | "high" |
    /// "xhigh" | "off" | "minimal"); the caller maps it to
    /// `ThinkingLevel`.
    pub fn take_pending_next_thinking_level(&mut self) -> Option<String> {
        self.take_string_slot("harness-next-thinking-level")
    }

    /// Read and clear the `harness-stop-after-turn` flag. Set
    /// by plugins via `harness/request-stop-after-turn` to ask
    /// the loop to exit gracefully after the current turn.
    /// Polled by the agent_loop `shouldStopAfterTurn` hook.
    pub fn take_pending_stop_after_turn(&mut self) -> bool {
        // The slot is `nil` initially and `true` once set. Eval
        // returns "true" or "false" as text.
        let was_set = self
            .worker
            .eval("(if harness-stop-after-turn true false)")
            .map(|s| s == "true")
            .unwrap_or(false);
        if was_set {
            let _ = self.worker.eval("(set harness-stop-after-turn nil)");
        }
        was_set
    }

    /// Drain the `harness-steering-messages` blob — a newline-
    /// separated list of strings each plugins added via
    /// `harness/add-steering`. Returns one entry per message;
    /// empty Vec if no plugin added any.
    pub fn drain_steering_messages(&mut self) -> Vec<String> {
        self.drain_newline_blob("harness-steering-messages")
    }

    /// Drain the `harness-followup-messages` blob. Same shape
    /// as steering; read at the outer-loop boundary by the new
    /// loop's `getFollowUpMessages` hook.
    pub fn drain_followup_messages(&mut self) -> Vec<String> {
        self.drain_newline_blob("harness-followup-messages")
    }

    /// Drain the `harness-custom-messages` blob — UI-only
    /// notification entries. Pushed by plugins via
    /// `harness/add-custom-message`. The loop emits each as a
    /// `LoopMessage::Custom` wrapper carrying `customType`,
    /// `content`, and `display` at the top level (pi parity —
    /// CustomMessage shape, messages.ts:46). `convert_to_llm`
    /// drops the role so the LLM never sees them.
    pub fn drain_custom_messages(&mut self) -> Vec<CustomMessageEntry> {
        let raw = self
            .worker
            .eval("(if (string? harness-custom-messages) harness-custom-messages \"\")")
            .unwrap_or_default();
        let _ = self.worker.eval(r#"(set harness-custom-messages "")"#);
        raw.lines().filter_map(parse_custom_message_line).collect()
    }

    /// Shared body for `drain_*_messages` — read the slot's
    /// string contents, split on newline, filter empty entries,
    /// clear the slot to `""`.
    fn drain_newline_blob(&mut self, var: &str) -> Vec<String> {
        let raw = self
            .worker
            .eval(&format!("(if (string? {var}) {var} \"\")"))
            .unwrap_or_default();
        let _ = self.worker.eval(&format!("(set {var} \"\")"));
        raw.lines()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Read and clear the `harness-prompt-replace` slot. Set by plugins
    /// from `on-prompt` to rewrite the user turn before the agent runs.
    /// Distinct from `take_pending_prompt`, which carries the
    /// `request-prompt` queue for the *next* turn.
    pub fn take_pending_prompt_replace(&mut self) -> Option<String> {
        self.take_string_slot("harness-prompt-replace")
    }

    /// dirge-wqxj: read and clear the `harness-system-prompt-append`
    /// slot. Set by a `before-agent-start` hook via
    /// `harness/append-system-prompt`; the host appends it to the
    /// assembled system prompt before the agent starts.
    pub fn take_system_prompt_append(&mut self) -> Option<String> {
        self.take_string_slot("harness-system-prompt-append")
    }

    /// dirge-lsoq: read and clear the `harness-message-rewrite` slot.
    /// Set by a `message-end` hook via `harness/rewrite-message`; the
    /// host replaces the finalized assistant response with it.
    pub fn take_message_rewrite(&mut self) -> Option<String> {
        self.take_string_slot("harness-message-rewrite")
    }

    /// dirge-264x: read and clear the `harness-replace-context` slot.
    /// Set by a `transform-context` hook via `harness/replace-context`
    /// (a JSON array string); the host uses it for the next LLM call.
    pub fn take_replace_context(&mut self) -> Option<String> {
        self.take_string_slot("harness-replace-context")
    }

    /// dirge-jia8: read and clear the `harness-compact-summary` slot.
    /// Set by an `on-compact` hook via `harness/set-compact-summary`;
    /// the host uses it as the compaction summary instead of the LLM
    /// (subject to the same validity check).
    pub fn take_compact_summary(&mut self) -> Option<String> {
        self.take_string_slot("harness-compact-summary")
    }

    /// Shared body of the three `take_pending_*` functions: probe the type
    /// to disambiguate Janet's nil from a string with the characters "nil",
    /// fetch the value if it's a string, then clear the slot.
    fn take_string_slot(&mut self, var: &str) -> Option<String> {
        let is_string = self
            .worker
            .eval(&format!("(if (string? {var}) true false)"))
            .map(|s| s == "true")
            .unwrap_or(false);
        if !is_string {
            return None;
        }
        let val = self.worker.eval(var).ok()?;
        let _ = self.worker.eval(&format!("(set {var} nil)"));
        Some(val)
    }

    /// Specialized dispatcher for tool-hook events (`on-tool-start`,
    /// `on-tool-end`). Clears all tool-hook slots first so previous-call
    /// state doesn't leak, runs registered hooks in load order, then
    /// collects the slot values into a structured result.
    ///
    /// **First-blocker-wins**: as soon as ANY hook sets `harness-block`,
    /// dispatch stops and subsequent hooks do NOT run. This matches
    /// pi's `runner.ts:806-827` `tool_call` semantics and is more
    /// intuitive than the prior last-write-wins behavior — once a
    /// plugin has decided to deny the tool call, running additional
    /// hooks is wasted work and the block reason becomes load-order-
    /// dependent. Mutations (`mutate-input`, `replace-result`) keep
    /// last-write-wins chaining for compatibility.
    pub fn dispatch_tool_hook(
        &mut self,
        hook: &str,
        context_janet: &str,
    ) -> Result<ToolHookResult, String> {
        // Pre-clear so a stale (harness/block ...) left by an unrelated
        // hook can't cause us to mis-block this tool.
        let _ = self
            .worker
            .eval("(set harness-block nil) (set harness-mutate-input nil) (set harness-replace-result nil)");

        let names = match self.hooks.get(hook) {
            Some(names) => names.clone(),
            None => Vec::new(),
        };

        for name in &names {
            // Same catch-wrapping shape as `dispatch` — sanitize the
            // error, queue chat notification + tracing::warn on
            // failure, continue dispatch otherwise. Kept inline here
            // (rather than reusing `dispatch`) so we can check the
            // harness-block slot AFTER each plugin's hook returns
            // and break out early. Sharing `dispatch` would require
            // a flag parameter to either run all or stop-on-block.
            let hook_escaped = escape_janet_string(hook);
            let name_escaped = escape_janet_string(name);
            let code = format!(
                r#"(try (do (def ctx {ctx}) ({fname} ctx))
                       ([err fib]
                         (do
                           (def sanitized
                             (harness/sanitize-hook-err
                               (string "[plugin] hook "
                                       "\"{hook_escaped}\""
                                       "."
                                       "\"{name_escaped}\""
                                       " errored: "
                                       err)))
                           (harness/push-hook-err sanitized)
                           (string "DIRGE_HOOK_ERR:" err))))"#,
                ctx = context_janet,
                fname = name,
            );
            // Audit L6: tighter per-hook timeout than the default
            // EVAL_TIMEOUT (10 min). A hung `on-tool-start` used to
            // freeze every subsequent tool call for the full 10 min;
            // 5s is enough headroom for any reasonable hook (most
            // execute in < 100 ms) while still recovering quickly
            // from a plugin stuck in `(while true)` or a blocking
            // syscall.
            const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
            match self.worker.eval_with_timeout(&code, HOOK_TIMEOUT) {
                Ok(s) => {
                    if let Some(msg) = s.strip_prefix("DIRGE_HOOK_ERR:") {
                        tracing::warn!(
                            target: "dirge::plugin",
                            hook = %hook,
                            function = %name,
                            error = %msg,
                            "plugin hook errored — continuing dispatch",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::plugin",
                        hook = %hook,
                        function = %name,
                        error = %e,
                        "plugin hook timed out or worker disconnected — continuing dispatch without its result",
                    );
                }
            }

            // First-wins block check. Peek the slot WITHOUT clearing —
            // the final `take_pending_block` below does the clear.
            // If this plugin set the slot, stop iterating so remaining
            // plugins don't observe / mutate state for a tool call
            // that will be refused anyway.
            if self.has_pending_block() {
                break;
            }
        }

        Ok(ToolHookResult {
            block: self.take_pending_block(),
            mutate_input: self.take_pending_mutate_input(),
            replace_result: self.take_pending_replace_result(),
        })
    }

    /// Peek the `harness-block` slot without clearing it. Returns
    /// true when a plugin has set the slot in the current dispatch.
    /// Used by `dispatch_tool_hook` to implement first-wins block
    /// semantics; the slot is cleared by the final
    /// `take_pending_block` after iteration completes.
    fn has_pending_block(&mut self) -> bool {
        match self
            .worker
            .eval("(if (nil? harness-block) \"\" harness-block)")
        {
            Ok(s) => !s.is_empty() && s != "nil",
            Err(_) => false,
        }
    }

    /// Snapshot the plugin-registered slash commands as `(cmd-name,
    /// handler-fn-name)` pairs in load order. Read once after all plugins
    /// finish loading; subsequent registrations require a reload to take
    /// effect (kept simple for now — Phase 5 will add hot-reload).
    ///
    /// 9b: wire format is tab-separated, escape-encoded — matches the
    /// other phase-9 registries. Last-load-wins on cmd-name collision
    /// via `dedup_last_wins`.
    pub fn list_commands(&mut self) -> Vec<(String, String)> {
        let raw = match self.worker.eval("harness-cmd-list") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        let parsed: Vec<(String, String)> = raw
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let cmd = unescape_harness_field(parts.next()?);
                let handler = unescape_harness_field(parts.next()?);
                if cmd.is_empty() || handler.is_empty() {
                    None
                } else {
                    Some((cmd, handler))
                }
            })
            .collect();
        dedup_last_wins(parsed, "slash command", |(c, _)| c.clone())
    }

    /// Invoke a registered handler fn by name with the user-provided args
    /// string (everything after the command name). Returns `Ok(Some(text))`
    /// when the handler produced a non-nil string, `Ok(None)` when it
    /// returned nil/empty or when the handler raised inside Janet. The
    /// caller-visible error path is reserved for catastrophic Janet
    /// failures (VM dead, etc.) — handler-level errors are surfaced as
    /// a chat notification via `harness/push-hook-err` and a
    /// `tracing::warn` (so plugin authors get visible feedback) but the
    /// caller still sees `Ok(None)` to avoid tearing down the slash /
    /// shortcut dispatch on a buggy plugin (H5 fix).
    pub fn invoke_command(
        &mut self,
        handler_fn: &str,
        args: &str,
    ) -> Result<Option<String>, String> {
        let escaped_args = escape_janet_string(args);
        let escaped_fn = escape_janet_string(handler_fn);
        // Use `(get (curenv) (symbol ...))` to look up the handler so a
        // missing fn doesn't print Janet's "unknown symbol" error to
        // stderr. Then call it via `apply` if found.
        //
        // Error handling matches dispatch()'s pattern: on Janet
        // exception, queue a `[plugin] command <name> errored: <err>`
        // notification AND return `DIRGE_HOOK_ERR:<msg>` so the Rust
        // side can also tracing::warn. Caller still sees Ok(None).
        let handler_fn_escaped = escape_janet_string(handler_fn);
        let code = format!(
            r#"(try
                 (let [f (get (curenv) (symbol "{fname}"))]
                   (if (and f (function? (f :value)))
                     ((f :value) "{args}")
                     nil))
                 ([err fib]
                   (do
                     (def sanitized
                       (harness/sanitize-hook-err
                         (string "[plugin] command "
                                 "{handler_fn_escaped}"
                                 " errored: "
                                 err)))
                     (harness/push-hook-err sanitized)
                     (string "DIRGE_HOOK_ERR:" err))))"#,
            fname = escaped_fn,
            args = escaped_args,
        );
        let result = self.eval(&code)?;
        if let Some(msg) = result.strip_prefix("DIRGE_HOOK_ERR:") {
            tracing::warn!(
                target: "dirge::plugin",
                handler = %handler_fn,
                error = %msg,
                "plugin command/shortcut handler errored — surfaced via notification",
            );
            return Ok(None);
        }
        if result == "nil" || result.is_empty() {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }

    /// Snapshot plugin-registered LLM provider specs as
    /// `(name, type, base_url, api_key_env)` tuples. `api_key_env` is
    /// `None` when the plugin passed an empty string (meaning "use
    /// the default env var for this provider type"). Read once after
    /// all plugins load and merged into the host's resolver via
    /// [`crate::provider::install_plugin_providers`].
    pub fn list_providers(&mut self) -> Vec<(String, String, String, Option<String>)> {
        // 9b: wire format is tab-separated, escape-encoded — matches
        // the other phase-9 registries. Last-load-wins on provider
        // name collision via dedup_last_wins.
        let raw = match self.worker.eval("harness-providers-list") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        let parsed: Vec<(String, String, String, Option<String>)> = raw
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let name = unescape_harness_field(parts.next()?);
                let ptype = unescape_harness_field(parts.next()?);
                let base_url = unescape_harness_field(parts.next()?);
                let env_raw = unescape_harness_field(parts.next()?);
                if name.is_empty() || ptype.is_empty() || base_url.is_empty() {
                    return None;
                }
                let env = if env_raw.is_empty() {
                    None
                } else {
                    Some(env_raw)
                };
                Some((name, ptype, base_url, env))
            })
            .collect();
        dedup_last_wins(parsed, "plugin provider", |(n, _, _, _)| n.clone())
    }

    /// Snapshot plugin-registered LLM tools (P9a). Each entry has the
    /// raw JSON-schema parameters string, the Janet handler name, and
    /// an optional execution-mode override. Read once after all plugins
    /// load; the host wraps each entry in a `JanetLoopTool` adapter and
    /// appends them to the agent loop's tool registry.
    ///
    /// H4: duplicate `name` registrations resolve last-wins (matches
    /// pi's `extension.tools.set(name, ...)` Map semantics — second
    /// `set` replaces first). Each drop emits a `tracing::warn` so
    /// plugin authors see the collision.
    pub fn list_plugin_tools(&mut self) -> Vec<PluginToolMeta> {
        let raw = match self.worker.eval("harness-tools-list") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        let parsed: Vec<PluginToolMeta> = raw.lines().filter_map(parse_plugin_tool_line).collect();
        dedup_last_wins(parsed, "plugin tool", |t| t.name.clone())
    }

    /// Snapshot plugin-registered message renderers (P9d). Each
    /// entry is `(type-name, handler-fn-name)`. The UI looks up the
    /// `type` field of a `LoopMessage::Custom` payload here when
    /// rendering; no entry means the default formatter is used.
    ///
    /// H4: duplicate `type-name` registrations resolve last-wins.
    pub fn list_message_renderers(&mut self) -> Vec<(String, String)> {
        let raw = match self.worker.eval("harness-msg-renderers-list") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        let parsed: Vec<(String, String)> = raw
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let t = unescape_harness_field(parts.next()?);
                let h = unescape_harness_field(parts.next()?);
                if t.is_empty() || h.is_empty() {
                    None
                } else {
                    Some((t, h))
                }
            })
            .collect();
        dedup_last_wins(parsed, "message renderer", |(t, _)| t.clone())
    }

    /// Invoke a Janet message-renderer handler with the raw JSON
    /// payload string. The handler is called as `(handler payload)`
    /// and may return any value `(string ...)` can render. Returns
    /// `Ok(Some(text))` when the handler produced a non-empty string,
    /// `Ok(None)` when it returned nil/empty or when the handler
    /// raised (errors swallowed so a broken renderer doesn't tear
    /// down message dispatch). Catastrophic Janet failures still
    /// surface as `Err`.
    pub fn invoke_message_renderer(
        &mut self,
        handler: &str,
        payload_json: &str,
    ) -> Result<Option<String>, String> {
        let escaped_payload = escape_janet_string(payload_json);
        let escaped_fn = escape_janet_string(handler);
        let code = format!(
            r#"(try
                 (let [f (get (curenv) (symbol "{fname}"))]
                   (if (and f (function? (f :value)))
                     (let [r ((f :value) "{payload}")]
                       (if (string? r) r (string r)))
                     nil))
                 ([err fib] nil))"#,
            fname = escaped_fn,
            payload = escaped_payload,
        );
        let result = self.worker.eval(&code)?;
        if result == "nil" || result.is_empty() {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }

    /// Snapshot plugin-registered keyboard shortcuts (P9c). Each
    /// entry has the raw key-spec string (e.g. "ctrl-x"), the Janet
    /// handler name, and an optional description for UI listing.
    /// The UI key-dispatch path reads this once and matches against
    /// incoming `KeyEvent`s before built-in handling. Plugins that
    /// load after the first snapshot need a host restart for new
    /// bindings to take effect (kept simple for now).
    pub fn list_shortcuts(&mut self) -> Vec<PluginShortcutMeta> {
        let raw = match self.worker.eval("harness-shortcuts-list") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        // H4: duplicate key-specs resolve last-wins (matches pi's
        // Map.set behaviour for shortcut registration). Pi also emits
        // a shortcut diagnostic; dirge surfaces via tracing::warn.
        let parsed: Vec<PluginShortcutMeta> =
            raw.lines().filter_map(parse_plugin_shortcut_line).collect();
        dedup_last_wins(parsed, "plugin shortcut", |s| s.keys.clone())
    }

    /// Invoke a Janet `prepare-arguments` handler (H3 / pi parity —
    /// `prepareArguments?` at extensions/types.ts:443). The handler
    /// is called as `(handler args-json)` and is expected to return a
    /// mutated JSON string the loop will then validate against the
    /// tool's schema. Errors or non-string returns swallow back to
    /// `Ok(None)` — the caller falls through to the original args
    /// rather than failing the tool call on a buggy plugin.
    pub fn invoke_prepare_arguments(
        &mut self,
        handler: &str,
        args_json: &str,
    ) -> Result<Option<String>, String> {
        let escaped_args = escape_janet_string(args_json);
        let escaped_fn = escape_janet_string(handler);
        let code = format!(
            r#"(try
                 (let [f (get (curenv) (symbol "{fname}"))]
                   (if (and f (function? (f :value)))
                     (let [r ((f :value) "{args}")]
                       (if (string? r) r nil))
                     nil))
                 ([err fib] nil))"#,
            fname = escaped_fn,
            args = escaped_args,
        );
        let result = self.worker.eval(&code)?;
        if result == "nil" || result.is_empty() {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }

    /// Invoke a Janet tool handler with the raw JSON args string the
    /// LLM produced. The handler is called as `(handler args)` and may
    /// return any value `(string ...)` can render. Returns the tool's
    /// stringified output, or `Err` carrying the Janet exception text
    /// when the handler raises.
    pub fn invoke_plugin_tool(
        &mut self,
        handler: &str,
        args_json: &str,
        tool_call_id: &str,
    ) -> Result<String, String> {
        let escaped_args = escape_janet_string(args_json);
        let escaped_id = escape_janet_string(tool_call_id);
        // Set the current-tool-call slot BEFORE invoking the handler
        // so `harness/emit-tool-progress` knows which call to tag.
        // Wrap the whole sequence in a `try` so the slot is always
        // cleared, even on handler error — `try`'s catch arm runs
        // before the `do`'s tail returns. We use a top-level set
        // afterwards rather than `defer` so the slot reliably resets
        // for the next invocation.
        let code = format!(
            r#"(do
                 (set harness-current-tool-call "{tcid}")
                 (def result
                   (try (let [r ({handler} "{args}")]
                          (if (string? r) r (string r)))
                        ([err fib] (string "DIRGE_TOOL_ERR:" err))))
                 (set harness-current-tool-call nil)
                 result)"#,
            tcid = escaped_id,
            handler = handler,
            args = escaped_args,
        );
        let out = self.worker.eval(&code)?;
        if let Some(msg) = out.strip_prefix("DIRGE_TOOL_ERR:") {
            Err(msg.to_string())
        } else {
            Ok(out)
        }
    }

    /// Drain pending `(harness/emit-tool-progress ...)` entries as
    /// `(tool_call_id, text)` pairs (H2). The host forwards each to
    /// the matching `LoopTool::execute` `on_update` callback. Called
    /// at loop tick + after the plugin handler returns so streaming
    /// updates surface ASAP.
    pub fn drain_tool_progress(&mut self) -> Vec<(String, String)> {
        let raw = match self.worker.eval("harness-tool-progress") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        let _ = self.worker.eval(r#"(set harness-tool-progress "")"#);
        raw.lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let id = unescape_harness_field(parts.next()?);
                let text = unescape_harness_field(parts.next()?);
                if id.is_empty() {
                    None
                } else {
                    Some((id, text))
                }
            })
            .collect()
    }

    /// Drain pending `(harness/notify ...)` entries as `(level, msg)`
    /// pairs in insertion order. The UI calls this each loop tick and
    /// renders entries as colored chat lines. Returns an empty Vec when
    /// no plugin has posted anything.
    pub fn drain_notifications(&mut self) -> Vec<(String, String)> {
        // Flush any pending hook-error dedup count BEFORE reading
        // `harness-notif-list`. If a hook errored 50 times in a row,
        // the first error is already on the list and the next 49
        // got coalesced into `harness-last-hook-err-count`. The
        // flush appends a single "(repeated 49 times)" entry so the
        // count shows up in the next drain instead of being lost.
        // Resets the dedup slots regardless so a future drain
        // starts fresh.
        let _ = self.worker.eval(
            r#"(do
                 (when (and harness-last-hook-err-msg
                            (> harness-last-hook-err-count 1))
                   (set harness-notif-list
                        (string harness-notif-list
                                "error\t"
                                harness-last-hook-err-msg
                                " (repeated "
                                harness-last-hook-err-count
                                " times)\n")))
                 (set harness-last-hook-err-msg nil)
                 (set harness-last-hook-err-count 0))"#,
        );

        let raw = match self.worker.eval("harness-notif-list") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        let parsed: Vec<(String, String)> = raw
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, '\t');
                let level = parts.next()?.trim();
                let msg = parts.next()?;
                if level.is_empty() || msg.is_empty() {
                    None
                } else {
                    Some((level.to_string(), msg.to_string()))
                }
            })
            .collect();
        // Drain the slot after read so the next tick starts fresh.
        let _ = self.worker.eval(r#"(set harness-notif-list "")"#);
        parsed
    }

    /// Drain `(harness/append-entry ...)` calls as `(custom_type, data,
    /// display)` triples. Janet escapes embedded tabs/newlines/
    /// backslashes via the harness escape; we reverse it here so the
    /// host stores the original bytes.
    pub fn drain_entries(&mut self) -> Vec<(String, String, bool)> {
        let raw = match self.worker.eval("harness-entries-buf") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        let parsed: Vec<(String, String, bool)> = raw
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let custom_type = unescape_harness_field(parts.next()?);
                let data = unescape_harness_field(parts.next()?);
                let display = parts.next().is_some_and(|d| d.trim() == "1");
                if custom_type.is_empty() {
                    None
                } else {
                    Some((custom_type, data, display))
                }
            })
            .collect();
        let _ = self.worker.eval(r#"(set harness-entries-buf "")"#);
        parsed
    }

    /// Snapshot the plugin-registered renderers as `(custom_type,
    /// handler-fn-name)` pairs. Same blob format as `list_commands`.
    pub fn list_renderers(&mut self) -> Vec<(String, String)> {
        let raw = match self.worker.eval("harness-renderer-list") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        raw.lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, '|');
                let kind = parts.next()?.trim();
                let handler = parts.next()?.trim();
                if kind.is_empty() || handler.is_empty() {
                    None
                } else {
                    Some((kind.to_string(), handler.to_string()))
                }
            })
            .collect()
    }

    /// Invoke a registered renderer with the entry's `data` string.
    /// Returns the rendered lines as `(color, text)` pairs. The
    /// host pre-clears `harness-render-buf`, calls the renderer,
    /// then reads back the accumulated lines.
    ///
    /// On any failure (handler missing, raised Janet error, malformed
    /// output) returns Ok(empty); callers should fall back to the
    /// default JSON-dump rendering. We surface Err only for
    /// catastrophic Janet failures (VM gone).
    pub fn invoke_renderer(
        &mut self,
        handler_fn: &str,
        data: &str,
    ) -> Result<Vec<(String, String)>, String> {
        let _ = self.worker.eval(r#"(set harness-render-buf "")"#);
        let escaped_data = escape_janet_string(data);
        let escaped_fn = escape_janet_string(handler_fn);
        let code = format!(
            r#"(try
                 (let [f (get (curenv) (symbol "{fname}"))]
                   (if (and f (function? (f :value)))
                     ((f :value) "{data}")
                     nil))
                 ([err fib] nil))"#,
            fname = escaped_fn,
            data = escaped_data,
        );
        let _ = self.eval(&code)?;
        let raw = self.worker.eval("harness-render-buf").unwrap_or_default();
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: Vec<(String, String)> = raw
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, '\t');
                let color = parts.next()?.trim();
                let text = unescape_harness_field(parts.next()?);
                if color.is_empty() {
                    None
                } else {
                    Some((color.to_string(), text))
                }
            })
            .collect();
        let _ = self.worker.eval(r#"(set harness-render-buf "")"#);
        Ok(parsed)
    }

    /// Drain pending session-tree ops queued by plugins via the
    /// `harness/set-label`, `harness/fork`, `harness/navigate-tree`,
    /// `harness/new-session`, `harness/switch-session` APIs (P4d).
    ///
    /// The buffer is `op\targ1[\targ2]\n` per line; arguments are
    /// `harness/-escape`d so embedded tabs/newlines round-trip. Lines
    /// the host doesn't recognize are skipped (forward compat — a newer
    /// plugin shipping an op the host doesn't know yet is silently
    /// ignored rather than blowing up the whole queue).
    pub fn drain_tree_ops(&mut self) -> Vec<TreeOp> {
        let raw = match self.worker.eval("harness-tree-ops") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if raw.is_empty() {
            return Vec::new();
        }
        let parsed: Vec<TreeOp> = raw.lines().filter_map(parse_tree_op_line).collect();
        let _ = self.worker.eval(r#"(set harness-tree-ops "")"#);
        parsed
    }
}

/// Snapshot of a plugin-registered LLM tool (P9a). Each field maps
/// 1:1 to the `(harness/register-tool …)` call site. `execution_mode`
/// is `None` when the plugin omitted the optional argument; the agent
/// loop then uses its default (parallel).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub struct PluginToolMeta {
    pub name: String,
    pub description: String,
    pub label: String,
    /// Raw JSON-schema string for the tool's `parameters` field.
    /// Stored unparsed so the host can hand it straight to the agent
    /// loop's `LoopTool::parameters()` (which is itself `Value`).
    pub parameters: String,
    /// Name of the Janet function that handles invocations.
    pub handler: String,
    /// `"sequential"` or `"parallel"`, or `None` when unset.
    pub execution_mode: Option<String>,
    /// Name of an optional Janet function that runs BEFORE schema
    /// validation to normalize the LLM-supplied args. Pi parity —
    /// `prepareArguments?` (extensions/types.ts:443). `None` means
    /// the loop validates args verbatim.
    pub prepare_handler: Option<String>,
}

fn parse_plugin_tool_line(line: &str) -> Option<PluginToolMeta> {
    let mut parts = line.split('\t');
    let name = unescape_harness_field(parts.next()?);
    let description = unescape_harness_field(parts.next()?);
    let label = unescape_harness_field(parts.next()?);
    let parameters = unescape_harness_field(parts.next()?);
    let handler = unescape_harness_field(parts.next()?);
    let mode_raw = parts.next().unwrap_or("").trim();
    // 7th field (prepare-arguments handler name) is optional — a
    // line without it (legacy pre-H3 emitters) parses with
    // prepare_handler = None.
    let prepare_raw = parts.next().map(unescape_harness_field).unwrap_or_default();
    if name.is_empty() || handler.is_empty() {
        return None;
    }
    // L5: validate the LLM-facing tool name against the charset
    // LLM providers (Anthropic, OpenAI, etc.) accept for tool
    // names — `[a-zA-Z0-9_-]+`. A plugin that registers `"my tool"`
    // (with a space) would otherwise reach the provider and either
    // be rejected at the API boundary or get silently renamed.
    // Drop the entry with a tracing::warn so the author sees it.
    if !is_valid_tool_name(&name) {
        tracing::warn!(
            target: "dirge::plugin",
            tool = %name,
            "plugin tool name contains chars outside [a-zA-Z0-9_-]; dropping",
        );
        return None;
    }
    let execution_mode = match mode_raw {
        "sequential" | "parallel" => Some(mode_raw.to_string()),
        _ => None,
    };
    let prepare_handler = if prepare_raw.is_empty() {
        None
    } else {
        Some(prepare_raw)
    };
    Some(PluginToolMeta {
        name,
        description,
        label,
        parameters,
        handler,
        execution_mode,
        prepare_handler,
    })
}

/// L5: LLM provider tool names must match `[a-zA-Z0-9_-]+`. Spaces,
/// dots, slashes, unicode etc. either get rejected at the API
/// boundary or silently renamed — neither is what the plugin author
/// expected. Drop the registration upfront with a tracing::warn.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// One entry drained from `harness-custom-messages`. Pi parity —
/// `CustomMessage` (messages.ts:46) has `customType` + `content` +
/// `display` as top-level fields. The wrapper JSON the loop emits
/// surfaces all three so registered message renderers can dispatch
/// by `customType` and the UI can honor `display=false`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub struct CustomMessageEntry {
    /// Renderer-lookup key. Empty when the plugin used the
    /// single-string `(harness/add-custom-message "text")` form.
    pub custom_type: String,
    pub content: String,
    /// Whether the UI should render the chat line. `display=false`
    /// keeps the message in the transcript (where plugin handlers
    /// can still observe it) but suppresses the visible row.
    pub display: bool,
}

/// Generic last-add-wins deduplicator for plugin registries.
///
/// Pi keeps each registry as a `Map<key, value>`, so the second
/// `.set(key, …)` replaces the first. Dirge's append-only line
/// format produces a `Vec<entry>` instead; this helper restores the
/// same semantics. Each dropped entry emits a `tracing::warn` carrying
/// the registry name and the collided key — surfaces in `RUST_LOG`
/// tail so plugin authors notice typos / accidental dual registration.
///
/// Order: the surviving entry occupies the LAST position the key
/// appeared at. Stable across the kept items.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
fn dedup_last_wins<T, K, F>(entries: Vec<T>, kind: &str, key_of: F) -> Vec<T>
where
    T: Clone,
    K: Eq + std::hash::Hash + std::fmt::Display + Clone,
    F: Fn(&T) -> K,
{
    use std::collections::HashMap;
    // Two-pass: count occurrences, then keep only the last entry
    // per key. Warn on the dropped occurrences.
    let mut last_index: HashMap<K, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        last_index.insert(key_of(e), i);
    }
    let mut out = Vec::with_capacity(last_index.len());
    let mut seen_drops: HashMap<K, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let k = key_of(e);
        let last = *last_index.get(&k).expect("populated above");
        if i == last {
            out.push(e);
        } else {
            *seen_drops.entry(k.clone()).or_insert(0) += 1;
        }
    }
    for (k, dropped) in seen_drops {
        tracing::warn!(
            target: "dirge::plugin",
            kind = %kind,
            key = %k,
            dropped = dropped,
            "duplicate plugin registration — keeping last-load-wins entry",
        );
    }
    out.into_iter().cloned().collect()
}

fn parse_custom_message_line(line: &str) -> Option<CustomMessageEntry> {
    let mut parts = line.split('\t');
    let custom_type = unescape_harness_field(parts.next()?);
    let content = unescape_harness_field(parts.next()?);
    let display_raw = parts.next().unwrap_or("1").trim();
    // Empty content + empty type is meaningless — drop. (A bare
    // empty payload would just be noise in the chat.)
    if custom_type.is_empty() && content.is_empty() {
        return None;
    }
    Some(CustomMessageEntry {
        custom_type,
        content,
        display: display_raw != "0",
    })
}

/// Snapshot of a plugin-registered keyboard shortcut (P9c). The key
/// spec is the raw plugin-supplied string (e.g. `"ctrl-x"`); the UI
/// layer parses it into a `(KeyCode, KeyModifiers)` pair lazily so
/// PluginManager itself doesn't depend on crossterm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub struct PluginShortcutMeta {
    /// Raw key spec — `"ctrl-x"`, `"alt-shift-f"`, `"f5"`, `"enter"`,
    /// etc. Parsed by `extension::parse_key_spec`.
    pub keys: String,
    /// Janet function the host invokes when the key fires.
    pub handler: String,
    /// Optional human-readable description for UI listing.
    pub description: String,
}

fn parse_plugin_shortcut_line(line: &str) -> Option<PluginShortcutMeta> {
    let mut parts = line.split('\t');
    let keys = unescape_harness_field(parts.next()?);
    let handler = unescape_harness_field(parts.next()?);
    let description = unescape_harness_field(parts.next().unwrap_or(""));
    if keys.is_empty() || handler.is_empty() {
        return None;
    }
    Some(PluginShortcutMeta {
        keys,
        handler,
        description,
    })
}

/// Plugin-issued session-tree mutation. Carries the args as Strings so
/// the UI dispatch layer can resolve id-prefixes / persist the previous
/// session / etc., before applying.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum TreeOp {
    /// `harness/set-label <id> <label-or-empty>`. Empty label = clear.
    SetLabel { id: String, label: Option<String> },
    /// `harness/fork <id> <position>`. position is "before" or "at".
    Fork { id: String, restore_text: bool },
    /// `harness/navigate-tree <id>`.
    NavigateTree { id: String },
    /// `harness/new-session [parent-session]`. parent empty = no lineage.
    NewSession { parent: Option<String> },
    /// `harness/switch-session <session-id-prefix>`.
    SwitchSession { id_prefix: String },
}

fn parse_tree_op_line(line: &str) -> Option<TreeOp> {
    let mut parts = line.split('\t');
    let op = parts.next()?.trim();
    if op.is_empty() {
        return None;
    }
    let arg1 = parts.next().map(unescape_harness_field).unwrap_or_default();
    let arg2 = parts.next().map(unescape_harness_field).unwrap_or_default();
    match op {
        "set-label" => {
            if arg1.is_empty() {
                None
            } else {
                Some(TreeOp::SetLabel {
                    id: arg1,
                    label: if arg2.is_empty() { None } else { Some(arg2) },
                })
            }
        }
        "fork" => {
            if arg1.is_empty() {
                None
            } else {
                Some(TreeOp::Fork {
                    id: arg1,
                    // Plugins choosing :at opt out of editor restoration.
                    restore_text: arg2 != "at",
                })
            }
        }
        "navigate-tree" => {
            if arg1.is_empty() {
                None
            } else {
                Some(TreeOp::NavigateTree { id: arg1 })
            }
        }
        "new-session" => Some(TreeOp::NewSession {
            parent: if arg1.is_empty() { None } else { Some(arg1) },
        }),
        "switch-session" => {
            if arg1.is_empty() {
                None
            } else {
                Some(TreeOp::SwitchSession { id_prefix: arg1 })
            }
        }
        // Forward compat: a future plugin shipping an op verb we
        // don't know yet shouldn't poison the rest of the drain. Log
        // at WARN so a confused plugin author can spot the typo
        // instead of silently failing.
        other => {
            tracing::warn!(target: "dirge::plugin", op = other, "drain_tree_ops: unknown op verb (skipped)");
            None
        }
    }
}

/// Reverse the harness's tab/newline/backslash escape so the data
/// the plugin passed round-trips back through Rust unchanged.
fn unescape_harness_field(s: &str) -> String {
    // Three-byte escapes: \\, \t, \n. Process linearly.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(other) => {
                // Unknown escape — pass through literally so we never
                // silently corrupt plugin data.
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Outcome of a tool-hook dispatch. All fields are `None` when no plugin
/// set the corresponding slot. The host calls `dispatch_tool_hook` once
/// per tool boundary and interprets the result:
///
/// - `block: Some(reason)` — abort the tool call, surface `reason` to the
///   LLM as the tool error.
/// - `mutate_input: Some(json)` — re-deserialize tool args from `json`
///   before invoking the inner tool.
/// - `replace_result: Some(output)` — discard the real tool output and
///   return `output` to the LLM instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolHookResult {
    pub block: Option<String>,
    pub mutate_input: Option<String>,
    pub replace_result: Option<String>,
}
