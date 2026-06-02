//! DAP session manager — launch, attach, breakpoint cache, event handling.
//!
//! Manages a single active debug session. Launching a new session
//! terminates any existing one (single-session enforcement).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use crate::agent::agent_loop::tool::AbortSignal;
use crate::agent::tools::ToolError;
use crate::dap::client::{DapClient, RpcError};

#[cfg(test)]
use crate::dap::client::DapRpc;
use crate::dap::types::*;
use crate::permission::checker::PermCheck;

/// Global DAP session manager — set during `DebugTool` construction,
/// read by the UI loop for debug panel snapshots. Uses a std Mutex
/// (not tokio) so it can be written from sync constructors and read
/// from the UI loop without an async context.
pub static DAP_MANAGER: StdMutex<Option<std::sync::Arc<DapSessionManager>>> = StdMutex::new(None);

/// Global DAP permission checker — set during `DebugTool` construction
/// alongside `DAP_MANAGER`. Read by the Janet FFI bridge to gate
/// expression evaluation (`dap/eval`) through the same permission
/// engine the agent tool path uses. Ask results from the engine are
/// treated as denial (no dialog in the bridge task).
pub static DAP_PERM_CHECK: StdMutex<Option<PermCheck>> = StdMutex::new(None);

// ---------------------------------------------------------------------------
// Output cap
// ---------------------------------------------------------------------------

/// Maximum bytes of accumulated output we retain per session.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

// ---------------------------------------------------------------------------
// Per-session event channels
// ---------------------------------------------------------------------------

/// Bundled receivers for DAP events during a session.
struct EventReceivers {
    stopped: mpsc::UnboundedReceiver<StoppedEventBody>,
    output: mpsc::UnboundedReceiver<OutputEventBody>,
    terminated: mpsc::UnboundedReceiver<TerminatedEventBody>,
    exited: mpsc::UnboundedReceiver<ExitedEventBody>,
}

/// Register handlers on `client` that forward events into channels.
async fn register_event_channels(client: &DapClient) -> EventReceivers {
    let (stopped_tx, stopped_rx) = mpsc::unbounded_channel();
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    let (terminated_tx, terminated_rx) = mpsc::unbounded_channel();
    let (exited_tx, exited_rx) = mpsc::unbounded_channel();

    client
        .on_event(
            "stopped",
            Box::new(move |v| {
                if let Ok(body) = serde_json::from_value::<StoppedEventBody>(v) {
                    let _ = stopped_tx.send(body);
                }
            }),
        )
        .await;
    client
        .on_event(
            "output",
            Box::new(move |v| {
                if let Ok(body) = serde_json::from_value::<OutputEventBody>(v) {
                    let _ = output_tx.send(body);
                }
            }),
        )
        .await;
    client
        .on_event(
            "terminated",
            Box::new(move |v| {
                if let Ok(body) = serde_json::from_value::<TerminatedEventBody>(v) {
                    let _ = terminated_tx.send(body);
                }
            }),
        )
        .await;
    client
        .on_event(
            "exited",
            Box::new(move |v| {
                if let Ok(body) = serde_json::from_value::<ExitedEventBody>(v) {
                    let _ = exited_tx.send(body);
                }
            }),
        )
        .await;

    EventReceivers {
        stopped: stopped_rx,
        output: output_rx,
        terminated: terminated_rx,
        exited: exited_rx,
    }
}

// ---------------------------------------------------------------------------
// DapSession — active debug session state
// ---------------------------------------------------------------------------

struct DapSession {
    id: String,
    client: DapClient,
    status: SessionStatus,
    breakpoints: HashMap<PathBuf, Vec<BreakpointRecord>>,
    function_breakpoints: Vec<FunctionBreakpoint>,
    output: String,
    output_truncated: bool,
    exit_code: Option<u32>,
    events: EventReceivers,
    /// Cached for TUI debug panel snapshots.
    cached_threads: Vec<Thread>,
    /// Cached for TUI debug panel snapshots.
    cached_frames: Vec<StackFrame>,
    /// Cached for TUI debug panel snapshots (last variables request).
    cached_variables: Vec<Variable>,
    languages: Vec<String>,
}

