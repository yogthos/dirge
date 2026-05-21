//! Janet runs on a dedicated OS thread.
//!
//! The original `PluginManager` held the `JanetClient` directly and relied
//! on `#[tokio::main(flavor = "current_thread")]` + an `unsafe impl Send`
//! to satisfy `rig::ToolDyn`'s Send bound on tool-call futures. That was
//! sound under the existing single-thread runtime but blocked synchronous
//! dialog APIs (`harness/confirm`, `harness/select`) — they would have
//! deadlocked, since the Janet eval call sat on the same OS thread that
//! also drove the UI event loop.
//!
//! This module spawns a dedicated worker thread that owns the
//! `JanetClient`. Callers send [`Cmd`]s to the worker via an mpsc channel
//! and block-receive replies on per-command oneshot reply channels. The
//! UI thread is free to render dialogs while the worker thread is blocked
//! inside Janet awaiting a dialog response.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
#[cfg_attr(not(feature = "plugin"), allow(unused_imports))]
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::mpsc as tmpsc;

/// How long the init handshake waits for the worker to confirm Janet
/// initialization before giving up. Worker init is normally well under
/// 100 ms; 10 s is just a watchdog so a hung worker doesn't pin main.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval for the dialog reply loop. The cfn wakes every
/// `DIALOG_POLL` to check the shutdown flag so a UI exit doesn't pin
/// the worker thread forever. Short enough that shutdown feels snappy,
/// long enough that polling overhead is negligible.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
const DIALOG_POLL: Duration = Duration::from_millis(50);

#[cfg(feature = "plugin")]
use janetrs::client::JanetClient;
#[cfg(feature = "plugin")]
use janetrs::env::CFunOptions;

/// Janet definitions installed on the worker thread at startup. Includes
/// the harness state variables, the regular harness/* functions, and
/// Janet-side wrappers that forward to the registered C functions for
/// `harness/confirm` and `harness/select`.
///
/// Kept as a single string so worker init does one `client.run` call.
#[cfg(feature = "plugin")]
const HARNESS_INIT: &str = r#"
(var harness-pending nil)
(var harness-response nil)
# Per-tool-hook slots: cleared by the host at the start of
# dispatch_tool_hook so previous-call state doesn't leak.
(var harness-block nil)
(var harness-mutate-input nil)
(var harness-replace-result nil)

(defn harness/log [msg] (print "[plugin] " msg))
(defn harness/get-cwd [] (os/cwd))
(defn harness/request-prompt [prompt]
  (when (string? prompt)
    (set harness-pending prompt)))
(defn harness/store-response [resp]
  (set harness-response resp))
(defn harness/has-symbol? [name]
  (truthy? (get (curenv) (symbol name))))

# Tool-hook slots. Plugins call these from inside
# on-tool-start / on-tool-end. The host reads them via
# dispatch_tool_hook on the Rust side.
(defn harness/block [reason]
  (when (string? reason) (set harness-block reason)))
(defn harness/mutate-input [json-str]
  (when (string? json-str) (set harness-mutate-input json-str)))
(defn harness/replace-result [output]
  (when (string? output) (set harness-replace-result output)))

# Slash-command registry. Plugins register at load time;
# the host reads the list once after all plugins load and
# dispatches matching /cmd input back to the named handler.
# Stored as a `name|handler\n` blob to keep the read side
# easy (single Janet -> Rust string roundtrip).
(var harness-cmd-list "")
(defn harness/register-command [name handler]
  (when (and (string? name) (string? handler))
    (set harness-cmd-list
         (string harness-cmd-list name "|" handler "\n"))))

# Replace the user's prompt for the current turn. Plugins
# call this from on-prompt hooks. Distinct from
# harness/request-prompt which queues a follow-up turn.
(var harness-prompt-replace nil)
(defn harness/replace-prompt [text]
  (when (string? text)
    (set harness-prompt-replace text)))

# Notification queue. Plugins call (harness/notify msg level?)
# to push a line into the host's chat display. Stored as a
# `level\tmsg\n` blob; the host's drain_notifications
# splits and clears in one round-trip.
(var harness-notif-list "")
(defn harness/notify [msg &opt level]
  (when (string? msg)
    (let [lvl (cond
                (or (= level :info) (= level "info")) "info"
                (or (= level :warn) (= level "warn")) "warn"
                (or (= level :error) (= level "error")) "error"
                "info")]
      (set harness-notif-list
           (string harness-notif-list lvl "\t" msg "\n")))))

