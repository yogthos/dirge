use super::*;
use std::collections::HashMap;

/// Build an env-lookup closure backed by a HashMap. Avoids
/// mutating process-wide env vars — `std::env::set_var` is
/// thread-unsafe and the previous test suite raced under
/// parallel `cargo test`, producing intermittent failures.
fn mock_env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

#[test]
fn auto_detect_returns_none_when_no_vars_set() {
    assert_eq!(auto_detect_provider_from(mock_env(&[])), None);
}

#[test]
fn auto_detect_finds_deepseek_when_key_set() {
    let env = mock_env(&[("DEEPSEEK_API_KEY", "sk-test-123")]);
    assert_eq!(auto_detect_provider_from(env), Some("deepseek"));
}

#[test]
fn auto_detect_finds_openai_when_key_set() {
    let env = mock_env(&[("OPENAI_API_KEY", "sk-test-456")]);
    assert_eq!(auto_detect_provider_from(env), Some("openai"));
}

#[test]
fn auto_detect_skips_empty_var() {
    let env = mock_env(&[("DEEPSEEK_API_KEY", ""), ("OPENAI_API_KEY", "sk-test-789")]);
    assert_eq!(auto_detect_provider_from(env), Some("openai"));
}

#[test]
fn auto_detect_returns_first_match_in_order() {
    let env = mock_env(&[("DEEPSEEK_API_KEY", "sk-ds"), ("OPENAI_API_KEY", "sk-oai")]);
    assert_eq!(auto_detect_provider_from(env), Some("deepseek"));
}

/// Cover every provider in the autodetect list — guards
/// against accidentally dropping or reordering an entry.
#[test]
fn auto_detect_each_provider_in_isolation() {
    for &(env_var, expected) in PROVIDER_AUTODETECT_ORDER {
        let env = mock_env(&[(env_var, "sk-x")]);
        assert_eq!(
            auto_detect_provider_from(env),
            Some(expected),
            "env_var={env_var}",
        );
    }
}

/// `ZHIPU_API_KEY` alone resolves to glm provider — Zhipu's
/// canonical env-var name doesn't require users to alias.
#[test]
fn auto_detect_zhipu_api_key_resolves_to_glm() {
    let env = mock_env(&[("ZHIPU_API_KEY", "fake-zhipu-key")]);
    assert_eq!(auto_detect_provider_from(env), Some("glm"));
}

/// When BOTH GLM_API_KEY and ZHIPU_API_KEY are set, the
/// dirge-primary GLM_API_KEY wins (it's earlier in
/// PROVIDER_AUTODETECT_ORDER). The fallback only fires when
/// the primary is absent.
#[test]
fn auto_detect_glm_api_key_wins_over_zhipu_when_both_set() {
    let env = mock_env(&[("GLM_API_KEY", "primary"), ("ZHIPU_API_KEY", "fallback")]);
    // Both map to "glm" so the answer is the same kind, but
    // this guards against a future reordering breaking the
    // primary-first invariant. We can't observe WHICH var
    // resolve_api_key picked from auto_detect alone — that's
    // tested below.
    assert_eq!(auto_detect_provider_from(env), Some("glm"));
}

/// `provider_env_var_fallbacks` lists canonical alternatives
/// for GLM (Zhipu's name), Anthropic (OAuth token), and Gemini
/// (Google's canonical form). Other providers have no
/// alternatives.
#[test]
fn fallback_list_covers_canonical_alternatives() {
    assert_eq!(
        provider_env_var_fallbacks(ProviderKind::Glm),
        &["ZHIPU_API_KEY"]
    );
    // B3-3: Anthropic OAuth, Google's two canonical names.
    assert_eq!(
        provider_env_var_fallbacks(ProviderKind::Anthropic),
        &["ANTHROPIC_OAUTH_TOKEN"]
    );
    assert_eq!(
        provider_env_var_fallbacks(ProviderKind::Gemini),
        &["GOOGLE_GENERATIVE_AI_API_KEY", "GOOGLE_API_KEY"]
    );
    for kind in [
        ProviderKind::OpenAI,
        ProviderKind::DeepSeek,
        ProviderKind::OpenRouter,
        ProviderKind::Ollama,
        ProviderKind::Custom,
    ] {
        assert!(
            provider_env_var_fallbacks(kind).is_empty(),
            "no fallback expected for {kind:?}",
        );
    }
}

// ============================================================
// Phase 4.5h-2: AnyAgent::build_stream_fn dispatch tests
// ============================================================

