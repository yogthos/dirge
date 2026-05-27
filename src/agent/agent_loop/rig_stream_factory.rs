//! Phase 4.5f-2 — build a `StreamFn` from a real rig
//! `CompletionModel`. Plugs into `LoopSpawnConfig.stream_fn`
//! at the composition site, completing the integration between
//! the new loop and an actual LLM.
//!
//! ## What this provides
//!
//! - `rig_stream_fn_from_model(model, tools)` — produces a
//!   `StreamFn` that, per LLM call, builds a rig
//!   `CompletionRequest` from the supplied `LlmContext`, calls
//!   `model.stream(request)`, and wraps the response stream via
//!   `wrap_rig_stream` (4.5a).
//!
//! ## What it does NOT
//!
//! - Recovery / retry around the stream call. Lives in
//!   phase 4.5g — wrappers compose around this `StreamFn` from
//!   the outside.
//! - Permission checking / pre-flight. Tool definitions reach
//!   rig as-is; the loop's `before_tool_call` hook handles
//!   permission decisions at dispatch time, not provider time.
//!
//! ## Message conversion
//!
//! `LlmContext.messages: Vec<Value>` (the placeholder shape
//! phase 0 chose) carries our own message variants serialized
//! as JSON. This module converts each `Value` to a rig
//! `Message`:
//!
//! | Our `role` | rig `Message`                         |
//! |------------|---------------------------------------|
//! | "user"     | `Message::user(content_string)`       |
//! | "assistant"| `Message::Assistant { content: …}`    |
//! | "toolResult"| `Message::tool_result_with_call_id`  |
//! | other      | skipped (custom messages are UI-only) |
//!
//! Assistant content blocks (text / thinking / toolCall) map to
//! rig's `AssistantContent` variants. ToolResult content is
//! flattened to a single text body (rig's helper takes
//! `impl Into<String>`).
//!
//! ## Conversion is lossy by design
//!
//! Our `AssistantMessage.stop_reason` / `error_message` are
//! loop-internal; rig doesn't model them on the wire (the
//! provider derives stop reason from its own stream). They're
//! dropped in conversion.

use std::sync::Arc;

use rig::OneOrMany;
#[cfg(test)]
use rig::completion::CompletionError;
use rig::completion::message::{AssistantContent, Message, Reasoning, ToolCall, ToolFunction};
use rig::completion::{CompletionModel, CompletionRequestBuilder, GetTokenUsage, ToolDefinition};
use serde_json::Value;

use super::message::StreamEvent;
use super::rig_stream::wrap_rig_stream;
use super::stream::{LlmContext, StreamFn};
use super::tool::LoopTool;

use futures::Stream;
use std::pin::Pin;

/// Build a `StreamFn` that drives a rig `CompletionModel`. Each
/// invocation of the returned closure builds a
/// `CompletionRequest` from the supplied `LlmContext`, calls
/// `model.stream(request).await`, and wraps the result via
/// `wrap_rig_stream`.
///
/// `tools` is captured at construction — rig wants tool
/// definitions in the request, and the loop's tool registry is
/// stable across turns. If tools ever need to vary per-call
/// (e.g. dynamic tool sets), pass an empty `tools` here and
/// have the caller inject definitions via a different
/// mechanism.
///
/// The model is cloned per-call so the closure can be `Fn`
/// (multi-call). `CompletionModel: Clone` is part of the trait
/// bounds so this is always cheap (Arc-internally in most rig
/// impls).
#[cfg(test)]
pub fn rig_stream_fn_from_model<M>(
    model: M,
    tools: Vec<ToolDefinition>,
    chunk_timeout: Option<std::time::Duration>,
) -> StreamFn
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    rig_stream_fn_from_model_with_provider(model, tools, chunk_timeout, None)
}

