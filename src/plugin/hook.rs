//! Plugin-driven interception of rig tool calls.
//!
//! [`HookedToolDyn`] wraps any `Box<dyn rig::tool::ToolDyn>` and runs the
//! `on-tool-start` Janet hook before delegating to the inner tool, and the
//! `on-tool-end` hook after. Plugins can:
//!
//! - **block** the call entirely by calling `(harness/block "reason")` in
//!   `on-tool-start` — the wrapper returns a `ToolError` with that reason
//!   instead of invoking the inner tool.
//! - **mutate input** by calling `(harness/mutate-input json-string)` in
//!   `on-tool-start` — the wrapper invokes the inner tool with `json-string`
//!   as its args (re-deserialized by rig from JSON, same as a real LLM call).
//! - **replace the result** by calling `(harness/replace-result "new")` in
//!   `on-tool-end` — the LLM sees the replacement string instead of the
//!   real tool output.
//!
//! The `PluginManager` is held in a process-global `OnceLock` so individual
//! tool wrappers don't have to plumb it through. Tests construct
//! `HookedToolDyn::with_manager(inner, pm)` directly to bypass the global.

use std::sync::{Arc, Mutex, OnceLock};

use rig::tool::{ToolDyn, ToolError};

use super::{PluginManager, escape_janet_string};

#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
static PLUGIN_MANAGER: OnceLock<Arc<Mutex<PluginManager>>> = OnceLock::new();

/// Install the process-global PluginManager so [`HookedToolDyn::wrap_global`]
/// can find it. Only the first call wins (matches `OnceLock` semantics).
/// Safe to call from main before any tools execute.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub fn init_global(manager: Arc<Mutex<PluginManager>>) {
    let _ = PLUGIN_MANAGER.set(manager);
}

#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub fn global() -> Option<Arc<Mutex<PluginManager>>> {
    PLUGIN_MANAGER.get().cloned()
}

/// Wraps an inner `Box<dyn ToolDyn>` and surfaces plugin tool-hooks.
/// Cheap to construct; the actual hook dispatch happens lazily inside
/// `call()` so non-plugin builds (where `pm` is always `None`) pay only
/// one Option check per tool invocation.
pub struct HookedToolDyn {
    inner: Box<dyn ToolDyn>,
    pm: Option<Arc<Mutex<PluginManager>>>,
}

impl HookedToolDyn {
    /// Wrap `inner` and read the PluginManager from the process-global
    /// slot set by `init_global`. If no global is installed, the wrapper
    /// is a transparent passthrough.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub fn wrap_global(inner: Box<dyn ToolDyn>) -> Box<dyn ToolDyn> {
        let pm = global();
        if pm.is_none() {
            // Avoid an extra dispatch box for the no-plugin case so
            // every tool call doesn't pay a vtable hop for nothing.
            return inner;
        }
        Box::new(HookedToolDyn { inner, pm })
    }

    /// Wrap with an explicit manager, bypassing the global. Used by tests
    /// and by callers that hold their own PluginManager.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_manager(inner: Box<dyn ToolDyn>, pm: Option<Arc<Mutex<PluginManager>>>) -> Self {
        HookedToolDyn { inner, pm }
    }
}

impl ToolDyn for HookedToolDyn {
    fn name(&self) -> String {
        self.inner.name()
    }

    fn definition<'a>(
        &'a self,
        prompt: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = rig::completion::ToolDefinition> + Send + 'a>,
    > {
        self.inner.definition(prompt)
    }

