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

/// Dropping an idle worker must complete promptly (well under
/// the `JOIN_TIMEOUT` upper bound). This is the regression guard
/// for the bounded-join change in `Worker::Drop` — without the
/// poll loop the change would have introduced a fixed 2s delay
/// on every shutdown.
#[test]
fn worker_drop_completes_promptly_when_idle() {
    let mgr = PluginManager::try_new().unwrap();
    let start = std::time::Instant::now();
    drop(mgr);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "idle Drop should be near-instant; took {:?}",
        elapsed,
    );
}

/// Sanity-check that `eval` still returns the worker's reply
/// after the switch from `recv()` to `recv_timeout(EVAL_TIMEOUT)`.
/// Without this, a typo in the new match arms could mask the
/// happy-path break by always returning the timeout error.
#[test]
fn worker_eval_still_returns_reply_after_recv_timeout_switch() {
    let mut mgr = PluginManager::try_new().unwrap();
    let out = mgr.eval("(+ 1 2)").unwrap();
    assert_eq!(out, "3", "got: {out:?}");
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

/// First-blocker-wins precedence (Phase 1, matches pi's
/// `runner.ts:806-827` `tool_call` semantics). When multiple
/// hooks register and one calls `harness/block`, the FIRST
/// blocker wins and dispatch stops — subsequent hooks do NOT
/// run. Previously last-write-wins, which made the block
/// reason depend on plugin load order and ran observers after
/// a deny that the user might want to skip on perf grounds.
#[cfg(feature = "plugin")]
#[test]
fn dispatch_tool_hook_first_blocker_stops_dispatch() {
    let mut mgr = PluginManager::try_new().unwrap();
    // Plugin A blocks with reason "first". Plugin B would also
    // block with reason "second" AND fire a notification to
    // prove it ran. After the fix, B never runs.
    mgr.eval(r#"(defn first-block [ctx] (harness/block "first"))"#)
        .unwrap();
    mgr.eval(
        r#"(defn second-block [ctx]
                 (harness/notify "second-also-ran" :warn)
                 (harness/block "second"))"#,
    )
    .unwrap();
    mgr.register("on-tool-start", "first-block");
    mgr.register("on-tool-start", "second-block");

    let result = mgr.dispatch_tool_hook("on-tool-start", "@{}").unwrap();
    assert_eq!(
        result.block,
        Some("first".to_string()),
        "first blocker's reason must win",
    );
    let pending = mgr.drain_notifications();
    assert!(
        !pending.iter().any(|(_, m)| m.contains("second-also-ran")),
        "second hook should not have run after first blocked: {:?}",
        pending,
    );
}

/// When no hook blocks, all hooks run. Confirms the early-stop
/// only triggers on an actual `harness/block` call.
#[cfg(feature = "plugin")]
#[test]
fn dispatch_tool_hook_runs_all_when_no_block() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn a [ctx] (harness/notify "a-ran"))"#)
        .unwrap();
    mgr.eval(r#"(defn b [ctx] (harness/notify "b-ran"))"#)
        .unwrap();
    mgr.register("on-tool-start", "a");
    mgr.register("on-tool-start", "b");

    let _ = mgr.dispatch_tool_hook("on-tool-start", "@{}").unwrap();
    let pending = mgr.drain_notifications();
    let combined: String = pending
        .iter()
        .map(|(_, m)| m.clone())
        .collect::<Vec<_>>()
        .join("|");
    assert!(combined.contains("a-ran"), "got: {combined}");
    assert!(combined.contains("b-ran"), "got: {combined}");
}

/// Mutations (mutate-input, replace-result) keep last-write-wins
/// semantics — only `harness/block` is first-wins. This matches
/// pi's `runner.ts:858-888` chaining: each handler sees the
/// prior's mutation and can override. Confirms the Phase 1
/// change to block didn't accidentally also short-circuit on
/// mutation slots.
#[cfg(feature = "plugin")]
#[test]
fn dispatch_tool_hook_mutations_still_chain_last_wins() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn rewrite-a [ctx] (harness/replace-result "from-a"))"#)
        .unwrap();
    mgr.eval(r#"(defn rewrite-b [ctx] (harness/replace-result "from-b"))"#)
        .unwrap();
    mgr.register("on-tool-end", "rewrite-a");
    mgr.register("on-tool-end", "rewrite-b");

    let result = mgr.dispatch_tool_hook("on-tool-end", "@{}").unwrap();
    assert_eq!(
        result.replace_result,
        Some("from-b".to_string()),
        "last-write-wins for mutations",
    );
    // No block fired — block field stays None.
    assert_eq!(result.block, None);
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

// --- P9a: plugin-registered LLM tools -------------------------------

/// `harness/register-tool` with the minimum positional args records
/// the spec with no execution-mode override.
#[cfg(feature = "plugin")]
#[test]
fn test_register_tool_records_spec_default_mode() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(
        r#"(harness/register-tool "echo" "Echo args back" "Echo"
                                       "{\"type\":\"object\"}" "echo-handler")"#,
    )
    .unwrap();
    let tools = mgr.list_plugin_tools();
    assert_eq!(tools.len(), 1);
    let t = &tools[0];
    assert_eq!(t.name, "echo");
    assert_eq!(t.description, "Echo args back");
    assert_eq!(t.label, "Echo");
    assert_eq!(t.parameters, "{\"type\":\"object\"}");
    assert_eq!(t.handler, "echo-handler");
    assert_eq!(t.execution_mode, None);
}

/// `:sequential` keyword maps to the execution_mode override.
#[cfg(feature = "plugin")]
#[test]
fn test_register_tool_sequential_mode() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-tool "mutate" "Side effects" "Mutate" "{}" "h" :sequential)"#)
        .unwrap();
    let tools = mgr.list_plugin_tools();
    assert_eq!(tools[0].execution_mode.as_deref(), Some("sequential"));
}

/// Non-string positional args drop the registration silently so a
/// typo can't crash the plugin host.
#[cfg(feature = "plugin")]
#[test]
fn test_register_tool_ignores_non_string_args() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-tool 1 "d" "l" "{}" "h")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "n" 2 "l" "{}" "h")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "n" "d" "l" {} "h")"#)
        .unwrap();
    assert!(mgr.list_plugin_tools().is_empty());
}

/// Multiple registrations surface in insertion order. Parameters
/// with embedded tabs/newlines round-trip correctly through
/// `harness/-escape` / `unescape_harness_field`.
#[cfg(feature = "plugin")]
#[test]
fn test_register_multiple_tools_round_trip_escapes() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-tool "a" "first" "A" "{}" "ha")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "b" "with\ttab\nand newline" "B" "{\"x\":1}" "hb")"#)
        .unwrap();
    let tools = mgr.list_plugin_tools();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "a");
    assert_eq!(tools[1].name, "b");
    assert_eq!(tools[1].description, "with\ttab\nand newline");
}

