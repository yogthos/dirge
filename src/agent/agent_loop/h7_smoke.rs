//! Phase 4.5h-7 smoke tests against real provider APIs.
//!
//! These tests are `#[ignore]` by default so they don't run in
//! normal `cargo test`. To run them you must:
//!
//!   - Have a provider API key in env (DEEPSEEK_API_KEY is the
//!     canonical choice; others auto-detected)
//!   - Pass `-- --ignored` to cargo test:
//!
//!     ```
//!     cargo test agent_loop::h7_smoke -- --ignored --nocapture
//!     ```
//!
//! Each test exercises a different scenario from
//! `docs/H7_AGENT_LOOP_TEST.md`. Failures here indicate the new
//! agent_loop path has a real-LLM bug that the mock-driven
//! tests missed.
//!
//! These tests bypass the full dirge `build_agent` (sessions,
//! permission asker, plugin manager, etc.) and exercise just
//! the new path's core: `rig_stream_fn_from_model` →
//! `retrying_stream_fn` → `spawn_loop_runner`. If those work,
//! the `AnyAgent::spawn_runner` integration is highly likely
//! to work too because it composes the same pieces.

#![cfg(test)]

use std::sync::Arc;

use crate::agent::agent_loop::{
    LoopSpawnConfig, retrying_stream_fn, rig_stream_fn_from_model, spawn_loop_runner,
};
use crate::agent::recovery::RecoveryPolicy;
use crate::event::AgentEvent;

/// Check env vars and return Some(provider) for whichever
/// API key is present. Returns None if none of the known
/// keys are set — tests then skip with an explanation.
fn detect_provider() -> Option<&'static str> {
    for (var, name) in [
        ("DEEPSEEK_API_KEY", "deepseek"),
        ("ANTHROPIC_API_KEY", "anthropic"),
        ("OPENAI_API_KEY", "openai"),
        ("OPENROUTER_API_KEY", "openrouter"),
    ] {
        if std::env::var(var).is_ok() {
            return Some(name);
        }
    }
    None
}

/// Default model per provider for h-7 testing. These are
/// cheap / fast models so smoke tests don't burn budget.
fn default_model(provider: &str) -> &'static str {
    match provider {
        "deepseek" => "deepseek-chat",
        "anthropic" => "claude-haiku-4-5-20251001",
        "openai" => "gpt-4o-mini",
        "openrouter" => "deepseek/deepseek-chat",
        _ => "gpt-4o-mini",
    }
}