    fn call<'a>(
        &'a self,
        args: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
    {
        Box::pin(async move {
            let name = self.inner.name();

            // Pre-hook: `on-tool-start`. Plugin may block or mutate input.
            // We hold the lock just long enough to run all hooks; releasing
            // before the inner tool runs lets other tools (in parallel
            // tool-mode) hit the manager too.
            let (block, mutated) = match &self.pm {
                Some(pm) => {
                    let ctx = format!(
                        "@{{:tool \"{}\" :args \"{}\"}}",
                        escape_janet_string(&name),
                        escape_janet_string(&args),
                    );
                    let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                    let result = mgr
                        .dispatch_tool_hook("on-tool-start", &ctx)
                        .unwrap_or_default();
                    (result.block, result.mutate_input)
                }
                None => (None, None),
            };

            if let Some(reason) = block {
                return Err(ToolError::ToolCallError(Box::<
                    dyn std::error::Error + Send + Sync,
                >::from(format!(
                    "blocked by plugin: {}",
                    reason
                ))));
            }

            let final_args = mutated.unwrap_or(args);

            // Run the inner tool. Plugin pre-hook ran above; even if the
            // inner returns an error we still want to fire on-tool-end so
            // plugins observing tool boundaries see the symmetric pair.
            let result = self.inner.call(final_args).await;

            // Post-hook: `on-tool-end`. Only consults `replace-result`;
            // `block`/`mutate-input` set in on-tool-end are ignored
            // (semantically meaningless past tool exec).
            let replace = match &self.pm {
                Some(pm) => {
                    let output_for_ctx = match &result {
                        Ok(s) => s.clone(),
                        Err(e) => e.to_string(),
                    };
                    let ctx = format!(
                        "@{{:tool \"{}\" :output \"{}\"}}",
                        escape_janet_string(&name),
                        escape_janet_string(&output_for_ctx),
                    );
                    let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                    mgr.dispatch_tool_hook("on-tool-end", &ctx)
                        .unwrap_or_default()
                        .replace_result
                }
                None => None,
            };

            match (result, replace) {
                // Replacement applies regardless of inner success/failure.
                // A plugin that asks to replace an error result is choosing
                // to lie to the LLM about the failure — explicit and rare.
                (_, Some(new_output)) => Ok(new_output),
                (other, None) => other,
            }
        })
    }
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use super::*;
    use rig::completion::ToolDefinition;

    /// A trivial inner tool that echoes its JSON args back as the output.
    /// Lets us assert that mutation actually changed what reached the tool.
    struct Echo;

    impl ToolDyn for Echo {
        fn name(&self) -> String {
            "echo".to_string()
        }

        fn definition<'a>(
            &'a self,
            _prompt: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolDefinition> + Send + 'a>>
        {
            Box::pin(async move {
                ToolDefinition {
                    name: "echo".to_string(),
                    description: "echo".to_string(),
                    parameters: serde_json::json!({}),
                }
            })
        }

        fn call<'a>(
            &'a self,
            args: String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>,
        > {
            Box::pin(async move { Ok(args) })
        }
    }

    fn pm() -> Arc<Mutex<PluginManager>> {
        Arc::new(Mutex::new(PluginManager::try_new().unwrap()))
    }

    async fn wrap_and_call(
        pm_arc: Arc<Mutex<PluginManager>>,
        args: &str,
    ) -> Result<String, ToolError> {
        let wrapper = HookedToolDyn::with_manager(Box::new(Echo), Some(pm_arc));
        wrapper.call(args.to_string()).await
    }