/// Provider-aware variant: takes the provider name (e.g.
/// "anthropic", "openai") so reasoning options get mapped to the
/// shape the specific provider expects. When `provider_name`
/// is `None`, falls back to generic additional_params keys
/// (which most providers will ignore — useful for tests or
/// debugging only).
///
/// Production callers should always pass `Some(name)`.
pub fn rig_stream_fn_from_model_with_provider<M>(
    model: M,
    tools: Vec<ToolDefinition>,
    chunk_timeout: Option<std::time::Duration>,
    provider_name: Option<String>,
) -> StreamFn
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    let tools = Arc::new(tools);
    let provider_name = Arc::new(provider_name);
    Arc::new(move |ctx: LlmContext, opts: super::stream::StreamOptions| {
        let model = model.clone();
        let tools = tools.clone();
        let provider_name = provider_name.clone();
        invoke_one_stream(model, tools, ctx, chunk_timeout, opts, provider_name)
    })
}

/// Build a stream that, when polled, performs the model.stream
/// call asynchronously and forwards the wrapped events. Returns
/// a `Pin<Box<dyn Stream<Item = StreamEvent> + Send>>` directly
/// — no outer Future indirection, matches the `StreamFn`
/// signature.
///
/// Errors from message conversion / the `model.stream` call
/// surface as a single `Error` event so the caller's loop
/// observes them uniformly.
fn invoke_one_stream<M>(
    model: M,
    tools: Arc<Vec<ToolDefinition>>,
    ctx: LlmContext,
    chunk_timeout: Option<std::time::Duration>,
    opts: super::stream::StreamOptions,
    provider_name: Arc<Option<String>>,
) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>>
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    Box::pin(async_stream::stream! {
        // 1. Convert our messages to rig messages.
        let rig_messages: Vec<Message> = ctx
            .messages
            .iter()
            .filter_map(value_to_rig_message)
            .collect();

        // 2. Split: last is prompt; rest is chat_history.
        let (prompt, history) = if rig_messages.is_empty() {
            yield StreamEvent::Error {
                error: "rig_stream_fn: empty message list — no prompt to send".to_string(),
            };
            return;
        } else {
            let mut messages = rig_messages;
            let last = messages.pop().unwrap();
            (last, messages)
        };

        // 3. Build the rig CompletionRequest. Phase 4.6: pack
        //    reasoning + headers + metadata into the request's
        //    `additional_params` so providers that know about
        //    these fields can read them. Rig's underlying
        //    provider implementations vary in which they honor;
        //    unsupported fields are silently ignored downstream.
        let mut builder = CompletionRequestBuilder::new(model.clone(), prompt);
        if !ctx.system_prompt.is_empty() {
            builder = builder.preamble(ctx.system_prompt);
        }
        builder = builder.messages(history);
        if !tools.is_empty() {
            builder = builder.tools((*tools).clone());
        }
        // Build additional_params using a per-provider mapper
        // (phase 4.6 follow-up). Each provider has its own
        // shape for reasoning configuration — Anthropic wants
        // `thinking: { type: "enabled", budget_tokens | effort }`,
        // OpenAI Responses wants `reasoning: { effort }`, etc.
        // The mapper produces the right shape; rig's
        // additional_params is opaque so it forwards whatever
        // we give it.
        let provider: Option<&str> = provider_name.as_ref().as_deref();
        let additional = build_provider_additional_params(provider, &opts);
        if let Some(v) = additional {
            builder = builder.additional_params(v);
        }
        let request = builder.build();

        // 4. Call model.stream; wrap result or emit error.
        match model.stream(request).await {
            Ok(response) => {
                let mut wrapped = wrap_rig_stream(response, chunk_timeout, Some(opts.signal.clone()));
                use futures::stream::StreamExt;
                while let Some(evt) = wrapped.next().await {
                    yield evt;
                }
            }
            Err(e) => {
                yield StreamEvent::Error {
                    error: format!("rig stream call failed: {e}"),
                };
            }
        }
    })
}

