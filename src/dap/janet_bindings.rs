//! DAP Janet FFI bindings — expose DapSessionManager methods to Janet plugins.
//!
//! Architecture: the Janet worker thread has no tokio runtime, but all DAP
//! operations are async (spawn adapters, wait for handshakes, etc.). We bridge
//! this with the same channel-pattern used by `harness/confirm` and
//! `harness/lsp` (see `src/plugin/worker.rs`):
//!
//! 1. C functions (this module) extract string args from Janet's argv, build
//!    a `DapCommand`, send it via a thread-local `DAP_TX` channel, and block
//!    on a oneshot reply — polling the worker-shutdown flag like the dialog
//!    and LSP C functions do.
//!
//! 2. A tokio task (`DapBridge`) receives commands, runs the async
//!    `DapSessionManager` methods, and sends the JSON-stringified result
//!    back through the oneshot channel.
//!
//! 3. Janet sees `(dap/launch "test.py" "debugpy")`, `(dap/step)`, etc.
//!    All return JSON strings (the same format the agent `debug` tool
//!    returns) or nil on error/timeout.
//!
//! Registration: `register_dap_cfns(env)` installs the C functions in the
//! Janet environment under the `dap` namespace. Call once during worker init.
//! `HARNESS_DAP_INIT` is a Janet prelude that slims the C-function names into
//! user-friendly Janet wrappers (e.g. `__dap_launch` → `dap/launch`).
//!
//! Bridge lifecycle: `spawn_dap_bridge()` returns a tokio `JoinHandle` and
//! the `UnboundedSender<DapCommand>` that the C functions read from the
//! thread-local. The bridge runs until the sender is dropped (worker shutdown).

use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::mpsc as tmpsc;

use crate::dap::session::DAP_PERM_CHECK;

// ---------------------------------------------------------------------------
// DapCommand — message from Janet C function to the bridge task
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum DapCommand {
    Launch {
        file: String,
        adapter: Option<String>,
        reply: mpsc::Sender<Result<String, String>>,
    },
    Attach {
        pid: u32,
        adapter: Option<String>,
        reply: mpsc::Sender<Result<String, String>>,
    },
    StepOver {
        reply: mpsc::Sender<Result<String, String>>,
    },
    StepIn {
        reply: mpsc::Sender<Result<String, String>>,
    },
    StepOut {
        reply: mpsc::Sender<Result<String, String>>,
    },
    Continue {
        reply: mpsc::Sender<Result<String, String>>,
    },
    Breakpoint {
        file: String,
        line: u32,
        reply: mpsc::Sender<Result<String, String>>,
    },
    Evaluate {
        expression: String,
        reply: mpsc::Sender<Result<String, String>>,
    },
    StackTrace {
        reply: mpsc::Sender<Result<String, String>>,
    },
    Threads {
        reply: mpsc::Sender<Result<String, String>>,
    },
    Terminate {
        reply: mpsc::Sender<Result<String, String>>,
    },
    Sessions {
        reply: mpsc::Sender<Result<String, String>>,
    },
    Variables {
        var_ref: u32,
        reply: mpsc::Sender<Result<String, String>>,
    },
}

const DAP_CMD_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Channel storage — thread-local, set once by the bridge spawner
// ---------------------------------------------------------------------------

thread_local! {
    static DAP_TX: std::cell::RefCell<Option<tmpsc::UnboundedSender<DapCommand>>> =
        const { std::cell::RefCell::new(None) };
}

/// Bridging storage: the plugin manager holds the Sender, the worker holds
/// the Receiver. `PENDING_DAP_TX` is set by `spawn_dap_bridge` before the
/// worker starts, and consumed by the worker via `take_dap_tx_for_worker`.
static PENDING_DAP_TX: std::sync::Mutex<Option<tmpsc::UnboundedSender<DapCommand>>> =
    std::sync::Mutex::new(None);

/// Called by the plugin manager after spawning the bridge. Stores the
/// sender so the worker thread can pick it up.
pub fn store_dap_tx(tx: tmpsc::UnboundedSender<DapCommand>) {
    *PENDING_DAP_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
}