impl DapSession {
    fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            adapter_name: self.client.adapter_name.clone(),
            program: None,
            status: self.status.clone(),
            breakpoint_count: self.breakpoints.values().map(|v| v.len()).sum(),
            function_breakpoint_count: self.function_breakpoints.len(),
            stop_reason: None,
            thread_id: None,
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            capabilities: self
                .client
                .capabilities
                .try_lock()
                .ok()
                .and_then(|g| g.clone()),
            languages: self.languages.clone(),
        }
    }

    /// Drain all pending output events into the output buffer.
    fn drain_output(&mut self) {
        while let Ok(evt) = self.events.output.try_recv() {
            // Stop appending once at the cap (keep draining the channel so a
            // flooding adapter can't back it up), so the buffer can't grow
            // unbounded before the post-hoc truncate.
            if self.output.len() >= MAX_OUTPUT_BYTES {
                self.output_truncated = true;
                continue;
            }
            self.output.push_str(&evt.output);
        }
        if self.output.len() > MAX_OUTPUT_BYTES {
            // `String::truncate` panics if the index isn't on a char
            // boundary, and `evt.output` is adapter-controlled — back off to
            // the nearest boundary at or below the cap.
            let mut cut = MAX_OUTPUT_BYTES;
            while cut > 0 && !self.output.is_char_boundary(cut) {
                cut -= 1;
            }
            self.output.truncate(cut);
            self.output_truncated = true;
        }
    }

    /// Drain and check for terminated/exited events.
    fn drain_termination(&mut self) {
        if self.events.terminated.try_recv().is_ok() {
            self.status = SessionStatus::Terminated;
        }
        if let Ok(evt) = self.events.exited.try_recv() {
            self.exit_code = Some(evt.exit_code as u32);
        }
    }

    /// Wait for a stopped event with timeout.
    async fn wait_for_stopped(&mut self, timeout: Duration) -> Result<StoppedEventBody, ToolError> {
        tokio::time::timeout(timeout, self.events.stopped.recv())
            .await
            .map_err(|_| {
                ToolError::Msg(format!(
                    "timed out after {timeout:?} waiting for stopped event"
                ))
            })?
            .ok_or_else(|| ToolError::Msg("debug adapter disconnected".into()))
    }
}

// ---------------------------------------------------------------------------
// DapSessionManager — public API
// ---------------------------------------------------------------------------

pub struct DapSessionManager {
    active: Mutex<Option<DapSession>>,
    next_id: std::sync::atomic::AtomicU64,
    /// Last successfully-built panel snapshot. The session methods hold
    /// `active` across their adapter round-trip, so the UI's `try_lock` in
    /// `debug_snapshot` fails for that whole window — returning this cached
    /// copy keeps the debug panel showing the last-known state instead of
    /// blanking out. A plain `std::sync::Mutex`, never held across `.await`.
    last_snapshot: std::sync::Mutex<Option<DebugPanelData>>,
}