/// `invoke_plugin_tool` dispatches to the named Janet handler and
/// returns its stringified output. The handler sees the raw JSON
/// args string so it can parse/inspect them at its discretion.
#[cfg(feature = "plugin")]
#[test]
fn test_invoke_plugin_tool_dispatches_to_handler() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn echo-handler [args] (string "got:" args))"#)
        .unwrap();
    let out = mgr
        .invoke_plugin_tool("echo-handler", r#"{"x":1}"#, "test-tc-1")
        .unwrap();
    assert_eq!(out, r#"got:{"x":1}"#);
}

/// Handler exceptions bubble up as `Err(message)` rather than
/// crashing the worker.
#[cfg(feature = "plugin")]
#[test]
fn test_invoke_plugin_tool_propagates_handler_error() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn boom-handler [args] (error "kaboom"))"#)
        .unwrap();
    let err = mgr
        .invoke_plugin_tool("boom-handler", "{}", "test-tc-2")
        .unwrap_err();
    assert!(err.contains("kaboom"), "got: {err}");
}

// --- P9e: end-to-end load of example plugins -----------------------

/// Smoke test (P9e): load the three example plugins shipped under
/// `plugins/example_*.janet` and verify each phase-9 registry
/// surfaces them. This is the integration guard against
/// silently breaking the documented plugin contract — if a
/// future refactor changes the wire format, this test fails
/// before the docs do.
#[cfg(feature = "plugin")]
#[test]
fn phase9_example_plugins_load_end_to_end() {
    let mut mgr = PluginManager::try_new().unwrap();
    let plugins_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins");

    mgr.load_file(&plugins_dir.join("example_tool.janet"))
        .unwrap();
    mgr.load_file(&plugins_dir.join("example_shortcut.janet"))
        .unwrap();
    mgr.load_file(&plugins_dir.join("example_message_renderer.janet"))
        .unwrap();

    // 9a — registered tool surfaces in the registry with the
    // documented metadata.
    let tools = mgr.list_plugin_tools();
    assert_eq!(
        tools.len(),
        1,
        "example_tool.janet registers exactly one tool"
    );
    assert_eq!(tools[0].name, "plugin_echo");
    assert_eq!(tools[0].label, "Plugin Echo");
    assert_eq!(tools[0].handler, "echo-tool-handler");

    // Tool dispatch round-trips: the LLM-supplied args reach
    // the Janet handler intact.
    let out = mgr
        .invoke_plugin_tool("echo-tool-handler", r#"{"msg":"hi"}"#, "test-tc")
        .unwrap();
    assert_eq!(out, r#"echo received args: {"msg":"hi"}"#);

    // 9c — example_shortcut.janet registers two bindings.
    let shortcuts = mgr.list_shortcuts();
    assert_eq!(shortcuts.len(), 2);
    let specs: Vec<_> = shortcuts.iter().map(|s| s.keys.as_str()).collect();
    assert!(specs.contains(&"f5"));
    assert!(specs.contains(&"ctrl-s"));

    // 9d — message renderer registered for "status".
    let renderers = mgr.list_message_renderers();
    assert_eq!(
        renderers,
        vec![("status".to_string(), "render-status".to_string())]
    );

    // C1/C2 end-to-end: the plugin's prepare-next-run hook
    // pushes a typed custom message; the drain produces an
    // entry with customType="status"; the bridge-equivalent
    // wrapper resolves through the registered renderer with
    // the FULL wrapper payload (NOT just the inner content).
    // This is the path that was broken before C1 — the smoke
    // test now walks it explicitly.
    mgr.eval(r#"(harness/add-custom-message "status" "build done")"#)
        .unwrap();
    let drained = mgr.drain_custom_messages();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].custom_type, "status");
    assert_eq!(drained[0].content, "build done");

    // Build the wrapper exactly as plugin_hooks.rs does and
    // resolve through the renderer-resolver. Without C1's
    // top-level customType this returned the default fallback.
    let wrapper = serde_json::json!({
        "role": "custom",
        "customType": drained[0].custom_type,
        "content": drained[0].content,
        "display": drained[0].display,
    });
    let pm_arc = std::sync::Arc::new(std::sync::Mutex::new(mgr));
    let resolved = crate::plugin::extension::resolve_custom_message_render(&wrapper, Some(&pm_arc))
        .expect("display=true must resolve to Some");
    assert_eq!(resolved.label, "plugin:status");
    // Renderer's output is "■ status from plugin: <full wrapper>" —
    // contains both the type and content fields proving the
    // renderer saw the structured payload.
    assert!(
        resolved.body.contains("\"customType\":\"status\""),
        "renderer must receive the full wrapper; got: {}",
        resolved.body,
    );
    assert!(
        resolved.body.contains("build done"),
        "renderer output must include content; got: {}",
        resolved.body,
    );
}

/// The bundled `backpressured` plugin loads end-to-end, registers its
/// three commands, stays off without the keyword, and engages on it —
/// injecting the loop discipline into the system prompt.
#[cfg(feature = "plugin")]
#[test]
fn backpressured_plugin_loads_and_engages() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/backpressured");

    // Off without the keyword: no system-prompt injection. Load via the
    // directory loader (shares the env AND aliases the bare hooks so
    // dispatch finds them) — the same path dirge uses at startup.
    {
        let mut mgr = PluginManager::try_new().unwrap();
        super::load_plugin(&mut mgr, &dir).unwrap();
        assert_eq!(mgr.list_commands().len(), 3, "registers three commands");
        mgr.dispatch("on-prompt", "@{:prompt \"just a normal request\"}")
            .unwrap();
        mgr.dispatch("before-agent-start", "@{}").unwrap();
        assert!(
            mgr.take_system_prompt_append().is_none(),
            "stays off without the backpressure keyword",
        );
    }

    // Engaged by the keyword: discipline injected.
    {
        let mut mgr = PluginManager::try_new().unwrap();
        super::load_plugin(&mut mgr, &dir).unwrap();
        mgr.dispatch("on-prompt", "@{:prompt \"do this backpressured\"}")
            .unwrap();
        mgr.dispatch("before-agent-start", "@{}").unwrap();
        let sp = mgr
            .take_system_prompt_append()
            .expect("engaged → injects a system prompt");
        assert!(sp.contains("backpressured loop"), "discipline present");
        assert!(
            sp.contains("Independent reviewer"),
            "reviewer section present"
        );
    }

    // dirge-99ic: `auto_start` engages the loop at load time — no keyword.
    {
        let mut mgr = PluginManager::try_new().unwrap();
        // Host injects this plugin's config.json settings before loading.
        mgr.set_loading_plugin_config(/* enabled */ true, /* auto_start */ true);
        super::load_plugin(&mut mgr, &dir).unwrap();
        mgr.clear_loading_plugin_config();

        // No on-prompt keyword — engaged purely by auto_start.
        mgr.dispatch("before-agent-start", "@{}").unwrap();
        let sp = mgr
            .take_system_prompt_append()
            .expect("auto_start → discipline injected without the keyword");
        assert!(sp.contains("backpressured loop"), "discipline present");
    }
}

