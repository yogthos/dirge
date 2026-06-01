use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::agent_loop::tool::AbortSignal;
use crate::agent::tools::background::{BackgroundStore, TaskState};
use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::provider::AnyModel;

/// dirge-ov2 Phase D: subagent chat-window event. Sent by `TaskTool`
/// when it spawns / completes a subagent so the UI loop can surface
/// the subagent's lifecycle as a chat-window (Ctrl-N/P/X to switch
/// to it via the multi-chat infrastructure landed in Phases A-C).
///
/// `id` is the subagent's task id (UUID for background tasks; a
/// freshly-generated UUID for foreground tasks). The UI loop keys
/// chat windows on this id so multiple concurrent subagents get
/// distinct windows.
///
/// First-pass design: prompt + final result are emitted; per-token
/// streaming isn't wired through. A follow-up will route the full
/// agent-loop event stream once `TaskTool` migrates from `btw_query`
/// (one-shot, tool-less) to a proper sub-runner with the parent's
/// tool set. Phase A-C laid the multi-chat infrastructure that
/// rewrite needs; Phase D ships visibility today.
#[derive(Debug, Clone)]
// dirge-781c: Reasoning / ToolCall / ToolResult variants are part of
// the streaming surface the chat-tab routes; production producers
// (`btw_query`-based foreground/background subagents) emit only
// Token + Complete + Failed + Aborted today. Sub-runner migration
// will fire the rest. The `Complete.result` field is also kept for
// the same reason — the UI handler currently reads only `id` (the
// Token event carries the text) but a future runner can populate
// it with the final assembled reply when a separate Token stream
// isn't used.
#[allow(dead_code)]
pub enum SubagentChatEvent {
    /// A new subagent is starting. UI loop creates a chat window
    /// named after a short truncation of the prompt and writes the
    /// prompt as the first line.
    Spawn { id: String, prompt: String },
    /// Subagent finished successfully. UI loop writes `result` to
    /// the matching chat window.
    Complete { id: String, result: String },
    /// Subagent errored or timed out. UI loop writes the failure
    /// reason in error color.
    Failed { id: String, error: String },
    /// dirge-781c: streaming assistant token from the subagent.
    /// Currently emitted as a single chunk when `btw_query` returns
    /// (one-shot model has no per-token stream); when the task tool
    /// migrates to a sub-runner this fires per chunk so the user can
    /// watch the reply build up in the subagent's chat slot.
    Token { id: String, text: String },
    /// dirge-781c: streaming reasoning text from the subagent.
    /// Renders dim to mirror the parent chat's reasoning style.
    Reasoning { id: String, text: String },
    /// dirge-781c: subagent emitted a tool call. `args_summary` is a
    /// short, human-readable rendering of the args (one-liner).
    ToolCall {
        id: String,
        tool_name: String,
        args_summary: String,
    },
    /// dirge-781c: subagent tool result. `output_summary` is a short
    /// human-readable preview (single line, truncated) so the tab
    /// shows progress without dumping multi-KB blobs.
    ToolResult {
        id: String,
        tool_name: String,
        output_summary: String,
    },
    /// dirge-781c: subagent was killed via `/kill` or Ctrl+K. UI
    /// writes `(aborted)` to the matching chat slot.
    Aborted { id: String },
}

/// dirge-02tn: subagent chat events are DISPLAY-ONLY — the subagent's
/// real result returns through the normal tool-result path, not this
/// channel. So the channel is BOUNDED and producers use `try_send`:
/// under a sustained UI stall the live chat view degrades (a few dropped
/// tokens/updates) but memory stays bounded and correctness is
/// unaffected. 1024 is generous — normal streaming never fills it.
pub const SUBAGENT_CHAT_CAP: usize = 1024;

pub type SubagentChatSender = mpsc::Sender<SubagentChatEvent>;

/// Receiver side of the subagent chat-event channel — exposed for
/// the UI loop's `tokio::select!` arm. Only consumed in main.rs +
/// ui/mod.rs; marked `dead_code`-allow because the producer side
/// (TaskTool) lives in this module and `cargo check` sees only the
/// definition site, not the cross-module consumer.
#[allow(dead_code)]
pub type SubagentChatReceiver = mpsc::Receiver<SubagentChatEvent>;

/// dirge-ov2 Phase D: process-global sender for subagent chat
/// events. Set once at interactive-session startup; every TaskTool
/// reads it lazily so the builder doesn't need to thread the
/// channel through 13 call sites.
///
/// A follow-up could replace this with proper threading through
/// `BuilderContext` — for now the global keeps the Phase D diff
/// small and the test path (no global set) behaves like pre-ov2.
static SUBAGENT_CHAT_SINK: std::sync::OnceLock<SubagentChatSender> = std::sync::OnceLock::new();

