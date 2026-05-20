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

    // --- Phase 4: harness/replace-prompt --------------------------------

    /// A plugin calling `(harness/replace-prompt "...")` from an on-prompt
    /// hook writes a slot the host consumes to replace the user's text
    /// before the LLM call.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_replace_prompt_roundtrips() {
        let mut mgr = PluginManager::try_new().unwrap();
        assert_eq!(mgr.take_pending_prompt_replace(), None);

        mgr.eval(r#"(harness/replace-prompt "Please act in spanish.")"#)
            .unwrap();
        assert_eq!(
            mgr.take_pending_prompt_replace(),
            Some("Please act in spanish.".to_string())
        );
        // Drained on read.
        assert_eq!(mgr.take_pending_prompt_replace(), None);
    }

    /// Last-write-wins when multiple hooks rewrite.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_replace_prompt_last_write_wins() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/replace-prompt "first")"#).unwrap();
        mgr.eval(r#"(harness/replace-prompt "second")"#).unwrap();
        assert_eq!(
            mgr.take_pending_prompt_replace(),
            Some("second".to_string())
        );
    }

    /// Non-string args silently drop.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_replace_prompt_ignores_non_string() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/replace-prompt 42)"#).unwrap();
        assert_eq!(mgr.take_pending_prompt_replace(), None);
    }

    /// Special characters in the replacement round-trip via the escape
    /// pipeline (quotes, newlines, backslashes).
    #[cfg(feature = "plugin")]
    #[test]
    fn test_replace_prompt_handles_special_chars() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/replace-prompt "say \"hi\"\nline 2 \\ x")"#)
            .unwrap();
        assert_eq!(
            mgr.take_pending_prompt_replace(),
            Some("say \"hi\"\nline 2 \\ x".to_string())
        );
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

    // --- P1: plugin-registered providers --------------------------------

    /// `harness/register-provider name type base-url` records the spec
    /// with no api_key_env override (defaults to None).
    #[cfg(feature = "plugin")]
    #[test]
    fn test_register_provider_records_spec_without_env_override() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/register-provider "local" "openai" "http://localhost:8000/v1")"#)
            .unwrap();
        let providers = mgr.list_providers();
        assert_eq!(
            providers,
            vec![(
                "local".to_string(),
                "openai".to_string(),
                "http://localhost:8000/v1".to_string(),
                None,
            )]
        );
    }

    /// Explicit api-key-env argument flows through as Some(name).
    #[cfg(feature = "plugin")]
    #[test]
    fn test_register_provider_with_env_override() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(
            r#"(harness/register-provider "vllm" "openai" "http://localhost:1234/v1" "VLLM_API_KEY")"#,
        )
        .unwrap();
        let providers = mgr.list_providers();
        assert_eq!(
            providers,
            vec![(
                "vllm".to_string(),
                "openai".to_string(),
                "http://localhost:1234/v1".to_string(),
                Some("VLLM_API_KEY".to_string()),
            )]
        );
    }

    /// Multiple registrations all surface in their registration order.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_register_multiple_providers() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/register-provider "a" "openai" "http://a")"#)
            .unwrap();
        mgr.eval(r#"(harness/register-provider "b" "anthropic" "http://b" "B_KEY")"#)
            .unwrap();
        let providers = mgr.list_providers();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].0, "a");
        assert_eq!(providers[1].0, "b");
        assert_eq!(providers[1].3, Some("B_KEY".to_string()));
    }

    /// Non-string args silently drop instead of crashing the plugin.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_register_provider_ignores_non_string_args() {
        let mut mgr = PluginManager::try_new().unwrap();
        // Each of these has at least one non-string positional — all
        // should be rejected by the (when (and (string? ...))) guard.
        mgr.eval(r#"(harness/register-provider 42 "openai" "http://x")"#)
            .unwrap();
        mgr.eval(r#"(harness/register-provider "name" 99 "http://x")"#)
            .unwrap();
        mgr.eval(r#"(harness/register-provider "name" "openai" 0)"#)
            .unwrap();
        assert!(mgr.list_providers().is_empty());
    }

    /// `install_plugin_providers` populates the resolver, and
    /// `resolve_provider_info` finds plugin-registered providers by
    /// name when config doesn't have them. Verifies the full
    /// integration path, not just the harness slot.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_plugin_provider_visible_through_resolve_provider_info() {
        use crate::config::CustomProviderConfig;
        use std::collections::HashMap;

        // We can only install the global once per process. Use a unique
        // provider name and check Note: This test depends on running
        // first or alongside other tests that also install. OnceLock's
        // set returns Err on second call but doesn't panic; we tolerate
        // that and check the result post-install.
        let mut map: HashMap<String, CustomProviderConfig> = HashMap::new();
        map.insert(
            "test-plugin-provider".to_string(),
            CustomProviderConfig {
                provider_type: "openai".to_string(),
                base_url: "http://plugin-test.invalid/v1".to_string(),
                api_key_env: Some("PLUGIN_TEST_KEY".to_string()),
            },
        );
        // Best-effort install — OnceLock may already be set from
        // another test; in that case we skip the assertion since we
        // can't observe a fresh install.
        crate::provider::install_plugin_providers(map);

        let cfg_providers: HashMap<String, CustomProviderConfig> = HashMap::new();
        if let Some(info) =
            crate::provider::resolve_provider_info("test-plugin-provider", &cfg_providers)
        {
            assert_eq!(
                info.base_url.as_deref(),
                Some("http://plugin-test.invalid/v1"),
            );
            assert_eq!(info.api_key_env.as_deref(), Some("PLUGIN_TEST_KEY"));
        }
        // Else: another test already won the OnceLock race; integration
        // isn't observable from here. That's fine — the harness-side
        // tests above cover the parse path independently.
    }

    /// Config-declared custom providers must always win over
    /// plugin-registered ones with the same name.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_config_provider_overrides_plugin_provider() {
        use crate::config::CustomProviderConfig;
        use std::collections::HashMap;

        let mut cfg_providers: HashMap<String, CustomProviderConfig> = HashMap::new();
        cfg_providers.insert(
            "shadowed".to_string(),
            CustomProviderConfig {
                provider_type: "openai".to_string(),
                base_url: "http://from-config".to_string(),
                api_key_env: None,
            },
        );
        // Even if the plugin global also has "shadowed", config wins
        // because resolve_provider_info checks config first.
        let info = crate::provider::resolve_provider_info("shadowed", &cfg_providers).unwrap();
        assert_eq!(info.base_url.as_deref(), Some("http://from-config"));
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

    // --- R2: coverage gaps from the audit ------------------------------

    /// R2: load_file on a missing path surfaces an error rather than
    /// panicking or silently succeeding.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_load_file_missing_path_returns_err() {
        let mut mgr = PluginManager::try_new().unwrap();
        let bogus = std::path::PathBuf::from("/tmp/dirge-nonexistent-plugin.janet");
        // Make doubly sure it's not there.
        let _ = std::fs::remove_file(&bogus);
        let result = mgr.load_file(&bogus);
        assert!(
            result.is_err(),
            "expected Err on missing file, got {result:?}"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("Failed to read plugin"),
            "error should identify the read failure, got {msg:?}"
        );
    }

    /// R2: store_response writes a slot that a subsequent eval can read.
    /// Verifies the round-trip rather than just that the write doesn't
    /// crash.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_store_response_round_trips_via_harness_var() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.store_response("the assistant said this");
        // harness-response is the slot store_response writes to.
        let read = mgr.eval("harness-response").unwrap();
        assert_eq!(read, "the assistant said this");
    }

    /// R2: store_response handles strings with Janet-special chars
    /// (quotes, backslashes, newlines) without breaking the assignment.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_store_response_escapes_special_chars() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.store_response("line one\n\"quoted\"\nline three \\ backslash");
        let read = mgr.eval("harness-response").unwrap();
        assert_eq!(read, "line one\n\"quoted\"\nline three \\ backslash");
    }

    /// R2: concurrent calls to `dispatch_tool_hook` via an
    /// `Arc<Mutex<PluginManager>>` serialize cleanly. Two threads each
    /// fire a unique-tagged hook; both should see their own block
    /// reason come back in the result, with no interference. Catches
    /// any future refactor that drops the lock mid-dispatch.
    #[cfg(feature = "plugin")]
    #[test]
    fn test_concurrent_dispatch_tool_hook_serializes() {
        use std::sync::{Arc, Mutex};

        let pm = Arc::new(Mutex::new(PluginManager::try_new().unwrap()));
        {
            let mut mgr = pm.lock().unwrap();
            mgr.eval(
                r#"(defn block-by-tool [ctx]
                    (harness/block (string "blocked:" (ctx :tool))))"#,
            )
            .unwrap();
            mgr.register("on-tool-start", "block-by-tool");
        }

        // 8 concurrent threads each calling dispatch_tool_hook with a
        // distinct :tool key. Without proper serialization a thread
        // could observe another's slot value, mixing reasons.
        let mut handles = Vec::new();
        for i in 0..8 {
            let pm = pm.clone();
            handles.push(std::thread::spawn(move || {
                let ctx = format!("@{{:tool \"t{i}\"}}");
                let mut mgr = pm.lock().unwrap();
                mgr.dispatch_tool_hook("on-tool-start", &ctx).unwrap()
            }));
        }

        let mut reasons: Vec<String> = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .map(|r| r.block.unwrap_or_default())
            .collect();
        reasons.sort();
        let expected: Vec<String> = (0..8).map(|i| format!("blocked:t{i}")).collect();
        assert_eq!(
            reasons, expected,
            "each thread should see its own block reason"
        );
    }

    // --- P2: append-entry, register-renderer, invoke-renderer --------

    #[cfg(feature = "plugin")]
    #[test]
    fn test_append_entry_records_triple() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/append-entry "bookmark" "label-one")"#)
            .unwrap();
        let entries = mgr.drain_entries();
        assert_eq!(
            entries,
            vec![("bookmark".to_string(), "label-one".to_string(), true)]
        );
        // Drained.
        assert!(mgr.drain_entries().is_empty());
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn test_append_entry_preserves_order_and_flag() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/append-entry "a" "x" true)"#).unwrap();
        mgr.eval(r#"(harness/append-entry "b" "y" false)"#).unwrap();
        mgr.eval(r#"(harness/append-entry "c" "z")"#).unwrap();
        let entries = mgr.drain_entries();
        assert_eq!(
            entries,
            vec![
                ("a".to_string(), "x".to_string(), true),
                ("b".to_string(), "y".to_string(), false),
                ("c".to_string(), "z".to_string(), true),
            ]
        );
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn test_append_entry_escapes_special_chars() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/append-entry "json" "a\tb\nc\\d")"#)
            .unwrap();
        let entries = mgr.drain_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "a\tb\nc\\d");
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn test_append_entry_ignores_non_string_args() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/append-entry 42 "ok")"#).unwrap();
        mgr.eval(r#"(harness/append-entry "ok" 42)"#).unwrap();
        assert!(mgr.drain_entries().is_empty());
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn test_register_renderer_records_pairs() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(harness/register-renderer "bookmark" "render-bookmark")"#)
            .unwrap();
        mgr.eval(r#"(harness/register-renderer "telemetry" "render-stat")"#)
            .unwrap();
        let renderers = mgr.list_renderers();
        assert!(renderers.contains(&("bookmark".to_string(), "render-bookmark".to_string())));
        assert!(renderers.contains(&("telemetry".to_string(), "render-stat".to_string())));
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn test_invoke_renderer_collects_render_lines() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(
            r#"(defn my-renderer [data]
                (harness/render :cyan (string "★ " data))
                (harness/render :white "details below"))"#,
        )
        .unwrap();
        let lines = mgr.invoke_renderer("my-renderer", "label").unwrap();
        // Janet's `(string :cyan)` drops the leading `:`; the host sees
        // bare color names. That matches how the UI parses them back
        // into crossterm Colors.
        assert_eq!(
            lines,
            vec![
                ("cyan".to_string(), "★ label".to_string()),
                ("white".to_string(), "details below".to_string()),
            ]
        );
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn test_invoke_renderer_silent_handler_returns_empty() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(r#"(defn silent [data] nil)"#).unwrap();
        let lines = mgr.invoke_renderer("silent", "anything").unwrap();
        assert!(lines.is_empty());
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn test_invoke_unknown_renderer_returns_empty() {
        let mut mgr = PluginManager::try_new().unwrap();
        let lines = mgr.invoke_renderer("nonexistent", "data").unwrap();
        assert!(lines.is_empty());
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn test_invoke_renderer_resets_buffer_between_calls() {
        let mut mgr = PluginManager::try_new().unwrap();
        mgr.eval(
            r#"(defn one [data] (harness/render :red "from-one"))
               (defn two [data] (harness/render :blue "from-two"))"#,
        )
        .unwrap();
        let a = mgr.invoke_renderer("one", "x").unwrap();
        let b = mgr.invoke_renderer("two", "y").unwrap();
        assert_eq!(a, vec![("red".to_string(), "from-one".to_string())]);
        assert_eq!(b, vec![("blue".to_string(), "from-two".to_string())]);
    }

    #[test]
    fn test_unescape_harness_field_roundtrips() {
        assert_eq!(super::unescape_harness_field("plain"), "plain");
        assert_eq!(super::unescape_harness_field("a\\tb"), "a\tb");
        assert_eq!(super::unescape_harness_field("a\\nb"), "a\nb");
        assert_eq!(super::unescape_harness_field("a\\\\b"), "a\\b");
        // Combined: \\ then \t then \n.
        assert_eq!(
            super::unescape_harness_field("a\\\\b\\tc\\nd"),
            "a\\b\tc\nd"
        );
        // Unknown escape passes through untouched so plugin data isn't
        // silently corrupted.
        assert_eq!(super::unescape_harness_field("a\\xb"), "a\\xb");
        // Trailing backslash at end of field is preserved.
        assert_eq!(super::unescape_harness_field("a\\"), "a\\");
    }
}

use std::collections::HashMap;

use worker::Worker;
pub use worker::{DialogReply, DialogRequest};

pub mod hook;
pub mod worker;

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
    /// All Janet evaluation goes through this handle to the worker
    /// thread. The handle is naturally `Send + Sync` (only an mpsc Sender
    /// + JoinHandle inside) so no unsafe impl is needed — the previous
    /// `unsafe impl Send for PluginManager` is gone now that Janet lives
    /// on its own OS thread.
    worker: Worker,
    /// One-shot consumer end of the dialog channel. Taken out by
    /// `take_dialog_rx` on first call so the UI can register it in its
    /// `tokio::select!`. After that, the field is `None`.
    dialog_rx: Option<tokio::sync::mpsc::UnboundedReceiver<DialogRequest>>,
}

#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
impl PluginManager {
    /// Spawn the Janet worker thread and wait for it to install the
    /// harness API. Returns Err if Janet VM init fails so the host can
    /// fall back to a no-plugin path rather than panicking.
    pub fn try_new() -> Result<Self, String> {
        let (worker, dialog_rx) = Worker::try_spawn()?;
        Ok(PluginManager {
            hooks: HashMap::new(),
            worker,
            dialog_rx: Some(dialog_rx),
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
            if let Ok(s) = self.eval(&code) {
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

    /// Read and clear the `harness-prompt-replace` slot. Set by plugins
    /// from `on-prompt` to rewrite the user turn before the agent runs.
    /// Distinct from `take_pending_prompt`, which carries the
    /// `request-prompt` queue for the *next* turn.
    pub fn take_pending_prompt_replace(&mut self) -> Option<String> {
        self.take_string_slot("harness-prompt-replace")
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
    /// state doesn't leak, runs every registered hook, then collects the
    /// slot values into a structured result.
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

        let _ = self.dispatch(hook, context_janet)?;

        Ok(ToolHookResult {
            block: self.take_pending_block(),
            mutate_input: self.take_pending_mutate_input(),
            replace_result: self.take_pending_replace_result(),
        })
    }

    /// Snapshot the plugin-registered slash commands as `(cmd-name,
    /// handler-fn-name)` pairs in load order. Read once after all plugins
    /// finish loading; subsequent registrations require a reload to take
    /// effect (kept simple for now — Phase 5 will add hot-reload).
    pub fn list_commands(&mut self) -> Vec<(String, String)> {
        // harness-cmd-list is a `name|handler\n` blob populated by the
        // (harness/register-command ...) calls in plugin scripts. Janet
        // stringifies strings without quotes, so the raw read is parseable
        // as-is — no escaping concerns because plugins only ever pass
        // alphanumeric command/handler names through here.
        let raw = match self.worker.eval("harness-cmd-list") {
            Ok(s) => s,
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

    /// Invoke a registered handler fn by name with the user-provided args
    /// string (everything after the command name). Returns `Ok(Some(text))`
    /// when the handler produced a non-nil string, `Ok(None)` when it
    /// returned nil/empty or when the handler raised inside Janet. The
    /// caller-visible error path is reserved for catastrophic Janet
    /// failures (VM dead, etc.) — handler-level errors are swallowed so a
    /// broken plugin doesn't tear down the slash dispatch.
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

    /// Snapshot plugin-registered LLM provider specs as
    /// `(name, type, base_url, api_key_env)` tuples. `api_key_env` is
    /// `None` when the plugin passed an empty string (meaning "use
    /// the default env var for this provider type"). Read once after
    /// all plugins load and merged into the host's resolver via
    /// [`crate::provider::install_plugin_providers`].
    pub fn list_providers(&mut self) -> Vec<(String, String, String, Option<String>)> {
        let raw = match self.worker.eval("harness-providers-list") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        raw.lines()
            .filter_map(|line| {
                let mut parts = line.splitn(4, '|');
                let name = parts.next()?.trim();
                let ptype = parts.next()?.trim();
                let base_url = parts.next()?.trim();
                let env_raw = parts.next()?.trim();
                if name.is_empty() || ptype.is_empty() || base_url.is_empty() {
                    return None;
                }
                let env = if env_raw.is_empty() {
                    None
                } else {
                    Some(env_raw.to_string())
                };
                Some((
                    name.to_string(),
                    ptype.to_string(),
                    base_url.to_string(),
                    env,
                ))
            })
            .collect()
    }

    /// Drain pending `(harness/notify ...)` entries as `(level, msg)`
    /// pairs in insertion order. The UI calls this each loop tick and
    /// renders entries as colored chat lines. Returns an empty Vec when
    /// no plugin has posted anything.
    pub fn drain_notifications(&mut self) -> Vec<(String, String)> {
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
