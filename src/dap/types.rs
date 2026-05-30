//! DAP protocol types.
//!
//! Hand-rolled per the Debug Adapter Protocol specification.
//! The `dap-types` crate on crates.io only has v0.0.1 which is
//! too incomplete for the 27 operations we support.
//!
//! Wire format: each message is a JSON object with a `type` field
//! discriminating `request`, `response`, or `event`. This is NOT
//! JSON-RPC 2.0 — DAP uses its own envelope.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Message envelope
// ---------------------------------------------------------------------------

/// Top-level DAP message. The `type` field discriminates the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DapMessage {
    #[serde(rename = "request")]
    Request(DapRequest),
    #[serde(rename = "response")]
    Response(DapResponse),
    #[serde(rename = "event")]
    Event(DapEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapRequest {
    pub seq: u64,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapResponse {
    pub seq: u64,
    pub request_seq: u64,
    pub success: bool,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapEvent {
    pub seq: u64,
    /// The event type string, e.g. "stopped", "output", "terminated".
    #[serde(rename = "event")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

// ---------------------------------------------------------------------------
// Common argument structures
// ---------------------------------------------------------------------------

/// Arguments for the `initialize` request (handshake).
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

/// Arguments for the `launch` request.
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
    /// Catch-all for adapter-specific fields (e.g. dlv wants `mode`, `host`, `port`).
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

/// Arguments for the `attach` request.
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

/// Arguments for `configurationDone`.
/// No mandatory fields — adapter-specific extras go through `extra`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigurationDoneArgs {
    #[serde(flatten)]
    pub extra: Value,
}

/// Arguments for `disconnect`.
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

/// Arguments for `setBreakpoints`.
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

/// Arguments for `setFunctionBreakpoints`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetFunctionBreakpointsArgs {
    pub breakpoints: Vec<FunctionBreakpoint>,
}

/// Arguments for `continue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueArgs {
    #[serde(rename = "threadId")]
    pub thread_id: u32,
    #[serde(rename = "singleThread")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub single_thread: Option<bool>,
}

/// Arguments for `next` (step over).
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

/// Arguments for `stepIn`.
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

/// Arguments for `stepOut`.
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

/// Arguments for `pause`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PauseArgs {
    #[serde(rename = "threadId")]
    pub thread_id: u32,
}

/// Arguments for `stackTrace`.
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

/// Arguments for `scopes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopesArgs {
    #[serde(rename = "frameId")]
    pub frame_id: u32,
}

/// Arguments for `variables`.
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

/// Arguments for `evaluate`.
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

/// Arguments for `threads`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThreadsArgs {}

/// Arguments for `terminate`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TerminateArgs {
    #[serde(rename = "restart")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<bool>,
}