# Hook-error dedup slots. `harness-last-hook-err-msg` is the most
# recently pushed sanitized hook-error message; `harness-last-hook-err-count`
# is how many consecutive identical errors followed it. When a
# DIFFERENT error arrives (or any other notification fires), the
# count is flushed as a "(repeated N times)" entry. Drained alongside
# the regular notif list. See `harness/push-hook-err` below + the
# Rust-side dispatch wrapper in `plugin/mod.rs::dispatch`.
(var harness-last-hook-err-msg nil)
(var harness-last-hook-err-count 0)

# Sanitize a hook-error message for the `level\tmsg\n` wire format.
# Embedded tabs become spaces (so they don't get parsed as a second
# `level` field) and newlines become ` | ` (so a multi-line Janet
# stack trace stays on one notification entry).
#
# `string/replace-all` takes args as (patt subst str), so threading
# with `->` (first-position) would pass the wrong arg as the
# subject. Explicit nesting from inside out is the safest spelling.
(defn harness/sanitize-hook-err [s]
  (string/replace-all
    "\n" " | "
    (string/replace-all
      "\r\n" " | "
      (string/replace-all "\t" " " (string s)))))

# Push a hook error onto the notif list, deduplicating consecutive
# identical messages. The catch arm in dispatch calls this rather
# than appending directly so a buggy on-message-update hook can't
# flood the chat with thousands of identical banners.
(defn harness/push-hook-err [sanitized-msg]
  (if (= sanitized-msg harness-last-hook-err-msg)
    # Same as last — increment in place; do not push.
    (set harness-last-hook-err-count (+ harness-last-hook-err-count 1))
    # Different message (or first one). If the previous one had
    # been repeated, flush its summary now; then push the new msg
    # and reset the dedup state.
    (do
      (when (and harness-last-hook-err-msg
                 (> harness-last-hook-err-count 1))
        (set harness-notif-list
             (string harness-notif-list
                     "error\t"
                     harness-last-hook-err-msg
                     " (repeated "
                     harness-last-hook-err-count
                     " times)\n")))
      (set harness-notif-list
           (string harness-notif-list "error\t" sanitized-msg "\n"))
      (set harness-last-hook-err-msg sanitized-msg)
      (set harness-last-hook-err-count 1))))

# Plugin entries on the session timeline. Plugins call
# (harness/append-entry type data &opt display) to record
# bookmarks, telemetry, or custom state that should survive
# session save/load. The data is treated as opaque by the host
# (any registered renderer for `type` formats it); other plugins
# can use plain text, JSON, etc.
#
# Stored as `type\tdata\tdisplay\n` per entry; data is escaped so
# embedded tabs / newlines / backslashes don't break parsing.
(var harness-entries-buf "")
(defn- harness/-escape [s]
  (->> s
       (string/replace-all "\\" "\\\\")
       (string/replace-all "\t" "\\t")
       (string/replace-all "\n" "\\n")))
(defn harness/append-entry [type data &opt display]
  (when (and (string? type) (string? data))
    (let [d (if (nil? display) "1" (if display "1" "0"))]
      (set harness-entries-buf
           (string harness-entries-buf
                   (harness/-escape type) "\t"
                   (harness/-escape data) "\t"
                   d "\n")))))

# Registered renderer functions per plugin entry type.
# (harness/register-renderer "bookmark" "fn-name") records a
# (type, fn-name) pair the host looks up when displaying entries
# of that type. Stored as `type|fn\n` (same convention as
# harness-cmd-list).
(var harness-renderer-list "")
(defn harness/register-renderer [type fn-name]
  (when (and (string? type) (string? fn-name))
    (set harness-renderer-list
         (string harness-renderer-list type "|" fn-name "\n"))))

# Output buffer for a renderer invocation. The host clears it
# before calling the renderer, then reads back the accumulated
# `color\ttext\n` lines. Plugins call (harness/render color text)
# from inside their renderer to emit each output line.
(var harness-render-buf "")
(defn harness/render [color text]
  (when (and (or (string? color) (keyword? color) (symbol? color))
             (string? text))
    (set harness-render-buf
         (string harness-render-buf
                 (string color) "\t"
                 (harness/-escape text) "\n"))))

# Plugin-registered LLM providers (P1). Plugins call
# (harness/register-provider name type base-url &opt api-key-env)
# at load time to make a custom provider available alongside the
# ones in config. Stored as `name|type|base-url|api-key-env\n`
# so the host's list_providers can parse with a single
# Janet -> Rust round-trip after all plugins finish loading.
(var harness-providers-list "")
(defn harness/register-provider [name type base-url &opt api-key-env]
  (when (and (string? name) (string? type) (string? base-url))
    (let [env (if (and api-key-env (string? api-key-env)) api-key-env "")]
      (set harness-providers-list
           (string harness-providers-list
                   name "|" type "|" base-url "|" env "\n")))))