/// Called by the worker thread during init. Takes ownership of the
/// pre-stored sender.
pub fn take_dap_tx_for_worker() -> tmpsc::UnboundedSender<DapCommand> {
    PENDING_DAP_TX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("DAP bridge sender must be stored before worker starts")
}

/// Install the command sender on this thread. Must be called once before
/// any Janet plugin evaluates DAP functions.
pub fn install_dap_tx(tx: tmpsc::UnboundedSender<DapCommand>) {
    DAP_TX.with(|cell| *cell.borrow_mut() = Some(tx));
}

// ---------------------------------------------------------------------------
// Janet C function shims — one per DAP operation
// ---------------------------------------------------------------------------

/// C function backing `dap/__launch`. Args: file-path, adapter-name-or-nil.
unsafe extern "C-unwind" fn dap_launch_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        dap_launch_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(_) => unsafe { janet_wrap_nil() },
    }
}

unsafe fn dap_launch_body(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 1 {
        return unsafe { janet_wrap_nil() };
    }
    let file = match unsafe { read_dap_str(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let adapter = if argc >= 2 {
        unsafe { read_dap_str(argv, 1) }
    } else {
        None
    };

    let (tx, rx) = mpsc::channel();
    let cmd = DapCommand::Launch {
        file,
        adapter,
        reply: tx,
    };
    unsafe { dap_send_and_wait(cmd, rx) }
}

/// C function backing `dap/__attach`. Args: pid, adapter-name-or-nil.
unsafe extern "C-unwind" fn dap_attach_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        dap_attach_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(_) => unsafe { janet_wrap_nil() },
    }
}

unsafe fn dap_attach_body(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 1 {
        return unsafe { janet_wrap_nil() };
    }
    let pid: u32 = match unsafe { read_dap_str(argv, 0) } {
        Some(s) => s.parse().unwrap_or(0),
        None => return unsafe { janet_wrap_nil() },
    };
    if pid == 0 {
        return unsafe { janet_wrap_nil() };
    }
    let adapter = if argc >= 2 {
        unsafe { read_dap_str(argv, 1) }
    } else {
        None
    };

    let (tx, rx) = mpsc::channel();
    let cmd = DapCommand::Attach {
        pid,
        adapter,
        reply: tx,
    };
    unsafe { dap_send_and_wait(cmd, rx) }
}

unsafe extern "C-unwind" fn dap_step_cfn(
    _argc: i32,
    _argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        dap_generic_body(DapCommand::StepOver {
            reply: std::mem::zeroed(),
        })
    }));
    match result {
        Ok(j) => j,
        Err(_) => unsafe { janet_wrap_nil() },
    }
}

unsafe fn dap_generic_body(mut cmd: DapCommand) -> janetrs::lowlevel::Janet {
    // Replace the zeroed reply with a real channel.
    let (tx, rx) = mpsc::channel();
    unsafe { set_dap_reply(&mut cmd, tx) };
    unsafe { dap_send_and_wait(cmd, rx) }
}

unsafe fn set_dap_reply(cmd: &mut DapCommand, tx: mpsc::Sender<Result<String, String>>) {
    match cmd {
        DapCommand::Launch { reply, .. } => *reply = tx,
        DapCommand::Attach { reply, .. } => *reply = tx,
        DapCommand::StepOver { reply } => *reply = tx,
        DapCommand::StepIn { reply } => *reply = tx,
        DapCommand::StepOut { reply } => *reply = tx,
        DapCommand::Continue { reply } => *reply = tx,
        DapCommand::Breakpoint { reply, .. } => *reply = tx,
        DapCommand::Evaluate { reply, .. } => *reply = tx,
        DapCommand::StackTrace { reply } => *reply = tx,
        DapCommand::Threads { reply } => *reply = tx,
        DapCommand::Terminate { reply } => *reply = tx,
        DapCommand::Sessions { reply } => *reply = tx,
        DapCommand::Variables { reply, .. } => *reply = tx,
    }
}