pub fn set_subagent_chat_sink(sink: SubagentChatSender) {
    // OnceLock — first writer wins. Re-set is a no-op (logged via
    // tracing for visibility but not fatal because tests / hot
    // reload may try to set twice).
    if SUBAGENT_CHAT_SINK.set(sink).is_err() {
        tracing::debug!("subagent chat sink already set; ignoring re-set");
    }
}

pub fn subagent_chat_sink() -> Option<SubagentChatSender> {
    SUBAGENT_CHAT_SINK.get().cloned()
}

/// dirge-781c: process-global registry mapping in-flight subagent ids
/// to their `AbortSignal`. Populated when a `TaskTool::call` spawns a
/// subagent; cleared on terminal events (complete / failed / aborted).
///
/// Used by `/kill <id-prefix>` and Ctrl+K to find a live subagent and
/// trigger its abort signal. The map is keyed on the FULL subagent id
/// (UUID for background, freshly-minted UUID for foreground). Prefix
/// resolution lives in `kill_subagent`.
static SUBAGENT_ABORT_REGISTRY: std::sync::OnceLock<Mutex<HashMap<String, AbortSignal>>> =
    std::sync::OnceLock::new();

fn abort_registry() -> &'static Mutex<HashMap<String, AbortSignal>> {
    SUBAGENT_ABORT_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a subagent's abort signal so `/kill` can find it later.
/// Idempotent — re-registering replaces the previous signal (which
/// shouldn't happen in practice since ids are fresh UUIDs).
pub fn register_subagent_abort(id: &str, signal: AbortSignal) {
    let mut map = abort_registry().lock().unwrap_or_else(|e| e.into_inner());
    map.insert(id.to_string(), signal);
}

/// Remove a subagent's abort entry. Called at terminal lifecycle
/// events so the registry doesn't accumulate stale ids.
pub fn unregister_subagent_abort(id: &str) {
    let mut map = abort_registry().lock().unwrap_or_else(|e| e.into_inner());
    map.remove(id);
}

/// Outcome of a `/kill <id-prefix>` resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    /// No in-flight subagent matched the prefix.
    NotFound,
    /// Multiple in-flight subagents matched the prefix — ambiguous;
    /// the caller should ask the user to supply more characters.
    /// Carries the matching full ids so the UI can list them.
    Ambiguous(Vec<String>),
    /// Exactly one match — its `AbortSignal::cancel()` was triggered.
    /// Carries the full id so the UI can echo back what got killed.
    Killed(String),
}

/// Resolve `id_prefix` against the in-flight subagent registry and,
/// when it matches exactly one entry, fire the abort signal.
///
/// Resolution rules:
///   - Empty prefix → `NotFound` (refuse to kill blindly).
///   - Exact match on a full id → kill that one even if other ids
///     also start with the same string.
///   - One id starts with the prefix → kill it.
///   - Multiple ids start with the prefix → `Ambiguous` (no-op).
///   - Zero matches → `NotFound`.
pub fn kill_subagent(id_prefix: &str) -> KillOutcome {
    let trimmed = id_prefix.trim();
    if trimmed.is_empty() {
        return KillOutcome::NotFound;
    }
    let map = abort_registry().lock().unwrap_or_else(|e| e.into_inner());
    // Exact match wins outright.
    if let Some(sig) = map.get(trimmed) {
        sig.cancel();
        return KillOutcome::Killed(trimmed.to_string());
    }
    let matches: Vec<String> = map
        .keys()
        .filter(|k| k.starts_with(trimmed))
        .cloned()
        .collect();
    match matches.len() {
        0 => KillOutcome::NotFound,
        1 => {
            let id = matches.into_iter().next().unwrap();
            if let Some(sig) = map.get(&id) {
                sig.cancel();
            }
            KillOutcome::Killed(id)
        }
        _ => KillOutcome::Ambiguous(matches),
    }
}

/// Snapshot of currently-registered in-flight subagent ids. Used by
/// the UI to drive Ctrl+K (resolve the focused-tab's id back to a
/// full registry key) without exposing the mutex.
#[allow(dead_code)]
pub fn registered_subagent_ids() -> Vec<String> {
    let map = abort_registry().lock().unwrap_or_else(|e| e.into_inner());
    map.keys().cloned().collect()
}