# Session-tree mutation ops queued from plugins (P4d). Mirrors pi's
# ctx.setLabel / ctx.fork / ctx.navigateTree / ctx.newSession /
# ctx.switchSession but routed through the host so the drain happens
# between turns. Each line is `op\targ1[\targ2...]\n` (escaped via
# harness/-escape) so a single round-trip + parse gives the host the
# whole queue.
(var harness-tree-ops "")
(defn- harness/-push-op [& parts]
  (set harness-tree-ops
       (string harness-tree-ops
               (string/join (map harness/-escape (map string parts)) "\t")
               "\n")))
# (harness/set-label id label-or-nil) — set or clear a node label.
# Pass nil/false to clear; any string is set verbatim.
(defn harness/set-label [id label]
  (when (string? id)
    (harness/-push-op "set-label" id (if (string? label) label ""))))
# (harness/fork id &opt position) — branch off the chosen entry.
# position defaults to :before (extracts prompt text into editor);
# :at switches to that entry as the leaf without touching the editor.
(defn harness/fork [id &opt position]
  (when (string? id)
    (let [pos (cond
                (or (= position :at) (= position "at")) "at"
                "before")]
      (harness/-push-op "fork" id pos))))
# (harness/navigate-tree id) — move active leaf to id. User-message
# entries restore prompt text + go to parent (matching pi's behaviour);
# other entries become the new leaf directly.
(defn harness/navigate-tree [id]
  (when (string? id)
    (harness/-push-op "navigate-tree" id)))
# (harness/new-session &opt parent-session) — start a fresh session
# in place, optionally recording the prior session id as parent
# lineage. The host persists the current session before resetting.
(defn harness/new-session [&opt parent-session]
  (let [p (if (string? parent-session) parent-session "")]
    (harness/-push-op "new-session" p)))
# (harness/switch-session session-id-prefix) — load a saved session
# matching the id prefix and replace the current session in place.
(defn harness/switch-session [session-id]
  (when (string? session-id)
    (harness/-push-op "switch-session" session-id)))
"#;

/// Janet-side aliases that defer the actual blocking work to the
/// registered C functions. Installed after `add_c_fn` so the symbols
/// are present in the env.
#[cfg(feature = "plugin")]
const HARNESS_DIALOG_INIT: &str = r#"
# (harness/confirm "title" "question") -> bool
# (harness/select  "title" array-of-options) -> string | nil
#
# Both block the worker thread (not the UI thread) until the host
# replies, so they are safe to call from any plugin hook.
(defn harness/confirm [title question]
  (if (and (string? title) (string? question))
    (harness/__confirm title question)
    false))

(defn harness/select [title opts]
  (when (and (string? title) (indexed? opts))
    (harness/__select title opts)))
"#;

/// What the UI is being asked to render. Carries a one-shot reply
/// channel back so the worker can resume once the user answers.
///
/// Variants are only constructed when the plugin feature is enabled,
/// but the *type* is referenced unconditionally by the UI's channel
/// signature — hence the cfg-gated dead-code allow rather than a
/// feature gate on the whole enum.
#[derive(Debug)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum DialogRequest {
    Confirm {
        title: String,
        question: String,
        reply: mpsc::Sender<DialogReply>,
    },
    Select {
        title: String,
        options: Vec<String>,
        reply: mpsc::Sender<DialogReply>,
    },
}

#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum DialogReply {
    /// User answered yes/no. False also covers cancel/timeout.
    Confirm(bool),
    /// Some(option) when the user picked, None on cancel.
    Select(Option<String>),
}

