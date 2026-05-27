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

use crate::cli::Cli;
use crate::config::Config;
use crate::session::Session;
use crate::ui::renderer::Renderer;
use crate::ui::tool_display::CollapsedToolResult;

pub(super) mod context_overflow;
pub(super) mod done;
pub(super) mod interjected;
pub(super) mod tool_result;

pub(super) use context_overflow::handle_context_overflow;
pub(super) use done::handle_done;
pub(super) use interjected::handle_interjected;
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
