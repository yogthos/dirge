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
//!
//! After phase 4.5h-6 cutover, the agent_loop module is wired
//! into production via provider::spawn_runner.

// Re-exports constitute the module's public API surface. Many are
// consumed only by test code or external crates; suppress the
// per-item unused-import warnings on this block.
#![allow(unused_imports)]

pub mod bridge;
pub mod context_depth;
pub mod context_manager;
#[cfg(test)]
mod h7_smoke;
pub mod heal;
pub mod hooks;
pub mod inflight;
pub mod integration;
#[cfg(test)]
mod integration_tests;
pub mod message;
#[cfg(feature = "plugin")]
pub mod plugin_hooks;
#[cfg(all(test, feature = "plugin"))]
mod plugin_hooks_tests;
pub mod reflexion;
pub mod result;
pub mod retry;
pub mod rig_stream;
pub mod rig_stream_factory;
pub mod rig_tool;
pub mod run;
pub mod scavenge;
pub mod schema_flatten;
pub mod steering;
pub mod storm;
pub mod stream;
pub mod tool;
pub mod tool_input_repair;
pub mod tools;
pub mod types;

pub use bridge::EventBridge;
pub use hooks::{
    AfterToolCallContext, AfterToolCallFn, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallReturn, GetFollowupMessagesFn, GetSteeringMessagesFn, PrepareNextTurnFn,
    ShouldStopAfterTurnFn, TurnHookContext,
};
pub use integration::{
    LoopRunner, LoopSpawnConfig, default_convert_to_llm, rig_history_system_prompt,
    rig_history_to_loop_messages, rig_message_to_loop_messages, spawn_loop_runner,
};
pub use message::{
    AssistantMessage, ContentBlock, DeltaPhase, EscalationReason, LoopEvent, LoopMessage,
    StopReason, StreamEvent, TokenUsage, ToolResultMessage, UserMessage,
};
#[cfg(feature = "plugin")]
pub use plugin_hooks::{after_hook_from_plugin_manager, before_hook_from_plugin_manager};
pub use result::{AfterToolCallResult, BeforeToolCallResult, LoopToolResult};
pub use retry::retrying_stream_fn;
pub use rig_stream::{wrap_rig_stream, wrap_streamed_assistant};
#[cfg(test)]
pub use rig_stream_factory::rig_stream_fn_from_model;
pub use rig_stream_factory::{
    build_provider_additional_params, loop_tool_to_rig_definition,
    rig_stream_fn_from_model_with_filter, rig_stream_fn_from_model_with_provider,
    value_to_rig_message,
};
pub use rig_tool::RigToolAdapter;
pub use run::{run_agent_loop, run_loop};
pub use steering::steering_from_queue;
pub use stream::{LlmContext, StreamFn, StreamOptions, stream_assistant_response};
pub use tool::LoopTool;
pub use tool_input_repair::{
    RepairKind, RepairResult, format_structured_error, is_path_field_name, validate_and_repair,
};
#[cfg(test)]
pub use tools::execute_tool_calls_from_msg;
pub use tools::{
    ExecutedToolCallBatch, ToolCall, execute_tool_calls, execute_tool_calls_parallel,
    execute_tool_calls_sequential, extract_tool_calls,
};
pub use types::{
    Context, ConvertToLlmFn, GetApiKeyFn, LoopConfig, QueueMode, ThinkingLevel, ToolExecutionMode,
    TransformContextFn, TurnUpdate,
};