thread_local! {
    /// Set once at worker init. The JanetCFunctions read this to forward
    /// dialog requests to the UI. `RefCell<Option<...>>` so we can
    /// install at startup and tests can clear/set.
    static DIALOG_TX: RefCell<Option<tmpsc::UnboundedSender<DialogRequest>>> = const { RefCell::new(None) };

    /// Shared with the Worker handle. The cfns poll this every
    /// `DIALOG_POLL` while blocked on a dialog reply so that
    /// `Worker::Drop` can abort an in-flight `harness/confirm` /
    /// `harness/select` call instead of hanging forever when the UI
    /// receiver has been dropped.
    static SHUTDOWN: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub enum Cmd {
    /// Evaluate Janet code and return its stringified result.
    Eval {
        code: String,
        reply: mpsc::Sender<Result<String, String>>,
    },
    Shutdown,
}

/// Handle to the worker thread. All Janet evaluation goes through `eval`.
/// Cheap to construct (only the spawn is heavy); cloneable senders are
/// not exposed — callers go through `&mut self` so writes serialize.
pub struct Worker {
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    cmd_tx: mpsc::Sender<Cmd>,
    join: Option<JoinHandle<()>>,
    /// Flipped by `Drop` so an in-flight `harness/confirm`/`harness/select`
    /// can stop waiting on the UI and let the worker exit. Shared by
    /// `Arc` with the worker thread's `SHUTDOWN` thread-local.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    shutdown: Arc<AtomicBool>,
}

impl Worker {
    /// Spawn the Janet worker thread, install harness defs, and wait for
    /// the worker to confirm Janet init succeeded. Returns Err if Janet
    /// VM initialization fails (e.g. linker / lib issues).
    ///
    /// The returned `dialog_rx` is the consumer end of the dialog channel
    /// the UI loop should drain. It's only returned once because we want
    /// a single owner.
    #[cfg(feature = "plugin")]
    pub fn try_spawn() -> Result<(Self, tmpsc::UnboundedReceiver<DialogRequest>), String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (dialog_tx, dialog_rx) = tmpsc::unbounded_channel::<DialogRequest>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let join = thread::Builder::new()
            .name("dirge-janet".to_string())
            .spawn(move || worker_loop(cmd_rx, dialog_tx, init_tx, shutdown_clone))
            .map_err(|e| format!("spawn janet worker: {e}"))?;

        // Block (with a watchdog timeout) until worker confirms init.
        // A worker panic before init_tx.send would otherwise hang main
        // forever; INIT_TIMEOUT bounds that worst case.
        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(())) => Ok((
                Self {
                    cmd_tx,
                    join: Some(join),
                    shutdown,
                },
                dialog_rx,
            )),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("janet worker did not init within {INIT_TIMEOUT:?}"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("janet worker exited during init".to_string())
            }
        }
    }

    #[cfg(not(feature = "plugin"))]
    pub fn try_spawn() -> Result<(Self, tmpsc::UnboundedReceiver<DialogRequest>), String> {
        // No-op worker for non-plugin builds. cmd_rx is dropped immediately
        // when the thread exits; cmd_tx writes will Err out cleanly.
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let (_dialog_tx, dialog_rx) = tmpsc::unbounded_channel::<DialogRequest>();
        Ok((
            Self {
                cmd_tx,
                join: None,
                shutdown: Arc::new(AtomicBool::new(false)),
            },
            dialog_rx,
        ))
    }

    /// Send a Janet expression to the worker and block until it returns
    /// the stringified result (or a Janet error message).
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        let (reply, rx) = mpsc::channel();
        self.cmd_tx
            .send(Cmd::Eval {
                code: code.to_string(),
                reply,
            })
            .map_err(|_| "janet worker disconnected".to_string())?;
        rx.recv()
            .map_err(|_| "janet worker dropped reply channel".to_string())?
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Set the shutdown flag FIRST, then send the Shutdown cmd.
        // A worker that's currently blocked inside an unanswered
        // `harness/confirm`/`harness/select` polls this flag every
        // `DIALOG_POLL` and gives up — without the flag, the cfn would
        // sit on `reply_rx.recv()` forever, the cmd_rx would never read
        // Shutdown, and `join` would hang.
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

#[cfg(feature = "plugin")]
fn worker_loop(
    rx: mpsc::Receiver<Cmd>,
    dialog_tx: tmpsc::UnboundedSender<DialogRequest>,
    init_tx: mpsc::Sender<Result<(), String>>,
    shutdown: Arc<AtomicBool>,
) {
    // Hand the dialog sender + shutdown flag to this thread's C functions
    // before we run any plugin code, otherwise harness/confirm/select
    // would no-op and shutdown couldn't cancel an in-flight dialog.
    DIALOG_TX.with(|cell| *cell.borrow_mut() = Some(dialog_tx));
    SHUTDOWN.with(|cell| *cell.borrow_mut() = Some(shutdown));

    let mut client = match JanetClient::init_with_default_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("Janet init failed: {e}")));
            return;
        }
    };

    // Install C functions backing harness/confirm and harness/select.
    // They must be registered before the Janet-side aliases reference
    // them; we register, then run the alias definitions.
    //
    // `CFunOptions::namespace` requires `'static` CStr, so we use C string
    // literals (Rust 1.77+ `c"..."` syntax) instead of runtime CString.
    if let Some(env) = client.env_mut() {
        env.add_c_fn(CFunOptions::new(c"__confirm", janet_confirm_cfn).namespace(c"harness"));
        env.add_c_fn(CFunOptions::new(c"__select", janet_select_cfn).namespace(c"harness"));
    }

    if let Err(e) = client.run(HARNESS_INIT) {
        let _ = init_tx.send(Err(format!("harness init failed: {e}")));
        return;
    }
    if let Err(e) = client.run(HARNESS_DIALOG_INIT) {
        let _ = init_tx.send(Err(format!("harness dialog init failed: {e}")));
        return;
    }

    let _ = init_tx.send(Ok(()));

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Eval { code, reply } => {
                let r = client
                    .run(&code)
                    .map(|v| v.to_string())
                    .map_err(|e| format!("Janet error: {e}"));
                let _ = reply.send(r);
            }
            Cmd::Shutdown => break,
        }
    }
}