impl DapSessionManager {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
            next_id: std::sync::atomic::AtomicU64::new(1),
            last_snapshot: std::sync::Mutex::new(None),
        }
    }

    fn next_id(&self) -> String {
        use std::sync::atomic::Ordering;
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("dap-{n}")
    }

    /// Launch a debug session.
    ///
    /// Terminates any existing active session first.
    /// Returns a summary once the program is stopped (on entry or breakpoint).
    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        &self,
        adapter_name: &str,
        adapter_cmd: &str,
        adapter_args: &[String],
        cwd: &str,
        program: &str,
        program_args: &[String],
        stop_on_entry: Option<bool>,
        launch_extra: Option<serde_json::Value>,
        signal: &AbortSignal,
        timeout: Duration,
        languages: Vec<String>,
    ) -> Result<SessionSummary, ToolError> {
        self.terminate_active().await;

        let client = DapClient::spawn_stdio(
            adapter_name,
            Path::new(adapter_cmd),
            adapter_args,
            Path::new(cwd),
        )
        .await
        .map_err(|e| ToolError::Msg(format!("failed to spawn adapter: {e}")))?;

        self.launch_with_client(
            adapter_name,
            cwd,
            program,
            program_args,
            stop_on_entry,
            launch_extra,
            signal,
            client,
            timeout,
            languages,
        )
        .await
    }

    /// Core launch logic — used by both public launch and tests.
    ///
    /// `_signal` is reserved for future cancellation integration — when wired,
    /// a `tokio::select!` on `signal.received()` will abort the initial-stop
    /// wait so a user can cancel a hung launch.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn launch_with_client(
        &self,
        adapter_name: &str,
        cwd: &str,
        program: &str,
        program_args: &[String],
        stop_on_entry: Option<bool>,
        launch_extra: Option<serde_json::Value>,
        _signal: &AbortSignal,
        client: DapClient,
        timeout: Duration,
        languages: Vec<String>,
    ) -> Result<SessionSummary, ToolError> {
        // Register event handlers.
        let mut events = register_event_channels(&client).await;

        // Initialize handshake.
        let init_args = InitializeArgs {
            adapter_id: adapter_name.to_string(),
            ..Default::default()
        };

        let caps: Capabilities = client
            .request("initialize", &init_args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        *client.capabilities.lock().await = Some(caps.clone());

        // Build launch arguments.
        let mut launch_args = LaunchArgs {
            program: Some(program.to_string()),
            cwd: Some(cwd.to_string()),
            args: Some(program_args.to_vec()),
            stop_on_entry,
            ..Default::default()
        };

        if let Some(extra) = launch_extra {
            launch_args.extra = extra;
        }

        // Send launch request as fire-and-forget — some adapters (debugpy)
        // won't respond to launch until configurationDone is received. We must
        // send configurationDone immediately to avoid a deadlock.
        //
        // Tradeoff: launch errors (bad program path, permissions) from adapters
        // that don't reply to launch-notify surface only as a stopped-event
        // timeout rather than a direct failure response. This is a protocol
        // limitation — the adapter won't respond to launch until we signal
        // configurationDone, and by then the launch is already in flight.
        client
            .notify("launch", &launch_args)
            .await
            .map_err(rpc_to_tool_error)?;

        // Send configurationDone if adapter supports it.
        if caps.supports_configuration_done_request.unwrap_or(false) {
            client
                .notify("configurationDone", &ConfigurationDoneArgs::default())
                .await
                .map_err(rpc_to_tool_error)?;
        }

        // Wait for the initial stopped event (stopOnEntry).
        // events is moved into DapSession later, so we destructure carefully.
        let stopped = tokio::time::timeout(timeout, events.stopped.recv())
            .await
            .map_err(|_| {
                ToolError::Msg(format!(
                    "timed out after {timeout:?} waiting for initial stop"
                ))
            })?
            .ok_or_else(|| {
                ToolError::Msg("debug adapter disconnected before stopped event".into())
            })?;

        let id = self.next_id();
        let mut session = DapSession {
            id: id.clone(),
            status: SessionStatus::Stopped,
            breakpoints: HashMap::new(),
            function_breakpoints: Vec::new(),
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            events,
            client,
            cached_threads: Vec::new(),
            cached_frames: Vec::new(),
            cached_variables: Vec::new(),
            languages,
        };
        session.drain_output();

        let mut summary = session.summary();
        summary.stop_reason = Some(stopped.reason.as_str().to_string());
        summary.thread_id = stopped.thread_id.map(|id| id as u32);
        *self.active.lock().await = Some(session);

        Ok(summary)
    }

    /// Attach to a running process.
    ///
    /// `_signal` is reserved for future cancellation integration.
    #[allow(clippy::too_many_arguments)]
    pub async fn attach(
        &self,
        adapter_name: &str,
        adapter_cmd: &str,
        adapter_args: &[String],
        cwd: &str,
        pid: Option<u32>,
        port: Option<u16>,
        host: Option<String>,
        attach_extra: Option<serde_json::Value>,
        _signal: &AbortSignal,
        timeout: Duration,
        languages: Vec<String>,
    ) -> Result<SessionSummary, ToolError> {
        self.terminate_active().await;

        let client = DapClient::spawn_stdio(
            adapter_name,
            Path::new(adapter_cmd),
            adapter_args,
            Path::new(cwd),
        )
        .await
        .map_err(|e| ToolError::Msg(format!("failed to spawn adapter: {e}")))?;

        let mut events = register_event_channels(&client).await;

        let init_args = InitializeArgs {
            adapter_id: adapter_name.to_string(),
            ..Default::default()
        };
        let caps: Capabilities = client
            .request("initialize", &init_args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        *client.capabilities.lock().await = Some(caps.clone());

        let mut attach_args = AttachArgs {
            pid,
            port,
            host,
            cwd: Some(cwd.to_string()),
            ..Default::default()
        };

        if let Some(extra) = attach_extra {
            attach_args.extra = extra;
        }

        client
            .request::<_, Value>("attach", &attach_args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        if caps.supports_configuration_done_request.unwrap_or(false) {
            client
                .notify("configurationDone", &ConfigurationDoneArgs::default())
                .await
                .map_err(rpc_to_tool_error)?;
        }

        // For attach, a stopped event may or may not arrive immediately.
        let stopped = match tokio::time::timeout(timeout, events.stopped.recv()).await {
            Ok(Some(body)) => Some(body),
            _ => None,
        };

        let id = self.next_id();
        let mut session = DapSession {
            id: id.clone(),
            status: SessionStatus::Stopped,
            breakpoints: HashMap::new(),
            function_breakpoints: Vec::new(),
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            events,
            client,
            cached_threads: Vec::new(),
            cached_frames: Vec::new(),
            cached_variables: Vec::new(),
            languages,
        };
        session.drain_output();

        let mut summary = session.summary();
        if let Some(stopped) = stopped {
            summary.stop_reason = Some(stopped.reason.as_str().to_string());
            summary.thread_id = stopped.thread_id.map(|id| id as u32);
        }

        *self.active.lock().await = Some(session);
        Ok(summary)
    }

    /// Set file breakpoints for the active session.
    pub async fn set_breakpoints(
        &self,
        file: &str,
        breakpoints: Vec<SourceBreakpoint>,
        timeout: Duration,
    ) -> Result<Vec<Breakpoint>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let source = Source {
            path: Some(file.to_string()),
            ..Default::default()
        };

        let args = SetBreakpointsArgs {
            source,
            breakpoints: Some(breakpoints.clone()),
            breakpoints_deprecated: None,
            source_modified: None,
        };

        let response: SetBreakpointsResponse = session
            .client
            .request("setBreakpoints", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        let path = PathBuf::from(file);
        session.breakpoints.insert(
            path,
            vec![BreakpointRecord {
                file: file.to_string(),
                breakpoints,
                verified: Some(response.breakpoints.clone()),
            }],
        );

        Ok(response.breakpoints)
    }

    /// Set function breakpoints.
    #[allow(dead_code)] // reserved for future agent tool action
    pub async fn set_function_breakpoints(
        &self,
        breakpoints: Vec<FunctionBreakpoint>,
        timeout: Duration,
    ) -> Result<Vec<Breakpoint>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = SetFunctionBreakpointsArgs {
            breakpoints: breakpoints.clone(),
        };

        let response: SetFunctionBreakpointsResponse = session
            .client
            .request("setFunctionBreakpoints", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.function_breakpoints = breakpoints;
        Ok(response.breakpoints)
    }

    /// Continue execution and wait for the next stop event.
    pub async fn continue_(
        &self,
        thread_id: u32,
        _signal: &AbortSignal,
        timeout: Duration,
    ) -> Result<ContinueOutcome, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = ContinueArgs {
            thread_id,
            single_thread: None,
        };

        session
            .client
            .request::<_, ContinueResponse>("continue", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.status = SessionStatus::Running;

        // Wait for stopped or terminated.
        let (stop_reason, stop_thread_id) = tokio::select! {
            s = session.events.stopped.recv() => {
                if let Some(stopped) = s {
                    session.status = SessionStatus::Stopped;
                    (Some(stopped.reason.as_str().to_string()), stopped.thread_id.map(|id| id as u32))
                } else {
                    return Err(ToolError::Msg("debug adapter disconnected".into()));
                }
            }
            _ = session.events.terminated.recv() => {
                session.status = SessionStatus::Terminated;
                (Some("terminated".into()), None)
            }
            _ = tokio::time::sleep(timeout) => {
                return Err(ToolError::Msg(format!(
                    "timed out after {timeout:?} waiting for stop after continue"
                )));
            }
        };

        session.drain_output();
        session.drain_termination();

        Ok(ContinueOutcome {
            status: session.status.clone(),
            output: session.output.clone(),
            output_truncated: session.output_truncated,
            exit_code: session.exit_code,
            stop_reason,
            thread_id: stop_thread_id,
        })
    }

    /// Step over (next).
    pub async fn step_over(
        &self,
        thread_id: u32,
        _signal: &AbortSignal,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        self.step("next", thread_id, timeout).await
    }

    /// Step into.
    pub async fn step_in(
        &self,
        thread_id: u32,
        _signal: &AbortSignal,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        self.step("stepIn", thread_id, timeout).await
    }

    /// Step out.
    pub async fn step_out(
        &self,
        thread_id: u32,
        _signal: &AbortSignal,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        self.step("stepOut", thread_id, timeout).await
    }

    async fn step(
        &self,
        command: &str,
        thread_id: u32,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = match command {
            "next" => serde_json::to_value(NextArgs {
                thread_id,
                single_thread: None,
                granularity: None,
            })
            .unwrap(),
            "stepIn" => serde_json::to_value(StepInArgs {
                thread_id,
                single_thread: None,
                granularity: None,
                target_id: None,
            })
            .unwrap(),
            "stepOut" => serde_json::to_value(StepOutArgs {
                thread_id,
                single_thread: None,
                granularity: None,
            })
            .unwrap(),
            _ => return Err(ToolError::Msg(format!("unknown step command: {command}"))),
        };
        session
            .client
            .request::<_, Value>(command, &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.status = SessionStatus::Running;

        let stopped = session.wait_for_stopped(timeout).await?;
        session.status = SessionStatus::Stopped;
        session.drain_output();
        session.drain_termination();

        let mut summary = session.summary();
        summary.stop_reason = Some(stopped.reason.as_str().to_string());
        summary.thread_id = stopped.thread_id.map(|id| id as u32);
        Ok(summary)
    }

    /// Pause execution.
    pub async fn pause(
        &self,
        thread_id: u32,
        timeout: Duration,
    ) -> Result<SessionSummary, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = PauseArgs { thread_id };
        session
            .client
            .request::<_, Value>("pause", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        let stopped = session.wait_for_stopped(timeout).await?;
        session.status = SessionStatus::Stopped;
        session.drain_output();
        session.drain_termination();

        let mut summary = session.summary();
        summary.stop_reason = Some(stopped.reason.as_str().to_string());
        summary.thread_id = stopped.thread_id.map(|id| id as u32);
        Ok(summary)
    }

    /// Get stack trace.
    pub async fn stack_trace(
        &self,
        thread_id: u32,
        levels: Option<u32>,
        timeout: Duration,
    ) -> Result<Vec<StackFrame>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = StackTraceArgs {
            thread_id,
            start_frame: None,
            levels,
            format: None,
        };

        let response: StackTraceResponse = session
            .client
            .request("stackTrace", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.cached_frames = response.stack_frames.clone();
        Ok(response.stack_frames)
    }

    /// Get scopes for a frame.
    pub async fn scopes(&self, frame_id: u32, timeout: Duration) -> Result<Vec<Scope>, ToolError> {
        let active = self.active.lock().await;
        let session = active
            .as_ref()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = ScopesArgs { frame_id };
        let response: ScopesResponse = session
            .client
            .request("scopes", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        Ok(response.scopes)
    }

    /// Get variables.
    pub async fn variables(
        &self,
        variables_reference: u32,
        timeout: Duration,
    ) -> Result<Vec<Variable>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = VariablesArgs {
            variables_reference,
            filter: None,
            start: None,
            count: None,
            format: None,
        };

        let response: VariablesResponse = session
            .client
            .request("variables", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.cached_variables = response.variables.clone();
        Ok(response.variables)
    }

    /// Evaluate expression.
    pub async fn evaluate(
        &self,
        expression: &str,
        frame_id: Option<u32>,
        context: Option<&str>,
        timeout: Duration,
    ) -> Result<EvaluateResponse, ToolError> {
        let active = self.active.lock().await;
        let session = active
            .as_ref()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = EvaluateArgs {
            expression: expression.to_string(),
            frame_id,
            context: context.map(|s| s.to_string()),
            format: None,
        };

        let response: EvaluateResponse = session
            .client
            .request("evaluate", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        Ok(response)
    }

    /// List threads.
    pub async fn threads(&self, timeout: Duration) -> Result<Vec<Thread>, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let response: ThreadsResponse = session
            .client
            .request("threads", &ThreadsArgs {}, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.cached_threads = response.threads.clone();
        Ok(response.threads)
    }

    /// Terminate the debuggee.
    pub async fn terminate(&self, timeout: Duration) -> Result<SessionSummary, ToolError> {
        let mut active = self.active.lock().await;
        let session = active
            .as_mut()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        session
            .client
            .request::<_, Value>("terminate", &TerminateArgs::default(), timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        session.drain_output();
        session.drain_termination();
        session.status = SessionStatus::Terminated;

        Ok(session.summary())
    }

    /// Disconnect from the debug adapter.
    pub async fn disconnect(&self, restart: bool, timeout: Duration) -> Result<(), ToolError> {
        let mut active = self.active.lock().await;
        if let Some(session) = active.as_mut() {
            let args = DisconnectArgs {
                restart: Some(restart),
                terminate_debuggee: None,
                extra: Default::default(),
            };
            session
                .client
                .request::<_, Value>("disconnect", &args, timeout)
                .await
                .map_err(rpc_to_tool_error)?;
            session.status = SessionStatus::Terminated;
        }
        *active = None;
        Ok(())
    }

    /// Restart a stack frame — re-execute from the beginning of the frame.
    /// Useful for edit-and-continue workflows after modifying source code.
    pub async fn restart_frame(&self, frame_id: u32, timeout: Duration) -> Result<(), ToolError> {
        let active = self.active.lock().await;
        let session = active
            .as_ref()
            .ok_or_else(|| ToolError::Msg("no active debug session".into()))?;

        let args = RestartFrameArgs { frame_id };
        session
            .client
            .request::<_, Value>("restartFrame", &args, timeout)
            .await
            .map_err(rpc_to_tool_error)?;

        Ok(())
    }

    /// Return a summary of the active session, if any.
    pub async fn active_summary(&self) -> Option<SessionSummary> {
        let active = self.active.lock().await;
        active.as_ref().map(|s| s.summary())
    }

    /// Build a `DebugPanelData` snapshot from the active session's
    /// cached state. Non-async — uses `try_lock` so the UI loop
    /// never blocks waiting for a DAP tool call. Returns `None`
    /// when no session is active or the lock is held by a tool.
    pub fn debug_snapshot(&self) -> Option<DebugPanelData> {
        // If `active` is locked (an op is mid-round-trip), fall back to the
        // last cached snapshot so the panel doesn't blank out for the whole
        // call. On a successful lock, rebuild and refresh the cache.
        let active = match self.active.try_lock() {
            Ok(active) => active,
            Err(_) => {
                return self
                    .last_snapshot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
            }
        };
        let snapshot = active.as_ref().map(|session| DebugPanelData {
            adapter: session.client.adapter_name.clone(),
            status: session.status.clone(),
            session_summary: Some(session.summary()),
            threads: session.cached_threads.clone(),
            frames: session.cached_frames.clone(),
            variables: session.cached_variables.clone(),
            scopes: Vec::new(),
            breakpoints: session.breakpoints.values().flatten().cloned().collect(),
            output: session.output.clone(),
            output_truncated: session.output_truncated,
            exit_code: session.exit_code,
        });
        *self.last_snapshot.lock().unwrap_or_else(|e| e.into_inner()) = snapshot.clone();
        snapshot
    }

    /// Force-terminate the active session (drop = kill_on_drop).
    async fn terminate_active(&self) {
        let mut active = self.active.lock().await;
        if let Some(session) = active.as_mut() {
            // Best-effort graceful disconnect (terminate the debuggee) before
            // dropping, which otherwise hard-SIGKILLs the process group. Short
            // timeout; errors are ignored — the drop is the fallback.
            let args = DisconnectArgs {
                restart: Some(false),
                terminate_debuggee: Some(true),
                extra: Default::default(),
            };
            let _ = session
                .client
                .request::<_, Value>("disconnect", &args, Duration::from_secs(2))
                .await;
        }
        *active = None;
    }
}

impl Default for DapSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rpc_to_tool_error(e: RpcError) -> ToolError {
    match &e {
        RpcError::Server(msg) => ToolError::Msg(format!("adapter error: {msg}")),
        other => ToolError::Msg(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dap::framing::{decode_frame, encode_frame};
    use serde_json::Value;
    use tokio::io::{AsyncBufRead, AsyncWrite};

    /// A fake DAP adapter that handles:
    /// 1. initialize request → capabilities response
    /// 2. launch request → success response → stopped event
    /// 3. configurationDone request → success response
    async fn fake_launch_adapter(
        mut reader: impl AsyncBufRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
    ) {
        // --- initialize ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "initialize");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 1,
            "request_seq": seq,
            "success": true,
            "command": "initialize",
            "body": {
                "supportsConfigurationDoneRequest": true,
                "supportsFunctionBreakpoints": false,
            }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // --- launch ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "launch");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 2,
            "request_seq": seq,
            "success": true,
            "command": "launch",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // Stopped event (stopOnEntry).
        let evt = serde_json::json!({
            "type": "event",
            "seq": 3,
            "event": "stopped",
            "body": {
                "reason": "entry",
                "threadId": 1,
            }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&evt).unwrap())
            .await
            .unwrap();

        // --- configurationDone ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "configurationDone");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 4,
            "request_seq": seq,
            "success": true,
            "command": "configurationDone",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // --- setBreakpoints ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "setBreakpoints");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 5,
            "request_seq": seq,
            "success": true,
            "command": "setBreakpoints",
            "body": {
                "breakpoints": [
                    {"id": 1, "verified": true, "line": 10}
                ]
            }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // --- continue ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "continue");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 6,
            "request_seq": seq,
            "success": true,
            "command": "continue",
            "body": { "allThreadsContinued": true }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // Stopped event (breakpoint hit).
        let evt = serde_json::json!({
            "type": "event",
            "seq": 7,
            "event": "stopped",
            "body": {
                "reason": "breakpoint",
                "threadId": 1,
            }
        });
        encode_frame(&mut writer, &serde_json::to_vec(&evt).unwrap())
            .await
            .unwrap();

        // --- terminate ---
        let frame = decode_frame(&mut reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["command"], "terminate");
        let seq = msg["seq"].as_u64().unwrap();

        let resp = serde_json::json!({
            "type": "response",
            "seq": 8,
            "request_seq": seq,
            "success": true,
            "command": "terminate",
        });
        encode_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();
    }

    /// Build a DapClient over duplex channels connected to a fake adapter.
    fn client_with_fake_adapter() -> DapClient {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);

        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _read_task) = DapRpc::new(client_reader, client_write);

        tokio::spawn(async move {
            fake_launch_adapter(tokio::io::BufReader::new(server_read), server_write).await;
        });

        DapClient::from_rpc(rpc, "fake-adapter")
    }

    /// Full launch → setBreakpoints → continue → terminate flow over duplex.
    #[tokio::test]
    async fn launch_breakpoint_continue_terminate() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_fake_adapter();

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                "test-program",
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(summary.status, SessionStatus::Stopped);
        assert_eq!(summary.stop_reason.as_deref(), Some("entry"));
        assert_eq!(summary.thread_id, Some(1));

        // set breakpoints
        let bps = mgr
            .set_breakpoints(
                "/tmp/test.rs",
                vec![SourceBreakpoint {
                    line: 10,
                    column: None,
                    condition: None,
                    hit_condition: None,
                    log_message: None,
                }],
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(bps.len(), 1);
        assert_eq!(bps[0].id, Some(1));
        assert!(bps[0].verified);

        // continue → wait for breakpoint hit
        let outcome = mgr
            .continue_(1, &signal, Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(outcome.status, SessionStatus::Stopped);
        assert_eq!(outcome.stop_reason.as_deref(), Some("breakpoint"));

        // terminate
        let term = mgr.terminate(Duration::from_secs(5)).await.unwrap();
        assert_eq!(term.status, SessionStatus::Terminated);
    }

    /// Session summary reflects the active session.
    #[tokio::test]
    async fn active_summary_after_launch() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_fake_adapter();

        let summary = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                "hello",
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(summary.status, SessionStatus::Stopped);

        let active = mgr.active_summary().await;
        assert!(active.is_some());
        assert_eq!(active.unwrap().id, summary.id);
    }

    /// Single-session enforcement: launching a new session drops the old one.
    #[tokio::test]
    async fn second_launch_replaces_first() {
        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();
        let client = client_with_fake_adapter();

        let first = mgr
            .launch_with_client(
                "fake-adapter",
                "/tmp",
                "first",
                &[],
                Some(true),
                None,
                &signal,
                client,
                Duration::from_secs(5),
                vec![],
            )
            .await
            .unwrap();

        let first_id = first.id;

        let active = mgr.active_summary().await;
        assert!(active.is_some());
        assert_eq!(active.unwrap().id, first_id);

        // Manually clear to verify terminate_active works
        mgr.terminate_active().await;
        assert!(mgr.active_summary().await.is_none());
    }

    /// E2E: DapSessionManager::launch_with_client against real debugpy.
    /// Reproduces dirge-go4b timeout bug.
    #[tokio::test]
    async fn e2e_debugpy_launch_with_client() {
        if std::process::Command::new("python3")
            .args(["-c", "import debugpy"])
            .output()
            .map_or(true, |o| !o.status.success())
        {
            eprintln!("SKIP: debugpy not installed");
            return;
        }

        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("tests")
            .join("dap")
            .join("fixtures")
            .join("test_program.py");
        assert!(fixture.exists(), "test_program.py must exist");

        let client = DapClient::spawn_stdio(
            "debugpy",
            std::path::Path::new("python3"),
            &["-m".to_string(), "debugpy.adapter".to_string()],
            std::path::Path::new("."),
        )
        .await
        .expect("debugpy adapter should spawn");

        let mgr = DapSessionManager::new();
        let signal = AbortSignal::new();

        let summary = mgr
            .launch_with_client(
                "debugpy",
                ".",
                fixture.to_str().unwrap(),
                &[],
                Some(true),
                None,
                &signal,
                client,
                std::time::Duration::from_secs(15),
                vec!["python".into()],
            )
            .await
            .expect("launch_with_client should succeed");

        assert_eq!(summary.status, SessionStatus::Stopped);
        assert!(summary.stop_reason.is_some(), "should have stop reason");

        // Terminate and disconnect.
        mgr.terminate(std::time::Duration::from_secs(10))
            .await
            .expect("terminate should succeed");

        mgr.disconnect(false, std::time::Duration::from_secs(10))
            .await
            .expect("disconnect should succeed");
    }
} // mod tests