// --- 9b: register-command + register-provider wire alignment -------

/// Duplicate command name resolves last-wins (matches H4 semantics
/// for the phase-9 registries). Plugin authors using the reload
/// pattern now get the same predictable behavior across all
/// register-* APIs.
#[cfg(feature = "plugin")]
#[test]
fn list_commands_dedups_last_wins() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-command "echo" "h1")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-command "echo" "h2")"#)
        .unwrap();
    let cmds = mgr.list_commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0], ("echo".to_string(), "h2".to_string()));
}

/// Command names with characters that would have broken the old
/// pipe-separated wire format (embedded `|`) now round-trip
/// through harness/-escape just like the other registries.
#[cfg(feature = "plugin")]
#[test]
fn list_commands_round_trips_special_chars() {
    let mut mgr = PluginManager::try_new().unwrap();
    // Tabs and newlines escape; `|` is fine now too.
    mgr.eval(r#"(harness/register-command "cmd|with|pipes" "h")"#)
        .unwrap();
    let cmds = mgr.list_commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].0, "cmd|with|pipes");
}

/// Duplicate provider name resolves last-wins.
#[cfg(feature = "plugin")]
#[test]
fn list_providers_dedups_last_wins() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-provider "local" "openai" "http://a")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-provider "local" "anthropic" "http://b" "API_KEY")"#)
        .unwrap();
    let providers = mgr.list_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].0, "local");
    assert_eq!(providers[0].1, "anthropic");
    assert_eq!(providers[0].2, "http://b");
    assert_eq!(providers[0].3, Some("API_KEY".to_string()));
}

/// Provider base-urls with `|` in query params (previously
/// would have corrupted the pipe-separated parser) now
/// round-trip cleanly.
#[cfg(feature = "plugin")]
#[test]
fn list_providers_round_trips_pipe_in_url() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-provider "p" "openai" "http://x?a=1|b=2")"#)
        .unwrap();
    let providers = mgr.list_providers();
    assert_eq!(providers[0].2, "http://x?a=1|b=2");
}

// --- L5: tool-name charset validation ------------------------------

/// Tool names with spaces, dots, slashes, etc. drop with a
/// tracing::warn instead of reaching the LLM provider.
#[cfg(feature = "plugin")]
#[test]
fn list_plugin_tools_drops_invalid_name_chars() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-tool "good_name-1" "" "" "{}" "h")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "bad name" "" "" "{}" "h")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "with.dot" "" "" "{}" "h")"#)
        .unwrap();
    let tools = mgr.list_plugin_tools();
    assert_eq!(tools.len(), 1, "only the valid-charset name survives");
    assert_eq!(tools[0].name, "good_name-1");
}

// --- H2: tool_call_id slot + emit-tool-progress queue --------------

/// Inside an `invoke_plugin_tool` call, `harness/current-tool-call`
/// is set to the tool_call_id the host passed. After the call
/// returns, the slot resets to nil so a subsequent handler
/// observing nil knows no plugin tool is active.
#[cfg(feature = "plugin")]
#[test]
fn invoke_plugin_tool_sets_current_tool_call_slot() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(
        r#"(var --observed-tcid nil)
               (defn capturer [args]
                 (set --observed-tcid harness-current-tool-call)
                 "ok")"#,
    )
    .unwrap();
    mgr.invoke_plugin_tool("capturer", "{}", "tc-7").unwrap();
    // During the call the slot held "tc-7".
    let observed = mgr.eval("--observed-tcid").unwrap();
    assert_eq!(observed, "tc-7");
    // After the call the slot is cleared.
    let post = mgr.eval("harness-current-tool-call").unwrap();
    assert_eq!(post, "nil");
}

/// Even when the handler errors, the current-tool-call slot
/// resets to nil. Otherwise a stale id would leak into the next
/// invocation's progress events.
#[cfg(feature = "plugin")]
#[test]
fn invoke_plugin_tool_clears_slot_after_handler_error() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn bad [args] (error "kaboom"))"#).unwrap();
    let _ = mgr.invoke_plugin_tool("bad", "{}", "tc-9");
    let post = mgr.eval("harness-current-tool-call").unwrap();
    assert_eq!(post, "nil");
}

/// `harness/emit-tool-progress` tags entries with the current
/// tool-call id; drain returns them in order. Calls made OUTSIDE
/// a tool invocation (current-tool-call nil) are silently dropped.
#[cfg(feature = "plugin")]
#[test]
fn emit_tool_progress_tags_entries_with_current_tool_call() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(
        r#"(defn worker [args]
                 (harness/emit-tool-progress "step 1")
                 (harness/emit-tool-progress "step 2")
                 "done")"#,
    )
    .unwrap();
    // Pre-invocation calls have nil current-tool-call → no-op.
    mgr.eval(r#"(harness/emit-tool-progress "dropped")"#)
        .unwrap();

    mgr.invoke_plugin_tool("worker", "{}", "tc-prog").unwrap();
    let drained = mgr.drain_tool_progress();
    assert_eq!(drained.len(), 2, "got: {drained:?}");
    assert_eq!(drained[0], ("tc-prog".to_string(), "step 1".to_string()));
    assert_eq!(drained[1], ("tc-prog".to_string(), "step 2".to_string()));

    // Drain clears the queue.
    assert!(mgr.drain_tool_progress().is_empty());
}

// --- H3: register-tool prepare-arguments field ---------------------

/// `(harness/register-tool ... :parallel "my-prep")` records the
/// 7th positional and surfaces it in `PluginToolMeta::prepare_handler`.
#[cfg(feature = "plugin")]
#[test]
fn test_register_tool_records_prepare_handler() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-tool "t" "d" "L" "{}" "h" :parallel "my-prep")"#)
        .unwrap();
    let tools = mgr.list_plugin_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].prepare_handler.as_deref(), Some("my-prep"));
}

/// Omitting the 7th positional leaves `prepare_handler == None`.
/// Backwards compat: existing 5- and 6-positional callers
/// continue to work.
#[cfg(feature = "plugin")]
#[test]
fn test_register_tool_without_prepare_handler_leaves_field_none() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-tool "t" "d" "L" "{}" "h" :parallel)"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "u" "d" "L" "{}" "h2")"#)
        .unwrap();
    let tools = mgr.list_plugin_tools();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].prepare_handler, None);
    assert_eq!(tools[1].prepare_handler, None);
}