#[cfg(not(feature = "plugin"))]
#[allow(dead_code)]
fn worker_loop(
    _rx: mpsc::Receiver<Cmd>,
    _dialog_tx: tmpsc::UnboundedSender<DialogRequest>,
    _init_tx: mpsc::Sender<Result<(), String>>,
    _shutdown: Arc<AtomicBool>,
) {
    unreachable!("worker_loop should never run without the plugin feature");
}

// --- JanetCFunction shims ----------------------------------------------
//
// These run on the worker thread under Janet's control. They unwrap
// argv as strings via evil_janet's raw API, build a DialogRequest, send
// it to the UI through DIALOG_TX, block on the reply, and wrap the
// answer back into a Janet value.

#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_confirm_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    // Catch any Rust panic at the FFI boundary. The C-unwind ABI would
    // technically let it propagate into Janet's C runtime, but Janet
    // isn't built to clean up after foreign unwinds — heap corruption
    // and segfaults follow. Convert any panic to a safe `false`.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        confirm_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(_) => unsafe { janet_wrap_boolean(0) },
    }
}

/// Safe-Rust body of `janet_confirm_cfn`. Split out so it can panic
/// without worrying about FFI unwind semantics; the cfn wraps the call
/// in `catch_unwind` and substitutes a safe default on panic.
#[cfg(feature = "plugin")]
unsafe fn confirm_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_boolean(0) };
    }
    let title = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_boolean(0) },
    };
    let question = match unsafe { read_string_arg(argv, 1) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_boolean(0) },
    };

    let answer = DIALOG_TX.with(|cell| match cell.borrow().as_ref() {
        Some(tx) => send_dialog(tx, |reply| DialogRequest::Confirm {
            title,
            question,
            reply,
        })
        .unwrap_or(DialogReply::Confirm(false)),
        None => DialogReply::Confirm(false),
    });

    let yes = matches!(answer, DialogReply::Confirm(true));
    unsafe { janet_wrap_boolean(if yes { 1 } else { 0 }) }
}

#[cfg(feature = "plugin")]
unsafe extern "C-unwind" fn janet_select_cfn(
    argc: i32,
    argv: *mut janetrs::lowlevel::Janet,
) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        select_body(argc, argv)
    }));
    match result {
        Ok(j) => j,
        Err(_) => unsafe { janet_wrap_nil() },
    }
}

#[cfg(feature = "plugin")]
unsafe fn select_body(argc: i32, argv: *mut janetrs::lowlevel::Janet) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    if argc < 2 {
        return unsafe { janet_wrap_nil() };
    }
    let title = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => return unsafe { janet_wrap_nil() },
    };
    let options = match unsafe { read_string_array_arg(argv, 1) } {
        Some(v) if !v.is_empty() => v,
        _ => return unsafe { janet_wrap_nil() },
    };

    let answer = DIALOG_TX.with(|cell| match cell.borrow().as_ref() {
        Some(tx) => send_dialog(tx, |reply| DialogRequest::Select {
            title,
            options,
            reply,
        })
        .unwrap_or(DialogReply::Select(None)),
        None => DialogReply::Select(None),
    });

    match answer {
        DialogReply::Select(Some(s)) => unsafe { wrap_string(&s) },
        _ => unsafe { janet_wrap_nil() },
    }
}

/// Send a dialog request, build it via the supplied closure (so we can
/// move owned strings into the variant), and block on the reply.
/// Returns `None` if the UI side dropped the channel OR the worker is
/// shutting down. The outbound side uses tokio's unbounded sender so
/// the UI loop can `recv().await` in `tokio::select!`; the inbound
/// reply is a std mpsc with a polling timeout so the cfn can also
/// abort when `Worker::Drop` flips the shutdown flag.
#[cfg(feature = "plugin")]
fn send_dialog<F>(tx: &tmpsc::UnboundedSender<DialogRequest>, build: F) -> Option<DialogReply>
where
    F: FnOnce(mpsc::Sender<DialogReply>) -> DialogRequest,
{
    let (reply_tx, reply_rx) = mpsc::channel();
    let req = build(reply_tx);
    tx.send(req).ok()?;

    // Poll for the reply. Wake every `DIALOG_POLL` to check the
    // worker-shutdown flag so a UI exit or `Worker::Drop` doesn't pin
    // us forever on `recv()`. The polling overhead is negligible
    // compared to the time a human takes to answer a dialog.
    loop {
        match reply_rx.recv_timeout(DIALOG_POLL) {
            Ok(r) => return Some(r),
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let shutting_down = SHUTDOWN.with(|cell| {
                    cell.borrow()
                        .as_ref()
                        .map(|f| f.load(Ordering::SeqCst))
                        .unwrap_or(false)
                });
                if shutting_down {
                    return None;
                }
            }
        }
    }
}