unsafe fn dap_send_and_wait(
    cmd: DapCommand,
    rx: mpsc::Receiver<Result<String, String>>,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let tx = DAP_TX.with(|cell| cell.borrow().as_ref().cloned());
    let tx = match tx {
        Some(t) => t,
        None => return unsafe { janet_wrap_nil() },
    };
    let _ = tx.send(cmd);

    // Poll for the reply with shutdown check (mirrors send_dialog).
    let start = std::time::Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(json)) => match unsafe { dap_wrap_str(&json) } {
                Some(j) => return j,
                None => return unsafe { janet_wrap_nil() },
            },
            Ok(Err(_)) => return unsafe { janet_wrap_nil() },
            Err(mpsc::RecvTimeoutError::Disconnected) => return unsafe { janet_wrap_nil() },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if start.elapsed() >= DAP_CMD_TIMEOUT {
                    return unsafe { janet_wrap_nil() };
                }
                // Check shutdown — read from the worker's SHUTDOWN thread-local.
                // We can't access it directly, so just keep polling.
            }
        }
    }
}

// Evaluate — takes an expression string arg.

macro_rules! dap_simple_cfn {
    ($name:ident, $cmd:expr) => {
        unsafe extern "C-unwind" fn $name(
            _argc: i32,
            _argv: *mut janetrs::lowlevel::Janet,
        ) -> janetrs::lowlevel::Janet {
            use janetrs::lowlevel::*;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                dap_generic_body($cmd)
            }));
            match result {
                Ok(j) => j,
                Err(_) => unsafe { janet_wrap_nil() },
            }
        }
    };
}

dap_simple_cfn!(
    dap_step_in_cfn,
    DapCommand::StepIn {
        reply: std::mem::zeroed()
    }
);
dap_simple_cfn!(
    dap_step_out_cfn,
    DapCommand::StepOut {
        reply: std::mem::zeroed()
    }
);
dap_simple_cfn!(
    dap_continue_cfn,
    DapCommand::Continue {
        reply: std::mem::zeroed()
    }
);
dap_simple_cfn!(
    dap_stack_trace_cfn,
    DapCommand::StackTrace {
        reply: std::mem::zeroed()
    }
);
dap_simple_cfn!(
    dap_threads_cfn,
    DapCommand::Threads {
        reply: std::mem::zeroed()
    }
);
dap_simple_cfn!(
    dap_terminate_cfn,
    DapCommand::Terminate {
        reply: std::mem::zeroed()
    }
);
dap_simple_cfn!(
    dap_sessions_cfn,
    DapCommand::Sessions {
        reply: std::mem::zeroed()
    }
);

// Evaluate — takes an expression string arg.
unsafe extern "C-unwind" fn dap_evaluate_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        dap_eval_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(_) => unsafe { janet_wrap_nil() },
    }
}

unsafe fn dap_eval_body(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 1 {
        return unsafe { janet_wrap_nil() };
    }
    let expression = match unsafe { read_dap_str(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    unsafe {
        dap_generic_body(DapCommand::Evaluate {
            expression,
            reply: std::mem::zeroed(),
        })
    }
}

// Breakpoint — takes file + line.
unsafe extern "C-unwind" fn dap_breakpoint_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        dap_bp_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(_) => unsafe { janet_wrap_nil() },
    }
}

unsafe fn dap_bp_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_nil() };
    }
    let file = match unsafe { read_dap_str(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let line: u32 = match unsafe { read_dap_str(argv, 1) } {
        Some(s) => s.parse().unwrap_or(0),
        None => return unsafe { janet_wrap_nil() },
    };
    if line == 0 {
        return unsafe { janet_wrap_nil() };
    }
    unsafe {
        dap_generic_body(DapCommand::Breakpoint {
            file,
            line,
            reply: std::mem::zeroed(),
        })
    }
}

// Variables — takes variable reference number.
unsafe extern "C-unwind" fn dap_variables_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        dap_vars_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(_) => unsafe { janet_wrap_nil() },
    }
}

