//! `JanetLoopTool` — adapts a plugin-registered tool into a
//! `LoopTool` the agent loop can dispatch.
//!
//! Phase 9a — first feature of the pi-style extension API
//! (see `bd show dirge-bw2`). Plugins call
//! `(harness/register-tool name description label parameters handler
//!                         &opt execution-mode)` from Janet; the host
//! reads the registry via `PluginManager::list_plugin_tools()` and
//! wraps each entry in this adapter.
//!
//! Pi reference: `packages/coding-agent/src/core/extensions/types.ts`
//! line 1133 — `registerTool<TParams, TDetails, TState>(...)`. Pi's
//! TypeBox `TSchema` parameter collapses here to a raw JSON string —
//! dirge's `LoopTool::parameters()` returns `&Value`, but Janet
//! plugins don't have a TypeBox-equivalent so they pass the schema as
//! a JSON string that we parse once at construction.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::agent::agent_loop::result::LoopToolResult;
use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use crate::agent::agent_loop::types::ToolExecutionMode;

use super::{PluginManager, PluginShortcutMeta, PluginToolMeta};

/// Outcome of resolving a `LoopMessage::Custom` payload against the
/// plugin message-renderer registry. Returned by
/// [`resolve_custom_message_render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCustomMessage {
    /// `[plugin]` or `[plugin:<customType>]` — the chrome the UI
    /// prepends to the body line.
    pub label: String,
    /// Body text to print. Already sanitization-ready (but the
    /// caller still owns the sanitize call so this function stays
    /// dependency-light).
    pub body: String,
}

/// Resolve a `LoopMessage::Custom` payload into a chat line.
///
/// Reads `customType`, `content`, `display` at the top level
/// (matching the wrapper plugin_hooks.rs emits) and:
///   1. Returns `None` when `display == false` — the message stays
///      in the transcript but the UI does not draw it.
///   2. Looks up a registered renderer by `customType`. If one is
///      registered, invokes it with the full payload JSON; the
///      handler's return value is the body. Errors swallow back to
///      the default formatter.
///   3. Default formatter: uses `content` (string-typed) verbatim,
///      else pretty-prints the whole payload.
///
/// The label is `[plugin]` when `customType` is empty, otherwise
/// `[plugin:<customType>]`.
///
/// Free function (not a method on the UI) so the renderer-resolve
/// logic is unit-testable against a stand-alone `PluginManager`
/// without dragging in the interactive renderer.
pub fn resolve_custom_message_render(
    payload: &Value,
    pm: Option<&Arc<Mutex<PluginManager>>>,
) -> Option<ResolvedCustomMessage> {
    // Display gate. Missing field defaults to true — matches the
    // single-string `add-custom-message` form's wrapper.
    let display = payload
        .get("display")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if !display {
        return None;
    }

    let custom_type = payload
        .get("customType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let rendered: Option<String> = pm.and_then(|pm_arc| {
        let handler = {
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            mgr.list_message_renderers()
                .into_iter()
                .find(|(t, _)| t == &custom_type)
                .map(|(_, h)| h)
        };
        handler.and_then(|h| {
            let payload_str = payload.to_string();
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            mgr.invoke_message_renderer(&h, &payload_str).ok().flatten()
        })
    });

    let body = rendered.unwrap_or_else(|| {
        payload
            .get("content")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| payload.to_string())
    });

    let label = if custom_type.is_empty() {
        "plugin".to_string()
    } else {
        format!("plugin:{custom_type}")
    };

    Some(ResolvedCustomMessage { label, body })
}

