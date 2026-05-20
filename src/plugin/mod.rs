#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_manager_new() {
        let mgr = PluginManager::try_new().expect("init must succeed in test env");
        assert!(mgr.hooks.is_empty());
    }

    #[test]
    fn test_try_new_returns_ok() {
        // Construction must be fallible rather than panicking.
        assert!(PluginManager::try_new().is_ok());
    }

    #[test]
    fn test_dispatch_returns_per_hook_results() {
        // Multiple plugins registering the same hook must each contribute
        // a distinct result instead of being silently joined.
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn h1 [ctx] "from-one")"#).unwrap();
        mgr.eval(r#"(defn h2 [ctx] "from-two")"#).unwrap();
        mgr.eval(r#"(defn h-nil [ctx] nil)"#).unwrap();
        mgr.register("on-prompt", "h1");
        mgr.register("on-prompt", "h-nil");
        mgr.register("on-prompt", "h2");

        let out = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
        assert_eq!(out, vec!["from-one".to_string(), "from-two".to_string()]);

        // No hooks registered for this name -> empty vec, still Ok.
        let out = mgr.dispatch("on-error", "@{}").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_take_pending_prompt_returns_literal_nil_string() {
        // A plugin may legitimately request "nil" as a prompt. The
        // harness must distinguish Janet's nil value from a string
        // containing the characters "nil".
        let mut mgr = PluginManager::try_new().unwrap();

        // No pending -> None.
        assert_eq!(mgr.take_pending_prompt(), None);

        // Literal string "nil" must round-trip.
        mgr.eval(r#"(harness/request-prompt "nil")"#).unwrap();
        assert_eq!(mgr.take_pending_prompt(), Some("nil".to_string()));

        // After take, slot is cleared.
        assert_eq!(mgr.take_pending_prompt(), None);

        // Non-string requests are rejected by the harness.
        mgr.eval(r#"(harness/request-prompt 42)"#).unwrap();
        assert_eq!(mgr.take_pending_prompt(), None);
    }

    #[test]
    fn test_post_done_action() {
        // Plugin followup must take precedence over the loop iteration
        // so we never silently drop a queued prompt.
        let followup = Some("retry".to_string());
        assert_eq!(
            decide_post_done_action(followup.clone(), true, false),
            PostDoneAction::Followup("retry".into())
        );
        assert_eq!(
            decide_post_done_action(followup.clone(), false, false),
            PostDoneAction::Followup("retry".into())
        );
        // Loop iteration only when no followup.
        assert_eq!(
            decide_post_done_action(None, true, false),
            PostDoneAction::LoopIter
        );
        // Loop stop only when no followup and should_stop.
        assert_eq!(
            decide_post_done_action(None, true, true),
            PostDoneAction::LoopStop
        );
        // Idle: nothing to do.
        assert_eq!(
            decide_post_done_action(None, false, false),
            PostDoneAction::Idle
        );
    }

    #[test]
    fn test_poisoned_mutex_recovery_pattern() {
        // PluginManager owns a JanetClient which is !Send, so we can't
        // poison it across threads directly. Verify the recovery
        // pattern itself: `unwrap_or_else(|e| e.into_inner())` must
        // still hand us the inner value after a thread panic.
        use std::sync::{Arc, Mutex};
        let m: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("intentional poison");
        })
        .join();

        assert!(m.is_poisoned(), "thread panic must poison the mutex");
        let mut guard = m.lock().unwrap_or_else(|e| e.into_inner());
        guard.push("ok".to_string());
        assert_eq!(guard.as_slice(), &["ok".to_string()]);
    }

    #[test]
    fn test_filter_existing_dirs() {
        use std::path::PathBuf;
        let tmp = std::env::temp_dir().join(format!("dirge-plugin-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let exists = tmp.clone();
        let missing = tmp.join("does-not-exist");
        let kept = filter_existing_dirs(&[exists.clone(), missing.clone()]);
        assert_eq!(kept, vec![exists.clone()]);
        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
        // Empty input -> empty output
        let none: Vec<PathBuf> = filter_existing_dirs(&[]);
        assert!(none.is_empty());
    }

    #[test]
    fn test_register_hook() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.register("on-init", "test-init");
        assert_eq!(mgr.hooks.len(), 1);
        assert!(mgr.hooks.contains_key("on-init"));
    }

    #[test]
    fn test_register_multiple_hooks() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.register("on-init", "test-init");
        mgr.register("on-prompt", "test-prompt");
        mgr.register("on-response", "test-response");
        assert_eq!(mgr.hooks.len(), 3);
    }

    #[test]
    fn test_load_and_eval_janet() {
        let mut mgr = PluginManager::try_new().unwrap();
        let result = mgr.eval("(+ 1 2)");
        assert_eq!(result, Ok("3".to_string()));
    }

    #[test]
    fn test_load_and_eval_janet_error() {
        let mut mgr = PluginManager::try_new().unwrap();
        let result = mgr.eval("(undefined-fn 1)");
        assert!(result.is_err());
    }

    #[test]
    fn test_dispatch_hook() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval("(defn on-init [ctx] (string \"loaded with model: \" (ctx :model)))")
            .unwrap();
        mgr.register("on-init", "on-init");
        let result = mgr.dispatch("on-init", "@{:model \"gpt-4\"}").unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("loaded with model: gpt-4"));
    }

    #[test]
    fn test_harness_log() {
        let mut mgr = PluginManager::try_new().unwrap();
        let result = mgr.eval("(harness/log \"hello from plugin\")");
        assert!(result.is_ok());
    }

    #[test]
    fn test_load_file() {
        let mut mgr = PluginManager::try_new().unwrap();
        let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join("test_plugin.janet");
        mgr.load_file(&fixtures).unwrap();
        mgr.register("on-init", "on-init");
        let result = mgr.dispatch("on-init", "@{:model \"test\"}").unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("loaded with test"));
    }

    #[test]
    fn test_auto_discover_hooks() {
        let mut mgr = PluginManager::try_new().unwrap();
        let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join("test_plugin.janet");
        mgr.load_file(&fixtures).unwrap();

        // Simulate auto-discovery: check each hook and register if found.
        // Use has_symbol so missing hooks don't trigger Janet's
        // "unknown symbol" stderr noise.
        let hook_names = [
            "on-init",
            "on-prompt",
            "on-response",
            "on-tool-start",
            "on-tool-end",
            "on-error",
            "on-complete",
        ];
        let mut found = 0;
        for hook in &hook_names {
            if mgr.has_symbol(hook) {
                mgr.register(hook, hook);
                found += 1;
            }
        }
        assert_eq!(found, 3, "should find on-init, on-prompt, on-response");

        // Symbols that aren't defined must report false.
        assert!(!mgr.has_symbol("on-tool-start"));
        assert!(!mgr.has_symbol("totally-unknown-fn"));

        // on-init
        let r = mgr.dispatch("on-init", "@{:model \"test\"}").unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("loaded with test"));

        // on-prompt with matching text
        assert_eq!(
            mgr.dispatch("on-prompt", "@{:prompt \"hello world\"}")
                .unwrap(),
            vec!["greeting detected".to_string()]
        );

        // on-prompt with non-matching text (hook returns nil -> empty Vec)
        assert!(
            mgr.dispatch("on-prompt", "@{:prompt \"goodbye\"}")
                .unwrap()
                .is_empty()
        );

        // on-response with matching text
        assert_eq!(
            mgr.dispatch("on-response", "@{:response \"error: panic\"}")
                .unwrap(),
            vec!["error in response".to_string()]
        );

        // unknown hook returns empty
        assert!(
            mgr.dispatch("on-tool-start", "@{:tool \"bash\"}")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_janet_escaping() {
        let mut mgr = PluginManager::try_new().unwrap();

        // Define a test function
        mgr.eval(r#"(defn test-echo [ctx] (ctx :msg))"#).unwrap();
        mgr.register("on-prompt", "test-echo");

        // Quotes in text
        assert_eq!(
            mgr.dispatch("on-prompt", "@{:msg \"he said \\\"hello\\\"\"}")
                .unwrap(),
            vec!["he said \"hello\"".to_string()]
        );

        // Backslashes in text
        assert_eq!(
            mgr.dispatch("on-prompt", "@{:msg \"path\\\\to\\\\file\"}")
                .unwrap(),
            vec!["path\\to\\file".to_string()]
        );

        // Newlines in text
        assert_eq!(
            mgr.dispatch("on-prompt", "@{:msg \"line1\\nline2\"}")
                .unwrap(),
            vec!["line1\nline2".to_string()]
        );
    }

    #[test]
    fn test_escape_janet_string() {
        assert_eq!(escape_janet_string("simple"), "simple");
        assert_eq!(escape_janet_string("a\"b"), "a\\\"b");
        assert_eq!(escape_janet_string("a\\b"), "a\\\\b");
        assert_eq!(escape_janet_string("a\nb\tc\rd"), "a\\nb\\tc\\rd");
        // control char -> hex escape
        assert_eq!(escape_janet_string("a\x01b"), "a\\x01b");
    }

    #[test]
    fn test_dispatch_swallows_runtime_errors() {
        // A misbehaving plugin should not crash dispatch or pollute output.
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn broken [ctx] (string/find "x" nil))"#)
            .unwrap();
        mgr.register("on-prompt", "broken");
        let result = mgr.dispatch("on-prompt", "@{:prompt \"hi\"}").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_dispatch_with_json_args_as_string() {
        // Tool args arrive as JSON; the harness escapes them into a
        // Janet string so the parser never has to handle {":", ","}.
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn capture [ctx] (ctx :args))"#).unwrap();
        mgr.register("on-tool-start", "capture");
        let args_json = r#"{"path": "/tmp/x", "n": null, "xs": [1, 2, 3]}"#;
        let ctx = format!(
            "@{{:tool \"Bash\" :args \"{}\"}}",
            escape_janet_string(args_json)
        );
        let result = mgr.dispatch("on-tool-start", &ctx).unwrap();
        assert_eq!(result, vec![args_json.to_string()]);
    }

    #[test]
    fn test_has_symbol() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval("(defn my-hook [ctx] :ok)").unwrap();
        assert!(mgr.has_symbol("my-hook"));
        assert!(!mgr.has_symbol("nope-not-here"));
        // weird names with hyphens/quotes shouldn't crash
        assert!(!mgr.has_symbol("a\"b-c"));
    }

    #[test]
    fn test_janet_phase_tracking() {
        let mut mgr = PluginManager::try_new().unwrap();

        // Define test functions that use harness APIs
        mgr.eval(
            r#"
            (var test-phase :idle)
            (defn test-on-init [ctx]
              (harness/log "phase test loaded")
              nil)
            (defn test-on-prompt [ctx]
              (case test-phase
                :idle (do (set test-phase :active) "entered active")
                :active (do (set test-phase :done) "entered done")
                nil))
        "#,
        )
        .unwrap();

        mgr.register("on-init", "test-on-init");
        mgr.register("on-prompt", "test-on-prompt");

        // on-init should work
        let result = mgr.dispatch("on-init", "@{}");
        assert!(result.is_ok());

        // First prompt: idle -> active
        assert_eq!(
            mgr.dispatch("on-prompt", "@{:prompt \"any\"}").unwrap(),
            vec!["entered active".to_string()]
        );

        // Second prompt: active -> done
        assert_eq!(
            mgr.dispatch("on-prompt", "@{:prompt \"any\"}").unwrap(),
            vec!["entered done".to_string()]
        );

        // Third prompt: done -> nil -> empty
        assert!(
            mgr.dispatch("on-prompt", "@{:prompt \"any\"}")
                .unwrap()
                .is_empty()
        );
    }

    // --- Phase 1: tool-hook return-value slots --------------------------
    //
    // These all exercise Janet evaluation, so they're gated to the
    // `plugin` feature. (The pre-existing test module mixes gated and
    // non-gated tests; new ones gate explicitly.)

    /// `harness/block` sets a string slot the host reads after dispatch.
    /// Take consumes the value, leaving the slot None for the next call.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_take_pending_block_roundtrips() {
        let mut mgr = PluginManager::try_new().unwrap();
        // Initially empty.
        assert_eq!(mgr.take_pending_block(), None);

        mgr.eval(r#"(harness/block "rm -rf is not allowed")"#)
            .unwrap();
        assert_eq!(
            mgr.take_pending_block(),
            Some("rm -rf is not allowed".to_string())
        );
        // Drained.
        assert_eq!(mgr.take_pending_block(), None);
    }

    /// `harness/mutate-input` carries a JSON string the host will use to
    /// re-deserialize the next tool's args.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_take_pending_mutate_input_roundtrips() {
        let mut mgr = PluginManager::try_new().unwrap();
        assert_eq!(mgr.take_pending_mutate_input(), None);

        mgr.eval(r#"(harness/mutate-input "{\"path\":\"/safe\"}")"#)
            .unwrap();
        assert_eq!(
            mgr.take_pending_mutate_input(),
            Some("{\"path\":\"/safe\"}".to_string())
        );
        assert_eq!(mgr.take_pending_mutate_input(), None);
    }

    /// `harness/replace-result` swaps the next tool's output string.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_take_pending_replace_result_roundtrips() {
        let mut mgr = PluginManager::try_new().unwrap();
        assert_eq!(mgr.take_pending_replace_result(), None);

        mgr.eval(r#"(harness/replace-result "filtered output")"#)
            .unwrap();
        assert_eq!(
            mgr.take_pending_replace_result(),
            Some("filtered output".to_string())
        );
        assert_eq!(mgr.take_pending_replace_result(), None);
    }

    /// `dispatch_tool_hook` resets slots before running so previous-call
    /// state doesn't leak into the current tool's decision.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_dispatch_tool_hook_clears_slots_before_running() {
        let mut mgr = PluginManager::try_new().unwrap();
        // Pre-populate as if a stale hook left junk.
        mgr.eval(r#"(harness/block "stale") (harness/replace-result "stale")"#)
            .unwrap();

        // A hook that doesn't touch any slot.
        mgr.eval(r#"(defn passthrough [ctx] nil)"#).unwrap();
        mgr.register("on-tool-start", "passthrough");

        let result = mgr
            .dispatch_tool_hook("on-tool-start", "@{:tool \"x\"}")
            .unwrap();
        assert_eq!(result.block, None);
        assert_eq!(result.mutate_input, None);
        assert_eq!(result.replace_result, None);
    }

    /// A hook that calls (harness/block "...") surfaces via the
    /// combined dispatch_tool_hook result.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_dispatch_tool_hook_captures_block() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn deny [ctx] (harness/block "denied by policy"))"#)
            .unwrap();
        mgr.register("on-tool-start", "deny");

        let result = mgr
            .dispatch_tool_hook("on-tool-start", "@{:tool \"bash\"}")
            .unwrap();
        assert_eq!(result.block, Some("denied by policy".to_string()));
    }

    /// A hook that calls (harness/mutate-input json) is exposed via
    /// dispatch_tool_hook so the host can re-deserialize args.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_dispatch_tool_hook_captures_mutate_input() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn rewrite [ctx] (harness/mutate-input "{\"command\":\"echo safe\"}"))"#)
            .unwrap();
        mgr.register("on-tool-start", "rewrite");

        let result = mgr
            .dispatch_tool_hook("on-tool-start", "@{:tool \"bash\"}")
            .unwrap();
        assert_eq!(
            result.mutate_input,
            Some("{\"command\":\"echo safe\"}".to_string())
        );
    }

    /// `on-tool-end` hooks can replace the tool's textual output.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_dispatch_tool_hook_captures_replace_result() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn truncate [ctx] (harness/replace-result "[truncated]"))"#)
            .unwrap();
        mgr.register("on-tool-end", "truncate");

        let result = mgr
            .dispatch_tool_hook("on-tool-end", "@{:tool \"read\"}")
            .unwrap();
        assert_eq!(result.replace_result, Some("[truncated]".to_string()));
    }

    /// Block-precedence: when multiple hooks register, any one calling
    /// `harness/block` wins. Hooks after the blocker still run (we don't
    /// short-circuit at the Janet level for simplicity) but the block flag
    /// stays set.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_dispatch_tool_hook_block_sticks_across_hooks() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn block-it [ctx] (harness/block "no"))"#)
            .unwrap();
        mgr.eval(r#"(defn noop [ctx] nil)"#).unwrap();
        mgr.register("on-tool-start", "block-it");
        mgr.register("on-tool-start", "noop");

        let result = mgr.dispatch_tool_hook("on-tool-start", "@{}").unwrap();
        assert_eq!(result.block, Some("no".to_string()));
    }

    // --- Phase 3: harness/notify ----------------------------------------

    /// A single notify writes one entry the host can drain.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_notify_writes_one_entry() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/notify "hello" :info)"#).unwrap();
        let pending = mgr.drain_notifications();
        assert_eq!(pending, vec![("info".to_string(), "hello".to_string())]);
        // Drained.
        assert!(mgr.drain_notifications().is_empty());
    }

    /// Multiple notifies queue in call order.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_notify_preserves_order() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/notify "first" :info)"#).unwrap();
        mgr.eval(r#"(harness/notify "second" :warn)"#).unwrap();
        mgr.eval(r#"(harness/notify "third" :error)"#).unwrap();
        let pending = mgr.drain_notifications();
        assert_eq!(
            pending,
            vec![
                ("info".to_string(), "first".to_string()),
                ("warn".to_string(), "second".to_string()),
                ("error".to_string(), "third".to_string()),
            ]
        );
    }

    /// Level defaults to "info" when omitted.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_notify_default_level_is_info() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/notify "no level given")"#).unwrap();
        let pending = mgr.drain_notifications();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "info");
    }

    /// Non-string msg silently drops instead of crashing the plugin.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_notify_ignores_non_string_msg() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/notify 42 :warn)"#).unwrap();
        assert!(mgr.drain_notifications().is_empty());
    }

    /// Unknown level falls back to "info" so plugins typo-ing the level
    /// keyword still see their messages.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_notify_unknown_level_falls_back_to_info() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/notify "msg" :weird)"#).unwrap();
        let pending = mgr.drain_notifications();
        assert_eq!(pending, vec![("info".to_string(), "msg".to_string())]);
    }

    // --- Phase 2: plugin-registered slash commands ----------------------

    /// `harness/register-command` records a (cmd-name, handler-fn) pair
    /// readable by the host via `list_commands`.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_register_command_records_pair() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/register-command "hello" "say-hello")"#)
            .unwrap();
        let cmds = mgr.list_commands();
        assert_eq!(cmds, vec![("hello".to_string(), "say-hello".to_string())]);
    }

    /// Multiple registrations all surface; order matches load order.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_register_multiple_commands() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/register-command "alpha" "fn-a")"#)
            .unwrap();
        mgr.eval(r#"(harness/register-command "beta" "fn-b")"#)
            .unwrap();
        let cmds = mgr.list_commands();
        assert_eq!(cmds.len(), 2);
        assert!(cmds.contains(&("alpha".to_string(), "fn-a".to_string())));
        assert!(cmds.contains(&("beta".to_string(), "fn-b".to_string())));
    }

    /// Non-string args silently drop the registration instead of crashing.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_register_command_ignores_non_string_args() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/register-command 42 "ok")"#).unwrap();
        mgr.eval(r#"(harness/register-command "name" 99)"#).unwrap();
        assert_eq!(mgr.list_commands().len(), 0);
    }

    /// Invoking a registered handler runs the Janet fn with the args
    /// string and returns its output as `Some(text)`. nil/empty becomes
    /// `None` so the slash UI knows there was no message to display.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_invoke_command_runs_handler_and_returns_output() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn greet [args] (string "hello " args))"#)
            .unwrap();
        let r = mgr.invoke_command("greet", "world").unwrap();
        assert_eq!(r, Some("hello world".to_string()));
    }

    /// Handler returning nil → None so the UI doesn't print "nil".
    #[cfg(feature = "plugin")]
    #[test]
    fn test_invoke_command_returns_none_for_nil_handler_output() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn quiet [args] nil)"#).unwrap();
        let r = mgr.invoke_command("quiet", "anything").unwrap();
        assert_eq!(r, None);
    }

    /// Unknown handler doesn't crash dispatch; returns None so the slash
    /// UI can fall through to its "unknown command" path.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_invoke_unknown_handler_returns_none() {
        let mut mgr = PluginManager::try_new().unwrap();
        let r = mgr.invoke_command("nonexistent-fn", "args").unwrap();
        assert_eq!(r, None);
    }

    /// Args with special characters round-trip through the escape pipeline.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_invoke_command_passes_escaped_args() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn echo [args] args)"#).unwrap();
        // Quotes, newlines, and backslashes in the args string.
        let r = mgr
            .invoke_command("echo", "he said \"hi\"\nline 2 \\ x")
            .unwrap();
        assert_eq!(r, Some("he said \"hi\"\nline 2 \\ x".to_string()));
    }
}