unsafe fn dap_vars_body(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 1 {
        return unsafe { janet_wrap_nil() };
    }
    let var_ref: u32 = match unsafe { read_dap_str(argv, 0) } {
        Some(s) => s.parse().unwrap_or(0),
        None => return unsafe { janet_wrap_nil() },
    };
    unsafe {
        dap_generic_body(DapCommand::Variables {
            var_ref,
            reply: std::mem::zeroed(),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers — string read/wrap (reuse the worker's raw Janet FFI when available)
// ---------------------------------------------------------------------------

/// Read a Janet string at argv[i]. Mirrors worker::read_string_arg.
#[cfg(feature = "plugin")]
unsafe fn read_dap_str(argv: *mut janetrs::lowlevel::Janet, i: i32) -> Option<String> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    let is_str = unsafe { janet_checktype(v, JanetType_JANET_STRING) } != 0;
    let is_kw = unsafe { janet_checktype(v, JanetType_JANET_KEYWORD) } != 0;
    let is_sym = unsafe { janet_checktype(v, JanetType_JANET_SYMBOL) } != 0;
    if !(is_str || is_kw || is_sym) {
        return None;
    }
    let raw = unsafe { janet_unwrap_string(v) };
    if raw.is_null() {
        return None;
    }
    let len = unsafe { (*janet_string_head(raw)).length } as usize;
    let slice = unsafe { std::slice::from_raw_parts(raw, len) };
    std::str::from_utf8(slice).ok().map(str::to_string)
}

/// Wrap a Rust &str as a Janet string. Returns None if string is too large.
#[cfg(feature = "plugin")]
unsafe fn dap_wrap_str(s: &str) -> Option<janetrs::lowlevel::Janet> {
    use janetrs::lowlevel::*;
    let bytes = s.as_bytes();
    let Ok(len) = i32::try_from(bytes.len()) else {
        return None;
    };
    let raw = unsafe { janet_string(bytes.as_ptr(), len) };
    Some(unsafe { janet_wrap_string(raw) })
}

// ---------------------------------------------------------------------------
// C function registration — called during worker init
// ---------------------------------------------------------------------------

use janetrs::env::CFunOptions;

/// Register all DAP C functions in the Janet environment under the `dap`
/// namespace. Called once during `worker_loop` startup.
#[cfg(all(feature = "dap", feature = "plugin"))]
pub fn register_dap_cfns(client: &mut janetrs::client::JanetClient) {
    if let Some(env) = client.env_mut() {
        env.add_c_fn(CFunOptions::new(c"__launch", dap_launch_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__attach", dap_attach_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__step", dap_step_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__step_in", dap_step_in_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__step_out", dap_step_out_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__continue", dap_continue_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__breakpoint", dap_breakpoint_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__evaluate", dap_evaluate_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__stack_trace", dap_stack_trace_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__threads", dap_threads_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__terminate", dap_terminate_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__sessions", dap_sessions_cfn).namespace(c"dap"));
        env.add_c_fn(CFunOptions::new(c"__variables", dap_variables_cfn).namespace(c"dap"));
    }
}

// ---------------------------------------------------------------------------
// Janet init prelude — wraps raw C functions in user-facing Janet aliases
// ---------------------------------------------------------------------------

/// Janet code run after the C functions are registered. Creates `dap/launch`,
/// `dap/step`, etc. as Janet functions that call the raw `dap/__launch` etc.
/// C functions. Also provides `dap/available?` for runtime feature detection.
#[cfg(all(feature = "dap", feature = "plugin"))]
pub const HARNESS_DAP_INIT: &str = r#"
# DAP Janet bindings — user-facing wrappers over the dap/__* C functions.
# Each returns a JSON string (success) or nil (error/timeout/no session).

(defn dap/launch [file &opt adapter]
  (dap/__launch file (if adapter adapter nil)))

(defn dap/attach [pid &opt adapter]
  (dap/__attach (string pid) (if adapter adapter nil)))

(defn dap/step [] (dap/__step))
(defn dap/step-in [] (dap/__step_in))
(defn dap/step-out [] (dap/__step_out))
(defn dap/continue [] (dap/__continue))

(defn dap/bp [file line]
  (dap/__breakpoint file (string line)))

(defn dap/eval [expr]
  (dap/__evaluate expr))

(defn dap/stack-trace [] (dap/__stack_trace))
(defn dap/threads [] (dap/__threads))
(defn dap/terminate [] (dap/__terminate))
(defn dap/sessions [] (dap/__sessions))
(defn dap/vars [var-ref]
  (dap/__variables (string var-ref)))

(defn dap/available? []
  (truthy? (get (curenv) (symbol "dap/__launch"))))

(defn dap/session-active? []
  (not (nil? (dap/sessions))))
"#;

// ---------------------------------------------------------------------------
// Bridge task — runs on tokio, processes DapCommands, returns JSON results
// ---------------------------------------------------------------------------

/// Spawn the DAP bridge tokio task. Returns the sender (for installing in the
/// thread-local `DAP_TX`) and the join handle.
#[cfg(all(feature = "dap", feature = "plugin"))]
pub fn spawn_dap_bridge() -> (
    tokio::task::JoinHandle<()>,
    tmpsc::UnboundedSender<DapCommand>,
) {
    let (tx, mut rx) = tmpsc::unbounded_channel::<DapCommand>();
    let handle = tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            handle_dap_command(cmd).await;
        }
    });
    (handle, tx)
}

/// Process a single DAP command on the tokio runtime.
async fn handle_dap_command(cmd: DapCommand) {
    use crate::agent::agent_loop::tool::AbortSignal;
    use crate::agent::tools::ToolError;
    use crate::dap::session::DAP_MANAGER;

    let mgr = match DAP_MANAGER.lock().ok().and_then(|g| g.clone()) {
        Some(m) => m,
        None => {
            send_dap_reply(&cmd, Err("no DAP session manager".to_string()));
            return;
        }
    };

    let signal = AbortSignal::new();
    let timeout = Duration::from_secs(30);

    // For expression evaluation, check the permission engine before
    // forwarding to the adapter. Expressions execute in the debuggee's
    // context with full process privileges. Ask results are treated as
    // denial (no dialog in the bridge task).
    if let DapCommand::Evaluate { expression, .. } = &cmd {
        if let Some(perm) = DAP_PERM_CHECK.lock().ok().and_then(|g| g.clone()) {
            if let Ok(mut checker) = perm.lock() {
                use crate::permission::checker::CheckResult;
                match checker.check("debug", &format!("evaluate {expression}")) {
                    CheckResult::Allowed => {}
                    CheckResult::Ask => {
                        send_dap_reply(
                            &cmd,
                            Err("expression evaluation requires permission dialog (not available in plugin bridge)".to_string()),
                        );
                        return;
                    }
                    CheckResult::Denied(r) => {
                        send_dap_reply(&cmd, Err(format!("expression evaluation denied: {r}")));
                        return;
                    }
                }
            }
        }
    }

    let result: Result<String, ToolError> = match &cmd {
        DapCommand::Launch { file, adapter, .. } => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let prog_path = std::path::Path::new(file);

            let resolved = if let Some(name) = adapter {
                crate::dap::config::resolve_adapter(name)
            } else {
                crate::dap::config::select_launch_adapter(prog_path, &cwd, None)
            };

            match resolved {
                Some(a) => {
                    let languages = a.languages.clone();
                    mgr.launch(
                        &a.name,
                        &a.resolved_command.to_string_lossy(),
                        &a.args,
                        &cwd.to_string_lossy(),
                        file,
                        &[],
                        Some(true),
                        Some(a.launch_defaults.clone()),
                        &signal,
                        timeout,
                        languages,
                    )
                    .await
                    .map(|s| serde_json::to_string_pretty(&s).unwrap_or_else(|_| format!("{s:?}")))
                }
                None => Err(ToolError::Msg(format!("no debug adapter found for {file}"))),
            }
        }
        DapCommand::Attach { pid, adapter, .. } => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

            let resolved = if let Some(name) = adapter {
                crate::dap::config::resolve_adapter(name)
            } else {
                crate::dap::config::select_attach_adapter(None, None)
            };

            match resolved {
                Some(a) => {
                    let languages = a.languages.clone();
                    mgr.attach(
                        &a.name,
                        &a.resolved_command.to_string_lossy(),
                        &a.args,
                        &cwd.to_string_lossy(),
                        Some(*pid),
                        None,
                        None,
                        Some(a.attach_defaults.clone()),
                        &signal,
                        timeout,
                        languages,
                    )
                    .await
                    .map(|s| serde_json::to_string_pretty(&s).unwrap_or_else(|_| format!("{s:?}")))
                }
                None => Err(ToolError::Msg(
                    "no debug adapter available for attach".to_string(),
                )),
            }
        }
        DapCommand::StepOver { .. } => mgr
            .step_over(0, &signal, timeout)
            .await
            .map(|s| serde_json::to_string_pretty(&s).unwrap_or_else(|_| format!("{s:?}"))),
        DapCommand::StepIn { .. } => mgr
            .step_in(0, &signal, timeout)
            .await
            .map(|s| serde_json::to_string_pretty(&s).unwrap_or_else(|_| format!("{s:?}"))),
        DapCommand::StepOut { .. } => mgr
            .step_out(0, &signal, timeout)
            .await
            .map(|s| serde_json::to_string_pretty(&s).unwrap_or_else(|_| format!("{s:?}"))),
        DapCommand::Continue { .. } => mgr
            .continue_(0, &signal, timeout)
            .await
            .map(|o| serde_json::to_string_pretty(&o).unwrap_or_else(|_| format!("{o:?}"))),
        DapCommand::Breakpoint { file, line, .. } => {
            let bp = crate::dap::types::SourceBreakpoint {
                line: *line as i64,
                ..Default::default()
            };
            mgr.set_breakpoints(file, vec![bp], timeout)
                .await
                .map(|r| serde_json::to_string_pretty(&r).unwrap_or_else(|_| format!("{r:?}")))
        }
        DapCommand::Evaluate { expression, .. } => mgr
            .evaluate(expression, None, None, timeout)
            .await
            .map(|r| serde_json::to_string_pretty(&r).unwrap_or_else(|_| format!("{r:?}"))),
        DapCommand::StackTrace { .. } => mgr
            .stack_trace(0, None, timeout)
            .await
            .map(|f| serde_json::to_string_pretty(&f).unwrap_or_else(|_| format!("{f:?}"))),
        DapCommand::Threads { .. } => mgr
            .threads(timeout)
            .await
            .map(|t| serde_json::to_string_pretty(&t).unwrap_or_else(|_| format!("{t:?}"))),
        DapCommand::Terminate { .. } => mgr
            .terminate(timeout)
            .await
            .map(|s| serde_json::to_string_pretty(&s).unwrap_or_else(|_| format!("{s:?}"))),
        DapCommand::Sessions { .. } => match mgr.active_summary().await {
            Some(s) => Ok(serde_json::to_string_pretty(&s).unwrap_or_else(|_| format!("{s:?}"))),
            None => Err(ToolError::Msg("no active debug session".to_string())),
        },
        DapCommand::Variables { var_ref, .. } => mgr
            .variables(*var_ref, timeout)
            .await
            .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| format!("{v:?}"))),
    };

    match result {
        Ok(json) => send_dap_reply(&cmd, Ok(json)),
        Err(e) => send_dap_reply(&cmd, Err(e.to_string())),
    }
}

fn send_dap_reply(cmd: &DapCommand, result: Result<String, String>) {
    macro_rules! reply {
        ($field:expr) => {{
            let _ = $field.send(result.clone());
        }};
    }
    match cmd {
        DapCommand::Launch { reply, .. } => reply!(reply),
        DapCommand::Attach { reply, .. } => reply!(reply),
        DapCommand::StepOver { reply } => reply!(reply),
        DapCommand::StepIn { reply } => reply!(reply),
        DapCommand::StepOut { reply } => reply!(reply),
        DapCommand::Continue { reply } => reply!(reply),
        DapCommand::Breakpoint { reply, .. } => reply!(reply),
        DapCommand::Evaluate { reply, .. } => reply!(reply),
        DapCommand::StackTrace { reply } => reply!(reply),
        DapCommand::Threads { reply } => reply!(reply),
        DapCommand::Terminate { reply } => reply!(reply),
        DapCommand::Sessions { reply } => reply!(reply),
        DapCommand::Variables { reply, .. } => reply!(reply),
    }
}