    #[tokio::test]
    async fn passthrough_when_no_hooks_registered() {
        // No plugins → wrapper is transparent (inner echo returns args).
        let result = wrap_and_call(pm(), r#"{"x":1}"#).await.unwrap();
        assert_eq!(result, r#"{"x":1}"#);
    }

    #[tokio::test]
    async fn block_returns_tool_error_with_reason() {
        let pm_arc = pm();
        {
            let mut mgr = pm_arc.lock().unwrap();
            mgr.eval(r#"(defn deny [ctx] (harness/block "danger"))"#)
                .unwrap();
            mgr.register("on-tool-start", "deny");
        }
        let err = wrap_and_call(pm_arc, r#"{"x":1}"#).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("blocked by plugin"), "got: {msg}");
        assert!(msg.contains("danger"), "got: {msg}");
    }

    #[tokio::test]
    async fn mutate_input_replaces_args_before_inner_call() {
        let pm_arc = pm();
        {
            let mut mgr = pm_arc.lock().unwrap();
            mgr.eval(r#"(defn rewrite [ctx] (harness/mutate-input "{\"x\":42}"))"#)
                .unwrap();
            mgr.register("on-tool-start", "rewrite");
        }
        // Echo returns whatever args it received. If mutate worked the
        // result will be the rewritten JSON, not the original.
        let result = wrap_and_call(pm_arc, r#"{"x":1}"#).await.unwrap();
        assert_eq!(result, r#"{"x":42}"#);
    }

    #[tokio::test]
    async fn replace_result_swaps_inner_output() {
        let pm_arc = pm();
        {
            let mut mgr = pm_arc.lock().unwrap();
            mgr.eval(r#"(defn truncate [ctx] (harness/replace-result "[filtered]"))"#)
                .unwrap();
            mgr.register("on-tool-end", "truncate");
        }
        let result = wrap_and_call(pm_arc, r#"{"x":1}"#).await.unwrap();
        assert_eq!(result, "[filtered]");
    }

    #[tokio::test]
    async fn block_precedence_over_mutate_when_both_set() {
        // Pre-hook may set both. Block should win — we abort before the
        // inner tool runs, so the mutated args are moot.
        let pm_arc = pm();
        {
            let mut mgr = pm_arc.lock().unwrap();
            mgr.eval(
                r#"(defn paranoid [ctx]
                    (harness/mutate-input "{\"x\":99}")
                    (harness/block "no way"))"#,
            )
            .unwrap();
            mgr.register("on-tool-start", "paranoid");
        }
        let err = wrap_and_call(pm_arc, r#"{"x":1}"#).await.unwrap_err();
        assert!(err.to_string().contains("no way"));
    }

    #[tokio::test]
    async fn slots_reset_between_calls() {
        // First call blocks; second call from the same plugin manager
        // must not still be blocked (the slot must be cleared per-call).
        let pm_arc = pm();
        {
            let mut mgr = pm_arc.lock().unwrap();
            mgr.eval(
                r#"(var seen 0)
                   (defn once-blocker [ctx]
                     (set seen (+ seen 1))
                     (when (= seen 1) (harness/block "first only")))"#,
            )
            .unwrap();
            mgr.register("on-tool-start", "once-blocker");
        }
        let err = wrap_and_call(pm_arc.clone(), r#"{"x":1}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("first only"));

        // Second call: hook runs but doesn't set block. Must succeed.
        let result = wrap_and_call(pm_arc, r#"{"x":2}"#).await.unwrap();
        assert_eq!(result, r#"{"x":2}"#);
    }

    /// R2: on-tool-end fires even when the inner tool returned Err.
    /// Plugins watching tool boundaries expect a symmetric start/end
    /// pair regardless of outcome. The wrapper code is structured to
    /// do this; this test pins it in place against accidental refactors
    /// that might skip post-hook on error.
    ///
    /// We assert by having on-tool-end set a sentinel via
    /// harness/replace-result — if the hook ran, the wrapper returns
    /// Ok(sentinel) instead of the inner Err.
    #[tokio::test]
    async fn on_tool_end_fires_when_inner_returns_error() {
        /// Tool that always fails. The wrapper should still call
        /// `on-tool-end` after this, allowing the hook to substitute
        /// a replacement output.
        struct AlwaysFail;
        impl ToolDyn for AlwaysFail {
            fn name(&self) -> String {
                "always_fail".to_string()
            }
            fn definition<'a>(
                &'a self,
                _prompt: String,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolDefinition> + Send + 'a>>
            {
                Box::pin(async move {
                    ToolDefinition {
                        name: "always_fail".to_string(),
                        description: "always errors".to_string(),
                        parameters: serde_json::json!({}),
                    }
                })
            }
            fn call<'a>(
                &'a self,
                _args: String,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>,
            > {
                Box::pin(async move {
                    Err(ToolError::ToolCallError(Box::<
                        dyn std::error::Error + Send + Sync,
                    >::from(
                        "deliberate failure".to_string()
                    )))
                })
            }
        }

        let pm_arc = pm();
        {
            let mut mgr = pm_arc.lock().unwrap();
            // on-tool-end installs a replacement; if it doesn't fire
            // when the inner tool errored, the wrapper would surface
            // the underlying Err instead of this sentinel.
            mgr.eval(
                r#"(defn rewrite-error [ctx]
                    (harness/replace-result "[error swallowed by plugin]"))"#,
            )
            .unwrap();
            mgr.register("on-tool-end", "rewrite-error");
        }

        let wrapper = HookedToolDyn::with_manager(Box::new(AlwaysFail), Some(pm_arc));
        let result = wrapper.call(String::new()).await;
        // Plugin's replace-result rewrites the result regardless of
        // inner success/failure, so we get Ok(replacement).
        assert_eq!(result.unwrap(), "[error swallowed by plugin]");
    }
}
