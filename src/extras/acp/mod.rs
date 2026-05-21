pub mod config;

use std::sync::Arc;

use agent_client_protocol::on_receive_request;
use agent_client_protocol::schema::*;
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Responder, Stdio};

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::event::AgentEvent;
use crate::permission::ask::AskSender;
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{PermissionConfig, SecurityMode};
use crate::sandbox::Sandbox;

struct AcpState {
    cli: Cli,
    cfg: Config,
    context: ContextFiles,
}

pub async fn serve(cli: Cli, cfg: Config, context: ContextFiles) -> anyhow::Result<()> {
    let state = Arc::new(AcpState { cli, cfg, context });

    Agent
        .builder()
        .name("dirge")
        .on_receive_request(
            {
                let state = state.clone();
                move |req: InitializeRequest, responder, _cx| {
                    let state = state.clone();
                    async move { handle_initialize(req, responder, &state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                move |req: NewSessionRequest, responder, cx| {
                    let state = state.clone();
                    async move { handle_new_session(req, responder, cx, &state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                move |req: PromptRequest, responder, cx| {
                    let state = state.clone();
                    async move { handle_prompt(req, responder, cx, state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_dispatch(
            |dispatch: Dispatch<AgentRequest, AgentNotification>, cx: ConnectionTo<Client>| {
                async move {
                    dispatch.respond_with_error(
                        agent_client_protocol::util::internal_error("Unhandled ACP message"),
                        cx,
                    )
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
        .map_err(|e| anyhow::anyhow!("ACP server error: {}", e))?;

    Ok(())
}

async fn handle_initialize(
    req: InitializeRequest,
    responder: Responder<InitializeResponse>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let _ = state;

    let caps = AgentCapabilities::new();

    let resp = InitializeResponse::new(req.protocol_version)
        .agent_capabilities(caps)
        .agent_info(Implementation::new("dirge", "1.0.4"));

    responder.respond(resp)
}

async fn handle_new_session(
    req: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    _cx: ConnectionTo<Client>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());

    tracing::info!(
        "ACP new session: {} (cwd: {})",
        session_id,
        req.cwd.display()
    );

    let _ = state;

    let resp = NewSessionResponse::new(session_id);
    responder.respond(resp)
}

async fn handle_prompt(
    req: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    state: Arc<AcpState>,
) -> Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.clone();

    tracing::info!("ACP prompt for session {}", session_id);

    let prompt_text = req
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    cx.spawn({
        let cx = cx.clone();
        async move { run_prompt(&state, &prompt_text, session_id, responder, cx).await }
    })
}

async fn run_prompt(
    state: &AcpState,
    prompt_text: &str,
    session_id: SessionId,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), agent_client_protocol::Error> {
    let provider_str = state.cli.resolve_provider(&state.cfg);
    let model_str = if state.cli.model.is_none() && state.cfg.model.is_none() {
        compact_str::CompactString::new(crate::provider::default_model_for(&provider_str))
    } else {
        state.cli.resolve_model(&state.cfg)
    };

    let client =
        crate::provider::create_client(&provider_str, None, &state.cfg.custom_providers_map())
            .map_err(|e| agent_client_protocol::Error::new(-32603, e.to_string()))?;

    let model = client.completion_model(model_str.to_string());

    let (permission, ask_tx) = build_acp_permission(state);
    let sandbox = Sandbox::new(state.cli.resolve_sandbox(&state.cfg));

    let agent = crate::provider::build_agent(
        model,
        &state.cli,
        &state.cfg,
        &state.context,
        permission,
        ask_tx,
        None,
        None,
        None,
        #[cfg(feature = "lsp")]
        None,
        sandbox,
        #[cfg(feature = "mcp")]
        None::<&crate::extras::mcp::McpClientManager>,
        #[cfg(feature = "semantic")]
        None::<&crate::semantic::SemanticManager>,
    )
    .await;

    let runner = agent.spawn_runner(prompt_text.to_string(), vec![]);
    let mut rx = runner.event_rx;

    // F5: correlate rig tool-call ids with ACP ids so parallel
    // calls pair with their results correctly. See
    // `ToolCallCorrelator` doc for the dual-mode logic.
    let mut correlator = ToolCallCorrelator::default();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Token(text) => {
                let chunk =
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(chunk),
                );
                let _ = cx.send_notification(notif);
            }
            AgentEvent::Reasoning(text) => {
                let chunk =
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentThoughtChunk(chunk),
                );
                let _ = cx.send_notification(notif);
            }
            AgentEvent::ToolCall { id, name, args } => {
                let args_str = args.to_string();
                let acp_id = ToolCallId::new(uuid::Uuid::new_v4().to_string());
                correlator.record(id.as_str(), acp_id.clone());
                let tool_call = ToolCall::new(acp_id, name.to_string())
                    .raw_input(serde_json::from_str(&args_str).ok());
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCall(tool_call),
                );
                let _ = cx.send_notification(notif);
            }
            AgentEvent::ToolResult { id, output } => {
                let id = correlator
                    .resolve(id.as_str())
                    .unwrap_or_else(|| ToolCallId::new(String::new()));
                let fields = ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(output.to_string()),
                    ))]);
                let update = ToolCallUpdate::new(id, fields);
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCallUpdate(update),
                );
                let _ = cx.send_notification(notif);
            }
            AgentEvent::Done { .. } => {
                break;
            }
            AgentEvent::Error(_) => {
                break;
            }
            // Observability markers added for the interactive UI's
            // turn tracker + interjection queue. ACP doesn't have a
            // mid-stream interjection concept (the client owns
            // submission), so we treat these as no-ops.
            AgentEvent::TurnStart { .. } | AgentEvent::TurnEnd { .. } => {}
            AgentEvent::Interjected { .. } => {
                // An interjected turn shouldn't reach the ACP bridge —
                // ACP runs aren't interactive — but bail cleanly if
                // one does rather than panic on partial state.
                break;
            }
        }
    }

    let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
    Ok(())
}

fn build_acp_permission(state: &AcpState) -> (Option<PermCheck>, Option<AskSender>) {
    use std::sync::Mutex;

    let no_tools = state.cli.resolve_no_tools(&state.cfg);
    if no_tools {
        return (None, None);
    }

    let perm_config: PermissionConfig = state
        .cfg
        .permission
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let mode = resolve_acp_mode(&state.cli, &state.cfg);
    let checker = PermissionChecker::new(&perm_config, mode, None);
    let perm: PermCheck = Arc::new(Mutex::new(checker));

    let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(64);
    spawn_acp_ask_drain(ask_rx);
    (Some(perm), Some(ask_tx))
}

/// Two-mode correlator for matching rig `ToolResult` events back
/// to their originating `ToolCall` event when bridging to ACP.
/// Most providers (Anthropic, OpenAI) emit a stable `tool_call.id`
/// on the request and re-emit it on the result → use the id map.
/// Some providers (older OpenAI compat models) emit empty ids →
/// fall back to FIFO since rig emits results in request order.
///
/// Extracted from `run_prompt` so the F5 fix is unit-testable
/// without standing up a full ACP server.
#[derive(Default)]
struct ToolCallCorrelator {
    by_id: std::collections::HashMap<String, ToolCallId>,
    fifo: std::collections::VecDeque<ToolCallId>,
}

impl ToolCallCorrelator {
    /// Record a new `(rig_id → acp_id)` mapping. Empty rig_id
    /// pushes onto the FIFO queue.
    fn record(&mut self, rig_id: &str, acp_id: ToolCallId) {
        if rig_id.is_empty() {
            self.fifo.push_back(acp_id);
        } else {
            self.by_id.insert(rig_id.to_string(), acp_id);
        }
    }

    /// Resolve a result's rig_id to the originally-issued acp_id.
    /// Returns `None` if no matching call is in-flight; callers
    /// emit a stub empty id in that (shouldn't-happen) case.
    fn resolve(&mut self, rig_id: &str) -> Option<ToolCallId> {
        if !rig_id.is_empty() {
            self.by_id.remove(rig_id)
        } else {
            self.fifo.pop_front()
        }
    }
}

/// Drain `ask_rx` by responding to every permission ask with
/// `Deny`. ACP runs are non-interactive — there's no human at a
/// keyboard to confirm prompts. Previously the receiver was simply
/// dropped (`_ask_rx`), so any tool needing `Ask` confirmation
/// hit the 30s permission timeout and surfaced as a generic
/// failure to the editor client. Fail-fast with a clear deny is
/// strictly better: the LLM sees the denial immediately and can
/// re-plan, or the user can configure explicit allow rules.
///
/// **Future work**: route the ask through the ACP protocol as a
/// `requestPermission` notification so the editor client can
/// surface a real dialog. Out of scope for the F1 fix; that's a
/// Phase C5-ish feature requiring ACP protocol wiring.
fn spawn_acp_ask_drain(
    mut ask_rx: tokio::sync::mpsc::Receiver<crate::permission::ask::AskRequest>,
) {
    tokio::spawn(async move {
        while let Some(req) = ask_rx.recv().await {
            // The tool's caller is awaiting on `req.reply`. Dropping
            // it without sending would also surface as a tool error
            // ("Permission system unavailable"), but Deny is a
            // clearer signal that the call was *refused* rather
            // than the system being broken.
            let _ = req.reply.send(crate::permission::ask::UserDecision::Deny);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for F1: any `AskRequest` sent through the ACP
    /// ask channel must be promptly responded to with `Deny`,
    /// rather than hanging. Without `spawn_acp_ask_drain`, the
    /// `reply` oneshot is dropped on receiver drop → tool sees
    /// `Permission system unavailable` (technically OK, but slower
    /// and worse signal). With the drain, the tool sees `Deny`
    /// within a tick.
    #[tokio::test]
    async fn acp_ask_drain_responds_with_deny() {
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel::<crate::permission::ask::AskRequest>(8);
        spawn_acp_ask_drain(ask_rx);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        ask_tx
            .send(crate::permission::ask::AskRequest {
                tool: "bash".to_string(),
                input: "rm -rf /".to_string(),
                reply: reply_tx,
            })
            .await
            .expect("send must succeed");

        let resp = tokio::time::timeout(std::time::Duration::from_millis(200), reply_rx)
            .await
            .expect("must reply within 200ms — F1 regression")
            .expect("reply channel must not be dropped");
        assert!(
            matches!(resp, crate::permission::ask::UserDecision::Deny),
            "ACP ask must auto-deny; got {:?}",
            resp,
        );
    }

    /// F5: two parallel tool calls with distinct rig ids → two
    /// results MUST pair with the right ACP ids. Previously the
    /// `last_tool_call_id` single-slot lost the first id when the
    /// second call arrived.
    #[test]
    fn correlator_matches_parallel_tool_calls_by_id() {
        let mut c = ToolCallCorrelator::default();
        let acp_a = ToolCallId::new("acp-A".to_string());
        let acp_b = ToolCallId::new("acp-B".to_string());
        c.record("rig-A", acp_a.clone());
        c.record("rig-B", acp_b.clone());

        // Results can arrive in either order.
        assert_eq!(c.resolve("rig-B"), Some(acp_b));
        assert_eq!(c.resolve("rig-A"), Some(acp_a));
    }

    /// Provider-empty ids fall to the FIFO queue, preserving
    /// request order (rig emits results in dispatch order for
    /// providers that don't supply ids).
    #[test]
    fn correlator_uses_fifo_for_empty_rig_ids() {
        let mut c = ToolCallCorrelator::default();
        let acp_a = ToolCallId::new("acp-A".to_string());
        let acp_b = ToolCallId::new("acp-B".to_string());
        c.record("", acp_a.clone());
        c.record("", acp_b.clone());

        // First result pairs with first call; second with second.
        assert_eq!(c.resolve(""), Some(acp_a));
        assert_eq!(c.resolve(""), Some(acp_b));
    }

    /// Mixed: an id'd call alongside an empty-id call. Each falls
    /// to its respective bucket — no cross-contamination.
    #[test]
    fn correlator_separates_id_and_fifo_buckets() {
        let mut c = ToolCallCorrelator::default();
        let acp_named = ToolCallId::new("acp-named".to_string());
        let acp_anon = ToolCallId::new("acp-anon".to_string());
        c.record("rig-X", acp_named.clone());
        c.record("", acp_anon.clone());

        assert_eq!(c.resolve(""), Some(acp_anon));
        assert_eq!(c.resolve("rig-X"), Some(acp_named));
    }

    /// Stray result (no matching call) → resolve returns None;
    /// the caller can choose a stub id. Don't panic.
    #[test]
    fn correlator_returns_none_for_unknown_id() {
        let mut c = ToolCallCorrelator::default();
        assert_eq!(c.resolve("missing"), None);
        assert_eq!(c.resolve(""), None);
    }

    /// Multiple concurrent asks all get responded to.
    #[tokio::test]
    async fn acp_ask_drain_handles_multiple_concurrent_asks() {
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel::<crate::permission::ask::AskRequest>(8);
        spawn_acp_ask_drain(ask_rx);

        let mut replies = Vec::new();
        for i in 0..5 {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            ask_tx
                .send(crate::permission::ask::AskRequest {
                    tool: format!("bash-{i}"),
                    input: format!("cmd-{i}"),
                    reply: reply_tx,
                })
                .await
                .unwrap();
            replies.push(reply_rx);
        }

        for reply_rx in replies {
            let resp = tokio::time::timeout(std::time::Duration::from_millis(500), reply_rx)
                .await
                .expect("each reply must arrive promptly")
                .expect("reply channel dropped");
            assert!(matches!(resp, crate::permission::ask::UserDecision::Deny));
        }
    }
}

fn resolve_acp_mode(cli: &Cli, cfg: &Config) -> SecurityMode {
    if cli.yolo || cfg.yolo.unwrap_or(false) {
        SecurityMode::Yolo
    } else if cli.accept_all || cfg.accept_all.unwrap_or(false) {
        SecurityMode::Accept
    } else if cli.restrictive || cfg.restrictive.unwrap_or(false) {
        SecurityMode::Restrictive
    } else if let Some(m) = &cfg.default_permission_mode {
        match m.as_str() {
            "yolo" => SecurityMode::Yolo,
            "accept" => SecurityMode::Accept,
            "restrictive" => SecurityMode::Restrictive,
            _ => SecurityMode::Standard,
        }
    } else {
        SecurityMode::Standard
    }
}
