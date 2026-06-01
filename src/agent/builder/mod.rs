//! Agent builder. Wiring module for the agent-construction subsystem,
//! decomposed (dirge-4y4l stage 11) into concern-focused children:
//! [`agent_inner`] (the rig `Agent` constructor), [`loop_tools`] (the
//! `LoopTool` registry), and [`preamble`] (system-prompt assembly). All are
//! re-exported so `crate::agent::builder::*` paths are unchanged.

use crate::agent::tools;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

mod agent_inner;
mod loop_tools;
mod preamble;
pub use agent_inner::*;
pub use loop_tools::*;
pub(crate) use preamble::*;

/// Factory for the `SessionSearchTool` instance plumbed into both the
/// rig-side tool registry and the new agent_loop registry. Lives here
/// (rather than inline at each construction site) so the threading of
/// `session_id` is testable without downcasting through `dyn LoopTool`.
/// See dirge-502b.
pub(crate) fn build_session_search_tool(
    db_path: std::path::PathBuf,
    session_id: Option<String>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
) -> tools::SessionSearchTool {
    tools::SessionSearchTool::new(db_path, session_id, permission, ask_tx)
}

#[cfg(test)]
mod reminder_tests;