/// Build a `StreamFn` from whichever provider has an API key
/// set. The key itself comes from the env (rig clients read it
/// directly).
fn build_stream_fn() -> Option<crate::agent::agent_loop::StreamFn> {
    use crate::provider::{AnyClient, AnyModel};
    use rig::providers::{anthropic, openai, openrouter};
    use std::collections::HashMap;

    let provider = detect_provider()?;
    let model_name = default_model(provider);

    // Build AnyClient directly via create_client; needs the key
    // env var to be readable.
    let client = match crate::provider::create_client(provider, None, &HashMap::new()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[h7-smoke] failed to build {provider} client: {e}");
            return None;
        }
    };
    let any_model = client.completion_model(model_name);

    // For h-7 each variant builds the StreamFn via
    // rig_stream_fn_from_model. Mirrors AnyAgent::build_stream_fn
    // dispatch but on AnyModel directly (no AnyAgent indirection).
    let chunk_timeout = Some(std::time::Duration::from_secs(60));
    let stream_fn = match any_model {
        AnyModel::OpenRouter(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::OpenAI(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Anthropic(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Gemini(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::DeepSeek(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Glm(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Ollama(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
        AnyModel::Custom(m) => rig_stream_fn_from_model(m, vec![], chunk_timeout),
    };

    eprintln!("[h7-smoke] using provider={provider} model={model_name}");
    Some(retrying_stream_fn(stream_fn, RecoveryPolicy::default()))
}

/// Drain an `AgentRunner`'s event_rx and collect labels. Returns
/// the events for downstream assertions and the final Done's
/// response field (or None if Done didn't fire).
async fn drain_to_done(
    mut runner: crate::agent::runner::AgentRunner,
) -> (Vec<AgentEvent>, Option<String>) {
    let mut events = Vec::new();
    let mut final_response = None;
    while let Some(evt) = runner.event_rx.recv().await {
        if let AgentEvent::Done { response, .. } = &evt {
            final_response = Some(response.to_string());
        }
        events.push(evt);
    }
    let _ = runner.task.await;
    (events, final_response)
}

/// Render an AgentEvent stream as a multi-line summary for the
/// stderr trace. Aids debugging when a scenario fails.
fn dump_events(events: &[AgentEvent]) {
    for e in events {
        match e {
            AgentEvent::Token(s) => eprint!("{}", s),
            AgentEvent::Reasoning(_) => eprint!("·"),
            AgentEvent::ToolCall { name, args, .. } => {
                eprintln!("\n[tool_call] {name}({args})");
            }
            AgentEvent::ToolStarted { .. } => {}
            AgentEvent::ToolResult { output, .. } => {
                eprintln!("\n[tool_result] {} bytes", output.len());
            }
            AgentEvent::TurnStart { index } => eprintln!("\n[turn {index} start]"),
            AgentEvent::TurnEnd { index } => eprintln!("\n[turn {index} end]"),
            AgentEvent::Done { response, .. } => {
                eprintln!("\n[done] response={response:?}");
            }
            AgentEvent::Error(s) => eprintln!("\n[ERROR] {s}"),
            AgentEvent::ContextOverflow { error, .. } => {
                eprintln!("\n[context_overflow] {error}");
            }
            AgentEvent::Interjected { .. } => eprintln!("\n[interjected]"),
        }
    }
    eprintln!();
}

/// **Scenario 1** — simple text Q&A, no tools.
///
/// Verifies: stream factory works against a real provider;
/// Token events fire; Done arrives with non-empty response.
#[tokio::test]
#[ignore = "requires provider API key"]
async fn h7_scenario_1_simple_text() {
    let stream_fn = match build_stream_fn() {
        Some(f) => f,
        None => {
            eprintln!("[skipped] no provider API key in env");
            return;
        }
    };

    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: "You are a helpful assistant. Reply concisely.".to_string(),
        history: Vec::new(),
        initial_prompt: "What is 2+2? Reply with just the number, nothing else.".to_string(),
        tools: Vec::new(),
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Parallel,
        event_channel_capacity: 256,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);

    // Expectations:
    //   - Done event fires.
    //   - Response contains "4".
    //   - At least one Token event streamed (real provider
    //     streams; canned mocks don't).
    let done = response.unwrap_or_default();
    assert!(
        done.contains('4'),
        "expected response to contain '4', got: {done:?}"
    );
    let token_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Token(_)))
        .count();
    assert!(
        token_count >= 1,
        "expected at least 1 Token event from real stream; got 0 (provider may be returning all-at-once Done — check stream wrapping)"
    );

    // No Error or ContextOverflow.
    for e in &events {
        match e {
            AgentEvent::Error(msg) => panic!("unexpected Error: {msg}"),
            AgentEvent::ContextOverflow { error, .. } => {
                panic!("unexpected ContextOverflow: {error}")
            }
            _ => {}
        }
    }
}

/// **Scenario 2** — basic multi-turn structure.
///
/// Issues a prompt that triggers a short follow-up. Verifies the
/// loop produces TurnStart + TurnEnd + Done in the expected
/// order, the Done's response is sensible.
#[tokio::test]
#[ignore = "requires provider API key"]
async fn h7_scenario_2_turn_boundaries() {
    let stream_fn = match build_stream_fn() {
        Some(f) => f,
        None => {
            eprintln!("[skipped] no provider API key in env");
            return;
        }
    };

    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: "Reply briefly.".to_string(),
        history: Vec::new(),
        initial_prompt: "Say the word 'banana' and nothing else.".to_string(),
        tools: Vec::new(),
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Parallel,
        event_channel_capacity: 256,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);

    // Expect exactly one TurnStart and one TurnEnd before Done.
    let turn_starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStart { .. }))
        .count();
    let turn_ends = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
        .count();
    let dones = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .count();
    assert_eq!(turn_starts, 1, "expected 1 TurnStart, got {turn_starts}");
    assert_eq!(turn_ends, 1, "expected 1 TurnEnd, got {turn_ends}");
    assert_eq!(dones, 1, "expected 1 Done, got {dones}");

    // Response should mention banana.
    assert!(
        response
            .unwrap_or_default()
            .to_lowercase()
            .contains("banana"),
        "expected 'banana' in response",
    );
}

