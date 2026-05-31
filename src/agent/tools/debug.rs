//! Agent-facing `debug` tool — DAP debugger integration.
//!
//! Dispatches to [`crate::dap::session::DapSessionManager`] and uses
//! [`crate::dap::config`] for adapter resolution. One tool, one `action`
//! parameter; the agent picks which debug operation to invoke.
//!
//! When the LSP feature is also enabled, this tool gains DAP↔LSP bridge
//! actions (`run_to_cursor`, `restart_frame`, `backtrace_diagnostics`,
//! `error_analysis`) that coordinate the debugger with LSP code intelligence.

use std::sync::Arc;
use std::time::Duration;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

use crate::agent::agent_loop::tool::AbortSignal;
use crate::agent::tools::{
    AskSender, PermCheck, ToolError, check_perm, head_cap, required_nonblank,
};
use crate::dap::config::{self, ConnectMode, ResolvedAdapter};
use crate::dap::session::{DAP_MANAGER, DapSessionManager};
use crate::dap::types::{FunctionBreakpoint, SourceBreakpoint};

#[cfg(feature = "lsp")]
use crate::lsp::manager::LspManager;

const DESCRIPTION: &str = "\
Debug a program using the Debug Adapter Protocol (DAP). \
Supports launching, attaching, setting breakpoints, stepping, evaluating expressions, \
and inspecting stack frames, scopes, variables, and threads.\n\
\n\
Use this instead of printf debugging or adding temporary print statements to \
diagnose crashes, inspect runtime state, and trace execution flow. \
The debugger can stop at breakpoints, step through code, and show variable values \
without modifying source code.\n\
\n\
Actions:\n\
- launch: start a new debug session by launching a program\n\
- attach: attach to a running process\n\
- set_breakpoints: set breakpoints in a file\n\
- remove_breakpoints: clear all breakpoints from a file\n\
- continue: resume execution until next breakpoint or exit\n\
- step_over: execute the next line, stepping over function calls\n\
- step_in: step into the next function call\n\
- step_out: step out of the current function\n\
- pause: pause execution of a running program\n\
- evaluate: evaluate an expression in the debuggee\n\
- stack_trace: get the call stack for a thread\n\
- threads: list all threads\n\
- scopes: get variable scopes for a stack frame\n\
- variables: get variables within a scope\n\
- terminate: terminate the debuggee\n\
- sessions: show active debug session info\n\
- run_to_cursor: set breakpoint at cursor, continue, and show LSP hover info at the stop location\n\
- restart_frame: re-execute the current stack frame (edit-and-continue)\n\
- backtrace_diagnostics: get stack trace with LSP diagnostics for each frame\n\
- error_analysis: get stack trace with LSP error diagnostics and suggested breakpoints\n\
\n\
Timeouts in seconds (default 30, min 5, max 300).";

const MAX_OUTPUT_BYTES: usize = 128 * 1024;

pub struct DebugTool {
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    session: Arc<DapSessionManager>,
    #[cfg(feature = "lsp")]
    lsp_manager: Option<Arc<LspManager>>,
}

