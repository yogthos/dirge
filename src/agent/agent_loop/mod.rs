//! Pi-style agent loop. Faithful port of `pi/packages/agent/src/agent-loop.ts`.
//!
//! Phase 0 lands the value-type surface (enums + shape structs) and
//! the `LoopTool` trait. Nothing in this module is reachable from
//! production code until phase 4 of PLAN.md.
//!
//! Reference paths (read alongside this module — pi is authoritative):
//!   - `~/src/pi/packages/agent/src/types.ts`
//!   - `~/src/pi/packages/agent/src/agent-loop.ts`
//!   - `~/src/pi/packages/agent/test/agent-loop.test.ts`
//!
//! Each file in this directory cites the pi line range it maps to so
//! divergences can be audited against the reference. Pi is the spec —
//! we're not redesigning, we're porting.

// After phase 4.5h-6 cutover, the agent_loop module is wired
// into production via provider::spawn_runner. The module also
// exposes a library-like surface (LoopConfig hook slots, custom
// message types, optional steering / interjection plumbing,
// rig-stream helpers, recovery wrapper variants) that's
// available to future wiring but not all binary-reachable yet.
// Module-level `dead_code` + `unused_imports` allows cover the
// transitional state without scattering per-item gates.
#![allow(dead_code)]
#![allow(unused_imports)]

pub mod bridge;
#[cfg(test)]
mod h7_smoke;
pub mod hooks;
pub mod integration;
pub mod message;
#[cfg(feature = "plugin")]
pub mod plugin_hooks;
pub mod result;
pub mod retry;
pub mod rig_stream;
pub mod rig_stream_factory;
pub mod rig_tool;
pub mod run;
pub mod steering;
pub mod stream;
pub mod tool;
pub mod tools;
pub mod types;

pub use bridge::EventBridge;
pub use hooks::{
    AfterToolCallContext, AfterToolCallFn, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallReturn, GetFollowupMessagesFn, GetSteeringMessagesFn, PrepareNextTurnFn,
    ShouldStopAfterTurnFn, TurnHookContext,
};
pub use integration::{
    LoopRunner, LoopSpawnConfig, rig_history_system_prompt, rig_history_to_loop_messages,
    rig_message_to_loop_messages, spawn_loop_runner,
};
pub use message::{
    AssistantMessage, ContentBlock, DeltaPhase, LoopEvent, LoopMessage, StopReason, StreamEvent,
    ToolResultMessage, UserMessage,
};
#[cfg(feature = "plugin")]
pub use plugin_hooks::{after_hook_from_plugin_manager, before_hook_from_plugin_manager};
pub use result::{AfterToolCallResult, BeforeToolCallResult, LoopToolResult};
pub use retry::retrying_stream_fn;
pub use rig_stream::{wrap_rig_stream, wrap_streamed_assistant};
pub use rig_stream_factory::{
    loop_tool_to_rig_definition, rig_stream_fn_from_model, value_to_rig_message,
};
pub use rig_tool::RigToolAdapter;
pub use run::{LoopError, run_agent_loop, run_agent_loop_continue, run_loop};
pub use steering::{steering_from_queue, steering_from_queue_with_sanitizer};
pub use stream::{LlmContext, StreamFn, stream_assistant_response};
pub use tool::LoopTool;
pub use tools::{
    ExecutedToolCallBatch, ToolCall, execute_tool_calls, execute_tool_calls_parallel,
    execute_tool_calls_sequential, extract_tool_calls,
};
pub use types::{
    Context, ConvertToLlmFn, GetApiKeyFn, LoopConfig, QueueMode, ThinkingLevel, ToolExecutionMode,
    TransformContextFn, TurnUpdate,
};