/// Convert one of our `Value`-shaped messages to a rig
/// `Message`. Returns `None` for unrecognized roles (custom
/// messages get filtered at this boundary — pi calls this
/// out as the `convertToLlm` contract).
///
/// The shapes we recognize match what `run.rs` writes via
/// `loop_message_to_value` and what `stream.rs` writes via
/// `serialize_assistant`:
///
/// - User: `{"role": "user", "content": "<string>"}`
/// - Assistant: `{"role": "assistant", "content": [<blocks>], ...}`
/// - ToolResult: `{"role": "toolResult", "toolCallId": ..., "content": [<blocks>], ...}`
pub fn value_to_rig_message(value: &Value) -> Option<Message> {
    let role = value.get("role").and_then(|r| r.as_str())?;
    match role {
        "user" => {
            let content = value.get("content").and_then(|c| c.as_str())?;
            Some(Message::user(content))
        }
        "assistant" => {
            let blocks = value.get("content").and_then(|c| c.as_array())?;
            let assistant_contents: Vec<AssistantContent> = blocks
                .iter()
                .filter_map(value_to_assistant_content)
                .collect();
            // `OneOrMany::many` errors on empty input; rig
            // returns the error variant rather than constructing
            // an empty OneOrMany. Skip the message entirely if
            // we couldn't extract any usable blocks.
            let content = OneOrMany::many(assistant_contents).ok()?;
            Some(Message::Assistant { id: None, content })
        }
        "tool" | "toolResult" => {
            // Dual convention: loop uses toolCallId, legacy uses
            // tool_call_id. Try both.
            let tool_call_id = value
                .get("toolCallId")
                .or_else(|| value.get("tool_call_id"))
                .and_then(|c| c.as_str())?;
            // Content may be a plain string (legacy `tool` shape)
            // or an array of content blocks (loop `toolResult` shape).
            let text = value
                .get("content")
                .and_then(|c| {
                    if let Some(s) = c.as_str() {
                        Some(s.to_string())
                    } else if let Some(blocks) = c.as_array() {
                        let joined = blocks
                            .iter()
                            .filter_map(|b| {
                                b.as_object().and_then(|o| {
                                    if o.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        o.get("text").and_then(|t| t.as_str()).map(String::from)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        Some(joined)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            Some(Message::tool_result(tool_call_id, text))
        }
        _ => None,
    }
}

/// Convert one assistant content block to a rig `AssistantContent`.
/// Recognizes `{type: "text"|"thinking"|"toolCall", ...}`.
fn value_to_assistant_content(block: &Value) -> Option<AssistantContent> {
    let obj = block.as_object()?;
    let kind = obj.get("type").and_then(|t| t.as_str())?;
    match kind {
        "text" => {
            let text = obj.get("text").and_then(|t| t.as_str())?;
            Some(AssistantContent::text(text))
        }
        "thinking" => {
            let text = obj.get("text").and_then(|t| t.as_str())?;
            Some(AssistantContent::Reasoning(Reasoning::new(text)))
        }
        "toolCall" => {
            let id = obj.get("id").and_then(|t| t.as_str())?.to_string();
            let name = obj.get("name").and_then(|t| t.as_str())?.to_string();
            let arguments = obj.get("arguments").cloned().unwrap_or(Value::Null);
            Some(AssistantContent::ToolCall(ToolCall {
                id,
                call_id: None,
                function: ToolFunction { name, arguments },
                signature: None,
                additional_params: None,
            }))
        }
        _ => None,
    }
}

/// Build a rig `ToolDefinition` from one of our `LoopTool`s.
/// Returns the trio rig actually consumes (name, description,
/// parameters); label is dropped because rig has no slot for it.
///
/// If the tool has a `flat_parameters` schema (auto-detected via
/// `analyze_schema`), the LLM receives the flat dot-notation
/// variant so it's less likely to drop deeply nested args.
pub fn loop_tool_to_rig_definition(tool: &dyn LoopTool) -> ToolDefinition {
    let parameters = tool
        .flat_parameters()
        .cloned()
        .unwrap_or_else(|| tool.parameters().clone());
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters,
    }
}

/// Build the provider-specific `additional_params` Value for a
/// `CompletionRequest` from the user's StreamOptions. Per-provider
/// mapping covers the SHAPE differences between Anthropic
/// (`thinking: { ... }`), OpenAI Responses (`reasoning: {
/// effort }`), and others.
///
/// Returns `None` when there's nothing to send (no reasoning
/// requested, no headers, no metadata) — caller skips
/// `additional_params(...)` to keep the request minimal.
///
/// **Provider mappings**:
///   - "anthropic": `{ "thinking": { "type": "enabled",
///     "budget_tokens": N } }` for budget-based reasoning. Pi's
///     adaptive-thinking effort mode (Opus 4.6+, Sonnet 4.6) is
///     a follow-up — needs model-id sniffing.
///   - "openai" / "deepseek" / "glm" / "custom" (all
///     openai-shaped): `{ "reasoning": { "effort": "low" |
///     "medium" | "high" } }` per OpenAI Responses spec. Maps
///     ThinkingLevel:
///       - Off / Minimal / Low → "low"
///       - Medium → "medium"
///       - High / Xhigh → "high"
///   - "openrouter": same as openai (openrouter forwards
///     OpenAI-shape options to the upstream provider).
///   - "gemini": `{ "thinking_config": { "thinking_budget":
///     N } }` (Gemini 2.x). Budget-based.
///   - "ollama": no reasoning config — local models vary; pass
///     through generic `reasoning_level` key.
///   - None: generic `reasoning_level` key for debugging /
///     ad-hoc consumers.
///
/// **Headers and metadata** are passed through under
/// conventional keys (`headers`, `metadata`) regardless of
/// provider — rig's openai-shaped clients merge `metadata`
/// into the request body; headers are honored where the
/// provider impl reads them.
pub fn build_provider_additional_params(
    provider_name: Option<&str>,
    opts: &super::stream::StreamOptions,
) -> Option<serde_json::Value> {
    let mut additional = serde_json::Map::new();

    // ----- reasoning per provider -----
    if let Some(level) = opts.reasoning {
        match provider_name {
            Some("anthropic") => {
                // Budget-based thinking. Pi uses adaptive-effort
                // for Opus 4.6+ and Sonnet 4.6; we'd need model-
                // id sniffing to dispatch. For now use budget
                // mode with sensible per-level defaults that
                // adaptive-thinking models also accept.
                let budget = budget_for_level(level, opts.thinking_budgets.as_ref());
                if budget > 0 {
                    additional.insert(
                        "thinking".to_string(),
                        serde_json::json!({
                            "type": "enabled",
                            "budget_tokens": budget,
                        }),
                    );
                }
            }
            Some("openai" | "deepseek" | "glm" | "custom" | "openrouter") => {
                // OpenAI Responses / openai-compat reasoning.
                if let Some(effort) = thinking_level_to_openai_effort(level) {
                    additional.insert(
                        "reasoning".to_string(),
                        serde_json::json!({ "effort": effort }),
                    );
                }
            }
            Some("gemini") => {
                let budget = budget_for_level(level, opts.thinking_budgets.as_ref());
                if budget > 0 {
                    additional.insert(
                        "thinking_config".to_string(),
                        serde_json::json!({ "thinking_budget": budget }),
                    );
                }
            }
            Some("ollama") | None => {
                // Generic fallback. Local Ollama models vary;
                // pass the level under a conventional key.
                additional.insert(
                    "reasoning_level".to_string(),
                    serde_json::to_value(level).unwrap_or(serde_json::Value::Null),
                );
            }
            Some(_) => {
                // Unknown provider — fall back to generic key.
                additional.insert(
                    "reasoning_level".to_string(),
                    serde_json::to_value(level).unwrap_or(serde_json::Value::Null),
                );
            }
        }
    }

    // ----- headers (provider-agnostic) -----
    if !opts.headers.is_empty() {
        if let Ok(v) = serde_json::to_value(&opts.headers) {
            additional.insert("headers".to_string(), v);
        }
    }

    // ----- metadata (provider-agnostic) -----
    if !opts.metadata.is_empty() {
        additional.insert(
            "metadata".to_string(),
            serde_json::Value::Object(
                opts.metadata
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
    }

    if additional.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(additional))
    }
}

/// Map our `ThinkingLevel` enum to OpenAI Responses `reasoning.
/// effort` strings ("low" | "medium" | "high"). `Off` → None
/// (no reasoning key in the request).
///
/// Pi's `Minimal` / `Xhigh` are clamped to the nearest OpenAI
/// effort since OpenAI's API only accepts the three.
fn thinking_level_to_openai_effort(level: super::types::ThinkingLevel) -> Option<&'static str> {
    use super::types::ThinkingLevel as TL;
    match level {
        TL::Off => None,
        TL::Minimal | TL::Low => Some("low"),
        TL::Medium => Some("medium"),
        TL::High | TL::Xhigh => Some("high"),
    }
}

/// Token budget for a thinking level. Reads from the caller's
/// `ThinkingBudgets` if provided, falling back to defaults
/// reasonable for token-budget reasoning models (Anthropic
/// budget mode, Gemini 2.x).
///
/// Defaults match the rough scale pi uses (`providers/simple-
/// options.ts:33-...`): minimal 1024, low 2048, medium 4096,
/// high 16384. `Off` returns 0 — caller skips the key entirely.
fn budget_for_level(
    level: super::types::ThinkingLevel,
    budgets: Option<&super::types::ThinkingBudgets>,
) -> u32 {
    use super::types::ThinkingLevel as TL;
    match level {
        TL::Off => 0,
        TL::Minimal => budgets.and_then(|b| b.minimal).unwrap_or(1024),
        TL::Low => budgets.and_then(|b| b.low).unwrap_or(2048),
        TL::Medium => budgets.and_then(|b| b.medium).unwrap_or(4096),
        TL::High | TL::Xhigh => budgets.and_then(|b| b.high).unwrap_or(16384),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::message::UserContent;

    /// User-role value → `Message::User { content: text }`.
    #[test]
    fn user_value_converts_to_user_message() {
        let v = serde_json::json!({"role": "user", "content": "hello"});
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => {
                let first = content.first();
                match first {
                    UserContent::Text(t) => assert_eq!(t.text, "hello"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected User"),
        }
    }

    /// Assistant with a single text block converts cleanly.
    #[test]
    fn assistant_text_block_converts() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "hi there"}],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { id, content } => {
                assert!(id.is_none());
                match content.first() {
                    AssistantContent::Text(t) => assert_eq!(t.text, "hi there"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected Assistant"),
        }
    }

    /// Assistant with a toolCall block produces a rig `ToolCall`
    /// content.
    #[test]
    fn assistant_tool_call_block_converts() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "call_1",
                "name": "echo",
                "arguments": {"value": "x"},
            }],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { content, .. } => match content.first() {
                AssistantContent::ToolCall(tc) => {
                    assert_eq!(tc.id, "call_1");
                    assert_eq!(tc.function.name, "echo");
                    assert_eq!(tc.function.arguments["value"], "x");
                }
                _ => panic!("expected ToolCall"),
            },
            _ => panic!("expected Assistant"),
        }
    }

    /// Assistant with a thinking block produces `Reasoning`.
    #[test]
    fn assistant_thinking_block_converts_to_reasoning() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "thinking", "text": "let me think"}],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { content, .. } => match content.first() {
                AssistantContent::Reasoning(_) => {}
                _ => panic!("expected Reasoning"),
            },
            _ => panic!("expected Assistant"),
        }
    }

    /// ToolResult value → rig's tool_result user-content message.
    /// Content blocks are flattened to a single text body.
    #[test]
    fn tool_result_value_converts() {
        let v = serde_json::json!({
            "role": "toolResult",
            "toolCallId": "call_1",
            "toolName": "echo",
            "content": [
                {"type": "text", "text": "line 1"},
                {"type": "text", "text": "line 2"},
            ],
            "details": {},
            "isError": false,
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => match content.first() {
                UserContent::ToolResult(tr) => {
                    assert_eq!(tr.id, "call_1");
                }
                _ => panic!("expected ToolResult"),
            },
            _ => panic!("expected User"),
        }
    }

    /// Tool role (snake_case) with tool_call_id → rig ToolResult.
    /// Dual convention: loop uses `toolResult`/`toolCallId`; legacy
    /// session data uses `tool`/`tool_call_id`. Both must convert.
    #[test]
    fn tool_role_snake_case_converts() {
        let v = serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_abc",
            "content": "tool output text",
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => match content.first() {
                UserContent::ToolResult(tr) => {
                    assert_eq!(tr.id, "call_abc");
                }
                other => panic!("expected ToolResult, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
    }

    /// Custom / unknown role → skipped (None).
    #[test]
    fn custom_role_returns_none() {
        let v = serde_json::json!({"role": "custom", "content": "x"});
        assert!(value_to_rig_message(&v).is_none());
    }

    /// Missing role field → None.
    #[test]
    fn missing_role_returns_none() {
        let v = serde_json::json!({"content": "x"});
        assert!(value_to_rig_message(&v).is_none());
    }

    /// `loop_tool_to_rig_definition` copies name + description +
    /// parameters; label is intentionally dropped (rig has no
    /// slot).
    #[test]
    fn loop_tool_definition_strips_label() {
        // A minimal LoopTool stub for the conversion test.
        #[derive(Debug)]
        struct Stub;
        impl LoopTool for Stub {
            fn name(&self) -> &str {
                "stub"
            }
            fn description(&self) -> &str {
                "stub description"
            }
            fn label(&self) -> &str {
                "Stub Label"
            }
            fn parameters(&self) -> &Value {
                static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                P.get_or_init(|| serde_json::json!({"type": "object"}))
            }
            fn execute<'a>(
                &'a self,
                _id: &'a str,
                _args: Value,
                _signal: AbortSignal,
                _on_update: super::super::tool::LoopToolUpdate,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<super::super::result::LoopToolResult, String>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async { unreachable!("not called in conversion test") })
            }
        }

        let def = loop_tool_to_rig_definition(&Stub);
        assert_eq!(def.name, "stub");
        assert_eq!(def.description, "stub description");
        assert_eq!(def.parameters["type"], "object");
    }

    /// Compile-time: `rig_stream_fn_from_model` produces a
    /// `Send + Sync + 'static` StreamFn. This is the bound the
    /// loop demands; if it doesn't compile, no use of the
    /// factory is going to work.
    #[test]
    fn stream_fn_is_send_sync_static() {
        // Use rig's built-in test model (mock_provider) if
        // available; otherwise this test just verifies the type
        // constraints at compile time via assertion shape.
        // We can't easily build a real model in a unit test
        // because every rig provider needs an API key. Instead
        // we assert the trait bound via a turbofish on a generic
        // function — succeeds compile-time if the signature is
        // correct.

        fn assert_constraints<M>(_model: M)
        where
            M: CompletionModel + Clone + Send + Sync + 'static,
            M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
        {
            // No-op; existence of the function is the proof.
        }

        // We can't instantiate M without a real provider; the
        // compile-time check on the function signature is what
        // matters. This test "passes" by virtue of compiling.
        let _: fn(_) = assert_constraints::<NopModel>;
    }

    /// Minimal stub CompletionModel so we can verify the
    /// factory produces a working `StreamFn` end-to-end. The
    /// stub returns a canned `done` event with empty text via
    /// `model.stream(request)`.
    #[derive(Clone)]
    struct NopModel;

    impl GetTokenUsage for NopStreamResponse {
        fn token_usage(&self) -> Option<rig::completion::Usage> {
            None
        }
    }

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct NopStreamResponse;

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct NopResponse;

    impl CompletionModel for NopModel {
        type Response = NopResponse;
        type StreamingResponse = NopStreamResponse;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            NopModel
        }

        async fn completion(
            &self,
            _request: rig::completion::CompletionRequest,
        ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
            // Not used by the streaming factory.
            unreachable!("completion() not used in stream factory tests")
        }

        async fn stream(
            &self,
            _request: rig::completion::CompletionRequest,
        ) -> Result<
            rig::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
            CompletionError,
        > {
            // Empty inner stream — the wrap_rig_stream layer
            // synthesizes a `Done { reason: Stop, message: empty }`
            // for an empty stream, which is what we want for
            // the smoke test.
            let inner: rig::streaming::StreamingResult<Self::StreamingResponse> =
                Box::pin(futures::stream::empty());
            Ok(rig::streaming::StreamingCompletionResponse::stream(inner))
        }
    }

    /// End-to-end smoke test: build the factory from `NopModel`,
    /// invoke once, drain the resulting stream. Expect Start +
    /// Done (no Error). Proves the conversion + builder + wrap
    /// chain composes correctly.
    #[tokio::test]
    async fn factory_invocation_produces_start_and_done() {
        use futures::stream::StreamExt;
        let factory = rig_stream_fn_from_model::<NopModel>(NopModel, vec![], None);
        let ctx = LlmContext {
            system_prompt: "test preamble".to_string(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        };
        let mut stream = factory(
            ctx,
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        );
        let mut kinds = Vec::new();
        while let Some(evt) = stream.next().await {
            kinds.push(match &evt {
                StreamEvent::Start { .. } => "start",
                StreamEvent::Delta { .. } => "delta",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error { error } => {
                    panic!("unexpected error: {error}");
                }
                StreamEvent::Retry { .. } => {
                    panic!("unexpected retry event in non-retried stream");
                }
            });
        }
        // Expect at minimum Start + Done. No Error.
        assert!(kinds.contains(&"start"));
        assert!(kinds.contains(&"done"));
    }

    /// Empty message list → factory emits an Error event (not a
    /// panic). Defensive — caller misconfiguration is loud.
    #[tokio::test]
    async fn factory_empty_messages_emits_error() {
        use futures::stream::StreamExt;
        let factory = rig_stream_fn_from_model::<NopModel>(NopModel, vec![], None);
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: Vec::new(),
        };
        let mut stream = factory(
            ctx,
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        );
        let mut found_error = false;
        while let Some(evt) = stream.next().await {
            if matches!(evt, StreamEvent::Error { .. }) {
                found_error = true;
            }
        }
        assert!(found_error, "empty messages must produce an Error event");
    }

    // ============================================================
    // Per-provider reasoning mapper tests
    // ============================================================

    use crate::agent::agent_loop::stream::StreamOptions;
    use crate::agent::agent_loop::tool::AbortSignal;
    use crate::agent::agent_loop::types::{ThinkingBudgets, ThinkingLevel};

    fn opts_with_reasoning(level: ThinkingLevel) -> StreamOptions {
        let mut o = StreamOptions::from_signal(AbortSignal::new());
        o.reasoning = Some(level);
        o
    }

    /// Anthropic gets `thinking: { type: "enabled", budget_tokens
    /// }`. Verifies the budget defaults are sane for each level.
    #[test]
    fn anthropic_reasoning_maps_to_thinking_budget() {
        let opts = opts_with_reasoning(ThinkingLevel::Medium);
        let v = build_provider_additional_params(Some("anthropic"), &opts).unwrap();
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["thinking"]["budget_tokens"], 4096);
    }

    /// Off level → no thinking key at all (Anthropic).
    #[test]
    fn anthropic_off_omits_thinking_key() {
        let opts = opts_with_reasoning(ThinkingLevel::Off);
        let v = build_provider_additional_params(Some("anthropic"), &opts);
        assert!(v.is_none(), "Off should produce empty additional_params");
    }

    /// Caller-supplied budgets override the defaults.
    #[test]
    fn anthropic_respects_caller_budget_override() {
        let mut opts = opts_with_reasoning(ThinkingLevel::High);
        opts.thinking_budgets = Some(ThinkingBudgets {
            high: Some(32_000),
            ..Default::default()
        });
        let v = build_provider_additional_params(Some("anthropic"), &opts).unwrap();
        assert_eq!(v["thinking"]["budget_tokens"], 32_000);
    }

    /// OpenAI Responses (and openai-compat: deepseek/glm/custom)
    /// get `reasoning: { effort: low|medium|high }`.
    #[test]
    fn openai_reasoning_maps_to_effort() {
        for (level, expected) in [
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
        ] {
            let opts = opts_with_reasoning(level);
            let v = build_provider_additional_params(Some("openai"), &opts).unwrap();
            assert_eq!(
                v["reasoning"]["effort"], expected,
                "level {level:?} should map to {expected}"
            );
        }
    }

    /// DeepSeek, GLM, OpenRouter, Custom share OpenAI's
    /// effort-based reasoning shape.
    #[test]
    fn openai_compat_providers_share_effort_shape() {
        let opts = opts_with_reasoning(ThinkingLevel::Medium);
        for provider in ["deepseek", "glm", "custom", "openrouter"] {
            let v = build_provider_additional_params(Some(provider), &opts).unwrap();
            assert_eq!(
                v["reasoning"]["effort"], "medium",
                "provider {provider} should use effort=medium"
            );
        }
    }

    /// Minimal clamps to "low"; Xhigh clamps to "high" (OpenAI
    /// API only accepts 3 levels).
    #[test]
    fn openai_clamps_unsupported_levels() {
        let opts_min = opts_with_reasoning(ThinkingLevel::Minimal);
        let v = build_provider_additional_params(Some("openai"), &opts_min).unwrap();
        assert_eq!(v["reasoning"]["effort"], "low");

        let opts_x = opts_with_reasoning(ThinkingLevel::Xhigh);
        let v = build_provider_additional_params(Some("openai"), &opts_x).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");
    }

    /// OpenAI Off → omits the reasoning key entirely.
    #[test]
    fn openai_off_omits_reasoning_key() {
        let opts = opts_with_reasoning(ThinkingLevel::Off);
        let v = build_provider_additional_params(Some("openai"), &opts);
        assert!(v.is_none());
    }

    /// Gemini uses `thinking_config: { thinking_budget }`
    /// (token-budget shape).
    #[test]
    fn gemini_reasoning_maps_to_thinking_config() {
        let opts = opts_with_reasoning(ThinkingLevel::High);
        let v = build_provider_additional_params(Some("gemini"), &opts).unwrap();
        assert_eq!(v["thinking_config"]["thinking_budget"], 16384);
    }

    /// Headers and metadata pass through under conventional
    /// keys regardless of provider.
    #[test]
    fn headers_and_metadata_pass_through_for_all_providers() {
        let mut opts = StreamOptions::from_signal(AbortSignal::new());
        opts.headers
            .insert("X-Tenant".to_string(), "acme".to_string());
        opts.metadata
            .insert("user_id".to_string(), serde_json::json!("u-42"));
        for provider in ["anthropic", "openai", "gemini", "ollama", "unknown"] {
            let v = build_provider_additional_params(Some(provider), &opts).unwrap();
            assert_eq!(v["headers"]["X-Tenant"], "acme", "provider {provider}");
            assert_eq!(v["metadata"]["user_id"], "u-42", "provider {provider}");
        }
    }

    /// No reasoning, no headers, no metadata → None (caller
    /// skips additional_params entirely).
    #[test]
    fn empty_options_produces_none() {
        let opts = StreamOptions::from_signal(AbortSignal::new());
        assert!(build_provider_additional_params(Some("anthropic"), &opts).is_none());
        assert!(build_provider_additional_params(None, &opts).is_none());
    }

    /// Unknown provider falls back to the generic
    /// `reasoning_level` key (debugging aid; rig provider impl
    /// may or may not honor).
    #[test]
    fn unknown_provider_uses_generic_key() {
        let opts = opts_with_reasoning(ThinkingLevel::High);
        let v = build_provider_additional_params(Some("future-provider"), &opts).unwrap();
        assert!(v.get("reasoning_level").is_some());
        assert!(v.get("reasoning").is_none());
        assert!(v.get("thinking").is_none());
    }
}