/// `invoke_prepare_arguments` round-trips: Janet handler returns
/// a mutated JSON string; the host gets `Ok(Some(json))`.
#[cfg(feature = "plugin")]
#[test]
fn test_invoke_prepare_arguments_returns_handler_output() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn prep [args] (string "{\"normalized\":" args "}"))"#)
        .unwrap();
    let out = mgr.invoke_prepare_arguments("prep", r#"{"x":1}"#).unwrap();
    assert_eq!(out, Some(r#"{"normalized":{"x":1}}"#.to_string()));
}

/// Handler errors swallow to `Ok(None)` — caller falls back to
/// the original args.
#[cfg(feature = "plugin")]
#[test]
fn test_invoke_prepare_arguments_swallows_handler_error() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn boom [args] (error "kaboom"))"#).unwrap();
    let out = mgr.invoke_prepare_arguments("boom", "{}").unwrap();
    assert_eq!(out, None);
}

/// Non-string return values swallow to `Ok(None)` — the
/// contract is "JSON string back", anything else is treated as
/// no-op.
#[cfg(feature = "plugin")]
#[test]
fn test_invoke_prepare_arguments_non_string_return_swallows() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn t [args] @{:not "a-string"})"#).unwrap();
    let out = mgr.invoke_prepare_arguments("t", "{}").unwrap();
    assert_eq!(out, None);
}

// --- H4: dedup duplicate registrations -----------------------------

/// Registering two tools with the same name resolves last-wins —
/// matches pi's `Map.set` semantics. The dropped entry triggers
/// a `tracing::warn` (not asserted here; just confirm only the
/// last survives).
#[cfg(feature = "plugin")]
#[test]
fn list_plugin_tools_dedups_last_wins() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-tool "same" "v1" "L1" "{}" "h1")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "same" "v2" "L2" "{}" "h2")"#)
        .unwrap();
    let tools = mgr.list_plugin_tools();
    assert_eq!(tools.len(), 1, "duplicates should collapse to one entry");
    assert_eq!(tools[0].description, "v2");
    assert_eq!(tools[0].handler, "h2");
}

/// Multiple distinct names + one duplicate: distinct entries
/// retained, duplicate collapses.
#[cfg(feature = "plugin")]
#[test]
fn list_plugin_tools_preserves_distinct_entries_while_deduping() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-tool "a" "" "" "{}" "ha")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "b" "" "" "{}" "hb")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-tool "a" "" "" "{}" "ha2")"#)
        .unwrap();
    let tools = mgr.list_plugin_tools();
    assert_eq!(tools.len(), 2);
    // Surviving "a" entry uses the second handler.
    let a = tools.iter().find(|t| t.name == "a").unwrap();
    assert_eq!(a.handler, "ha2");
    let b = tools.iter().find(|t| t.name == "b").unwrap();
    assert_eq!(b.handler, "hb");
}

/// Shortcut dedup matches the same last-wins rule.
#[cfg(feature = "plugin")]
#[test]
fn list_shortcuts_dedups_last_wins() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-shortcut "ctrl-x" "h1")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-shortcut "ctrl-x" "h2" "second binding")"#)
        .unwrap();
    let shortcuts = mgr.list_shortcuts();
    assert_eq!(shortcuts.len(), 1);
    assert_eq!(shortcuts[0].handler, "h2");
    assert_eq!(shortcuts[0].description, "second binding");
}

/// Message-renderer dedup.
#[cfg(feature = "plugin")]
#[test]
fn list_message_renderers_dedups_last_wins() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-message-renderer "status" "r1")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-message-renderer "status" "r2")"#)
        .unwrap();
    let r = mgr.list_message_renderers();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0], ("status".to_string(), "r2".to_string()));
}

// --- H5: invoke_command surfaces handler errors via notif queue ------

/// A handler that raises an exception causes invoke_command to
/// return Ok(None) (caller-surface unchanged for backwards compat)
/// AND queue a `[plugin] command <handler> errored:` notification
/// so the user gets visible feedback on the next UI tick.
#[cfg(feature = "plugin")]
#[test]
fn invoke_command_surfaces_handler_errors_via_notification() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn boom [args] (error "intentional"))"#)
        .unwrap();
    let out = mgr.invoke_command("boom", "args").unwrap();
    assert_eq!(out, None, "handler error must return Ok(None)");

    let notifs = mgr.drain_notifications();
    assert_eq!(notifs.len(), 1);
    assert_eq!(notifs[0].0, "error");
    assert!(
        notifs[0].1.contains("command boom errored"),
        "got: {:?}",
        notifs[0].1
    );
    assert!(
        notifs[0].1.contains("intentional"),
        "got: {:?}",
        notifs[0].1
    );
}

/// Successful handler invocations don't pollute the notification
/// queue — the H5 fix only fires on the catch arm.
#[cfg(feature = "plugin")]
#[test]
fn invoke_command_success_does_not_emit_notification() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn ok-cmd [args] (string "got: " args))"#)
        .unwrap();
    let out = mgr.invoke_command("ok-cmd", "hi").unwrap();
    assert_eq!(out, Some("got: hi".to_string()));
    assert!(mgr.drain_notifications().is_empty());
}

// --- P9d (C1 fix): custom-message wrapper shape ---------------------

/// Single-string `(harness/add-custom-message "...")` form is
/// backwards compatible: produces an entry with empty
/// customType, display=true.
#[cfg(feature = "plugin")]
#[test]
fn add_custom_message_single_arg_form_is_backwards_compatible() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/add-custom-message "hello")"#).unwrap();
    let drained = mgr.drain_custom_messages();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].custom_type, "");
    assert_eq!(drained[0].content, "hello");
    assert!(drained[0].display);
}

/// Typed `(harness/add-custom-message customType content)` form
/// carries the customType field — what registered renderers
/// dispatch on (pi parity, messages.ts:46).
#[cfg(feature = "plugin")]
#[test]
fn add_custom_message_typed_form_carries_customtype() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/add-custom-message "status" "build done")"#)
        .unwrap();
    let drained = mgr.drain_custom_messages();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].custom_type, "status");
    assert_eq!(drained[0].content, "build done");
    assert!(drained[0].display);
}

/// `display=false` (third positional) flows through verbatim.
/// The UI must honor it to suppress the chat row.
#[cfg(feature = "plugin")]
#[test]
fn add_custom_message_respects_display_false() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/add-custom-message "telemetry" "x" false)"#)
        .unwrap();
    let drained = mgr.drain_custom_messages();
    assert_eq!(drained.len(), 1);
    assert!(!drained[0].display);
}

