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
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use tokio::sync::mpsc as tmpsc;

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
#[derive(Debug)]
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

        let join = thread::Builder::new()
            .name("dirge-janet".to_string())
            .spawn(move || worker_loop(cmd_rx, dialog_tx, init_tx))
            .map_err(|e| format!("spawn janet worker: {e}"))?;

        // Block until worker confirms init. If Janet failed to load we
        // surface the error here rather than discovering it on first eval.
        match init_rx.recv() {
            Ok(Ok(())) => Ok((
                Self {
                    cmd_tx,
                    join: Some(join),
                },
                dialog_rx,
            )),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("janet worker exited during init".to_string()),
        }
    }

    #[cfg(not(feature = "plugin"))]
    pub fn try_spawn() -> Result<(Self, tmpsc::UnboundedReceiver<DialogRequest>), String> {
        // No-op worker for non-plugin builds. cmd_rx is dropped immediately
        // when the thread exits; cmd_tx writes will Err out cleanly.
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let (_dialog_tx, dialog_rx) = tmpsc::unbounded_channel::<DialogRequest>();
        Ok((Self { cmd_tx, join: None }, dialog_rx))
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
) {
    // Hand the dialog sender to this thread's C functions before we run
    // any plugin code, otherwise harness/confirm/select would no-op.
    DIALOG_TX.with(|cell| *cell.borrow_mut() = Some(dialog_tx));

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
    if argc < 2 {
        unsafe {
            return janet_wrap_boolean(0);
        }
    }
    let title = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => unsafe { return janet_wrap_boolean(0) },
    };
    let question = match unsafe { read_string_arg(argv, 1) } {
        Some(s) => s,
        None => unsafe { return janet_wrap_boolean(0) },
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
    if argc < 2 {
        unsafe {
            return janet_wrap_nil();
        }
    }
    let title = match unsafe { read_string_arg(argv, 0) } {
        Some(s) => s,
        None => unsafe { return janet_wrap_nil() },
    };
    let options = match unsafe { read_string_array_arg(argv, 1) } {
        Some(v) if !v.is_empty() => v,
        _ => unsafe { return janet_wrap_nil() },
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
/// Returns `None` if the UI side dropped the channel. The outbound side
/// uses tokio's unbounded sender so the UI loop can `recv().await` in
/// `tokio::select!`; the inbound reply is a std mpsc since the worker
/// thread is the only blocker.
#[cfg(feature = "plugin")]
fn send_dialog<F>(tx: &tmpsc::UnboundedSender<DialogRequest>, build: F) -> Option<DialogReply>
where
    F: FnOnce(mpsc::Sender<DialogReply>) -> DialogRequest,
{
    let (reply_tx, reply_rx) = mpsc::channel();
    let req = build(reply_tx);
    tx.send(req).ok()?;
    reply_rx.recv().ok()
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
/// the copied bytes via janet_string.
#[cfg(feature = "plugin")]
unsafe fn wrap_string(s: &str) -> janetrs::lowlevel::Janet {
    use janetrs::lowlevel::*;
    let bytes = s.as_bytes();
    let raw = unsafe { janet_string(bytes.as_ptr(), bytes.len() as i32) };
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
}