/// Test-only helper: clear the abort registry between cases. Tests
/// run in parallel by default; without this they'd leak ids across
/// test invocations and corrupt prefix-resolution assertions.
#[cfg(test)]
pub fn clear_abort_registry_for_test() {
    let mut map = abort_registry().lock().unwrap_or_else(|e| e.into_inner());
    map.clear();
}

pub struct TaskTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    model: AnyModel,
    bg_store: BackgroundStore,
    /// dirge-ov2: send-side of the subagent-chat-event channel.
    /// `Option` so `--no-tools` paths / tests can omit the UI sink
    /// without forcing every TaskTool builder to manufacture one.
    chat_sink: Option<SubagentChatSender>,
}

impl TaskTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        model: AnyModel,
        bg_store: BackgroundStore,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            model,
            bg_store,
            chat_sink: None,
        }
    }

    /// dirge-ov2: wire the subagent-chat-event sender. Called by the
    /// agent builder when constructing the TaskTool for an
    /// interactive session. Headless / test paths skip this so the
    /// tool behaves identically to the pre-ov2 implementation.
    ///
    /// Currently unused in production — the process-global sink
    /// (set via `set_subagent_chat_sink`) is the wired path. Kept
    /// for tests + future per-instance overrides.
    #[allow(dead_code)]
    pub fn with_chat_sink(mut self, sink: SubagentChatSender) -> Self {
        self.chat_sink = Some(sink);
        self
    }

    /// dirge-ov2 helper: fire-and-forget a chat event. Prefers the
    /// instance-bound sink (set via `with_chat_sink`); falls back
    /// to the process-global sink set at UI-loop startup. If
    /// neither is installed (headless / tests) the event is
    /// silently discarded — never block the subagent or fail the
    /// tool call on UI plumbing trouble.
    fn emit_chat(&self, event: SubagentChatEvent) {
        if let Some(sink) = &self.chat_sink {
            let _ = sink.try_send(event);
            return;
        }
        if let Some(sink) = subagent_chat_sink() {
            let _ = sink.try_send(event);
        }
    }
}

#[derive(Deserialize)]
pub struct Args {
    pub prompt: String,
    #[serde(default)]
    pub background: Option<bool>,
}

impl Tool for TaskTool {
    const NAME: &'static str = "task";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let description = "Spawn a subagent to handle a specific subtask. The subagent runs as a one-shot query (no tools) and returns its result inline. Use for research, analysis, or planning subtasks that don't require file access. Set background=true to run asynchronously — completion is delivered to you automatically as a <system-reminder> at the start of your next turn. Do NOT poll task_status in a loop or sleep waiting; continue with other work."
            .to_string();