/// Embedded tabs/newlines in customType or content round-trip
/// through harness/-escape + unescape_harness_field.
#[cfg(feature = "plugin")]
#[test]
fn add_custom_message_round_trips_embedded_separators() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/add-custom-message "type\twith\ttabs" "line1\nline2")"#)
        .unwrap();
    let drained = mgr.drain_custom_messages();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].custom_type, "type\twith\ttabs");
    assert_eq!(drained[0].content, "line1\nline2");
}

/// Drain clears the slot — subsequent drains return empty.
#[cfg(feature = "plugin")]
#[test]
fn drain_custom_messages_clears_slot() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/add-custom-message "a" "1")"#).unwrap();
    assert_eq!(mgr.drain_custom_messages().len(), 1);
    assert_eq!(mgr.drain_custom_messages().len(), 0);
}

// --- P9d: plugin-registered message renderers -----------------------

#[cfg(feature = "plugin")]
#[test]
fn test_register_message_renderer_records_pair() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-message-renderer "status" "render-status")"#)
        .unwrap();
    let r = mgr.list_message_renderers();
    assert_eq!(r, vec![("status".to_string(), "render-status".to_string())]);
}

#[cfg(feature = "plugin")]
#[test]
fn test_register_message_renderer_multiple_in_load_order() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-message-renderer "a" "ra")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-message-renderer "b" "rb")"#)
        .unwrap();
    let r = mgr.list_message_renderers();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].0, "a");
    assert_eq!(r[1].0, "b");
}

#[cfg(feature = "plugin")]
#[test]
fn test_register_message_renderer_ignores_non_string_args() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-message-renderer 1 "h")"#)
        .unwrap();
    mgr.eval(r#"(harness/register-message-renderer "t" :sym)"#)
        .unwrap();
    assert!(mgr.list_message_renderers().is_empty());
}

/// `invoke_message_renderer` calls the named handler with the
/// raw payload string and returns its stringified output. The
/// handler can parse the JSON itself (or just use the string
/// verbatim for display).
#[cfg(feature = "plugin")]
#[test]
fn test_invoke_message_renderer_dispatches() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn render-status [payload] (string ">>" payload))"#)
        .unwrap();
    let out = mgr
        .invoke_message_renderer("render-status", r#"{"type":"status","content":"ok"}"#)
        .unwrap();
    assert_eq!(out.unwrap(), r#">>{"type":"status","content":"ok"}"#);
}

/// Handler errors swallow to `Ok(None)` — pi semantics keep
/// message dispatch alive even when a renderer is buggy.
#[cfg(feature = "plugin")]
#[test]
fn test_invoke_message_renderer_swallows_handler_error() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(defn boom [payload] (error "kaboom"))"#)
        .unwrap();
    let out = mgr.invoke_message_renderer("boom", "{}").unwrap();
    assert_eq!(out, None);
}

// --- P9c: plugin-registered keyboard shortcuts ---------------------

#[cfg(feature = "plugin")]
#[test]
fn test_register_shortcut_records_spec_and_description() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-shortcut "ctrl-x" "save-handler" "Save buffer")"#)
        .unwrap();
    let shortcuts = mgr.list_shortcuts();
    assert_eq!(shortcuts.len(), 1);
    assert_eq!(shortcuts[0].keys, "ctrl-x");
    assert_eq!(shortcuts[0].handler, "save-handler");
    assert_eq!(shortcuts[0].description, "Save buffer");
}

#[cfg(feature = "plugin")]
#[test]
fn test_register_shortcut_description_optional() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-shortcut "f5" "refresh")"#)
        .unwrap();
    let shortcuts = mgr.list_shortcuts();
    assert_eq!(shortcuts.len(), 1);
    assert!(shortcuts[0].description.is_empty());
}

#[cfg(feature = "plugin")]
#[test]
fn test_register_shortcut_ignores_non_string_args() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/register-shortcut 42 "h")"#).unwrap();
    mgr.eval(r#"(harness/register-shortcut "f5" :sym)"#)
        .unwrap();
    assert!(mgr.list_shortcuts().is_empty());
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
    use crate::config::ProviderEntry;
    use std::collections::HashMap;

    // We can only install the global once per process. Use a unique
    // provider name and check Note: This test depends on running
    // first or alongside other tests that also install. OnceLock's
    // set returns Err on second call but doesn't panic; we tolerate
    // that and check the result post-install.
    let mut map: HashMap<String, ProviderEntry> = HashMap::new();
    map.insert(
        "test-plugin-provider".to_string(),
        ProviderEntry {
            provider_type: Some("openai".to_string()),
            base_url: Some("http://plugin-test.invalid/v1".to_string()),
            api_key_env: Some("PLUGIN_TEST_KEY".to_string()),
            // test URL is http (not https) — must opt into insecure
            allow_insecure: true,
            ..Default::default()
        },
    );
    // Best-effort install — OnceLock may already be set from
    // another test; in that case we skip the assertion since we
    // can't observe a fresh install.
    crate::provider::install_plugin_providers(map);

    let cfg_providers: HashMap<String, ProviderEntry> = HashMap::new();
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
    use crate::config::ProviderEntry;
    use std::collections::HashMap;

    let mut cfg_providers: HashMap<String, ProviderEntry> = HashMap::new();
    cfg_providers.insert(
        "shadowed".to_string(),
        ProviderEntry {
            provider_type: Some("openai".to_string()),
            base_url: Some("http://from-config".to_string()),
            // test URL is http — opt into insecure
            allow_insecure: true,
            ..Default::default()
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

// --- Phase 4d: session-tree harness ops ------------------------------

/// `harness/set-label "node" "label"` queues a SetLabel op with
/// the literal label.
#[test]
fn harness_set_label_queues_op() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/set-label "node-abc" "checkpoint")"#)
        .unwrap();
    let ops = mgr.drain_tree_ops();
    assert_eq!(
        ops,
        vec![TreeOp::SetLabel {
            id: "node-abc".to_string(),
            label: Some("checkpoint".to_string()),
        }]
    );
    // Drained = next call returns empty.
    assert!(mgr.drain_tree_ops().is_empty());
}

/// Passing nil for the label clears it (label is None on the op).
#[test]
fn harness_set_label_with_nil_clears() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/set-label "node-abc" nil)"#).unwrap();
    let ops = mgr.drain_tree_ops();
    assert_eq!(
        ops,
        vec![TreeOp::SetLabel {
            id: "node-abc".to_string(),
            label: None,
        }]
    );
}

/// `harness/fork "id"` with no position arg defaults to :before
/// (restore prompt text into editor).
#[test]
fn harness_fork_defaults_to_restore_text() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/fork "node-1")"#).unwrap();
    let ops = mgr.drain_tree_ops();
    assert_eq!(
        ops,
        vec![TreeOp::Fork {
            id: "node-1".to_string(),
            restore_text: true,
        }]
    );
}