// ---------------------------------------------------------------------------
// DAP model types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(rename = "supportsConfigurationDoneRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_configuration_done_request: Option<bool>,
    #[serde(rename = "supportsFunctionBreakpoints")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_function_breakpoints: Option<bool>,
    #[serde(rename = "supportsConditionalBreakpoints")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_conditional_breakpoints: Option<bool>,
    #[serde(rename = "supportsHitConditionalBreakpoints")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_hit_conditional_breakpoints: Option<bool>,
    #[serde(rename = "supportsEvaluateForHovers")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_evaluate_for_hovers: Option<bool>,
    #[serde(rename = "supportsStepBack")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_step_back: Option<bool>,
    #[serde(rename = "supportsSetVariable")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_set_variable: Option<bool>,
    #[serde(rename = "supportsRestartFrame")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_restart_frame: Option<bool>,
    #[serde(rename = "supportsGotoTargetsRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_goto_targets_request: Option<bool>,
    #[serde(rename = "supportsStepInTargetsRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_step_in_targets_request: Option<bool>,
    #[serde(rename = "supportsCompletionsRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_completions_request: Option<bool>,
    #[serde(rename = "supportsModulesRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_modules_request: Option<bool>,
    #[serde(rename = "supportsRestartRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_restart_request: Option<bool>,
    #[serde(rename = "supportsExceptionOptions")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_exception_options: Option<bool>,
    #[serde(rename = "supportsValueFormattingOptions")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_value_formatting_options: Option<bool>,
    #[serde(rename = "supportsExceptionInfoRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_exception_info_request: Option<bool>,
    #[serde(rename = "supportTerminateDebuggee")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support_terminate_debuggee: Option<bool>,
    #[serde(rename = "supportSuspendDebuggee")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support_suspend_debuggee: Option<bool>,
    #[serde(rename = "supportsLoadedSourcesRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_loaded_sources_request: Option<bool>,
    #[serde(rename = "supportsLogPoints")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_log_points: Option<bool>,
    #[serde(rename = "supportsTerminateThreadsRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_terminate_threads_request: Option<bool>,
    #[serde(rename = "supportsSetExpression")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_set_expression: Option<bool>,
    #[serde(rename = "supportsTerminateRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_terminate_request: Option<bool>,
    #[serde(rename = "supportsDataBreakpoints")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_data_breakpoints: Option<bool>,
    #[serde(rename = "supportsReadMemoryRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_read_memory_request: Option<bool>,
    #[serde(rename = "supportsWriteMemoryRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_write_memory_request: Option<bool>,
    #[serde(rename = "supportsDisassembleRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_disassemble_request: Option<bool>,
    #[serde(rename = "supportsCancelRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_cancel_request: Option<bool>,
    #[serde(rename = "supportsBreakpointLocationsRequest")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_breakpoint_locations_request: Option<bool>,
    #[serde(rename = "supportsClipboardContext")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_clipboard_context: Option<bool>,
    #[serde(rename = "supportsSteppingGranularity")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_stepping_granularity: Option<bool>,
    #[serde(rename = "supportsInstructionBreakpoints")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_instruction_breakpoints: Option<bool>,
    #[serde(rename = "supportsExceptionFilterOptions")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_exception_filter_options: Option<bool>,
    #[serde(rename = "supportsSingleThreadExecutionRequests")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_single_thread_execution_requests: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Source {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "sourceReference")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_reference: Option<u32>,
    #[serde(rename = "presentationHint")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<Source>>,
    #[serde(rename = "adapterData")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksums: Option<Vec<Checksum>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checksum {
    pub algorithm: String,
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackFrame {
    pub id: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub line: u32,
    pub column: u32,
    #[serde(rename = "endLine")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(rename = "endColumn")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    #[serde(rename = "canRestart")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_restart: Option<bool>,
    #[serde(rename = "instructionPointerReference")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_pointer_reference: Option<String>,
    #[serde(rename = "moduleId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_id: Option<Value>,
    #[serde(rename = "presentationHint")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackFrameFormat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<bool>,
    #[serde(rename = "parameterTypes")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_types: Option<bool>,
    #[serde(rename = "parameterNames")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_names: Option<bool>,
    #[serde(rename = "parameterValues")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_values: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<bool>,
    #[serde(rename = "includeAll")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_all: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    pub name: String,
    #[serde(rename = "presentationHint")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<String>,
    #[serde(rename = "variablesReference")]
    pub variables_reference: u32,
    #[serde(rename = "namedVariables")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named_variables: Option<u32>,
    #[serde(rename = "indexedVariables")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_variables: Option<u32>,
    pub expensive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(rename = "endLine")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(rename = "endColumn")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub var_type: Option<String>,
    #[serde(rename = "presentationHint")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<Value>,
    #[serde(rename = "evaluateName")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluate_name: Option<String>,
    #[serde(rename = "variablesReference")]
    pub variables_reference: u32,
    #[serde(rename = "namedVariables")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named_variables: Option<u32>,
    #[serde(rename = "indexedVariables")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_variables: Option<u32>,
    #[serde(rename = "memoryReference")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueFormat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hex: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceBreakpoint {
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(rename = "hitCondition")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    #[serde(rename = "logMessage")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Breakpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(rename = "endLine")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(rename = "endColumn")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    #[serde(rename = "instructionReference")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionBreakpoint {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(rename = "hitCondition")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakpointRecord {
    pub file: String,
    pub breakpoints: Vec<SourceBreakpoint>,
    /// Cached server-verified breakpoints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<Vec<Breakpoint>>,
}