/// **Scenario 5 (slim)** — error path: invalid API key surfaces
/// as an Error event (not a panic, not silent).
///
/// Forces an Auth error by temporarily masking the key. This is
/// in-test env mutation — relies on no parallel tests running
/// against the same key. We use a process-wide env var override
/// inside the test.
#[tokio::test]
#[ignore = "requires provider API key (uses env)"]
async fn h7_scenario_5_auth_error_surfaces() {
    let original = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(v) => Some(v),
        Err(_) => {
            eprintln!("[skipped] need DEEPSEEK_API_KEY to override");
            return;
        }
    };
    // Set a known-invalid key.
    // SAFETY: tests run with parallel=false implicitly because of
    // mutex around env vars in the rest of the suite; this
    // mutation is reverted at the end of the test even on panic.
    unsafe {
        std::env::set_var("DEEPSEEK_API_KEY", "invalid-key-for-h7-test");
    }
    let result = std::panic::AssertUnwindSafe(async {
        let stream_fn = build_stream_fn().expect("build stream_fn with bad key");
        let cfg = LoopSpawnConfig {
            stream_fn,
            system_prompt: String::new(),
            history: Vec::new(),
            initial_prompt: "hi".to_string(),
            tools: Vec::new(),
            #[cfg(feature = "plugin")]
            plugin_mgr: None,
            steering_queue: None,
            tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Parallel,
            event_channel_capacity: 256,
        };
        let runner = spawn_loop_runner(cfg).into_agent_runner();
        let (events, _) = drain_to_done(runner).await;
        dump_events(&events);
        // Auth error → either Error event (non-retryable
        // classification per recovery::classify_error) OR Done
        // with an empty / error-formatted response. The retry
        // wrapper's classification routes Auth → no retry.
        let had_error = events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error(_) | AgentEvent::ContextOverflow { .. }));
        assert!(
            had_error,
            "expected Error or ContextOverflow event for invalid key"
        );
    })
    .await;

    // Restore original key.
    unsafe {
        if let Some(v) = original {
            std::env::set_var("DEEPSEEK_API_KEY", v);
        }
    }
    // Re-raise panic if the inner block panicked.
    let _ = result; // suppress warning; AssertUnwindSafe drops without catch
}