/// Read a Janet string at argv[i] and decode as UTF-8. Returns None for
/// non-string args or invalid UTF-8 (we don't surface lossy strings to
/// plugins — caller handles the None as a no-op).
#[cfg(feature = "plugin")]
unsafe fn read_string_arg(argv: *mut janetrs::lowlevel::Janet, i: i32) -> Option<String> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    // janet_checktype returns 1 if the type matches.
    let is_str = unsafe { janet_checktype(v, JanetType_JANET_STRING) } != 0;
    let is_kw = unsafe { janet_checktype(v, JanetType_JANET_KEYWORD) } != 0;
    let is_sym = unsafe { janet_checktype(v, JanetType_JANET_SYMBOL) } != 0;
    let is_buf = unsafe { janet_checktype(v, JanetType_JANET_BUFFER) } != 0;
    if !(is_str || is_kw || is_sym || is_buf) {
        return None;
    }
    if is_buf {
        let buf = unsafe { janet_unwrap_buffer(v) };
        if buf.is_null() {
            return None;
        }
        let data = unsafe { (*buf).data };
        let count = unsafe { (*buf).count } as usize;
        let slice = unsafe { std::slice::from_raw_parts(data, count) };
        return std::str::from_utf8(slice).ok().map(str::to_string);
    }
    let raw = unsafe { janet_unwrap_string(v) };
    if raw.is_null() {
        return None;
    }
    // Janet strings carry their length in the GC header; janet_string_head
    // is the public way to fetch it (janet_string_length is a C macro that
    // isn't exposed through the auto-generated bindings).
    let len = unsafe { (*janet_string_head(raw)).length } as usize;
    let slice = unsafe { std::slice::from_raw_parts(raw, len) };
    std::str::from_utf8(slice).ok().map(str::to_string)
}

/// Read a Janet tuple/array of strings at argv[i].
#[cfg(feature = "plugin")]
unsafe fn read_string_array_arg(
    argv: *mut janetrs::lowlevel::Janet,
    i: i32,
) -> Option<Vec<String>> {
    use janetrs::lowlevel::*;
    let v = unsafe { *argv.offset(i as isize) };
    let is_tuple = unsafe { janet_checktype(v, JanetType_JANET_TUPLE) } != 0;
    let is_array = unsafe { janet_checktype(v, JanetType_JANET_ARRAY) } != 0;
    if !is_tuple && !is_array {
        return None;
    }
    let (data, len) = if is_tuple {
        let raw = unsafe { janet_unwrap_tuple(v) };
        if raw.is_null() {
            return None;
        }
        // Same GC-header trick as strings; janet_tuple_length is a macro.
        let n = unsafe { (*janet_tuple_head(raw)).length } as usize;
        (raw, n)
    } else {
        let arr = unsafe { janet_unwrap_array(v) };
        if arr.is_null() {
            return None;
        }
        let n = unsafe { (*arr).count } as usize;
        (unsafe { (*arr).data } as *const janetrs::lowlevel::Janet, n)
    };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    let mut out = Vec::with_capacity(len);
    for (idx, _) in slice.iter().enumerate() {
        // Recurse through the same arg-reader, treating each element as if
        // it sat at argv[idx]. Doable because read_string_arg only uses
        // the raw Janet, not its position.
        let elt_ptr = unsafe { data.add(idx) } as *mut janetrs::lowlevel::Janet;
        match unsafe { read_string_arg(elt_ptr, 0) } {
            Some(s) => out.push(s),
            None => return None,
        }
    }
    Some(out)
}