/// Build a real `AnyAgent` from an openai-shaped client +
/// model. The Client::new doesn't connect (no network until
/// the first request), so this works in unit tests.
///
/// Use `completions_api()` to get the chat-completion model
/// (the variant `AnyAgentInner::OpenAI` holds); the default
/// `completion_model` on a fresh `Client` returns the
/// responses-api model, which is a different type.
fn build_openai_any_agent() -> AnyAgent {
    use rig::providers::openai;
    let client = openai::Client::new("test-key")
        .expect("openai Client::new should work")
        .completions_api();
    let model = client.completion_model("gpt-4o");
    let agent = rig::agent::AgentBuilder::new(model).build();
    AnyAgent::new(
        AnyAgentInner::OpenAI(agent),
        ToolCache::new(),
        std::time::Duration::from_secs(300),
        Vec::new(),    // loop_tools — empty for test fixture
        String::new(), // preamble — empty for test fixture
        "gpt-4o".to_string(),
    )
}

/// `build_stream_fn` returns a `Send + Sync + 'static`
/// `StreamFn` for the OpenAI variant. Compile-time check —
/// if the bounds don't match the type would fail to
/// construct.
#[test]
fn build_stream_fn_returns_send_sync_static() {
    fn assert_send_sync_static<T: Send + Sync + 'static>(_: &T) {}
    let agent = build_openai_any_agent();
    let stream_fn = agent.build_stream_fn(vec![]);
    assert_send_sync_static(&stream_fn);
}

/// `build_stream_fn` is callable as a `Fn` (multi-call) —
/// the loop invokes it once per turn. Verify by calling
/// twice and checking both invocations produce streams.
#[tokio::test]
async fn build_stream_fn_is_multi_callable() {
    use crate::agent::agent_loop::LlmContext;
    use crate::agent::agent_loop::tool::AbortSignal;
    use futures::stream::StreamExt;

    let agent = build_openai_any_agent();
    let stream_fn = agent.build_stream_fn(vec![]);

    // Call once with an empty context — should emit an
    // Error event (no prompt) without panicking.
    let ctx = LlmContext {
        system_prompt: String::new(),
        messages: vec![],
    };
    let mut s = stream_fn(
        ctx,
        crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
    );
    let first = s.next().await;
    assert!(first.is_some(), "first call should produce events");

    // Call again — same closure, same Arc, fresh stream.
    let ctx2 = LlmContext {
        system_prompt: String::new(),
        messages: vec![],
    };
    let mut s2 = stream_fn(
        ctx2,
        crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
    );
    let second = s2.next().await;
    assert!(second.is_some(), "second call should also produce events");
}

/// All 8 `AnyAgentInner` variants compile through
/// `build_stream_fn` — the match arms cover the full enum,
/// and the bounds on `rig_stream_fn_from_model<M>` are
/// satisfied by each provider's `CompletionModel`.
///
/// This test exists primarily as a compile-time
/// canary: if a future provider variant gets added to
/// `AnyAgentInner` without a matching arm in
/// `build_stream_fn`, the build breaks. Runtime
/// dispatch is exercised by the OpenAI-backed tests
/// above.
#[test]
fn build_stream_fn_covers_all_variants_compile_time() {
    // Just constructs one variant and calls
    // build_stream_fn; the rest are validated by the
    // match-arm exhaustiveness check at compile time.
    let agent = build_openai_any_agent();
    let _ = agent.build_stream_fn(vec![]);
}

// --- C6/C7: compaction prefix is full + includes tool calls -----

use super::summarize;
use crate::session::{MessageRole, SessionMessage, ToolCallEntry, ToolCallState};
use compact_str::CompactString;

fn sm(role: MessageRole, content: &str, tool_calls: Vec<ToolCallEntry>) -> SessionMessage {
    SessionMessage {
        role,
        content: CompactString::from(content),
        estimated_tokens: 0,
        id: CompactString::from("test-id"),
        timestamp: 0,
        tool_calls,
    }
}

/// C7: assistant tool calls land in the serialized form with
/// args + result. Previously they were dropped entirely so the
/// summarizer saw only `[Assistant]: <text>` with no record
/// that bash/read/edit ever ran.
#[test]
fn serialize_conversation_includes_tool_calls() {
    let msgs = vec![
        sm(MessageRole::User, "list rust files", vec![]),
        sm(
            MessageRole::Assistant,
            "I'll find them.",
            vec![ToolCallEntry {
                id: "call_1".into(),
                name: "find_files".into(),
                args: serde_json::json!({"pattern": "*.rs"}),
                state: ToolCallState::Completed {
                    result: "src/main.rs\nsrc/lib.rs".into(),
                },
            }],
        ),
    ];
    let out = summarize::serialize_conversation(&msgs);
    assert!(out.contains("[User]"), "missing role tag: {out}");
    assert!(
        out.contains("[Tool: find_files("),
        "missing tool call line: {out}"
    );
    assert!(
        out.contains("src/main.rs"),
        "missing tool result content: {out}"
    );
}