        let properties = serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Task description for the subagent"
                },
                "background": {
                    "type": "boolean",
                    "description": "Run asynchronously (default: false). When true, returns a task_id immediately. The result is delivered automatically as a <system-reminder> on your next turn — do NOT poll task_status."
                }
            },
            "required": ["prompt"]
        });

        ToolDefinition {
            name: "task".to_string(),
            description,
            parameters: properties,
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "task", &args.prompt).await?;

        let background = args.background.unwrap_or(false);

        if background {
            // Audit M2: refuse new background spawns past the
            // concurrency cap. The agent gets a clear refusal it
            // can act on (wait for an existing task to finish, then
            // retry) rather than fanning out unbounded.
            let running = self.bg_store.running_count();
            let cap = BackgroundStore::max_concurrent();
            if running >= cap {
                return Err(ToolError::Msg(format!(
                    "background subagent cap reached ({}/{} in flight). Wait for one to finish (use task_status) or run inline (background=false). Capping prevents fan-out from burning the API budget.",
                    running, cap,
                )));
            }
            let task_id = Uuid::new_v4().to_string();
            self.bg_store.insert(task_id.clone());
            self.bg_store.notify_started(&task_id);

            // dirge-ov2 Phase D: announce the subagent so the UI
            // loop creates a chat window for it.
            self.emit_chat(SubagentChatEvent::Spawn {
                id: task_id.clone(),
                prompt: args.prompt.clone(),
            });

            // dirge-781c: per-subagent AbortSignal so `/kill <id>` or
            // Ctrl+K on the focused tab can interrupt it. Registered
            // in the process-global registry; cleared on terminal
            // event below.
            let abort = AbortSignal::new();
            register_subagent_abort(&task_id, abort.clone());

            let model = self.model.clone();
            let prompt = args.prompt;
            let store = self.bg_store.clone();
            let tid = task_id.clone();
            let chat_sink = self.chat_sink.clone();
            let abort_for_task = abort.clone();

            // Cap the background subagent at 10 minutes. Without a
            // timeout, a stuck subagent (provider hang, runaway
            // multi-turn) would keep the task in `Running` state
            // forever, hold its model/network handle open, and
            // never deliver a system-reminder to the next turn.
            // 10 min matches the rough upper bound for a coherent
            // single-prompt LLM task; anything longer is the
            // subagent loop misbehaving.
            const SUBAGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
            let store_for_task = store.clone();
            let tid_for_task = tid.clone();
            let handle = tokio::spawn(async move {
                let fut = model.btw_query(format!(
                    "You are a subagent working on a specific subtask. Complete it thoroughly.\n\nTask: {}",
                    prompt
                ));
                // dirge-781c: race btw_query against the abort signal.
                // `btw_query` is one-shot so we can't propagate the
                // signal into the provider; instead we poll the flag
                // and bail out of the await at the next tick.
                let abort_check = abort_for_task.clone();
                let raced = async {
                    tokio::pin!(fut);
                    loop {
                        tokio::select! {
                            r = &mut fut => break Ok::<_, ()>(r),
                            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                                if abort_check.is_cancelled() {
                                    break Err(());
                                }
                            }
                        }
                    }
                };
                let outer = tokio::time::timeout(SUBAGENT_TIMEOUT, raced).await;
                let (state, chat_event) = match outer {
                    Ok(Ok(Ok(text))) => (
                        TaskState::Completed(text.clone()),
                        SubagentChatEvent::Token {
                            id: tid_for_task.clone(),
                            text: text.clone(),
                        },
                    ),
                    Ok(Ok(Err(e))) => {
                        let msg = e.to_string();
                        (
                            TaskState::Failed(msg.clone()),
                            SubagentChatEvent::Failed {
                                id: tid_for_task.clone(),
                                error: msg,
                            },
                        )
                    }
                    Ok(Err(())) => {
                        // dirge-781c: aborted via /kill.
                        let msg = "aborted by user".to_string();
                        (
                            TaskState::Failed(msg.clone()),
                            SubagentChatEvent::Aborted {
                                id: tid_for_task.clone(),
                            },
                        )
                    }
                    Err(_) => {
                        let msg =
                            format!("subagent timed out after {}s", SUBAGENT_TIMEOUT.as_secs(),);
                        (
                            TaskState::Failed(msg.clone()),
                            SubagentChatEvent::Failed {
                                id: tid_for_task.clone(),
                                error: msg,
                            },
                        )
                    }
                };
                // dirge-781c: emit the streaming Token first (if any),
                // then the terminal Complete. Lets the UI render the
                // payload through the same Token-handling code path
                // that a real per-token stream would use.
                let final_event = match &chat_event {
                    SubagentChatEvent::Token { id, text } => {
                        if let Some(sink) = &chat_sink {
                            let _ = sink.try_send(chat_event.clone());
                        } else if let Some(sink) = subagent_chat_sink() {
                            let _ = sink.try_send(chat_event.clone());
                        }
                        SubagentChatEvent::Complete {
                            id: id.clone(),
                            result: text.clone(),
                        }
                    }
                    _ => chat_event.clone(),
                };
                if let Some(sink) = chat_sink {
                    let _ = sink.try_send(final_event);
                } else if let Some(sink) = subagent_chat_sink() {
                    let _ = sink.try_send(final_event);
                }
                unregister_subagent_abort(&tid_for_task);
                store_for_task.notify(&tid_for_task, state);
            });
            // Register the handle so `BackgroundStore::cancel_all` (called
            // on session swap) can abort the subagent and free its
            // provider connection. Without this the task survived the
            // parent's session change and kept consuming API budget.
            store.attach_handle(&tid, handle);

            Ok(format!(
                "background task started — task_id: {}\n\nThe subagent runs in the background. Completion will be delivered automatically as a <system-reminder> at the start of your next turn. Do NOT poll task_status or sleep waiting — continue with other work.",
                task_id
            ))
        } else {
            // dirge-ov2 Phase D: foreground subagent. Emit Spawn /
            // Complete / Failed so the UI surfaces the call as a
            // chat window (Ctrl-N/P/X to view). Foreground tasks
            // still block the parent agent's tool call; the chat
            // window populates with prompt + final result.
            let task_id = Uuid::new_v4().to_string();
            self.emit_chat(SubagentChatEvent::Spawn {
                id: task_id.clone(),
                prompt: args.prompt.clone(),
            });
            // dirge-781c: register an AbortSignal so `/kill` / Ctrl+K
            // can interrupt the foreground subagent. Registered for
            // the duration of the btw_query call and removed on
            // every exit path.
            let abort = AbortSignal::new();
            register_subagent_abort(&task_id, abort.clone());

            let fut = self.model.btw_query(format!(
                "You are a subagent working on a specific subtask. Complete it thoroughly.\n\nTask: {}",
                args.prompt
            ));
            let abort_check = abort.clone();
            let raced = async {
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        r = &mut fut => break Ok::<_, ()>(r),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                            if abort_check.is_cancelled() {
                                break Err(());
                            }
                        }
                    }
                }
            };
            let result = raced.await;
            unregister_subagent_abort(&task_id);
            match result {
                Ok(Ok(text)) => {
                    // dirge-nmv5: the chat window always gets the FULL
                    // text so the user sees the complete subagent
                    // answer in its Ctrl-N/P window. The parent agent
                    // sees the relayed text — verbatim when small,
                    // a head/tail summary plus a `read`-tool hint
                    // when large (full payload at
                    // `~/.dirge/transient/<pid>/task-<ts>.txt`).
                    // Replaces the prior "drop everything past 3000
                    // chars" behavior that silently lost subagent
                    // output on the background path.
                    self.emit_chat(SubagentChatEvent::Token {
                        id: task_id.clone(),
                        text: text.clone(),
                    });
                    self.emit_chat(SubagentChatEvent::Complete {
                        id: task_id,
                        result: text.clone(),
                    });
                    let outcome =
                        crate::agent::tools::output_relay::relay_if_large("task", text, "");
                    Ok(outcome.text)
                }
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    self.emit_chat(SubagentChatEvent::Failed {
                        id: task_id,
                        error: msg.clone(),
                    });
                    Err(ToolError::Msg(format!("Subagent error: {}", msg)))
                }
                Err(()) => {
                    // dirge-781c: aborted via /kill or Ctrl+K. The
                    // parent agent sees an `aborted` error so the
                    // cancellation is visible in its loop, NOT a
                    // silent "subagent finished" with no payload.
                    self.emit_chat(SubagentChatEvent::Aborted { id: task_id });
                    Err(ToolError::Msg("Subagent aborted by user".to_string()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::background::BackgroundStore;
    use crate::provider::AnyModel;
    use rig::client::CompletionClient;
    use rig::providers::openrouter;

    fn mock_tool() -> TaskTool {
        // The model is never invoked in these tests — they exercise the
        // definition surface only.
        let client = openrouter::Client::new("test-key").unwrap();
        let model = client.completion_model("anthropic/claude-sonnet-4.5");
        TaskTool::new(
            None,
            None,
            AnyModel::OpenRouter(model),
            BackgroundStore::new(),
        )
    }

    // Regression: the task tool description must tell the agent that
    // background=true delivers completion automatically and instruct it
    // NOT to poll task_status. The previous text told the agent to "use
    // task_status to poll for the result", which produced wasteful loops.
    #[tokio::test]
    async fn definition_steers_agent_away_from_polling() {
        let tool = mock_tool();
        let def = tool.definition(String::new()).await;
        let desc = def.description.to_lowercase();
        assert!(
            desc.contains("system-reminder") || desc.contains("automatically"),
            "task description must reference automatic notification: {}",
            def.description
        );
        assert!(
            desc.contains("do not poll") || desc.contains("not poll"),
            "task description must explicitly discourage polling: {}",
            def.description
        );
    }

    /// dirge-mifq — Subagents spawned by `TaskTool` go through
    /// `AnyModel::btw_query`, which builds a fresh rig agent from
    /// the model alone with NO tools attached. This pins the
    /// invariant: TaskTool itself must not hold a tool registry,
    /// session_id, agent handle, or any state that would let a
    /// subagent reach the parent's `session_search`, `memory`, or
    /// `skill` tools. If a future change adds tools to subagents,
    /// the session-search-includes-current-session class of bugs
    /// (dirge-502b) re-emerges and the new tools must be considered
    /// for session_id propagation.
    ///
    /// The compiler enforces this — adding a `tools: ...` field to
    /// `TaskTool` would not break this test directly, but the
    /// review surface forces the change to be visible. If you do
    /// add tool support to subagents, also (a) audit btw_query for
    /// session_search wiring, (b) decide subagent session_id
    /// policy (inherit parent? Fresh? Excluded?), (c) update this
    /// test to cover the new shape.
    #[test]
    fn subagent_path_is_stateless_no_session_search_leakage() {
        // The fields a TaskTool legitimately holds today. Anything
        // beyond this set is a red flag for subagent-tool leakage.
        // (Using `_ =` to silence the dead-binding lint while
        // documenting the inventory.)
        let _expected_fields = ["permission", "ask_tx", "model", "bg_store", "chat_sink"];

        // Construct a TaskTool — if a future field is required,
        // this won't compile until the new field is provided. That
        // failure mode points the reader at this test, which then
        // forces the session_id audit per the docstring above.
        let _tool: TaskTool = mock_tool();

        // The btw_query path lives in provider/mod.rs. The build
        // inside it is `AgentBuilder::new(m).preamble(...).build()`
        // with no `.tool(...)` calls — verify by source inspection
        // that no tool-attaching call has crept in.
        let provider_src = include_str!("../../provider/mod.rs");
        let btw_idx = provider_src
            .find("pub async fn btw_query")
            .expect("btw_query must exist in provider/mod.rs");
        let btw_end = provider_src[btw_idx..]
            .find("\n    }\n")
            .map(|i| btw_idx + i)
            .unwrap_or(provider_src.len());
        let btw_body = &provider_src[btw_idx..btw_end];
        assert!(
            !btw_body.contains(".tool("),
            "btw_query must not attach tools to the subagent — that would \
             require auditing session_id propagation per dirge-mifq. \
             Source snippet:\n{btw_body}"
        );
        assert!(
            !btw_body.contains(".tools("),
            "btw_query must not attach tools to the subagent — that would \
             require auditing session_id propagation per dirge-mifq."
        );
    }

    #[tokio::test]
    async fn definition_advertises_background_field() {
        let tool = mock_tool();
        let def = tool.definition(String::new()).await;
        let props = def
            .parameters
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties present");
        assert!(props.contains_key("background"));
        let bg_desc = props["background"]["description"]
            .as_str()
            .unwrap()
            .to_lowercase();
        assert!(bg_desc.contains("automatically") || bg_desc.contains("system-reminder"));
        assert!(bg_desc.contains("do not poll") || bg_desc.contains("not poll"));
    }

    // dirge-nmv5: short subagent answers (under the 8 KiB / 200-line
    // budget) must be returned verbatim to the parent agent — no
    // summary, no relay file, no truncation. The relay is keyed on
    // the "task" tool name so this exercises exactly the same path
    // `TaskTool::call` runs.
    #[test]
    fn task_short_output_returned_verbatim() {
        let short = "subagent: 42 is the answer.\n".to_string();
        let outcome = crate::agent::tools::output_relay::relay_if_large("task", short.clone(), "");
        assert!(
            outcome.relayed_to.is_none(),
            "short output must not trigger the disk relay",
        );
        assert_eq!(
            outcome.text, short,
            "short subagent output must round-trip unchanged to the parent",
        );
    }

    // dirge-nmv5: large subagent answers must NOT silently truncate.
    // The full text is written to `~/.dirge/transient/<pid>/task-<ts>.txt`
    // and the parent agent receives a head/tail summary plus a
    // `read`-tool hint pointing at the transient file. This guards
    // against regressing to the prior "drop everything past 3000
    // chars" behavior that lost subagent output.
    #[test]
    fn task_large_output_relayed_to_disk_with_summary() {
        // 64 KiB payload — well past the default 8 KiB inline budget.
        let huge: String = "subagent line\n".repeat(5_000);
        let original_len = huge.len();
        let outcome = crate::agent::tools::output_relay::relay_if_large("task", huge, "");

        // Full payload landed on disk and is readable.
        let path = outcome
            .relayed_to
            .as_ref()
            .expect("large output must trigger the disk relay");
        assert!(path.exists(), "relayed file must exist at {path:?}");
        let written = std::fs::read_to_string(path).expect("read relayed file");
        assert_eq!(
            written.len(),
            original_len,
            "the FULL original payload must be on disk (not the truncated head)",
        );

        // Parent agent gets a much-smaller summary plus the recovery
        // hint — no silent truncation.
        let summary = &outcome.text;
        assert!(
            summary.len() < original_len,
            "summary should be much smaller than the original payload",
        );
        assert!(
            summary.contains("`read`"),
            "summary must mention the `read` tool so the agent can recover the full payload: {summary}",
        );
        assert!(
            summary.contains("transient") || summary.contains(".dirge"),
            "summary must reference the transient path: {summary}",
        );

        // Cleanup.
        let _ = std::fs::remove_file(path);
    }

    // dirge-781c: registry-backed kill resolution. These tests use a
    // serial mutex to ensure they don't trample each other's
    // registry state when run in parallel (cargo's default).
    fn registry_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// `/kill` against an empty registry or a never-spawned prefix
    /// must be a NoOp — never panic, never cancel anything.
    #[test]
    fn kill_unknown_id_no_op() {
        let _guard = registry_test_lock();
        clear_abort_registry_for_test();
        // Empty registry, any prefix → NotFound.
        assert_eq!(kill_subagent("abc"), KillOutcome::NotFound);
        assert_eq!(kill_subagent(""), KillOutcome::NotFound);
        // Populated registry, prefix doesn't match anything.
        let sig = AbortSignal::new();
        register_subagent_abort("aaa-1111", sig.clone());
        assert_eq!(kill_subagent("zzz"), KillOutcome::NotFound);
        assert!(
            !sig.is_cancelled(),
            "unmatched kill must NOT cancel the surviving subagent",
        );
        unregister_subagent_abort("aaa-1111");
    }

    /// `/kill <prefix>` with exactly one matching id resolves to
    /// `Killed(full_id)` and fires the abort signal.
    #[test]
    fn kill_resolves_by_prefix_unique_match() {
        let _guard = registry_test_lock();
        clear_abort_registry_for_test();
        let sig_a = AbortSignal::new();
        let sig_b = AbortSignal::new();
        register_subagent_abort("aa11-deadbeef", sig_a.clone());
        register_subagent_abort("bb22-cafef00d", sig_b.clone());

        // Unique 2-char prefix → kill exactly that one.
        match kill_subagent("aa") {
            KillOutcome::Killed(id) => assert_eq!(id, "aa11-deadbeef"),
            other => panic!("expected Killed; got {:?}", other),
        }
        assert!(sig_a.is_cancelled(), "matched signal must be cancelled");
        assert!(!sig_b.is_cancelled(), "unmatched signal must survive");

        // Ambiguous prefix (registering a second `aa…` id) → Ambiguous.
        let sig_a2 = AbortSignal::new();
        register_subagent_abort("aa99-othertask", sig_a2.clone());
        // sig_a already cancelled from previous step; check ambiguity
        // returns BOTH matching ids.
        match kill_subagent("aa") {
            KillOutcome::Ambiguous(ids) => {
                assert_eq!(ids.len(), 2);
                assert!(ids.iter().any(|i| i == "aa11-deadbeef"));
                assert!(ids.iter().any(|i| i == "aa99-othertask"));
            }
            other => panic!("expected Ambiguous; got {:?}", other),
        }
        assert!(
            !sig_a2.is_cancelled(),
            "ambiguous kill must NOT cancel any signal",
        );

        // Exact-id match wins over prefix collision: passing the
        // FULL id of one entry kills exactly that one even though
        // it's a prefix of itself.
        clear_abort_registry_for_test();
        let s1 = AbortSignal::new();
        let s2 = AbortSignal::new();
        register_subagent_abort("abc", s1.clone());
        register_subagent_abort("abcdef", s2.clone());
        match kill_subagent("abc") {
            KillOutcome::Killed(id) => assert_eq!(id, "abc"),
            other => panic!("expected exact-match Killed; got {:?}", other),
        }
        assert!(s1.is_cancelled());
        assert!(!s2.is_cancelled());

        clear_abort_registry_for_test();
    }

    /// `subagent_complete_after_kill_returns_aborted_result`: when
    /// `/kill` fires while the subagent's `btw_query` future is
    /// awaiting, the task tool emits an `Aborted` chat event and
    /// returns a `ToolError` containing "aborted" so the parent
    /// agent's tool-result block reflects the cancellation.
    ///
    /// The test exercises the racer directly because `btw_query`
    /// requires a real provider. The racer is the same code path
    /// the production `call()` runs.
    #[tokio::test]
    async fn subagent_complete_after_kill_returns_aborted_result() {
        let _guard = registry_test_lock();
        clear_abort_registry_for_test();
        let tid = "t-abort-1";
        let abort = AbortSignal::new();
        register_subagent_abort(tid, abort.clone());

        // Simulate a long-running btw_query future that never
        // returns. The select! racer polls the abort signal every
        // 100ms; cancelling here should make it bail out within ~200ms.
        let abort_check = abort.clone();
        let fut = async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok::<String, anyhow::Error>("never-arrives".to_string())
        };
        let raced = async {
            tokio::pin!(fut);
            loop {
                tokio::select! {
                    r = &mut fut => break Ok::<_, ()>(r),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                        if abort_check.is_cancelled() {
                            break Err(());
                        }
                    }
                }
            }
        };

        // Fire /kill in parallel; the racer should observe it on
        // its next 50ms poll.
        let killer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            assert!(matches!(kill_subagent("t-abort"), KillOutcome::Killed(_)));
        });

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), raced)
            .await
            .expect("racer must exit before the 2s test timeout");
        killer.await.unwrap();

        match result {
            Err(()) => { /* expected — aborted */ }
            Ok(other) => panic!("expected abort; got Ok({:?})", other),
        }
        unregister_subagent_abort(tid);
        clear_abort_registry_for_test();
    }

    /// dirge-781c: Token / Reasoning / ToolCall / ToolResult /
    /// Aborted events round-trip through the chat sink — the UI
    /// receiver sees them with the same id payload the producer
    /// sent. Guards the variant additions against accidental
    /// dirge-02tn: the chat channel is BOUNDED — once full, `try_send`
    /// drops (returns Err) rather than growing memory without bound.
    /// Events are display-only, so a dropped event under a UI stall only
    /// degrades the live view, never the subagent's result.
    #[test]
    fn subagent_chat_channel_is_bounded_and_drops_on_overflow() {
        let (tx, _rx) = mpsc::channel::<SubagentChatEvent>(SUBAGENT_CHAT_CAP);
        // Fill to capacity without draining (_rx kept alive so the channel
        // stays open — otherwise try_send would fail Closed, not Full).
        for i in 0..SUBAGENT_CHAT_CAP {
            tx.try_send(SubagentChatEvent::Token {
                id: "x".into(),
                text: format!("{i}"),
            })
            .expect("sends within capacity succeed");
        }
        let overflow = tx.try_send(SubagentChatEvent::Token {
            id: "x".into(),
            text: "overflow".into(),
        });
        assert!(
            overflow.is_err(),
            "channel must be bounded — an over-capacity try_send drops"
        );
    }

    /// silent drops when the dispatch is refactored.
    #[test]
    fn subagent_token_event_routes_to_chat_slot() {
        let (tx, mut rx) = mpsc::channel::<SubagentChatEvent>(SUBAGENT_CHAT_CAP);
        tx.try_send(SubagentChatEvent::Token {
            id: "a1".into(),
            text: "hello world".into(),
        })
        .unwrap();
        tx.try_send(SubagentChatEvent::Reasoning {
            id: "a1".into(),
            text: "thinking".into(),
        })
        .unwrap();
        tx.try_send(SubagentChatEvent::Aborted { id: "a1".into() })
            .unwrap();

        match rx.try_recv().unwrap() {
            SubagentChatEvent::Token { id, text } => {
                assert_eq!(id, "a1");
                assert_eq!(text, "hello world");
            }
            other => panic!("expected Token; got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            SubagentChatEvent::Reasoning { id, text } => {
                assert_eq!(id, "a1");
                assert_eq!(text, "thinking");
            }
            other => panic!("expected Reasoning; got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            SubagentChatEvent::Aborted { id } => assert_eq!(id, "a1"),
            other => panic!("expected Aborted; got {:?}", other),
        }
    }

    #[test]
    fn subagent_tool_call_event_routes_to_chat_slot() {
        let (tx, mut rx) = mpsc::channel::<SubagentChatEvent>(SUBAGENT_CHAT_CAP);
        tx.try_send(SubagentChatEvent::ToolCall {
            id: "a1".into(),
            tool_name: "read".into(),
            args_summary: "path=/tmp/x".into(),
        })
        .unwrap();
        tx.try_send(SubagentChatEvent::ToolResult {
            id: "a1".into(),
            tool_name: "read".into(),
            output_summary: "12 lines".into(),
        })
        .unwrap();

        match rx.try_recv().unwrap() {
            SubagentChatEvent::ToolCall {
                id,
                tool_name,
                args_summary,
            } => {
                assert_eq!(id, "a1");
                assert_eq!(tool_name, "read");
                assert_eq!(args_summary, "path=/tmp/x");
            }
            other => panic!("expected ToolCall; got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            SubagentChatEvent::ToolResult {
                id,
                tool_name,
                output_summary,
            } => {
                assert_eq!(id, "a1");
                assert_eq!(tool_name, "read");
                assert_eq!(output_summary, "12 lines");
            }
            other => panic!("expected ToolResult; got {:?}", other),
        }
    }
}