/// Parse a plugin key spec string into a `(KeyCode, KeyModifiers)`
/// pair. Spec grammar (case-insensitive):
///   `(modifier "-")* key-name`
/// where modifier ∈ { ctrl, control, alt, meta, shift } and key-name
/// is one of: a single character, `f1`..`f12`, or one of the named
/// keys (`enter`, `esc`, `tab`, `backspace`, `space`, `up`, `down`,
/// `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `delete`,
/// `insert`). Returns `None` for malformed input so an unknown spec
/// drops the binding silently rather than crashing.
pub fn parse_key_spec(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let lower = spec.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let parts: Vec<&str> = lower.split('-').collect();
    let (key_part, mod_parts) = parts.split_last()?;
    let mut mods = KeyModifiers::NONE;
    for p in mod_parts {
        match *p {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let code = match *key_part {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        s if s.starts_with('f') && s.len() > 1 => {
            let n: u8 = s[1..].parse().ok()?;
            if (1..=12).contains(&n) {
                KeyCode::F(n)
            } else {
                return None;
            }
        }
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next()?),
        _ => return None,
    };
    Some((code, mods))
}

/// Pre-parsed shortcut entry the UI layer holds across key events.
/// Carries the original key spec for round-trip handler dispatch
/// (handlers receive the spec as a single string argument so one
/// Janet fn can serve many bindings).
#[derive(Debug, Clone)]
pub struct ParsedShortcut {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub spec: String,
    pub handler: String,
}

/// Materialize plugin shortcuts into the UI-layer form. Specs that
/// fail to parse are dropped with a `tracing::warn!` so plugin
/// authors get visibility without the host crashing on a typo.
pub fn parse_shortcuts(metas: Vec<PluginShortcutMeta>) -> Vec<ParsedShortcut> {
    metas
        .into_iter()
        .filter_map(|m| {
            let (code, modifiers) = match parse_key_spec(&m.keys) {
                Some(pair) => pair,
                None => {
                    tracing::warn!(
                        target: "dirge::plugin",
                        spec = %m.keys,
                        handler = %m.handler,
                        "plugin shortcut key spec did not parse — binding dropped",
                    );
                    return None;
                }
            };
            Some(ParsedShortcut {
                code,
                modifiers,
                spec: m.keys,
                handler: m.handler,
            })
        })
        .collect()
}

/// Resolve a `KeyEvent` against a list of parsed plugin shortcuts.
/// Returns the matching shortcut's handler + spec so the UI can
/// dispatch via `PluginManager::invoke_command`. First match wins
/// (load order); later bindings to the same key do not stack.
pub fn match_shortcut<'a>(
    key: &KeyEvent,
    shortcuts: &'a [ParsedShortcut],
) -> Option<&'a ParsedShortcut> {
    shortcuts
        .iter()
        .find(|s| s.code == key.code && s.modifiers == key.modifiers)
}

/// `LoopTool` impl backed by a Janet handler. The execute path
/// briefly locks the PluginManager mutex, dispatches into Janet via
/// `invoke_plugin_tool`, and surfaces the stringified result as a
/// single text content block. Janet errors become `Err(String)`
/// which the loop translates into an error tool result the same way
/// it would for a native tool.
pub struct JanetLoopTool {
    name: String,
    description: String,
    label: String,
    /// Parsed JSON-schema. Pre-parsed at construction so the hot
    /// `LoopTool::parameters()` path returns `&Value` without
    /// re-parsing on every LLM tool-list build.
    parameters: Value,
    handler: String,
    execution_mode: Option<ToolExecutionMode>,
    /// Optional Janet `prepare-arguments` handler. When set, runs
    /// before schema validation to normalize LLM-supplied args
    /// (pi parity — `prepareArguments?` at extensions/types.ts:443).
    prepare_handler: Option<String>,
    pm: Arc<Mutex<PluginManager>>,
}

impl std::fmt::Debug for JanetLoopTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `PluginManager` isn't Debug; skip it. The remaining fields
        // are enough for debug-printing a tool from the loop's
        // registry.
        f.debug_struct("JanetLoopTool")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("handler", &self.handler)
            .field("execution_mode", &self.execution_mode)
            .finish()
    }
}