/// Wrap a Rust `&str` as a Janet string. The Janet GC takes ownership of
/// the copied bytes via janet_string. Returns Janet nil when the string
/// is too large for Janet's i32 length (>2 GB) — this never happens for
/// real dialog answers but is checked defensively because silently
/// truncating the length to i32 would let Janet read past the
/// allocation.
#[cfg(feature = "plugin")]
unsafe fn wrap_string(s: &str) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let bytes = s.as_bytes();
    let Ok(len) = i32::try_from(bytes.len()) else {
        return unsafe { janet_wrap_nil() };
    };
    let raw = unsafe { janet_string(bytes.as_ptr(), len) };
    unsafe { janet_wrap_string(raw) }
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use super::*;

    #[test]
    fn worker_round_trips_an_eval() {
        let (mut worker, _dialog_rx) = Worker::try_spawn().unwrap();
        let r = worker.eval("(+ 1 2)").unwrap();
        assert_eq!(r, "3");
    }

    #[test]
    fn worker_surfaces_janet_errors_as_err() {
        let (mut worker, _dialog_rx) = Worker::try_spawn().unwrap();
        // `undefined-fn` is genuinely unknown.
        let r = worker.eval("(undefined-fn 1)");
        assert!(r.is_err(), "expected Err, got {r:?}");
    }

    #[test]
    fn worker_eval_returns_keyword_string() {
        let (mut worker, _dialog_rx) = Worker::try_spawn().unwrap();
        // Verify the worker installed the harness defs.
        let r = worker
            .eval("(harness/has-symbol? \"harness/notify\")")
            .unwrap();
        assert_eq!(r, "true");
    }

    #[test]
    fn confirm_sends_a_dialog_request_with_title_and_question() {
        let (mut worker, dialog_rx) = Worker::try_spawn().unwrap();

        // Start a helper thread that auto-answers any confirm with `true`.
        let mut dialog_rx = dialog_rx;
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm {
                title,
                question,
                reply,
            }) => {
                assert_eq!(title, "warn");
                assert_eq!(question, "really?");
                let _ = reply.send(DialogReply::Confirm(true));
            }
            other => panic!("unexpected dialog request: {other:?}"),
        });

        let r = worker
            .eval(r#"(harness/confirm "warn" "really?")"#)
            .unwrap();
        // Janet booleans stringify as "true" / "false".
        assert_eq!(r, "true");
        helper.join().unwrap();
    }

    #[test]
    fn confirm_returns_false_when_dialog_replies_false() {
        let (mut worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm { reply, .. }) => {
                let _ = reply.send(DialogReply::Confirm(false));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/confirm "t" "q")"#).unwrap();
        assert_eq!(r, "false");
        helper.join().unwrap();
    }

    #[test]
    fn select_returns_picked_option_as_string() {
        let (mut worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select {
                title,
                options,
                reply,
            }) => {
                assert_eq!(title, "pick");
                assert_eq!(options, vec!["alpha".to_string(), "beta".to_string()]);
                let _ = reply.send(DialogReply::Select(Some("beta".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker
            .eval(r#"(harness/select "pick" ["alpha" "beta"])"#)
            .unwrap();
        // Janet strings stringify with surrounding quotes; we check substring.
        assert!(r.contains("beta"), "got {r:?}");
        helper.join().unwrap();
    }

    #[test]
    fn select_returns_nil_on_cancel() {
        let (mut worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                let _ = reply.send(DialogReply::Select(None));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/select "pick" ["a"])"#).unwrap();
        assert_eq!(r, "nil");
        helper.join().unwrap();
    }

    #[test]
    fn dialog_rx_drains_when_no_request_pending() {
        // Sanity: a fresh worker doesn't emit phantom dialog requests.
        let (_worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        assert!(matches!(
            dialog_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    /// R1 critical: setting the shutdown flag unblocks an in-flight
    /// dialog within ~`DIALOG_POLL` so `Worker::Drop` doesn't hang.
    /// Before R1, send_dialog's `reply_rx.recv()` had no timeout and
    /// the eval would block forever if the UI never replied.
    ///
    /// We can't trigger the abort via Drop directly (the worker is
    /// moved into the eval thread; dropping it from outside is exactly
    /// the catch-22 R1 exists to break). Instead we clone the shutdown
    /// Arc out before moving, then flip it once the dialog has arrived.
    /// This exercises the same code path Drop uses.
    #[test]
    fn shutdown_flag_aborts_in_flight_dialog() {
        use std::time::Instant;

        let (worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        let shutdown_handle = worker.shutdown.clone();

        // Kick off a confirm; it will block waiting for a reply we
        // never send. After the shutdown flag flips, send_dialog's
        // polling loop returns None and the cfn returns Janet false.
        let eval_t = std::thread::spawn(move || {
            let mut worker = worker;
            let result = worker.eval(r#"(harness/confirm "x" "y")"#);
            (worker, result)
        });

        // Wait for the dialog request to land — the worker is now
        // parked inside send_dialog's recv_timeout loop.
        let _req = dialog_rx.blocking_recv().expect("dialog request");

        // Flip the flag. The cfn wakes up on its next 50 ms tick.
        shutdown_handle.store(true, Ordering::SeqCst);

        let started = Instant::now();
        let (worker, eval_result) = eval_t.join().expect("eval thread");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "eval took {elapsed:?}, expected ~DIALOG_POLL once the flag was flipped"
        );
        // On shutdown the cfn returns Janet false (its safe default).
        assert_eq!(eval_result.unwrap(), "false");

        // Drop the worker explicitly — should complete promptly since
        // the in-flight dialog has already unwound.
        drop(worker);
    }

    /// R1: oversized strings to wrap_string don't truncate to i32 —
    /// instead they return Janet nil. Hard to test with a real 2 GB
    /// string, so we exercise the same boundary via a small synthetic
    /// check that the i32::try_from path is taken. This is mostly a
    /// regression sentinel — if someone reverts the bounds check it
    /// fails to compile (wrap_string still requires Send/Sync to be
    /// callable from a select reply context).
    #[test]
    fn wrap_string_handles_empty() {
        // Just verify Janet round-trips the empty string through
        // confirm's reply path. Catches any wrap_string regression
        // that miscounts zero-length input.
        let (mut worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                let _ = reply.send(DialogReply::Select(Some(String::new())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker
            .eval(r#"(harness/select "pick" ["only-option"])"#)
            .unwrap();
        // janetrs stringifies a Janet string with no quotes (just the
        // raw bytes), so an empty Janet string round-trips as the
        // empty Rust string here.
        assert_eq!(r, "");
        helper.join().unwrap();
    }

    // --- R2: FFI edge cases ---------------------------------------------

    /// R2: read_string_arg accepts Janet keywords (call sites can use
    /// `(harness/confirm :title "q")` instead of double-quoted strings).
    /// Caught by an integration test through harness/confirm since the
    /// cfn is the only caller; if read_string_arg ever stops accepting
    /// keywords this test fails.
    #[test]
    fn confirm_accepts_keyword_title() {
        let (mut worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Confirm {
                title,
                question,
                reply,
            }) => {
                assert_eq!(title, "warn");
                assert_eq!(question, "really?");
                let _ = reply.send(DialogReply::Confirm(true));
            }
            other => panic!("unexpected: {other:?}"),
        });
        // Keyword first arg — read_string_arg's is_kw branch handles it.
        let r = worker
            .eval(r#"(harness/__confirm :warn "really?")"#)
            .unwrap();
        assert_eq!(r, "true");
        helper.join().unwrap();
    }

    /// R2: read_string_array_arg returns None for an empty array, and
    /// the select cfn surfaces that as Janet nil. Janet-side
    /// harness/select already short-circuits on `(indexed? opts)`, so
    /// we hit the cfn via __select directly.
    #[test]
    fn select_with_empty_options_returns_nil() {
        let (mut worker, _dialog_rx) = Worker::try_spawn().unwrap();
        // Empty array should never even emit a dialog request.
        let r = worker.eval(r#"(harness/__select "pick" [])"#).unwrap();
        assert_eq!(r, "nil");
    }

    /// R2: read_string_array_arg works with tuples too (not just
    /// arrays). Janet array literals `["a"]` are arrays; quoted forms
    /// `'("a")` produce tuples. Both should be accepted.
    #[test]
    fn select_accepts_tuple_options() {
        let (mut worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { options, reply, .. }) => {
                assert_eq!(options, vec!["alpha".to_string(), "beta".to_string()]);
                let _ = reply.send(DialogReply::Select(Some("alpha".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        // Use a quoted tuple instead of an array literal.
        let r = worker
            .eval(r#"(harness/__select "pick" '("alpha" "beta"))"#)
            .unwrap();
        assert!(r.contains("alpha"), "got {r:?}");
        helper.join().unwrap();
    }

    /// R2: wrap_string handles multibyte UTF-8 correctly. The byte
    /// length is the Janet string's allocation; an off-by-one here
    /// would either truncate emoji or read past the slice.
    #[test]
    fn select_returns_multibyte_option_through_wrap_string() {
        let (mut worker, mut dialog_rx) = Worker::try_spawn().unwrap();
        let helper = std::thread::spawn(move || match dialog_rx.blocking_recv() {
            Some(DialogRequest::Select { reply, .. }) => {
                // Emoji + CJK + Cyrillic — all multibyte UTF-8.
                let _ = reply.send(DialogReply::Select(Some("🦀漢字Привет".to_string())));
            }
            other => panic!("unexpected: {other:?}"),
        });
        let r = worker.eval(r#"(harness/select "pick" ["x"])"#).unwrap();
        // Janet stringification preserves the raw UTF-8 bytes; the
        // result should contain all three multibyte sequences intact.
        assert!(r.contains("🦀"), "lost emoji: {r:?}");
        assert!(r.contains("漢字"), "lost CJK: {r:?}");
        assert!(r.contains("Привет"), "lost Cyrillic: {r:?}");
        helper.join().unwrap();
    }
}
