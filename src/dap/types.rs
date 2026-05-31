//! DAP protocol types — compatibility shim over the `dap` crate.
//!
//! Data types, response types, and event bodies are re-exported from
//! `dap` (dap-rs 0.4.1-alpha1). Argument structs are kept locally
//! because they carry `#[serde(flatten)] pub extra: Value` for
//! adapter-specific extensions — the upstream crate doesn't have
//! those fields.
//!
//! The one response type we don't re-export is `ContinueResponse`:
//! dap-rs 0.4.1-alpha1 is missing `#[serde(rename_all = "camelCase")]`
//! on that struct, so deserialization from real adapters would fail
//! (it sends `allThreadsContinued`, not `all_threads_continued`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Re-exports — data types from dap-rs
// ---------------------------------------------------------------------------

pub use dap::types::{
    Breakpoint, Capabilities, FunctionBreakpoint, Scope, Source, SourceBreakpoint, StackFrame,
    StackFrameFormat, StoppedEventReason, Thread, ValueFormat, Variable,
};

// ---------------------------------------------------------------------------
// Re-exports — response types from dap-rs
// ---------------------------------------------------------------------------

pub use dap::responses::{
    EvaluateResponse, ScopesResponse, SetBreakpointsResponse, SetFunctionBreakpointsResponse,
    StackTraceResponse, ThreadsResponse, VariablesResponse,
};

// ---------------------------------------------------------------------------
// Re-exports — event body types from dap-rs
// ---------------------------------------------------------------------------

pub use dap::events::{ExitedEventBody, OutputEventBody, StoppedEventBody, TerminatedEventBody};

// ---------------------------------------------------------------------------
// Argument types — kept local for `extra: Value` flatten fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeArgs {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "clientID")]
    pub client_id: Option<String>,
    #[serde(rename = "clientName")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(rename = "adapterID")]
    pub adapter_id: String,
    #[serde(rename = "pathFormat")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_format: Option<String>,
    #[serde(rename = "linesStartAt1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_start_at_1: Option<bool>,
    #[serde(rename = "columnsStartAt1")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns_start_at_1: Option<bool>,
    #[serde(rename = "supportsVariableType")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_variable_type: Option<bool>,
    #[serde(rename = "supportsVariablePaging")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_variable_paging: Option<bool>,
    #[serde(rename = "supportsRunInTerminalRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_run_in_terminal_request: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(rename = "supportsProgressReporting")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_progress_reporting: Option<bool>,
    #[serde(rename = "supportsInvalidatedEvent")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_invalidated_event: Option<bool>,
    #[serde(rename = "supportsMemoryReferences")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_memory_references: Option<bool>,
}