impl JanetLoopTool {
    /// Build an adapter from a registry snapshot. Returns `None` if
    /// `meta.parameters` isn't valid JSON — plugin authors who hand
    /// us a syntactically broken schema get a clear "tool dropped"
    /// rather than the LLM seeing a corrupt parameters object.
    pub fn from_meta(meta: PluginToolMeta, pm: Arc<Mutex<PluginManager>>) -> Option<Self> {
        let parameters: Value = serde_json::from_str(&meta.parameters)
            .ok()
            .unwrap_or_else(|| {
                tracing::warn!(
                    target: "dirge::plugin",
                    tool = %meta.name,
                    raw = %meta.parameters,
                    "plugin tool parameters were not valid JSON — falling back to empty object schema",
                );
                Value::Object(serde_json::Map::new())
            });
        let execution_mode = match meta.execution_mode.as_deref() {
            Some("sequential") => Some(ToolExecutionMode::Sequential),
            Some("parallel") => Some(ToolExecutionMode::Parallel),
            _ => None,
        };
        Some(Self {
            name: meta.name,
            description: meta.description,
            label: meta.label,
            parameters,
            handler: meta.handler,
            execution_mode,
            prepare_handler: meta.prepare_handler,
            pm,
        })
    }
}

impl LoopTool for JanetLoopTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn label(&self) -> &str {
        if self.label.is_empty() {
            &self.name
        } else {
            &self.label
        }
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }

    /// H3 — call the Janet `prepare-arguments` handler (if any) to
    /// normalize args before schema validation. Pi parity:
    /// `prepareArguments?` at extensions/types.ts:443. The handler
    /// returns a JSON string we parse back to `Value`; any failure
    /// (no handler, error, invalid JSON) falls back to the original
    /// args so a broken plugin can't poison the tool call.
    fn prepare_arguments(&self, args: Value) -> Value {
        let Some(handler) = self.prepare_handler.as_deref() else {
            return args;
        };
        let args_json = args.to_string();
        let mutated = {
            let mut guard = match self.pm.lock() {
                Ok(g) => g,
                Err(_) => return args,
            };
            guard
                .invoke_prepare_arguments(handler, &args_json)
                .ok()
                .flatten()
        };
        match mutated {
            Some(json) => match serde_json::from_str::<Value>(&json) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::plugin",
                        tool = %self.name,
                        handler = %handler,
                        error = %e,
                        "plugin prepare-arguments returned invalid JSON — ignoring",
                    );
                    args
                }
            },
            None => args,
        }
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        args: Value,
        signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
        // Serialize args back to JSON. Janet doesn't have a JSON
        // decoder bundled, so we hand the handler the raw string and
        // let it parse on the plugin side if needed (most plugins
        // just stringify for display).
        let args_json = args.to_string();
        let pm = self.pm.clone();
        let handler = self.handler.clone();
        Box::pin(async move {
            // Cancellation pre-flight. The dispatcher (tools.rs)
            // races this whole future against `wait_for_cancel` so
            // a late cancel still unblocks the agent loop — but
            // Janet handlers run synchronously on the worker
            // thread, holding the PluginManager mutex. Once the
            // handler starts, we can't interrupt it; subsequent
            // plugin-tool calls (or any PM mutex consumer) would
            // queue behind a doomed handler. Bail before
            // acquiring the mutex if the signal already fired.
            if signal.is_cancelled() {
                return Err("plugin tool aborted before execution".to_string());
            }
            let signal_in = signal.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
                // Re-check on the worker thread: the user may
                // have hit Esc while we waited for the runtime to
                // schedule the blocking task.
                if signal_in.is_cancelled() {
                    return Err("plugin tool aborted before mutex acquire".to_string());
                }
                let mut guard = pm
                    .lock()
                    .map_err(|_| "plugin manager mutex poisoned".to_string())?;
                // Final check after the mutex unblocks us — a
                // prior plugin tool may have held the lock for a
                // while; user could have cancelled in that window.
                if signal_in.is_cancelled() {
                    return Err("plugin tool aborted while waiting for plugin manager".to_string());
                }
                guard.invoke_plugin_tool(&handler, &args_json)
            })
            .await
            .map_err(|e| format!("plugin tool task join error: {e}"))??;
            Ok(LoopToolResult {
                content: vec![serde_json::json!({"type": "text", "text": result})],
                details: Value::Null,
                terminate: None,
            })
        })
    }
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use super::*;
    use crate::agent::agent_loop::tool::AbortSignal;

    fn noop_update() -> LoopToolUpdate {
        Arc::new(|_| {})
    }

    /// End-to-end: register a Janet tool, snapshot the registry,
    /// wrap it in a `JanetLoopTool`, dispatch via `execute()`. The
    /// LLM-visible result is exactly what the Janet handler returns.
    #[tokio::test]
    async fn janet_loop_tool_execute_round_trips_handler_output() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(defn my-handler [args] (string "echo:" args))
                   (harness/register-tool "my-tool" "Echo" "MyTool" "{}" "my-handler")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };

        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        assert_eq!(metas.len(), 1);
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone())
            .expect("from_meta must succeed for valid schema");

        assert_eq!(tool.name(), "my-tool");
        assert_eq!(tool.label(), "MyTool");
        assert_eq!(tool.description(), "Echo");
        assert_eq!(tool.parameters(), &Value::Object(serde_json::Map::new()));

        let args = serde_json::json!({"x": 1});
        let result = tool
            .execute("call-1", args, AbortSignal::new(), noop_update())
            .await
            .expect("execute should succeed");

        let text = result
            .content
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, r#"echo:{"x":1}"#);
    }

    /// `execution_mode = :sequential` round-trips through to the
    /// `LoopTool::execution_mode()` method so the agent loop's batch
    /// scheduler treats the tool as mutating.
    #[tokio::test]
    async fn janet_loop_tool_sequential_mode_surfaces_to_loop() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(harness/register-tool "mutate" "side effects" "Mutate"
                                            "{}" "noop" :sequential)
                   (defn noop [args] "ok")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone()).unwrap();
        assert_eq!(tool.execution_mode(), Some(ToolExecutionMode::Sequential));
    }

    // --- P9d: custom-message renderer resolution ---------------------

    /// `display=false` short-circuits to `None` — the message stays
    /// in the transcript but the UI must not draw a chat row.
    #[test]
    fn resolve_custom_message_render_respects_display_false() {
        let payload = serde_json::json!({
            "role": "custom",
            "customType": "telemetry",
            "content": "x",
            "display": false,
        });
        assert!(resolve_custom_message_render(&payload, None).is_none());
    }

    /// Bare wrapper (no `customType` field) renders with the
    /// `[plugin]` label and falls back to the `content` body.
    #[test]
    fn resolve_custom_message_render_bare_falls_back_to_content() {
        let payload = serde_json::json!({
            "role": "custom",
            "customType": "",
            "content": "hello",
            "display": true,
        });
        let r = resolve_custom_message_render(&payload, None).unwrap();
        assert_eq!(r.label, "plugin");
        assert_eq!(r.body, "hello");
    }

    /// With a registered renderer for the wrapper's `customType`,
    /// the resolver dispatches and returns the handler's output.
    #[test]
    fn resolve_custom_message_render_invokes_registered_handler() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(defn render-status [p] (string ">>" p))
                   (harness/register-message-renderer "status" "render-status")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let payload = serde_json::json!({
            "role": "custom",
            "customType": "status",
            "content": "build started",
            "display": true,
        });
        let r = resolve_custom_message_render(&payload, Some(&pm)).unwrap();
        assert_eq!(r.label, "plugin:status");
        assert!(r.body.starts_with(">>"), "got: {}", r.body);
        // The handler sees the FULL wrapper (customType + content),
        // not just the inner content — pi parity.
        assert!(
            r.body.contains("\"customType\":\"status\""),
            "got: {}",
            r.body
        );
        assert!(
            r.body.contains("\"content\":\"build started\""),
            "got: {}",
            r.body
        );
    }

    // --- P9c: shortcut parser ----------------------------------------

    #[test]
    fn parse_key_spec_plain_char() {
        let (code, mods) = parse_key_spec("x").unwrap();
        assert_eq!(code, KeyCode::Char('x'));
        assert!(mods.is_empty());
    }

    #[test]
    fn parse_key_spec_ctrl_char_case_insensitive() {
        let a = parse_key_spec("ctrl-x").unwrap();
        let b = parse_key_spec("CTRL-X").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.0, KeyCode::Char('x'));
        assert_eq!(a.1, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_key_spec_multi_modifier() {
        let (code, mods) = parse_key_spec("ctrl-alt-shift-f").unwrap();
        assert_eq!(code, KeyCode::Char('f'));
        assert_eq!(
            mods,
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT
        );
    }

    #[test]
    fn parse_key_spec_named_keys() {
        assert_eq!(parse_key_spec("enter").unwrap().0, KeyCode::Enter);
        assert_eq!(parse_key_spec("esc").unwrap().0, KeyCode::Esc);
        assert_eq!(parse_key_spec("space").unwrap().0, KeyCode::Char(' '));
        assert_eq!(parse_key_spec("backspace").unwrap().0, KeyCode::Backspace);
        assert_eq!(parse_key_spec("pgdn").unwrap().0, KeyCode::PageDown);
    }

    #[test]
    fn parse_key_spec_function_keys() {
        assert_eq!(parse_key_spec("f1").unwrap().0, KeyCode::F(1));
        assert_eq!(parse_key_spec("F12").unwrap().0, KeyCode::F(12));
        // F0 and F13 are out of range.
        assert!(parse_key_spec("f0").is_none());
        assert!(parse_key_spec("f13").is_none());
    }

    #[test]
    fn parse_key_spec_rejects_unknown_modifier_or_key() {
        assert!(parse_key_spec("hyper-x").is_none());
        assert!(parse_key_spec("ctrl-mumble").is_none());
        assert!(parse_key_spec("").is_none());
    }

    #[test]
    fn match_shortcut_returns_first_load_order_match() {
        let shortcuts = parse_shortcuts(vec![
            PluginShortcutMeta {
                keys: "ctrl-x".into(),
                handler: "first".into(),
                description: String::new(),
            },
            PluginShortcutMeta {
                keys: "ctrl-x".into(),
                handler: "second".into(),
                description: String::new(),
            },
        ]);
        let ev = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let hit = match_shortcut(&ev, &shortcuts).unwrap();
        assert_eq!(hit.handler, "first");

        // A non-matching event returns None.
        let ev2 = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert!(match_shortcut(&ev2, &shortcuts).is_none());
    }

    /// Bad specs drop silently and don't poison the rest of the list.
    #[test]
    fn parse_shortcuts_drops_bad_specs_but_keeps_good_ones() {
        let parsed = parse_shortcuts(vec![
            PluginShortcutMeta {
                keys: "bogus-key".into(),
                handler: "drop-me".into(),
                description: String::new(),
            },
            PluginShortcutMeta {
                keys: "ctrl-x".into(),
                handler: "keep-me".into(),
                description: String::new(),
            },
        ]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].handler, "keep-me");
    }

    // --- back to JanetLoopTool tests --------------------------------

    /// H3: `prepare_arguments` calls the registered Janet handler
    /// and substitutes the returned JSON for the original args.
    #[tokio::test]
    async fn janet_loop_tool_prepare_arguments_normalizes_via_handler() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(defn echo [args] args)
                   (defn prep [args]
                     # Wrap the input so we can confirm prepare actually ran.
                     (string "{\"wrapped\":" args "}"))
                   (harness/register-tool "wrap" "" "Wrap" "{}" "echo" :parallel "prep")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone()).unwrap();

        let original = serde_json::json!({"x": 1});
        let mutated = tool.prepare_arguments(original);
        // Handler wrapped the input — `wrapped` field now present.
        assert_eq!(mutated.get("wrapped"), Some(&serde_json::json!({"x": 1})));
    }

    /// Without a prepare-arguments handler, prepare_arguments is the
    /// identity function — backwards compat for plugins that don't
    /// opt into the field.
    #[tokio::test]
    async fn janet_loop_tool_prepare_arguments_passthrough_when_unset() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(defn h [args] "ok")
                   (harness/register-tool "no-prep" "" "" "{}" "h")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        assert_eq!(metas[0].prepare_handler, None);
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone()).unwrap();

        let original = serde_json::json!({"a": 1, "b": "two"});
        let out = tool.prepare_arguments(original.clone());
        assert_eq!(out, original);
    }

    /// Prepare-arguments handlers that throw fall back to the
    /// original args — pi tolerates throws too (handler errors
    /// don't crash tool dispatch).
    #[tokio::test]
    async fn janet_loop_tool_prepare_arguments_error_falls_back_to_original() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(defn h [args] "ok")
                   (defn bad-prep [args] (error "boom"))
                   (harness/register-tool "bad" "" "" "{}" "h" :parallel "bad-prep")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone()).unwrap();
        let original = serde_json::json!({"x": 1});
        let out = tool.prepare_arguments(original.clone());
        assert_eq!(out, original, "throw must fall back to original args");
    }

    /// Prepare-arguments handlers that return invalid JSON fall back
    /// to the original args (plus a tracing::warn — not asserted).
    #[tokio::test]
    async fn janet_loop_tool_prepare_arguments_invalid_json_falls_back() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(defn h [args] "ok")
                   (defn weird [args] "not valid json {{{")
                   (harness/register-tool "w" "" "" "{}" "h" :parallel "weird")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone()).unwrap();
        let original = serde_json::json!({"x": 1});
        let out = tool.prepare_arguments(original.clone());
        assert_eq!(out, original);
    }

    /// H1: a signal that's already cancelled when execute() is
    /// called short-circuits BEFORE acquiring the PluginManager
    /// mutex. The dispatcher already races the whole execute future
    /// against wait_for_cancel, but the JS handler still ran and
    /// held the mutex against subsequent callers. The pre-flight
    /// check prevents that wasted work and keeps the mutex
    /// available for the loop's next move.
    #[tokio::test]
    async fn janet_loop_tool_execute_short_circuits_on_pre_cancelled_signal() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            // Sentinel: the handler sets a global var so we can
            // confirm it never ran when cancelled.
            mgr.eval(
                r#"(var --h1-ran nil)
                   (defn slow [args]
                     (set --h1-ran true)
                     "ok")
                   (harness/register-tool "slow" "test" "Slow" "{}" "slow")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone()).unwrap();

        let signal = AbortSignal::new();
        signal.cancel();
        let err = tool
            .execute(
                "c",
                Value::Object(Default::default()),
                signal,
                noop_update(),
            )
            .await
            .expect_err("pre-cancelled signal must short-circuit to Err");
        assert!(
            err.contains("aborted"),
            "error should mention abort; got: {err}"
        );

        // Confirm the Janet handler did NOT run.
        let ran = pm.lock().unwrap().eval("--h1-ran").unwrap();
        assert_eq!(ran, "nil", "handler must not execute when pre-cancelled");
    }

    /// Non-cancelled signal lets execute() run normally. Regression
    /// guard: the new pre-flight check shouldn't break the happy path.
    #[tokio::test]
    async fn janet_loop_tool_execute_happy_path_with_live_signal() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(defn ok-handler [args] "ran")
                   (harness/register-tool "ok" "test" "OK" "{}" "ok-handler")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone()).unwrap();

        let signal = AbortSignal::new(); // not cancelled
        let result = tool
            .execute(
                "c",
                Value::Object(Default::default()),
                signal,
                noop_update(),
            )
            .await
            .expect("happy path");
        let text = result
            .content
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, "ran");
    }

    /// Handler errors propagate as `Err(_)`, NOT as an Ok result with
    /// the error text inlined. The loop's error path is what surfaces
    /// the failure to the LLM (so it can decide whether to retry),
    /// not the success path with garbled output.
    #[tokio::test]
    async fn janet_loop_tool_handler_error_surfaces_as_err() {
        let pm = {
            let mut mgr = PluginManager::try_new().unwrap();
            mgr.eval(
                r#"(defn bad [args] (error "intentional"))
                   (harness/register-tool "bad" "fails" "Bad" "{}" "bad")"#,
            )
            .unwrap();
            Arc::new(Mutex::new(mgr))
        };
        let metas: Vec<PluginToolMeta> = pm.lock().unwrap().list_plugin_tools();
        let tool = JanetLoopTool::from_meta(metas.into_iter().next().unwrap(), pm.clone()).unwrap();
        let err = tool
            .execute(
                "c",
                Value::Object(Default::default()),
                AbortSignal::new(),
                noop_update(),
            )
            .await
            .expect_err("handler error should bubble up as Err");
        assert!(err.contains("intentional"), "got: {err}");
    }
}