use std::collections::HashMap;

#[cfg(feature = "plugin")]
use janetrs::client::{Error as JanetError, JanetClient};

pub mod hook;

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
    candidates.iter().filter(|p| p.is_dir()).cloned().collect()
}

pub struct PluginManager {
    hooks: HashMap<String, Vec<String>>,
    #[cfg(feature = "plugin")]
    client: JanetClient,
}

// SAFETY: dirge uses `#[tokio::main(flavor = "current_thread")]` so every
// PluginManager access happens on the same OS thread. Janet's per-thread
// global state is therefore stable for the lifetime of the process.
//
// rig's ToolDyn trait requires Send+Sync on futures returned by `call`,
// which transitively requires the wrapper around PluginManager to be
// Sync. Without this impl HookedToolDyn cannot be constructed.
//
// If dirge ever switches to `#[tokio::main]` (multi-thread runtime) or
// otherwise lets tools execute on a different OS thread, this impl
// becomes unsound — replace it with a dedicated Janet worker thread
// and a message channel.
#[cfg(feature = "plugin")]
unsafe impl Send for PluginManager {}
#[cfg(feature = "plugin")]
unsafe impl Sync for PluginManager {}

#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
impl PluginManager {
    /// Initialize a Janet VM and the harness API. Returns Err if Janet
    /// init fails (e.g. already initialized on this thread) so the host
    /// can fall back instead of panicking.
    pub fn try_new() -> Result<Self, String> {
        #[cfg(feature = "plugin")]
        let client = {
            let c = JanetClient::init_with_default_env()
                .map_err(|e| format!("Failed to initialize Janet VM: {e}"))?;

            // Define harness API functions in Janet
            let _ = c.run(
                r#"
                (var harness-pending nil)
                (var harness-response nil)
                # Per-tool-hook slots: cleared by the host at the start of
                # dispatch_tool_hook so previous-call state doesn't leak.
                (var harness-block nil)
                (var harness-mutate-input nil)
                (var harness-replace-result nil)

                (defn harness/log [msg] (print "[plugin] " msg))
                (defn harness/get-cwd [] (os/cwd))
                (defn harness/request-prompt [prompt]
                  (when (string? prompt)
                    (set harness-pending prompt)))
                (defn harness/store-response [resp]
                  (set harness-response resp))
                (defn harness/has-symbol? [name]
                  (truthy? (get (curenv) (symbol name))))

                # Tool-hook slots. Plugins call these from inside
                # on-tool-start / on-tool-end. The host reads them via
                # dispatch_tool_hook on the Rust side.
                (defn harness/block [reason]
                  (when (string? reason) (set harness-block reason)))
                (defn harness/mutate-input [json-str]
                  (when (string? json-str) (set harness-mutate-input json-str)))
                (defn harness/replace-result [output]
                  (when (string? output) (set harness-replace-result output)))

                # Slash-command registry. Plugins register at load time;
                # the host reads the list once after all plugins load and
                # dispatches matching /cmd input back to the named handler.
                # Stored as a `name|handler\n` blob to keep the read side
                # easy (single Janet -> Rust string roundtrip).
                (var harness-cmd-list "")
                (defn harness/register-command [name handler]
                  (when (and (string? name) (string? handler))
                    (set harness-cmd-list
                         (string harness-cmd-list name "|" handler "\n"))))

                # Notification queue. Plugins call (harness/notify msg level?)
                # to push a line into the host's chat display. Stored as a
                # `level\tmsg\n` blob; the host's drain_notifications
                # splits and clears in one round-trip.
                (var harness-notif-list "")
                (defn harness/notify [msg &opt level]
                  (when (string? msg)
                    (let [lvl (cond
                                (or (= level :info) (= level "info")) "info"
                                (or (= level :warn) (= level "warn")) "warn"
                                (or (= level :error) (= level "error")) "error"
                                "info")]
                      (set harness-notif-list
                           (string harness-notif-list lvl "\t" msg "\n")))))
            "#,
            );

            c
        };

        Ok(PluginManager {
            hooks: HashMap::new(),
            #[cfg(feature = "plugin")]
            client,
        })
    }

