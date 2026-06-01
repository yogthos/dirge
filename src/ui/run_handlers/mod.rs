//! Event-handler extractions for the `run_interactive` event loop.
//!
//! `run_interactive` in `src/ui/mod.rs` hosts a `tokio::select!` whose
//! agent-event arms grew large enough that each one needed scrolling to
//! read. The biggest arms — `ToolResult`, `Done`, `ContextOverflow`,
//! `Interjected` — were extracted here as standalone async functions
//! taking a single `RunCtx<'_>` bundle plus per-handler extras for
//! state the bundle can't lifetime-share (the owning `agent`, the
//! cloneable `client`, runner-state mutables, etc.).
//!
//! Behavior is identical to the inline code; this is a pure refactor.

use crate::agent::tools::background::BackgroundStore;
use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;
use crate::cli::Cli;
use crate::config::Config;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::AnyClient;
use crate::sandbox::Sandbox;
use crate::session::Session;
use crate::ui::renderer::Renderer;
use crate::ui::tool_display::CollapsedToolResult;

#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;

pub(super) mod context_compacted;
pub(super) mod context_overflow;
pub(super) mod done;
pub(super) mod error;
pub(super) mod interjected;
pub(super) mod notices;
pub(super) mod streaming;
pub(super) mod tool_call;
pub(super) mod tool_result;

// dirge-5h5: isolated repro harness for the parallel-read chamber
// race. Drives the chamber state machine directly without the
// tokio::select! / bridge / agent_loop layers so the bug can be
// localised to (or ruled out of) the chamber logic itself.
#[cfg(test)]
mod dirge_5h5_repro;

pub(super) use context_compacted::handle_context_compacted;
pub(super) use context_overflow::handle_context_overflow;
pub(super) use done::handle_done;
pub(super) use error::handle_error;
pub(super) use interjected::handle_interjected;
pub(super) use tool_call::handle_tool_call;
pub(super) use tool_result::handle_tool_result;

/// Bundle of frequently-mutated UI state borrowed by every extracted
/// handler. Fields are `&mut` references into the locals of
/// `run_interactive`; the struct is built once per event dispatch
/// inside the match arm so the borrow lives only for the handler call.
///
/// Owning state that handlers replace wholesale (`agent`, `agent_rx`,
/// `agent_abort`, `agent_interject`) — or that is feature-gated and
/// therefore awkward to put behind a single struct field — is passed
/// as explicit per-handler parameters instead.
pub(super) struct RunCtx<'a> {
    pub renderer: &'a mut Renderer,
    pub session: &'a mut Session,
    pub response_buf: &'a mut String,
    pub response_start_line: &'a mut Option<usize>,
    pub reasoning_buf: &'a mut String,
    pub reasoning_start_line: &'a mut Option<usize>,
    pub agent_line_started: &'a mut bool,
    pub last_tool_name: &'a mut Option<String>,
    pub last_tool_call_id: &'a mut Option<String>,
    pub tool_chamber_open: &'a mut bool,
    pub chamber_top_start: &'a mut Option<usize>,
    pub chamber_top_end: &'a mut Option<usize>,
    pub tool_calls_buf: &'a mut Vec<crate::session::ToolCallEntry>,
    pub tool_calls_this_run: &'a mut u32,
    pub last_collapsed: &'a mut Option<CollapsedToolResult>,
    pub last_user_prompt: &'a mut String,
    pub cli: &'a Cli,
    pub cfg: &'a Config,
}

/// Shared immutable inputs to [`crate::provider::build_agent`], bundled so
/// the agent-rebuild handlers (`done`, `context_overflow`,
/// `context_compacted`) don't each thread ~10 individual parameters
/// through their signatures (dirge-4y4l). Built once per dispatch via the
/// `make_agent_build_deps!` macro in `run_interactive`; handlers destructure
/// it back into locals at the top so their bodies read unchanged.
///
/// All fields are cheap references / `Option<&_>` (Copy), so destructuring
/// a `&AgentBuildDeps` copies them out without moving.
pub(crate) struct AgentBuildDeps<'a> {
    pub client: &'a AnyClient,
    pub permission: &'a Option<PermCheck>,
    pub ask_tx: &'a Option<AskSender>,
    pub question_tx: &'a Option<QuestionSender>,
    pub plan_tx: &'a Option<PlanSwitchSender>,
    pub bg_store: &'a Option<BackgroundStore>,
    pub sandbox: &'a Sandbox,
    #[cfg(feature = "mcp")]
    pub mcp_manager: Option<&'a McpClientManager>,
    #[cfg(feature = "semantic")]
    pub semantic_manager: Option<&'a SemanticManager>,
    #[cfg(feature = "lsp")]
    pub lsp_manager: Option<&'a std::sync::Arc<crate::lsp::manager::LspManager>>,
}