impl DebugTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        let session = Arc::new(DapSessionManager::new());
        *DAP_MANAGER.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
        Self {
            permission,
            ask_tx,
            session,
            #[cfg(feature = "lsp")]
            lsp_manager: None,
        }
    }

    #[cfg(feature = "lsp")]
    pub fn new_with_lsp(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        lsp_manager: Arc<LspManager>,
    ) -> Self {
        let session = Arc::new(DapSessionManager::new());
        *DAP_MANAGER.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
        Self {
            permission,
            ask_tx,
            session,
            lsp_manager: Some(lsp_manager),
        }
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct DebugArgs {
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub adapter: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    // DEAD CODE: function breakpoints not yet exposed as a debug-tool action.
    #[serde(default)]
    pub function: Option<String>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub expression: Option<String>,
    #[serde(default)]
    pub frame_id: Option<u32>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub levels: Option<u32>,
    #[serde(default)]
    pub variable_ref: Option<u32>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub thread_id: Option<u32>,
    #[serde(default)]
    pub stop_on_entry: Option<bool>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub restart: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Launch,
    Attach,
    SetBreakpoints,
    RemoveBreakpoints,
    Continue,
    StepOver,
    StepIn,
    StepOut,
    Pause,
    Evaluate,
    StackTrace,
    Threads,
    Scopes,
    Variables,
    Terminate,
    Sessions,
    FunctionBreakpoints,
    #[cfg(feature = "lsp")]
    RunToCursor,
    #[cfg(feature = "lsp")]
    RestartFrame,
    #[cfg(feature = "lsp")]
    BacktraceDiagnostics,
    #[cfg(feature = "lsp")]
    ErrorAnalysis,
}

impl Action {
    fn parse(s: &str) -> Option<Action> {
        match s {
            "launch" => Some(Action::Launch),
            "attach" => Some(Action::Attach),
            "set_breakpoints" | "set_breakpoint" => Some(Action::SetBreakpoints),
            "remove_breakpoints" | "remove_breakpoint" => Some(Action::RemoveBreakpoints),
            "continue" => Some(Action::Continue),
            "step_over" => Some(Action::StepOver),
            "step_in" => Some(Action::StepIn),
            "step_out" => Some(Action::StepOut),
            "pause" => Some(Action::Pause),
            "evaluate" => Some(Action::Evaluate),
            "stack_trace" => Some(Action::StackTrace),
            "threads" => Some(Action::Threads),
            "scopes" => Some(Action::Scopes),
            "variables" => Some(Action::Variables),
            "terminate" => Some(Action::Terminate),
            "sessions" => Some(Action::Sessions),
            "function_breakpoints" | "function_breakpoint" => Some(Action::FunctionBreakpoints),
            #[cfg(feature = "lsp")]
            "run_to_cursor" => Some(Action::RunToCursor),
            #[cfg(feature = "lsp")]
            "restart_frame" => Some(Action::RestartFrame),
            #[cfg(feature = "lsp")]
            "backtrace_diagnostics" => Some(Action::BacktraceDiagnostics),
            #[cfg(feature = "lsp")]
            "error_analysis" => Some(Action::ErrorAnalysis),
            _ => None,
        }
    }
}

/// Clamp timeout to [5, 300] seconds, default 30.
fn clamp_timeout(secs: Option<u64>) -> u64 {
    secs.unwrap_or(30).clamp(5, 300)
}

/// Resolve an adapter for launch: if `adapter_name` given, use it directly;
/// otherwise auto-detect from program extension + cwd root markers.
fn resolve_launch_adapter(
    program: &str,
    cwd: &str,
    adapter_name: Option<&str>,
) -> Result<ResolvedAdapter, ToolError> {
    if let Some(name) = adapter_name {
        config::resolve_adapter(name)
            .ok_or_else(|| ToolError::Msg(format!("adapter not found on PATH: {name}")))
    } else {
        let cwd_path = std::path::Path::new(cwd);
        let prog_path = std::path::Path::new(program);
        config::select_launch_adapter(prog_path, cwd_path, None).ok_or_else(|| {
            ToolError::Msg(format!(
                "no debug adapter found for {program}. \
                     Install one (lldb-dap, gdb, dlv, debugpy, etc.) or specify --adapter"
            ))
        })
    }
}

/// Resolve an adapter for attach.
fn resolve_attach_adapter(
    adapter_name: Option<&str>,
    port: Option<u16>,
) -> Result<ResolvedAdapter, ToolError> {
    if let Some(name) = adapter_name {
        config::resolve_adapter(name)
            .ok_or_else(|| ToolError::Msg(format!("adapter not found on PATH: {name}")))
    } else {
        config::select_attach_adapter(None, port).ok_or_else(|| {
            ToolError::Msg(
                "no debug adapter found for attach. Install gdb, lldb-dap, or specify --adapter"
                    .into(),
            )
        })
    }
}

impl Tool for DebugTool {
    const NAME: &'static str = "debug";

    type Error = ToolError;
    type Args = DebugArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "debug".to_string(),
            description: DESCRIPTION.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "Debug action to perform",
                        "enum": [
                            "launch", "attach", "set_breakpoints", "remove_breakpoints",
                            "continue", "step_over", "step_in", "step_out",
                            "pause", "evaluate", "stack_trace", "threads",
                            "scopes", "variables", "terminate", "sessions",
                            "run_to_cursor", "restart_frame",
                            "backtrace_diagnostics", "error_analysis"
                        ]
                    },
                    "program": { "type": "string", "description": "Path to the program to debug (launch)" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Command line arguments for the program (launch)" },
                    "adapter": { "type": "string", "description": "Debug adapter name (auto-detected if omitted)" },
                    "cwd": { "type": "string", "description": "Working directory for the debug session" },
                    "file": { "type": "string", "description": "Source file path (set_breakpoints, remove_breakpoints)" },
                    "line": { "type": "integer", "description": "Line number (set_breakpoints)" },
                    "function": { "type": "string", "description": "Function name (set_breakpoints function breakpoint)" },
                    "condition": { "type": "string", "description": "Conditional breakpoint expression" },
                    "expression": { "type": "string", "description": "Expression to evaluate" },
                    "frame_id": { "type": "integer", "description": "Stack frame ID (scopes, evaluate)" },
                    "pid": { "type": "integer", "description": "Process ID to attach to" },
                    "port": { "type": "integer", "description": "Port for remote attach" },
                    "host": { "type": "string", "description": "Host for remote attach" },
                    "levels": { "type": "integer", "description": "Number of stack frames to fetch" },
                    "variable_ref": { "type": "integer", "description": "Variable reference ID from a scope" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (default 30, min 5, max 300)" },
                    "thread_id": { "type": "integer", "description": "Thread ID (continue, step, pause, stack_trace)" },
                    "stop_on_entry": { "type": "boolean", "description": "Stop at program entry (launch)" },
                    "context": { "type": "string", "description": "Evaluation context: watch, repl, hover" },
                    "restart": { "type": "boolean", "description": "Restart after disconnect (terminate)" }
                },
                "required": ["action"]
            }),
        }
    }

    async fn call(&self, args: DebugArgs) -> Result<String, ToolError> {
        let action_str = required_nonblank(args.action.as_deref(), "action", "debug")?;
        let action = Action::parse(action_str).ok_or_else(|| {
            ToolError::Msg(format!(
                "unknown debug action {action_str:?}; see the tool description for valid values"
            ))
        })?;

        check_perm(&self.permission, &self.ask_tx, "debug", action_str).await?;

        let timeout = Duration::from_secs(clamp_timeout(args.timeout));
        let signal = AbortSignal::new();
        let mgr = &self.session;

        match action {
            Action::Launch => {
                let program = required_nonblank(args.program.as_deref(), "program", "launch")?;
                let cwd = args.cwd.as_deref().unwrap_or(".");
                let adapter = resolve_launch_adapter(program, cwd, args.adapter.as_deref())?;

                if adapter.connect_mode == ConnectMode::Socket {
                    return Err(ToolError::Msg(
                        "socket-mode adapters are not yet supported. Use a stdio-mode adapter instead."
                            .into(),
                    ));
                }

                let program_args = args.args.unwrap_or_default();
                let summary = mgr
                    .launch(
                        &adapter.name,
                        &adapter.resolved_command.to_string_lossy(),
                        &adapter.args,
                        cwd,
                        program,
                        &program_args,
                        args.stop_on_entry,
                        Some(adapter.launch_defaults.clone()),
                        &signal,
                        timeout,
                        adapter.languages.clone(),
                    )
                    .await?;

                Ok(format_launch_summary(&summary, &adapter.name))
            }

            Action::Attach => {
                let cwd = args.cwd.as_deref().unwrap_or(".");
                let adapter = resolve_attach_adapter(args.adapter.as_deref(), args.port)?;

                if adapter.connect_mode == ConnectMode::Socket {
                    return Err(ToolError::Msg(
                        "socket-mode adapters are not yet supported. Use a stdio-mode adapter instead."
                            .into(),
                    ));
                }

                let summary = mgr
                    .attach(
                        &adapter.name,
                        &adapter.resolved_command.to_string_lossy(),
                        &adapter.args,
                        cwd,
                        args.pid,
                        args.port,
                        args.host,
                        Some(adapter.attach_defaults.clone()),
                        &signal,
                        timeout,
                        adapter.languages.clone(),
                    )
                    .await?;

                Ok(format_attach_summary(&summary, &adapter.name))
            }

            Action::SetBreakpoints => {
                let file = required_nonblank(args.file.as_deref(), "file", "set_breakpoints")?;
                let line = args.line.ok_or_else(|| {
                    ToolError::Msg("`line` is required for set_breakpoints".into())
                })?;

                let bp = SourceBreakpoint {
                    line: line as i64,
                    condition: args.condition,
                    ..Default::default()
                };

                let results = mgr.set_breakpoints(file, vec![bp], timeout).await?;

                Ok(format_breakpoints_result(file, line, &results))
            }

            Action::RemoveBreakpoints => {
                let file = required_nonblank(args.file.as_deref(), "file", "remove_breakpoints")?;

                let results = mgr.set_breakpoints(file, vec![], timeout).await?;

                Ok(format!(
                    "Removed all breakpoints from {} ({} remaining in adapter)",
                    file,
                    results.len()
                ))
            }

            Action::Continue => {
                let thread_id = args.thread_id.unwrap_or(0);
                let outcome = mgr.continue_(thread_id, &signal, timeout).await?;
                Ok(format_continue_outcome(&outcome))
            }

            Action::StepOver => {
                let thread_id = args.thread_id.ok_or_else(|| {
                    ToolError::Msg("`thread_id` is required for step_over".into())
                })?;
                let summary = mgr.step_over(thread_id, &signal, timeout).await?;
                Ok(format!(
                    "Step over complete.\n{}",
                    format_sessions(&summary)
                ))
            }

            Action::StepIn => {
                let thread_id = args
                    .thread_id
                    .ok_or_else(|| ToolError::Msg("`thread_id` is required for step_in".into()))?;
                let summary = mgr.step_in(thread_id, &signal, timeout).await?;
                Ok(format!("Step in complete.\n{}", format_sessions(&summary)))
            }

            Action::StepOut => {
                let thread_id = args
                    .thread_id
                    .ok_or_else(|| ToolError::Msg("`thread_id` is required for step_out".into()))?;
                let summary = mgr.step_out(thread_id, &signal, timeout).await?;
                Ok(format!("Step out complete.\n{}", format_sessions(&summary)))
            }

            Action::Pause => {
                let thread_id = args.thread_id.unwrap_or(0);
                let summary = mgr.pause(thread_id, timeout).await?;
                Ok(format!("Execution paused.\n{}", format_sessions(&summary)))
            }

            Action::Evaluate => {
                let expression =
                    required_nonblank(args.expression.as_deref(), "expression", "evaluate")?;
                let context = args.context.as_deref();
                let result = mgr
                    .evaluate(expression, args.frame_id, context, timeout)
                    .await?;

                let output = serde_json::to_string_pretty(&result)
                    .unwrap_or_else(|_| format!("{:?}", result));
                Ok(head_cap(output, MAX_OUTPUT_BYTES, "evaluate result"))
            }

            Action::StackTrace => {
                let thread_id = args.thread_id.ok_or_else(|| {
                    ToolError::Msg("`thread_id` is required for stack_trace".into())
                })?;
                let frames = mgr.stack_trace(thread_id, args.levels, timeout).await?;

                let output = serde_json::to_string_pretty(&frames)
                    .unwrap_or_else(|_| format!("{:?}", frames));
                Ok(format!(
                    "Stack trace for thread {} ({} frames):\n{}",
                    thread_id,
                    frames.len(),
                    head_cap(output, MAX_OUTPUT_BYTES, "stack trace")
                ))
            }

            Action::Threads => {
                let threads = mgr.threads(timeout).await?;
                let output = serde_json::to_string_pretty(&threads)
                    .unwrap_or_else(|_| format!("{:?}", threads));
                Ok(format!(
                    "{} threads:\n{}",
                    threads.len(),
                    head_cap(output, MAX_OUTPUT_BYTES, "threads")
                ))
            }

            Action::Scopes => {
                let frame_id = args
                    .frame_id
                    .ok_or_else(|| ToolError::Msg("`frame_id` is required for scopes".into()))?;
                let scopes = mgr.scopes(frame_id, timeout).await?;

                let output = serde_json::to_string_pretty(&scopes)
                    .unwrap_or_else(|_| format!("{:?}", scopes));
                Ok(format!(
                    "Scopes for frame {}:\n{}",
                    frame_id,
                    head_cap(output, MAX_OUTPUT_BYTES, "scopes")
                ))
            }

            Action::Variables => {
                let variable_ref = args.variable_ref.ok_or_else(|| {
                    ToolError::Msg("`variable_ref` is required for variables".into())
                })?;
                let vars = mgr.variables(variable_ref, timeout).await?;

                let output =
                    serde_json::to_string_pretty(&vars).unwrap_or_else(|_| format!("{:?}", vars));
                Ok(format!(
                    "Variables (ref {}):\n{}",
                    variable_ref,
                    head_cap(output, MAX_OUTPUT_BYTES, "variables")
                ))
            }

            Action::Terminate => {
                let restart = args.restart.unwrap_or(false);
                if restart {
                    mgr.disconnect(true, timeout).await?;
                    Ok("Disconnected with restart.".into())
                } else {
                    let summary = mgr.terminate(timeout).await?;
                    Ok(format!(
                        "Debug session terminated.\n{}",
                        format_sessions(&summary)
                    ))
                }
            }

            Action::Sessions => {
                let summary = mgr.active_summary().await;
                match summary {
                    Some(s) => Ok(format_sessions(&s)),
                    None => Ok("No active debug session.".into()),
                }
            }

            Action::FunctionBreakpoints => {
                let function_name = required_nonblank(
                    args.function.as_deref(),
                    "function",
                    "function_breakpoints",
                )?;
                let fb = FunctionBreakpoint {
                    name: function_name.to_string(),
                    condition: args.condition,
                    ..Default::default()
                };

                let results = mgr.set_function_breakpoints(vec![fb], timeout).await?;

                Ok(format!(
                    "Function breakpoint set on {} ({} breakpoints in adapter)",
                    function_name,
                    results.len()
                ))
            }

            #[cfg(feature = "lsp")]
            Action::RunToCursor => {
                let file = required_nonblank(args.file.as_deref(), "file", "run_to_cursor")?;
                let line = args
                    .line
                    .ok_or_else(|| ToolError::Msg("`line` is required for run_to_cursor".into()))?;
                let lsp = self
                    .lsp_manager
                    .as_ref()
                    .ok_or_else(|| ToolError::Msg("LSP not available for run_to_cursor".into()))?;

                run_to_cursor(mgr, lsp, file, line, args.thread_id, &signal, timeout).await
            }

            #[cfg(feature = "lsp")]
            Action::RestartFrame => {
                let frame_id = args.frame_id.ok_or_else(|| {
                    ToolError::Msg("`frame_id` is required for restart_frame".into())
                })?;
                mgr.restart_frame(frame_id, timeout).await?;
                Ok(format!(
                    "Restarted frame {frame_id}. Re-executing from frame start."
                ))
            }

            #[cfg(feature = "lsp")]
            Action::BacktraceDiagnostics => {
                let thread_id = args.thread_id.ok_or_else(|| {
                    ToolError::Msg("`thread_id` is required for backtrace_diagnostics".into())
                })?;
                let lsp = self.lsp_manager.as_ref().ok_or_else(|| {
                    ToolError::Msg("LSP not available for backtrace_diagnostics".into())
                })?;

                backtrace_diagnostics(mgr, lsp, thread_id, timeout).await
            }

            #[cfg(feature = "lsp")]
            Action::ErrorAnalysis => {
                let thread_id = args.thread_id.ok_or_else(|| {
                    ToolError::Msg("`thread_id` is required for error_analysis".into())
                })?;
                let lsp = self
                    .lsp_manager
                    .as_ref()
                    .ok_or_else(|| ToolError::Msg("LSP not available for error_analysis".into()))?;

                error_analysis(mgr, lsp, thread_id, timeout).await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn format_launch_summary(s: &crate::dap::types::SessionSummary, adapter: &str) -> String {
    let mut out = format!("Launched with {adapter} (session {}).\n", s.id);
    if let Some(reason) = &s.stop_reason {
        out.push_str(&format!("Program stopped: {reason}"));
        if let Some(tid) = s.thread_id {
            out.push_str(&format!(" (thread {tid})"));
        }
        out.push('\n');
    }
    out
}

fn format_attach_summary(s: &crate::dap::types::SessionSummary, adapter: &str) -> String {
    format!(
        "Attached with {adapter} (session {}). Status: {:?}.\n",
        s.id, s.status
    )
}

fn format_breakpoints_result(
    file: &str,
    line: u32,
    results: &[crate::dap::types::Breakpoint],
) -> String {
    let mut out = format!("Breakpoint set at {file}:{line}.\n");
    for (i, bp) in results.iter().enumerate() {
        let status = if bp.verified {
            "verified"
        } else {
            "unverified"
        };
        out.push_str(&format!(
            "  [{i}] id={} line={} {status}",
            bp.id.unwrap_or(0),
            bp.line.unwrap_or(0),
        ));
        if let Some(msg) = &bp.message {
            out.push_str(&format!(" ({msg})"));
        }
        out.push('\n');
    }
    out
}

fn format_continue_outcome(o: &crate::dap::types::ContinueOutcome) -> String {
    let mut out = String::new();
    if let Some(reason) = &o.stop_reason {
        out.push_str(&format!("Execution stopped: {reason}"));
        if let Some(tid) = o.thread_id {
            out.push_str(&format!(" (thread {tid})"));
        }
        out.push('\n');
    } else {
        out.push_str("Execution continued.\n");
    }
    if let Some(code) = o.exit_code {
        out.push_str(&format!("Exit code: {code}\n"));
    }
    if !o.output.is_empty() {
        out.push_str("Program output:\n");
        out.push_str(&o.output);
        if o.output_truncated {
            out.push_str("\n…[output truncated]");
        }
        out.push('\n');
    }
    out
}

fn format_sessions(s: &crate::dap::types::SessionSummary) -> String {
    let mut out = format!("Session {} ({}) — {:?}", s.id, s.adapter_name, s.status);
    if let Some(reason) = &s.stop_reason {
        out.push_str(&format!("\nStop reason: {reason}"));
        if let Some(tid) = s.thread_id {
            out.push_str(&format!(" (thread {tid})"));
        }
    }
    if !s.languages.is_empty() {
        out.push_str(&format!("\nLanguages: {}", s.languages.join(", ")));
    }
    out.push_str(&format!(
        "\nBreakpoints: {} file, {} function",
        s.breakpoint_count, s.function_breakpoint_count
    ));
    if s.capabilities.is_some() {
        out.push_str("\nCapabilities: loaded");
    }
    out
}

// ---------------------------------------------------------------------------
// DAP↔LSP bridge helpers (available when both features are enabled)
// ---------------------------------------------------------------------------

#[cfg(feature = "lsp")]
use std::path::Path;

/// Set a breakpoint at `file:line`, continue, then get LSP hover info
/// at the stopped location. Returns the stop location + hover results.
#[cfg(feature = "lsp")]
async fn run_to_cursor(
    mgr: &DapSessionManager,
    lsp: &LspManager,
    file: &str,
    line: u32,
    thread_id: Option<u32>,
    signal: &AbortSignal,
    timeout: Duration,
) -> Result<String, ToolError> {
    // Set a breakpoint at the target line.
    let bp = SourceBreakpoint {
        line: line as i64,
        column: None,
        condition: None,
        log_message: None,
        hit_condition: None,
    };
    mgr.set_breakpoints(file, vec![bp], timeout).await?;

    // Continue to the breakpoint.
    let outcome = mgr
        .continue_(thread_id.unwrap_or(0), signal, timeout)
        .await?;

    // Get hover info from LSP at the stopped location.
    let mut result = format_continue_outcome(&outcome);
    if let Some(ref reason) = outcome.stop_reason {
        if reason != "terminated" {
            let path = Path::new(file);
            let hover_results = lsp.hover(path, line.saturating_sub(1), 0).await;
            if !hover_results.is_empty() {
                let hover_json = serde_json::to_string_pretty(&hover_results).unwrap_or_default();
                result.push_str(&format!("\n\nHover info at {file}:{line}:\n{hover_json}"));
            }
        }
    }
    Ok(result)
}

/// Get a stack trace, then fetch LSP diagnostics for each source file in the frames.
#[cfg(feature = "lsp")]
async fn backtrace_diagnostics(
    mgr: &DapSessionManager,
    lsp: &LspManager,
    thread_id: u32,
    timeout: Duration,
) -> Result<String, ToolError> {
    let frames = mgr.stack_trace(thread_id, None, timeout).await?;

    let all_diags = lsp.all_diagnostics();
    let mut out = format!("Backtrace diagnostics for thread {thread_id}:\n\n");

    let mut seen_files = std::collections::HashSet::new();
    for (i, frame) in frames.iter().enumerate() {
        if let Some(ref source) = frame.source {
            if let Some(ref path) = source.path {
                if seen_files.insert(path.clone()) {
                    let frame_loc = match source.name.as_deref() {
                        Some(name) => format!("{name}:{}", frame.line),
                        None => path.clone(),
                    };

                    let p = std::path::PathBuf::from(path);
                    // Touch file to ensure LSP server is aware of it.
                    lsp.touch_file(&p, crate::lsp::manager::TouchMode::Notify)
                        .await;

                    let diags = all_diags.get(&p).map(|v| v.as_slice()).unwrap_or(&[]);
                    if diags.is_empty() {
                        out.push_str(&format!("  [{i}] {frame_loc} — no diagnostics\n"));
                    } else {
                        out.push_str(&format!(
                            "  [{i}] {frame_loc} — {} diagnostics:\n",
                            diags.len()
                        ));
                        for d in diags.iter().take(5) {
                            let severity = format!("{:?}", d.severity);
                            out.push_str(&format!(
                                "      L{} — {severity}: {}\n",
                                d.range.start.line + 1,
                                d.message
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Get a stack trace, then for each frame fetch LSP diagnostics and
/// document symbols. Identify error-prone locations and suggest breakpoints.
#[cfg(feature = "lsp")]
async fn error_analysis(
    mgr: &DapSessionManager,
    lsp: &LspManager,
    thread_id: u32,
    timeout: Duration,
) -> Result<String, ToolError> {
    let frames = mgr.stack_trace(thread_id, None, timeout).await?;

    let all_diags = lsp.all_diagnostics();
    let mut out = format!("Error analysis for thread {thread_id}:\n\n");
    out.push_str("Stack frames with diagnostics and suggested breakpoints:\n\n");

    let mut seen_files = std::collections::HashSet::new();
    for (i, frame) in frames.iter().enumerate() {
        if let Some(ref source) = frame.source {
            if let Some(ref path) = source.path {
                if seen_files.insert(path.clone()) {
                    let p = std::path::PathBuf::from(path);
                    lsp.touch_file(&p, crate::lsp::manager::TouchMode::Notify)
                        .await;

                    let frame_loc = match source.name.as_deref() {
                        Some(name) => format!("{name}:{}", frame.line),
                        None => path.clone(),
                    };

                    out.push_str(&format!("Frame [{i}]: {frame_loc}\n"));

                    let diags = all_diags.get(&p).map(|v| v.as_slice()).unwrap_or(&[]);
                    let error_diags: Vec<_> = diags
                        .iter()
                        .filter(|d| {
                            matches!(d.severity, Some(lsp_types::DiagnosticSeverity::ERROR))
                        })
                        .collect();

                    if error_diags.is_empty() {
                        out.push_str("  No error diagnostics in this file.\n");
                    } else {
                        for d in error_diags.iter().take(5) {
                            let bp_line = d.range.start.line + 1;
                            out.push_str(&format!("  Error at line {bp_line}: {}\n", d.message));
                            out.push_str(&format!(
                                "    → debug set_breakpoints file={path} line={bp_line}\n"
                            ));
                        }
                    }

                    // Show document symbols for context.
                    let symbols = lsp.document_symbol(&p).await;
                    if !symbols.is_empty() {
                        let sym_json = serde_json::to_string_pretty(&symbols).unwrap_or_default();
                        let capped = head_cap(sym_json, 2048, "document symbols");
                        out.push_str(&format!("  Top-level symbols:\n{capped}\n"));
                    }
                    out.push('\n');
                }
            }
        }
    }

    if frames.is_empty() {
        out.push_str("(no stack frames available)\n");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn action_parse_valid() {
        assert!(Action::parse("launch").is_some());
        assert!(Action::parse("attach").is_some());
        assert!(Action::parse("set_breakpoints").is_some());
        assert!(Action::parse("set_breakpoint").is_some());
        assert!(Action::parse("remove_breakpoints").is_some());
        assert!(Action::parse("continue").is_some());
        assert!(Action::parse("step_over").is_some());
        assert!(Action::parse("step_in").is_some());
        assert!(Action::parse("step_out").is_some());
        assert!(Action::parse("pause").is_some());
        assert!(Action::parse("evaluate").is_some());
        assert!(Action::parse("stack_trace").is_some());
        assert!(Action::parse("threads").is_some());
        assert!(Action::parse("scopes").is_some());
        assert!(Action::parse("variables").is_some());
        assert!(Action::parse("terminate").is_some());
        assert!(Action::parse("sessions").is_some());

        #[cfg(feature = "lsp")]
        {
            assert!(Action::parse("run_to_cursor").is_some());
            assert!(Action::parse("restart_frame").is_some());
            assert!(Action::parse("backtrace_diagnostics").is_some());
            assert!(Action::parse("error_analysis").is_some());
        }
    }

    #[test]
    fn action_parse_invalid() {
        assert!(Action::parse("disassemble").is_none());
        assert!(Action::parse("").is_none());
        assert!(Action::parse("unknown_action").is_none());

        #[cfg(not(feature = "lsp"))]
        {
            // Bridge actions require the lsp feature.
            assert!(Action::parse("run_to_cursor").is_none());
            assert!(Action::parse("restart_frame").is_none());
            assert!(Action::parse("backtrace_diagnostics").is_none());
            assert!(Action::parse("error_analysis").is_none());
        }
    }

    #[test]
    fn clamp_timeout_default() {
        assert_eq!(clamp_timeout(None), 30);
    }

    #[test]
    fn clamp_timeout_below_min() {
        assert_eq!(clamp_timeout(Some(3)), 5);
    }

    #[test]
    fn clamp_timeout_above_max() {
        assert_eq!(clamp_timeout(Some(500)), 300);
    }

    #[test]
    fn clamp_timeout_in_range() {
        assert_eq!(clamp_timeout(Some(60)), 60);
    }

    #[tokio::test]
    async fn sessions_no_active_session() {
        let tool = DebugTool::new(None, None);

        let result = tool
            .call(DebugArgs {
                action: Some("sessions".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(result.contains("No active debug session"));
    }

    #[test]
    fn definition_has_all_actions() {
        let tool = DebugTool::new(None, None);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let def = rt.block_on(tool.definition(String::new()));
        let params: Value = def.parameters;

        // Every action in the enum list must be in the schema.
        let actions: Vec<&str> = params["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        for expected in &[
            "launch",
            "attach",
            "set_breakpoints",
            "remove_breakpoints",
            "continue",
            "step_over",
            "step_in",
            "step_out",
            "pause",
            "evaluate",
            "stack_trace",
            "threads",
            "scopes",
            "variables",
            "terminate",
            "sessions",
            "run_to_cursor",
            "restart_frame",
            "backtrace_diagnostics",
            "error_analysis",
        ] {
            assert!(
                actions.contains(expected),
                "schema missing action {expected:?}"
            );
        }
    }

    #[test]
    fn definition_name_matches() {
        let tool = DebugTool::new(None, None);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let def = rt.block_on(tool.definition(String::new()));
        assert_eq!(def.name, "debug");
    }

    // The DebugArgs struct uses `Option` for all fields with serde defaults.
    // Verify deserialization of common patterns.
    #[test]
    fn deserialize_launch_args() {
        let json = json!({
            "action": "launch",
            "program": "/tmp/test.py",
            "cwd": "/tmp"
        });
        let args: DebugArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action.as_deref(), Some("launch"));
        assert_eq!(args.program.as_deref(), Some("/tmp/test.py"));
        assert_eq!(args.cwd.as_deref(), Some("/tmp"));
        assert!(args.args.is_none());
    }

    #[test]
    fn deserialize_minimal_args() {
        let json = json!({ "action": "sessions" });
        let args: DebugArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action.as_deref(), Some("sessions"));
    }

    #[test]
    fn deserialize_breakpoint_args() {
        let json = json!({
            "action": "set_breakpoints",
            "file": "/tmp/main.rs",
            "line": 42,
            "condition": "x > 5"
        });
        let args: DebugArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.file.as_deref(), Some("/tmp/main.rs"));
        assert_eq!(args.line, Some(42));
        assert_eq!(args.condition.as_deref(), Some("x > 5"));
    }
}