/// `harness/fork "id" :at` opts out of editor restoration.
#[test]
fn harness_fork_at_position_does_not_restore_text() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/fork "node-1" :at)"#).unwrap();
    let ops = mgr.drain_tree_ops();
    assert_eq!(
        ops,
        vec![TreeOp::Fork {
            id: "node-1".to_string(),
            restore_text: false,
        }]
    );
}

/// `harness/navigate-tree "id"` queues a NavigateTree op.
#[test]
fn harness_navigate_tree_queues_op() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/navigate-tree "tip")"#).unwrap();
    assert_eq!(
        mgr.drain_tree_ops(),
        vec![TreeOp::NavigateTree {
            id: "tip".to_string(),
        }]
    );
}

/// `harness/new-session` with no parent stores no lineage.
#[test]
fn harness_new_session_without_parent_has_none() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/new-session)"#).unwrap();
    assert_eq!(
        mgr.drain_tree_ops(),
        vec![TreeOp::NewSession { parent: None }]
    );
}

/// `harness/new-session "parent-id"` records the parent for
/// lineage tracking.
#[test]
fn harness_new_session_with_parent_records_lineage() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/new-session "prev-session-uuid")"#)
        .unwrap();
    assert_eq!(
        mgr.drain_tree_ops(),
        vec![TreeOp::NewSession {
            parent: Some("prev-session-uuid".to_string())
        }]
    );
}

/// `harness/switch-session "id-prefix"` queues a SwitchSession op.
#[test]
fn harness_switch_session_queues_op() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/switch-session "abc12345")"#).unwrap();
    assert_eq!(
        mgr.drain_tree_ops(),
        vec![TreeOp::SwitchSession {
            id_prefix: "abc12345".to_string(),
        }]
    );
}

/// Multiple ops queued in one eval drain in insertion order.
/// Order matters: the host applies sequentially (e.g. set-label
/// then fork should land the label before the branch shift).
#[test]
fn drain_tree_ops_preserves_insertion_order() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(
        r#"(do
                 (harness/set-label "a" "first")
                 (harness/fork "b")
                 (harness/navigate-tree "c"))"#,
    )
    .unwrap();
    let ops = mgr.drain_tree_ops();
    assert_eq!(ops.len(), 3);
    assert!(matches!(&ops[0], TreeOp::SetLabel { id, .. } if id == "a"));
    assert!(matches!(&ops[1], TreeOp::Fork { id, .. } if id == "b"));
    assert!(matches!(&ops[2], TreeOp::NavigateTree { id, .. } if id == "c"));
}