// ---------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackTraceResponse {
    #[serde(rename = "stackFrames")]
    pub stack_frames: Vec<StackFrame>,
    #[serde(rename = "totalFrames")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_frames: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopesResponse {
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariablesResponse {
    pub variables: Vec<Variable>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadsResponse {
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluateResponse {
    pub result: String,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub var_type: Option<String>,
    #[serde(rename = "presentationHint")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<Value>,
    #[serde(rename = "variablesReference")]
    pub variables_reference: u32,
    #[serde(rename = "namedVariables")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named_variables: Option<u32>,
    #[serde(rename = "indexedVariables")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_variables: Option<u32>,
    #[serde(rename = "memoryReference")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetBreakpointsResponse {
    pub breakpoints: Vec<Breakpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetFunctionBreakpointsResponse {
    pub breakpoints: Vec<Breakpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueResponse {
    #[serde(rename = "allThreadsContinued")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_threads_continued: Option<bool>,
}

/// Returned from step (next/stepIn/stepOut) and pause — shape is empty or
/// adapter-specific.
pub type StepResponse = Value;

// ---------------------------------------------------------------------------
// Event bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputEventBody {
    pub category: Option<String>,
    pub output: String,
    #[serde(rename = "groupId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables_reference: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoppedEventBody {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "threadId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<u32>,
    #[serde(rename = "preserveFocusHint")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_focus_hint: Option<bool>,
    #[serde(rename = "text")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(rename = "allThreadsStopped")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_threads_stopped: Option<bool>,
    #[serde(rename = "hitBreakpointIds")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_breakpoint_ids: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitedEventBody {
    #[serde(rename = "exitCode")]
    pub exit_code: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminatedEventBody {
    #[serde(rename = "restart")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEventBody {
    pub name: String,
    #[serde(rename = "systemProcessId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_process_id: Option<u32>,
    #[serde(rename = "isLocalProcess")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_local_process: Option<bool>,
    #[serde(rename = "startMethod")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_method: Option<String>,
    #[serde(rename = "pointerSize")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer_size: Option<u32>,
}

// ---------------------------------------------------------------------------
// Session types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Launching,
    Configuring,
    Stopped,
    Running,
    Terminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueOutcome {
    pub status: SessionStatus,
    pub output: String,
    pub output_truncated: bool,
    pub exit_code: Option<u32>,
    #[serde(rename = "stopReason")]
    pub stop_reason: Option<String>,
    #[serde(rename = "threadId")]
    pub thread_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub adapter_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub status: SessionStatus,
    #[serde(rename = "breakpointCount")]
    pub breakpoint_count: usize,
    #[serde(rename = "functionBreakpointCount")]
    pub function_breakpoint_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(rename = "threadId")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<u32>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the DapMessage tagged enum round-trips correctly for each
    /// variant. This is the most important serde test — if the tag doesn't
    /// work, the whole protocol is broken.
    #[test]
    fn request_roundtrip() {
        let msg = DapMessage::Request(DapRequest {
            seq: 1,
            command: "initialize".into(),
            arguments: Some(serde_json::json!({
                "adapterID": "test-adapter",
                "clientID": "dirge",
                "linesStartAt1": true,
                "columnsStartAt1": true,
            })),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let round: DapMessage = serde_json::from_str(&json).unwrap();
        match round {
            DapMessage::Request(req) => {
                assert_eq!(req.seq, 1);
                assert_eq!(req.command, "initialize");
            }
            _ => panic!("expected Request variant"),
        }
    }

    #[test]
    fn response_roundtrip() {
        let msg = DapMessage::Response(DapResponse {
            seq: 2,
            request_seq: 1,
            success: true,
            command: "initialize".into(),
            message: None,
            body: Some(serde_json::json!({
                "supportsConfigurationDoneRequest": true,
                "supportsFunctionBreakpoints": false,
            })),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let round: DapMessage = serde_json::from_str(&json).unwrap();
        match round {
            DapMessage::Response(resp) => {
                assert!(resp.success);
                assert_eq!(resp.request_seq, 1);
            }
            _ => panic!("expected Response variant"),
        }
    }

    #[test]
    fn event_roundtrip() {
        let msg = DapMessage::Event(DapEvent {
            seq: 3,
            event_type: "stopped".into(),
            body: Some(serde_json::json!({
                "reason": "breakpoint",
                "threadId": 1,
            })),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let round: DapMessage = serde_json::from_str(&json).unwrap();
        match round {
            DapMessage::Event(evt) => {
                assert_eq!(evt.event_type, "stopped");
            }
            _ => panic!("expected Event variant"),
        }
    }

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
        assert_eq!(body.reason, "breakpoint");
        assert_eq!(body.thread_id, Some(42));
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