    #[cfg(feature = "plugin")]
    pub fn load_file(&mut self, path: &std::path::Path) -> Result<(), String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read plugin: {e}"))?;
        self.eval(&content)?;
        Ok(())
    }

    #[cfg(not(feature = "plugin"))]
    pub fn load_file(&mut self, _path: &std::path::Path) -> Result<(), String> {
        Ok(())
    }

    pub fn register(&mut self, hook: &str, script: &str) {
        self.hooks
            .entry(hook.to_string())
            .or_default()
            .push(script.to_string());
    }

    #[cfg(feature = "plugin")]
    pub fn take_pending_prompt(&mut self) -> Option<String> {
        // Stringify on the Janet side so we can disambiguate Janet's
        // nil value from a string with the characters "nil". Probe the
        // type first; only fetch the value if it really is a string.
        let is_string = self
            .client
            .run("(if (string? harness-pending) true false)")
            .map(|v| v.to_string() == "true")
            .unwrap_or(false);
        if !is_string {
            return None;
        }
        let val = match self.client.run("harness-pending") {
            Ok(v) => v,
            Err(_) => return None,
        };
        let s = val.to_string();
        let _ = self.client.run("(set harness-pending nil)");
        Some(s)
    }

    #[cfg(not(feature = "plugin"))]
    pub fn take_pending_prompt(&mut self) -> Option<String> {
        None
    }

    #[cfg(feature = "plugin")]
    pub fn store_response(&mut self, response: &str) {
        let escaped = escape_janet_string(response);
        let _ = self
            .client
            .run(&format!(r#"(set harness-response "{}")"#, escaped));
    }

    #[cfg(not(feature = "plugin"))]
    pub fn store_response(&mut self, _response: &str) {}

    /// Check whether a top-level symbol is bound in the Janet env
    /// without triggering Janet's compile-error stderr output.
    #[cfg(feature = "plugin")]
    pub fn has_symbol(&mut self, name: &str) -> bool {
        let escaped = escape_janet_string(name);
        let code = format!(r#"(harness/has-symbol? "{}")"#, escaped);
        match self.client.run(&code) {
            Ok(val) => val.to_string() == "true",
            Err(_) => false,
        }
    }

    #[cfg(not(feature = "plugin"))]
    pub fn has_symbol(&mut self, _name: &str) -> bool {
        false
    }

    #[cfg(feature = "plugin")]
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        self.client
            .run(code)
            .map(|val| val.to_string())
            .map_err(|e: JanetError| format!("Janet error: {e}"))
    }

    #[cfg(not(feature = "plugin"))]
    pub fn eval(&mut self, _code: &str) -> Result<String, String> {
        Err("plugin feature not enabled".to_string())
    }

    #[cfg(feature = "plugin")]
    pub fn dispatch(&mut self, hook: &str, context_janet: &str) -> Result<Vec<String>, String> {
        let names = match self.hooks.get(hook) {
            Some(names) => names.clone(),
            None => return Ok(Vec::new()),
        };

        let mut results = Vec::new();
        for name in &names {
            // Wrap the call in (try ... ([err] nil)) so plugin runtime
            // errors don't print Janet stack traces to stderr.
            let code = format!(
                r#"(try (do (def ctx {ctx}) ({fname} ctx)) ([err fib] nil))"#,
                ctx = context_janet,
                fname = name,
            );
            if let Ok(result) = self.eval(&code) {
                let s = result.to_string();
                // Janet nil -> skip
                if s != "nil" && !s.is_empty() {
                    results.push(s);
                }
            }
        }

        Ok(results)
    }

    #[cfg(not(feature = "plugin"))]
    pub fn dispatch(&mut self, _hook: &str, _context_janet: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// Read and clear the `harness-block` slot. Returns the reason a plugin
    /// gave when calling `(harness/block "...")` from inside a tool hook,
    /// or `None` if no plugin set it.
    #[cfg(feature = "plugin")]
    pub fn take_pending_block(&mut self) -> Option<String> {
        self.take_string_slot("harness-block")
    }

    #[cfg(not(feature = "plugin"))]
    pub fn take_pending_block(&mut self) -> Option<String> {
        None
    }

    /// Read and clear the `harness-mutate-input` slot. The returned string,
    /// when present, is a JSON encoding of the new tool args that the host
    /// should re-deserialize before invoking the tool.
    #[cfg(feature = "plugin")]
    pub fn take_pending_mutate_input(&mut self) -> Option<String> {
        self.take_string_slot("harness-mutate-input")
    }

    #[cfg(not(feature = "plugin"))]
    pub fn take_pending_mutate_input(&mut self) -> Option<String> {
        None
    }

    /// Read and clear the `harness-replace-result` slot. The returned
    /// string, when present, is the tool output the LLM should see instead
    /// of the real one.
    #[cfg(feature = "plugin")]
    pub fn take_pending_replace_result(&mut self) -> Option<String> {
        self.take_string_slot("harness-replace-result")
    }

    #[cfg(not(feature = "plugin"))]
    pub fn take_pending_replace_result(&mut self) -> Option<String> {
        None
    }

    /// Shared body of the three `take_pending_*` functions: probe the type
    /// to disambiguate Janet's nil from a string with the characters "nil",
    /// fetch the value if it's a string, then clear the slot.
    #[cfg(feature = "plugin")]
    fn take_string_slot(&mut self, var: &str) -> Option<String> {
        let is_string = self
            .client
            .run(format!("(if (string? {var}) true false)"))
            .map(|v| v.to_string() == "true")
            .unwrap_or(false);
        if !is_string {
            return None;
        }
        let val = self.client.run(var).ok()?;
        let _ = self.client.run(format!("(set {var} nil)"));
        Some(val.to_string())
    }

    /// Specialized dispatcher for tool-hook events (`on-tool-start`,
    /// `on-tool-end`). Clears all tool-hook slots first so previous-call
    /// state doesn't leak, runs every registered hook, then collects the
    /// slot values into a structured result.
    #[cfg(feature = "plugin")]
    pub fn dispatch_tool_hook(
        &mut self,
        hook: &str,
        context_janet: &str,
    ) -> Result<ToolHookResult, String> {
        // Pre-clear so a stale (harness/block ...) left by an unrelated
        // hook can't cause us to mis-block this tool.
        let _ = self
            .client
            .run("(set harness-block nil) (set harness-mutate-input nil) (set harness-replace-result nil)");

        let _ = self.dispatch(hook, context_janet)?;

        Ok(ToolHookResult {
            block: self.take_pending_block(),
            mutate_input: self.take_pending_mutate_input(),
            replace_result: self.take_pending_replace_result(),
        })
    }

    #[cfg(not(feature = "plugin"))]
    pub fn dispatch_tool_hook(
        &mut self,
        _hook: &str,
        _context_janet: &str,
    ) -> Result<ToolHookResult, String> {
        Ok(ToolHookResult::default())
    }

    /// Snapshot the plugin-registered slash commands as `(cmd-name,
    /// handler-fn-name)` pairs in load order. Read once after all plugins
    /// finish loading; subsequent registrations require a reload to take
    /// effect (kept simple for now — Phase 5 will add hot-reload).
    #[cfg(feature = "plugin")]
    pub fn list_commands(&mut self) -> Vec<(String, String)> {
        // harness-cmd-list is a `name|handler\n` blob populated by the
        // (harness/register-command ...) calls in plugin scripts. Janet
        // stringifies strings without quotes, so the raw read is parseable
        // as-is — no escaping concerns because plugins only ever pass
        // alphanumeric command/handler names through here.
        let raw = match self.client.run("harness-cmd-list") {
            Ok(v) => v.to_string(),
            Err(_) => return Vec::new(),
        };
        raw.lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, '|');
                let cmd = parts.next()?.trim();
                let handler = parts.next()?.trim();
                if cmd.is_empty() || handler.is_empty() {
                    None
                } else {
                    Some((cmd.to_string(), handler.to_string()))
                }
            })
            .collect()
    }

    #[cfg(not(feature = "plugin"))]
    pub fn list_commands(&mut self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Invoke a registered handler fn by name with the user-provided args
    /// string (everything after the command name). Returns `Ok(Some(text))`
    /// when the handler produced a non-nil string, `Ok(None)` when it
    /// returned nil/empty or when the handler raised inside Janet. The
    /// caller-visible error path is reserved for catastrophic Janet
    /// failures (VM dead, etc.) — handler-level errors are swallowed so a
    /// broken plugin doesn't tear down the slash dispatch.
    #[cfg(feature = "plugin")]
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
        let code = format!(
            r#"(try
                 (let [f (get (curenv) (symbol "{fname}"))]
                   (if (and f (function? (f :value)))
                     ((f :value) "{args}")
                     nil))
                 ([err fib] nil))"#,
            fname = escaped_fn,
            args = escaped_args,
        );
        let result = self.eval(&code)?;
        if result == "nil" || result.is_empty() {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }

    #[cfg(not(feature = "plugin"))]
    pub fn invoke_command(
        &mut self,
        _handler_fn: &str,
        _args: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// Drain pending `(harness/notify ...)` entries as `(level, msg)`
    /// pairs in insertion order. The UI calls this each loop tick and
    /// renders entries as colored chat lines. Returns an empty Vec when
    /// no plugin has posted anything.
    #[cfg(feature = "plugin")]
    pub fn drain_notifications(&mut self) -> Vec<(String, String)> {
        let raw = match self.client.run("harness-notif-list") {
            Ok(v) => v.to_string(),
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
        // Atomic-ish clear: blank the slot after read so the next tick
        // starts fresh. A plugin could race a write between our read and
        // clear, but on a single-threaded runtime that can't happen since
        // PluginManager methods serialize through the lock.
        let _ = self.client.run(r#"(set harness-notif-list "")"#);
        parsed
    }

    #[cfg(not(feature = "plugin"))]
    pub fn drain_notifications(&mut self) -> Vec<(String, String)> {
        Vec::new()
    }
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