/// Ids/labels with embedded tabs and newlines round-trip cleanly —
/// harness/-escape ensures the tab-separated wire format isn't
/// corrupted by plugin payloads.
#[test]
fn drain_tree_ops_unescapes_payload_chars() {
    let mut mgr = PluginManager::try_new().unwrap();
    // \t in the label arg has to survive parsing.
    mgr.eval(r#"(harness/set-label "id" "with\ttab\nnewline")"#)
        .unwrap();
    let ops = mgr.drain_tree_ops();
    assert_eq!(
        ops,
        vec![TreeOp::SetLabel {
            id: "id".to_string(),
            label: Some("with\ttab\nnewline".to_string()),
        }]
    );
}

/// Non-string args are silently dropped (matches the rest of the
/// harness — bad type = no-op, not a panic).
#[test]
fn harness_tree_ops_reject_non_string_ids() {
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(harness/set-label 42 "label")"#).unwrap();
    mgr.eval(r#"(harness/fork nil)"#).unwrap();
    mgr.eval(r#"(harness/navigate-tree :keyword)"#).unwrap();
    assert!(mgr.drain_tree_ops().is_empty());
}

/// Unknown op verbs (forward-compat from a newer plugin) are
/// skipped rather than poisoning the rest of the drain.
#[test]
fn drain_tree_ops_skips_unknown_op_verbs() {
    // Drive harness-tree-ops directly to simulate a future op.
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(r#"(set harness-tree-ops "future-op\tfoo\nset-label\tnode\tlbl\n")"#)
        .unwrap();
    let ops = mgr.drain_tree_ops();
    assert_eq!(ops.len(), 1);
    assert!(matches!(&ops[0], TreeOp::SetLabel { .. }));
}

// --- load_plugin: single-file + directory + bare-name aliasing ------

/// Helper: write `text` into a unique tmp file and return its path.
/// Tests are responsible for cleanup but caller can skip on success.
fn tmpfile(label: &str, content: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "dirge-plugin-loadtest-{}-{}.janet",
        std::process::id(),
        label
    ));
    std::fs::write(&path, content).unwrap();
    path
}

/// A single-file plugin with a bare hook name gets the bare hook
/// aliased to `{stem}-{hook}` and registered for dispatch.
#[test]
fn load_plugin_aliases_bare_hooks_to_stem_prefix() {
    let path = tmpfile("bare-aliased", r#"(defn on-prompt [ctx] "from-bare")"#);
    let mut mgr = PluginManager::try_new().unwrap();
    let loaded = super::load_plugin(&mut mgr, &path).unwrap();
    let _ = std::fs::remove_file(&path);

    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    assert_eq!(loaded.stem, stem);
    assert!(loaded.hooks_registered.contains(&"on-prompt".to_string()));
    let out = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
    assert_eq!(out, vec!["from-bare".to_string()]);
}

/// Two plugins both using *bare* hook names don't clobber each
/// other — the alias step preserves each plugin's hook under its
/// own `{stem}-on-prompt` namespace, so both fire on dispatch.
#[test]
fn load_plugin_isolates_bare_hooks_across_plugins() {
    let p1 = tmpfile("alpha-iso", r#"(defn on-prompt [ctx] "from-alpha")"#);
    let p2 = tmpfile("beta-iso", r#"(defn on-prompt [ctx] "from-beta")"#);
    let mut mgr = PluginManager::try_new().unwrap();
    super::load_plugin(&mut mgr, &p1).unwrap();
    super::load_plugin(&mut mgr, &p2).unwrap();
    let _ = std::fs::remove_file(&p1);
    let _ = std::fs::remove_file(&p2);
    let out = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
    assert_eq!(out.len(), 2, "both plugins fire: {out:?}");
    assert!(out.contains(&"from-alpha".to_string()));
    assert!(out.contains(&"from-beta".to_string()));
}

/// Multi-line and tab-containing Janet error messages must not
/// break the `level\tmsg\n` notification format. drain_notifications
/// splits on `\n` per entry and on the first `\t` per level/msg,
/// so embedded control chars would corrupt parsing — show up as
/// truncated entries and orphaned "malformed" lines. The catch
/// arm now sanitizes via `string/replace-all` before the push.
#[test]
fn dispatch_sanitizes_multi_line_and_tab_in_hook_errors() {
    let path = tmpfile(
        "multiline-err",
        r#"(defn on-prompt [ctx] (error "line one\nline two\tline three"))"#,
    );
    let mut mgr = PluginManager::try_new().unwrap();
    super::load_plugin(&mut mgr, &path).unwrap();
    let _ = std::fs::remove_file(&path);

    let _ = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
    let pending = mgr.drain_notifications();

    // Exactly one entry, even though the error contained both \n
    // and \t. Without sanitization the multi-line error would
    // either produce multiple malformed entries (one per source
    // line) or split level/msg incorrectly at the embedded tab.
    let err_entries: Vec<_> = pending.iter().filter(|(lvl, _)| lvl == "error").collect();
    assert_eq!(err_entries.len(), 1, "got entries: {:?}", pending);
    let (_, msg) = err_entries[0];
    assert!(msg.contains("line one"), "msg missing 'line one': {msg}");
    assert!(msg.contains("line two"), "msg missing 'line two': {msg}");
    assert!(
        msg.contains("line three"),
        "msg missing 'line three': {msg}"
    );
    // The msg field on the *Rust side* has already been split out
    // of the wire format, so newlines/tabs inside it would only
    // appear if our Janet sanitization missed them. Assert none.
    assert!(!msg.contains('\n'), "msg leaked '\\n': {msg:?}");
    assert!(!msg.contains('\t'), "msg leaked '\\t': {msg:?}");
}

/// Consecutive identical hook errors (e.g. a buggy
/// on-message-update firing ~16x per response) must dedupe into a
/// single notification with a repeat-count suffix instead of
/// flooding the chat with 50+ identical banners.
#[test]
fn dispatch_dedupes_consecutive_identical_hook_errors() {
    let path = tmpfile(
        "repeat-err",
        r#"(defn on-prompt [ctx] (error "always the same"))"#,
    );
    let mut mgr = PluginManager::try_new().unwrap();
    super::load_plugin(&mut mgr, &path).unwrap();
    let _ = std::fs::remove_file(&path);

    for _ in 0..50 {
        let _ = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
    }
    let pending = mgr.drain_notifications();

    let err_entries: Vec<_> = pending.iter().filter(|(lvl, _)| lvl == "error").collect();
    // At most 2 entries (the first push + a "repeated N times"
    // summary that flushes on drain). Definitely not 50.
    assert!(
        err_entries.len() <= 2,
        "expected dedup (≤2 error entries); got {}: {:?}",
        err_entries.len(),
        pending,
    );
    let combined: String = err_entries
        .iter()
        .map(|(l, m)| format!("{l}\t{m}"))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        combined.contains("always the same"),
        "msg dropped: {combined}",
    );
    // The repeat-count summary must mention the number.
    assert!(
        combined.contains("repeated") && combined.contains("50"),
        "expected repeat-count summary mentioning 50; got: {combined}",
    );
}

/// Distinct hook errors must NOT be deduped — only consecutive
/// identical ones. A "B-error after A-error" should produce two
/// notifications, not one collapsed into the other.
#[test]
fn dispatch_distinct_hook_errors_are_not_deduped() {
    let path_a = tmpfile(
        "distinct-err-a",
        r#"(defn on-prompt [ctx] (error "alpha error"))"#,
    );
    let path_b = tmpfile(
        "distinct-err-b",
        r#"(defn on-response [ctx] (error "beta error"))"#,
    );
    let mut mgr = PluginManager::try_new().unwrap();
    super::load_plugin(&mut mgr, &path_a).unwrap();
    super::load_plugin(&mut mgr, &path_b).unwrap();
    let _ = std::fs::remove_file(&path_a);
    let _ = std::fs::remove_file(&path_b);

    let _ = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
    let _ = mgr.dispatch("on-response", "@{:response \"y\"}").unwrap();
    let pending = mgr.drain_notifications();

    let combined: String = pending
        .iter()
        .map(|(l, m)| format!("{l}\t{m}"))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(combined.contains("alpha"), "alpha missing: {combined}");
    assert!(combined.contains("beta"), "beta missing: {combined}");
}

/// A hook that throws is caught: dispatch continues (no panic,
/// no propagated error to the caller), `nil` is the effective
/// return value (filtered out of the results vec), AND the
/// error is pushed onto `harness-notif-list` with `error`
/// level so the next drain surfaces a chat-visible notification
/// — pi-style behavior layered on top of the structured
/// tracing::warn already emitted on the Rust side.
#[test]
fn dispatch_chat_surfaces_hook_errors_via_notification_queue() {
    let path = tmpfile("errored-hook", r#"(defn on-prompt [ctx] (error "boom"))"#);
    let mut mgr = PluginManager::try_new().unwrap();
    super::load_plugin(&mut mgr, &path).unwrap();
    let _ = std::fs::remove_file(&path);

    // Dispatch must NOT propagate the error — the host should
    // continue regardless of a broken plugin.
    let out = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
    // Hook returned nil after the catch, so no results.
    assert!(out.is_empty(), "errored hook should produce no result");

    // The error landed on the notification queue and shows up
    // in the next drain as an `error`-level entry.
    let pending = mgr.drain_notifications();
    assert!(
        pending.iter().any(|(level, msg)| level == "error"
            && msg.contains("on-prompt")
            && msg.contains("boom")),
        "expected an error notification mentioning on-prompt and boom; got: {:?}",
        pending,
    );
}

/// A directory plugin loads every `*.janet` file inside in
/// alphabetical order. The stem is the directory name; multi-file
/// plugins share the same Janet env so files can collaborate.
#[test]
fn load_plugin_supports_directory_of_files() {
    let dir = std::env::temp_dir().join(format!("dirge-multifile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("00-state.janet"), r#"(var shared-counter 0)"#).unwrap();
    std::fs::write(
        dir.join("01-hooks.janet"),
        r#"(defn on-prompt [ctx]
                 (++ shared-counter)
                 (string "counter=" shared-counter))"#,
    )
    .unwrap();

    let mut mgr = PluginManager::try_new().unwrap();
    let loaded = super::load_plugin(&mut mgr, &dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(loaded.files.len(), 2);
    // 00-state.janet sorts before 01-hooks.janet so the var is
    // defined before the hook references it.
    assert!(
        loaded.files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("00-")
    );
    assert!(loaded.stem.starts_with("dirge-multifile-"));
    let out = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
    assert_eq!(out, vec!["counter=1".to_string()]);
    // Counter persists — proves shared state across hook invocations.
    let out2 = mgr.dispatch("on-prompt", "@{:prompt \"x\"}").unwrap();
    assert_eq!(out2, vec!["counter=2".to_string()]);
}

/// Empty directory plugins return an error rather than silently
/// registering nothing — typo'd plugin dirs should surface visibly.
#[test]
fn load_plugin_rejects_empty_directory() {
    let dir = std::env::temp_dir().join(format!("dirge-empty-plugin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut mgr = PluginManager::try_new().unwrap();
    let err = super::load_plugin(&mut mgr, &dir).unwrap_err();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(err.contains("no .janet files"), "got: {err}");
}

/// `--auto-confirm yes` answers `harness/confirm` with `true` and
/// picks the first option for `harness/select`. Verifies the
/// dialog-drain task (the only thing that lets confirm/select
/// finish in headless modes) does not hang.
///
/// `spawn_blocking` is used for the std-mpsc receive so the
/// current-thread runtime can still drive the responder task.
#[tokio::test]
async fn auto_confirm_yes_responds_true_and_first_option() {
    use std::sync::mpsc;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<super::DialogRequest>();
    let _handle = super::spawn_headless_dialog_responder(rx, crate::cli::AutoConfirmMode::Yes);

    let (creply_tx, creply_rx) = mpsc::channel::<super::DialogReply>();
    tx.send(super::DialogRequest::Confirm {
        title: "t".into(),
        question: "q".into(),
        reply: creply_tx,
    })
    .unwrap();
    let reply = tokio::task::spawn_blocking(move || {
        creply_rx.recv_timeout(std::time::Duration::from_secs(2))
    })
    .await
    .unwrap()
    .unwrap();
    match reply {
        super::DialogReply::Confirm(b) => assert!(b),
        other => panic!("expected Confirm(true), got {:?}", other),
    }

    let (sreply_tx, sreply_rx) = mpsc::channel::<super::DialogReply>();
    tx.send(super::DialogRequest::Select {
        title: "t".into(),
        options: vec!["alpha".into(), "beta".into()],
        reply: sreply_tx,
    })
    .unwrap();
    let reply = tokio::task::spawn_blocking(move || {
        sreply_rx.recv_timeout(std::time::Duration::from_secs(2))
    })
    .await
    .unwrap()
    .unwrap();
    match reply {
        super::DialogReply::Select(picked) => assert_eq!(picked.as_deref(), Some("alpha")),
        other => panic!("expected Select(Some(\"alpha\")), got {:?}", other),
    }
}

/// `--auto-confirm no` answers `harness/confirm` with `false` and
/// returns `None` for `harness/select`.
#[tokio::test]
async fn auto_confirm_no_responds_false_and_none() {
    use std::sync::mpsc;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<super::DialogRequest>();
    let _handle = super::spawn_headless_dialog_responder(rx, crate::cli::AutoConfirmMode::No);

    let (creply_tx, creply_rx) = mpsc::channel::<super::DialogReply>();
    tx.send(super::DialogRequest::Confirm {
        title: "t".into(),
        question: "q".into(),
        reply: creply_tx,
    })
    .unwrap();
    let reply = tokio::task::spawn_blocking(move || {
        creply_rx.recv_timeout(std::time::Duration::from_secs(2))
    })
    .await
    .unwrap()
    .unwrap();
    match reply {
        super::DialogReply::Confirm(b) => assert!(!b),
        other => panic!("expected Confirm(false), got {:?}", other),
    }

    let (sreply_tx, sreply_rx) = mpsc::channel::<super::DialogReply>();
    tx.send(super::DialogRequest::Select {
        title: "t".into(),
        options: vec!["alpha".into(), "beta".into()],
        reply: sreply_tx,
    })
    .unwrap();
    let reply = tokio::task::spawn_blocking(move || {
        sreply_rx.recv_timeout(std::time::Duration::from_secs(2))
    })
    .await
    .unwrap()
    .unwrap();
    match reply {
        super::DialogReply::Select(picked) => assert_eq!(picked, None),
        other => panic!("expected Select(None), got {:?}", other),
    }
}

/// The shipped nREPL plugin (plugins/nrepl/) loads cleanly through the
/// real directory loader and registers its slash commands + the
/// `nrepl_eval` tool. Guards against a syntax error or a renamed
/// `harness/*` function silently breaking the bundled plugin. Loading
/// only evaluates the files (registering commands/tools); it does NOT
/// fire `on-init`, so no network connection is attempted here.
#[test]
fn shipped_nrepl_plugin_loads_and_registers() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/nrepl");
    assert!(dir.is_dir(), "plugins/nrepl missing at {}", dir.display());

    let mut mgr = PluginManager::try_new().unwrap();
    let loaded = super::load_plugin(&mut mgr, &dir)
        .unwrap_or_else(|e| panic!("nrepl plugin failed to load: {e}"));
    assert_eq!(loaded.stem, "nrepl");

    let cmds: Vec<String> = mgr.list_commands().into_iter().map(|(c, _)| c).collect();
    for expected in [
        "nrepl-connect",
        "nrepl-disconnect",
        "nrepl-eval",
        "nrepl-status",
        "nrepl-timeout",
        "nrepl-interrupt",
    ] {
        assert!(
            cmds.iter().any(|c| c == expected),
            "command {expected} not registered; got {cmds:?}"
        );
    }

    let tools: Vec<String> = mgr
        .list_plugin_tools()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        tools.iter().any(|t| t == "nrepl_eval"),
        "nrepl_eval tool not registered; got {tools:?}"
    );
}

/// The plugin's pure-Janet paren repair closes unbalanced delimiters in
/// the correct (stack) order — the behavior the eval path relies on to
/// hand the nREPL server valid syntax. Loads only 00-state.janet (no
/// network).
#[test]
fn nrepl_plugin_paren_repair_balances_delimiters() {
    let state = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/nrepl/00-state.janet"),
    )
    .unwrap();
    let mut mgr = PluginManager::try_new().unwrap();
    mgr.eval(&state).unwrap();

    assert_eq!(mgr.eval(r#"(paren-repair "(+ 1 2")"#).unwrap(), "(+ 1 2)");
    assert_eq!(
        mgr.eval(r#"(paren-repair "(defn f [x] {:a x")"#).unwrap(),
        "(defn f [x] {:a x})"
    );
    // Balanced input is returned unchanged.
    assert_eq!(
        mgr.eval(r#"(paren-repair "(+ 1 (* 2 3))")"#).unwrap(),
        "(+ 1 (* 2 3))"
    );
    // A ')' inside a string literal must not be treated as a closer,
    // so balanced-with-embedded-paren input is returned unchanged.
    // (Janet long-string `...` passes the code without escape noise.)
    assert_eq!(
        mgr.eval(r#"(paren-repair `(str ")")`)"#).unwrap(),
        r#"(str ")")"#
    );
}