impl Default for InitializeArgs {
    fn default() -> Self {
        Self {
            client_id: Some("dirge".into()),
            client_name: Some("dirge".into()),
            adapter_id: String::new(),
            path_format: Some("path".into()),
            lines_start_at_1: Some(true),
            columns_start_at_1: Some(true),
            supports_variable_type: Some(true),
            supports_variable_paging: Some(false),
            supports_run_in_terminal_request: Some(false),
            locale: Some("en-us".into()),
            supports_progress_reporting: Some(false),
            supports_invalidated_event: Some(false),
            supports_memory_references: Some(false),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchArgs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Value>,
    #[serde(rename = "stopOnEntry")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_on_entry: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "noDebug")]
    pub no_debug: Option<bool>,
    #[serde(flatten)]
    pub extra: Value,
}

impl Default for LaunchArgs {
    fn default() -> Self {
        Self {
            program: None,
            args: None,
            cwd: None,
            env: None,
            stop_on_entry: Some(true),
            no_debug: None,
            extra: Value::Object(Default::default()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachArgs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(flatten)]
    pub extra: Value,
}

impl Default for AttachArgs {
    fn default() -> Self {
        Self {
            program: None,
            pid: None,
            port: None,
            host: None,
            cwd: None,
            extra: Value::Object(Default::default()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigurationDoneArgs {
    #[serde(flatten)]
    pub extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DisconnectArgs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<bool>,
    #[serde(rename = "terminateDebuggee")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminate_debuggee: Option<bool>,
    #[serde(flatten)]
    pub extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetBreakpointsArgs {
    pub source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakpoints: Option<Vec<SourceBreakpoint>>,
    #[serde(rename = "lines")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakpoints_deprecated: Option<Vec<u32>>,
    #[serde(rename = "sourceModified")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_modified: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetFunctionBreakpointsArgs {
    pub breakpoints: Vec<FunctionBreakpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueArgs {
    #[serde(rename = "threadId")]
    pub thread_id: u32,
    #[serde(rename = "singleThread")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub single_thread: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextArgs {
    #[serde(rename = "threadId")]
    pub thread_id: u32,
    #[serde(rename = "singleThread")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub single_thread: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granularity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepInArgs {
    #[serde(rename = "threadId")]
    pub thread_id: u32,
    #[serde(rename = "singleThread")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub single_thread: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granularity: Option<String>,
    #[serde(rename = "targetId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutArgs {
    #[serde(rename = "threadId")]
    pub thread_id: u32,
    #[serde(rename = "singleThread")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub single_thread: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granularity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PauseArgs {
    #[serde(rename = "threadId")]
    pub thread_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackTraceArgs {
    #[serde(rename = "threadId")]
    pub thread_id: u32,
    #[serde(rename = "startFrame")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_frame: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub levels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<StackFrameFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopesArgs {
    #[serde(rename = "frameId")]
    pub frame_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariablesArgs {
    #[serde(rename = "variablesReference")]
    pub variables_reference: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ValueFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluateArgs {
    pub expression: String,
    #[serde(rename = "frameId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ValueFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThreadsArgs {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TerminateArgs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartFrameArgs {
    #[serde(rename = "frameId")]
    pub frame_id: u32,
}

// ---------------------------------------------------------------------------
// ContinueResponse — kept local because dap-rs misses camelCase serde
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueResponse {
    #[serde(rename = "allThreadsContinued")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_threads_continued: Option<bool>,
}

// ---------------------------------------------------------------------------
// Custom domain types — not in dap-rs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakpointRecord {
    pub file: String,
    pub breakpoints: Vec<SourceBreakpoint>,
    pub verified: Option<Vec<Breakpoint>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Stopped,
    Terminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueOutcome {
    pub status: SessionStatus,
    pub output: String,
    pub output_truncated: bool,
    pub exit_code: Option<u32>,
    pub stop_reason: Option<String>,
    pub thread_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub adapter_name: String,
    pub program: Option<String>,
    pub status: SessionStatus,
    pub thread_id: Option<u32>,
    pub stop_reason: Option<String>,
    pub output: String,
    pub output_truncated: bool,
    pub exit_code: Option<u32>,
    pub breakpoint_count: usize,
    pub function_breakpoint_count: usize,
    #[serde(skip)]
    pub capabilities: Option<Capabilities>,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugPanelData {
    pub adapter: String,
    pub status: SessionStatus,
    pub session_summary: Option<SessionSummary>,
    pub threads: Vec<Thread>,
    pub frames: Vec<StackFrame>,
    pub variables: Vec<Variable>,
    pub scopes: Vec<Scope>,
    pub breakpoints: Vec<BreakpointRecord>,
    pub output: String,
    pub output_truncated: bool,
    pub exit_code: Option<u32>,
}

/// Extension trait for `StoppedEventReason` which doesn't implement `Display`
/// in the upstream `dap` crate (0.4.1-alpha1).
pub(crate) trait StoppedEventReasonExt {
    fn as_str(&self) -> &str;
}

impl StoppedEventReasonExt for StoppedEventReason {
    fn as_str(&self) -> &str {
        match self {
            StoppedEventReason::Step => "step",
            StoppedEventReason::Breakpoint => "breakpoint",
            StoppedEventReason::Exception => "exception",
            StoppedEventReason::Pause => "pause",
            StoppedEventReason::Entry => "entry",
            StoppedEventReason::Goto => "goto",
            StoppedEventReason::Function => "function",
            StoppedEventReason::Data => "data",
            StoppedEventReason::Instruction => "instruction",
            StoppedEventReason::String(s) => s.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_args_defaults() {
        let args = InitializeArgs::default();
        assert_eq!(args.client_id.as_deref(), Some("dirge"));
        assert_eq!(args.lines_start_at_1, Some(true));
    }

    #[test]
    fn stopped_event_body_deserializes_reason() {
        let json = serde_json::json!({
            "reason": "breakpoint",
            "threadId": 42,
        });
        let body: StoppedEventBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.reason.as_str(), "breakpoint");
        assert_eq!(body.thread_id, Some(42i64));
    }

    #[test]
    fn session_status_serde() {
        let s = SessionStatus::Stopped;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"stopped\"");
        let back: SessionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, SessionStatus::Stopped);
    }
}