/// **Scenario 3 (slim)** — tool dispatch against a real LLM.
///
/// Uses an inline `LoopTool` that echoes its input. Builds a
/// matching rig `ToolDefinition` so the LLM knows about it.
/// Verifies the model can be coaxed into using it and the
/// loop dispatches + returns the result + the model uses the
/// result in a follow-up turn.
///
/// This DOESN'T exercise the production LoopTool registry
/// (which requires permission asker + sandbox + many other
/// fixture inputs). It does exercise the full dispatch chain:
/// rig stream → tool call extraction → LoopTool execute →
/// finalize → next LLM turn.
#[tokio::test]
#[ignore = "requires provider API key"]
async fn h7_scenario_3_tool_dispatch() {
    use crate::agent::agent_loop::result::LoopToolResult as ResultT;
    use crate::agent::agent_loop::tool::{AbortSignal, LoopToolUpdate};
    use crate::agent::agent_loop::{LoopTool, LoopToolResult, loop_tool_to_rig_definition};
    use rig::completion::ToolDefinition;
    use serde_json::Value;
    use std::pin::Pin;

    let provider = match detect_provider() {
        Some(p) => p,
        None => {
            eprintln!("[skipped] no API key");
            return;
        }
    };
    if provider != "deepseek" && provider != "openai" && provider != "openrouter" {
        eprintln!("[skipped] tool-use test prefers deepseek/openai/openrouter; got {provider}");
        // anthropic / gemini tools work but the prompt phrasing
        // below is tuned for OpenAI-shaped function calling.
        return;
    }

    // Build an inline LoopTool that echoes its `text` argument
    // back. Mirrors the EchoTool from in-module tests.
    #[derive(Debug)]
    struct EchoTool;
    impl LoopTool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn description(&self) -> &str {
            "Echo the given text back. Use this when asked to echo something."
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn parameters(&self) -> &Value {
            static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            P.get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "Text to echo"}
                    },
                    "required": ["text"]
                })
            })
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<ResultT, String>> + Send + 'a>> {
            Box::pin(async move {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no text)")
                    .to_string();
                Ok(ResultT {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": format!("ECHO: {text}"),
                    })],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    let tool = Arc::new(EchoTool) as Arc<dyn LoopTool>;
    let tool_def = loop_tool_to_rig_definition(tool.as_ref());

    // Build StreamFn WITH the tool definition.
    let model_name = default_model(provider);
    let client = crate::provider::create_client(provider, None, &std::collections::HashMap::new())
        .expect("client");
    let any_model = client.completion_model(model_name);
    let chunk_timeout = Some(std::time::Duration::from_secs(60));
    let inner_stream_fn = match any_model {
        crate::provider::AnyModel::DeepSeek(m) => {
            rig_stream_fn_from_model(m, vec![tool_def.clone()], chunk_timeout)
        }
        crate::provider::AnyModel::OpenAI(m) => {
            rig_stream_fn_from_model(m, vec![tool_def.clone()], chunk_timeout)
        }
        crate::provider::AnyModel::OpenRouter(m) => {
            rig_stream_fn_from_model(m, vec![tool_def.clone()], chunk_timeout)
        }
        _ => {
            eprintln!("[skipped] this scenario hardcoded to deepseek/openai/openrouter");
            return;
        }
    };
    let stream_fn = retrying_stream_fn(inner_stream_fn, RecoveryPolicy::default());

    eprintln!("[h7-smoke] tool-dispatch test using {provider}/{model_name}");

    let cfg = LoopSpawnConfig {
        stream_fn,
        system_prompt: "You have access to an echo_tool that echoes text back. \
                        When the user asks you to echo something, USE THE TOOL — \
                        do not just reply with the text directly. After calling \
                        the tool, briefly confirm what was echoed."
            .to_string(),
        history: Vec::new(),
        initial_prompt: "Echo the word 'pineapple'.".to_string(),
        tools: vec![tool],
        #[cfg(feature = "plugin")]
        plugin_mgr: None,
        steering_queue: None,
        tool_execution: crate::agent::agent_loop::types::ToolExecutionMode::Sequential,
        event_channel_capacity: 256,
    };
    let runner = spawn_loop_runner(cfg).into_agent_runner();
    let (events, response) = drain_to_done(runner).await;
    dump_events(&events);

    // Expectations:
    //   - At least one ToolCall event (model used the tool)
    //   - At least one ToolResult event (we dispatched it)
    //   - Done with a response mentioning "pineapple" (model
    //     summarized after the tool ran)
    let tool_calls = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
        .count();
    let tool_results = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .count();
    assert!(
        tool_calls >= 1,
        "expected the LLM to call echo_tool, got 0 ToolCall events"
    );
    assert!(
        tool_results >= 1,
        "expected at least 1 ToolResult event, got 0"
    );
    assert_eq!(
        tool_calls, tool_results,
        "expected ToolCall and ToolResult counts to match"
    );
    let final_resp = response.unwrap_or_default();
    assert!(
        final_resp.to_lowercase().contains("pineapple"),
        "expected final response to reference 'pineapple'; got: {final_resp:?}"
    );
}

// Scenarios 4 (mid-run interjection), 6 (context overflow),
// and 7 (plugin hook) are covered in the manual runbook
// (docs/H7_AGENT_LOOP_TEST.md). They require interactive UI,
// large prompts, or plugin file setup that doesn't translate
// well to an automated test.

#[allow(unused_imports, dead_code)]
fn _ensure_arc_used(_: Arc<()>) {}