/// C7: interrupted + failed tool calls also surface.
#[test]
fn serialize_conversation_marks_interrupted_and_failed() {
    let msgs = vec![sm(
        MessageRole::Assistant,
        "trying",
        vec![
            ToolCallEntry {
                id: "a".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "sleep 9999"}),
                state: ToolCallState::Interrupted,
            },
            ToolCallEntry {
                id: "b".into(),
                name: "read".into(),
                args: serde_json::json!({"path": "/missing"}),
                state: ToolCallState::Failed {
                    error: "no such file".into(),
                },
            },
        ],
    )];
    let out = summarize::serialize_conversation(&msgs);
    assert!(out.contains("<interrupted>"), "got: {out}");
    assert!(out.contains("<failed: no such file>"), "got: {out}");
}

/// C7 bound: a single tool result over the per-tool cap (2KB)
/// truncates with a marker, preserving structure of the rest
/// of the conversation.
#[test]
fn serialize_conversation_truncates_huge_tool_results() {
    let big: String = "x".repeat(5000);
    let msgs = vec![sm(
        MessageRole::Assistant,
        "huge",
        vec![ToolCallEntry {
            id: "c".into(),
            name: "grep".into(),
            args: serde_json::json!({"pattern": "."}),
            state: ToolCallState::Completed { result: big },
        }],
    )];
    let out = summarize::serialize_conversation(&msgs);
    assert!(
        out.contains("(truncated, 5000 bytes total)"),
        "expected truncation marker; got: {out}"
    );
}

/// C6: a long full-conversation prefix is NOT truncated by the
/// caller-side 6000-char cap any more. compress_messages no
/// longer slices `conversation`; the full string reaches the
/// summarizer. Regression test the unchanged-passthrough via
/// serialize_conversation's length on a large input.
#[test]
fn serialize_conversation_returns_full_prefix() {
    let msgs: Vec<SessionMessage> = (0..200)
        .map(|i| sm(MessageRole::Assistant, &format!("turn {i}"), vec![]))
        .collect();
    let out = summarize::serialize_conversation(&msgs);
    // 200 turns × ~10 chars each = ~2000 chars; below the old
    // 6000 cap but the principle still holds: the function is
    // a pure mapper, no length cap. Confirm by checking the
    // last turn is present.
    assert!(out.contains("turn 199"), "tail must be present: {out}");
    assert!(out.contains("turn 0"), "head must be present: {out}");
}

// ============================================================
// PROV-1: Custom-provider validation tests
// ============================================================

/// Custom provider with https base_url is accepted.
#[test]
fn custom_provider_https_is_allowed() {
    let custom = std::collections::HashMap::from([(
        "my-proxy".to_string(),
        CustomProviderConfig {
            provider_type: "custom".to_string(),
            base_url: "https://my-proxy.example.com/v1".to_string(),
            api_key_env: None,
            allow_insecure: false,
            stream_chunk_timeout_secs: None,
        },
    )]);
    let result = resolve_provider_info("my-proxy", &custom);
    assert!(result.is_some(), "https provider should resolve");
}

/// Custom provider with http base_url is rejected unless allow_insecure.
#[test]
fn custom_provider_http_rejected_without_allow_insecure() {
    let custom = std::collections::HashMap::from([(
        "bad-proxy".to_string(),
        CustomProviderConfig {
            provider_type: "custom".to_string(),
            base_url: "http://bad-proxy.example.com/v1".to_string(),
            api_key_env: None,
            allow_insecure: false,
            stream_chunk_timeout_secs: None,
        },
    )]);
    let result = resolve_provider_info("bad-proxy", &custom);
    assert!(
        result.is_none(),
        "http provider without allow_insecure should be rejected"
    );
}

/// Custom provider with http base_url + allow_insecure: true is accepted.
#[test]
fn custom_provider_http_allowed_with_allow_insecure() {
    let custom = std::collections::HashMap::from([(
        "local-ollama".to_string(),
        CustomProviderConfig {
            provider_type: "custom".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key_env: None,
            allow_insecure: true,
            stream_chunk_timeout_secs: None,
        },
    )]);
    let result = resolve_provider_info("local-ollama", &custom);
    assert!(
        result.is_some(),
        "http provider with allow_insecure should be accepted"
    );
}

/// Custom provider name colliding with built-in is rejected.
#[test]
fn custom_provider_builtin_name_collision_rejected() {
    // Plugin tries to shadow "openai".
    let custom = std::collections::HashMap::from([(
        "openai".to_string(),
        CustomProviderConfig {
            provider_type: "custom".to_string(),
            base_url: "https://evil.example.com/v1".to_string(),
            api_key_env: None,
            allow_insecure: false,
            stream_chunk_timeout_secs: None,
        },
    )]);
    let result = resolve_provider_info("openai", &custom);
    assert!(
        result.is_none(),
        "builtin name collision should be rejected"
    );
}
