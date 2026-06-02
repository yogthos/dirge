# dap-repl — interactive debug REPL driven by DAP Janet FFI
#
# Registers a /dap-repl slash command that opens a sub-mode where you
# type gdb/lldb-like commands directly. Every command calls the DAP
# Janet bindings (dap/launch, dap/step, dap/eval, dap/bp, etc.)
# and returns the JSON result.
#
# The REPL runs as a Janet coroutine: the harness/request-prompt
# mechanism injects each command's prompt into the chat. This means
# you get agent assistance between commands — you can ask "why is
# this variable null?" and the agent can call dap/stack-trace, etc.
#
# Architecture:
#   Plugin (this file) → dap/launch, dap/step, etc. (Janet FFI)
#   → C function (src/dap/janet_bindings.rs) → DAP_TX channel
#   → tokio bridge task → DapSessionManager (src/dap/session.rs)
#   → DapClient (src/dap/client.rs) → real adapter process

(def hooks ["on-prompt"])

# ── REPL state ──────────────────────────────────────────────────────

(var repl-active false)
(var last-file "")
(var last-adapter nil)

# ── Command dispatch ────────────────────────────────────────────────

(defn- repl-dispatch [cmd-str]
  (def parts (string/split " " cmd-str))
  (def head (get parts 0))
  (match head
    # Lifecycle
    "launch" (do
      (def file (get parts 1))
      (def adapter (get parts 2))
      (if (not file)
        "usage: launch <file> [adapter]"
        (do
          (set last-file file)
          (set last-adapter adapter)
          (def res (if adapter
                     (dap/launch file adapter)
                     (dap/launch file)))
          (or res "launch failed — check adapter"))))

    "attach" (do
      (def pid-str (get parts 1))
      (def adapter (get parts 2))
      (if (not pid-str)
        "usage: attach <pid> [adapter]"
        (do
          (def pid (math/parse-int pid-str))
          (if (not pid)
            (string "invalid pid: " pid-str)
            (do
              (def res (if adapter
                         (dap/attach pid adapter)
                         (dap/attach pid)))
              (or res "attach failed — check adapter and pid"))))))

    "terminate" (do
      (def res (dap/terminate))
      (set repl-active false)
      (or res "session terminated"))

    # Execution control
    "c" (dap/continue)
    "continue" (dap/continue)
    "n" (dap/step)
    "next" (dap/step)
    "step" (dap/step)
    "s" (dap/step-in)
    "step-in" (dap/step-in)
    "fin" (dap/step-out)
    "finish" (dap/step-out)
    "step-out" (dap/step-out)

    # Inspection
    "p" (dap/eval (string/join (array/slice parts 1) " "))
    "print" (dap/eval (string/join (array/slice parts 1) " "))
    "eval" (dap/eval (string/join (array/slice parts 1) " "))
    "bt" (dap/stack-trace)
    "backtrace" (dap/stack-trace)
    "info threads" (dap/threads)
    "threads" (dap/threads)
    "sessions" (dap/sessions)

    # Breakpoints
    "bp" (do
      (def file (get parts 1))
      (def line (get parts 2))
      (if (and file line)
        (dap/bp file line)
        "usage: bp <file> <line>"))

    "break" (do
      (def file (get parts 1))
      (def line (get parts 2))
      (if (and file line)
        (dap/bp file line)
        "usage: break <file> <line>"))

    # Variables
    "vars" (do
      (def ref (get parts 1))
      (if ref
        (dap/vars ref)
        "usage: vars <var-ref>"))

    # Meta
    "help" "
DAP REPL commands:
  launch <file> [adapter]  Start debugging a program
  attach <pid> [adapter]   Attach to a running process
  terminate                 End debug session
  c / continue              Resume execution
  n / next / step           Step over current line
  s / step-in               Step into function call
  fin / finish / step-out   Step out of current function
  p / print / eval <expr>   Evaluate expression
  bt / backtrace            Show call stack
  info threads              List threads
  bp / break <file> <line>  Set breakpoint
  sessions                  Show session status
  vars <var-ref>            Show variables
  q / quit                  Exit REPL
  help                      Show this message"

    "q" (do
      (set repl-active false)
      "REPL exited — session still active. Use `terminate` to end it.")
    "quit" (do
      (set repl-active false)
      "REPL exited — session still active. Use `terminate` to end it.")

    # Unknown
    (string "unknown command: " head " — try 'help'")))

# ── Hook — intercept prompts when REPL is active ────────────────────

(defn on-prompt [ctx]
  (if (not repl-active)
    nil
    (do
      (def prompt (ctx :prompt))
      # Strip REPL prefix if present
      (def clean (if (string/find "dap> " prompt)
                   (string/slice prompt 5)
                   prompt))
      (if (empty? clean)
        (do
          (harness/request-prompt "dap> ")
          "Type a command or 'help'")
        (do
          (def result (repl-dispatch clean))
          (if repl-active
            (harness/request-prompt "dap> "))
          result)))))

# ── Slash command entry point ───────────────────────────────────────

(defn repl-start [args]
  (if (not (dap/available?))
    "DAP not available — build with --features dap,plugin and install a debug adapter"
    (do
      (set repl-active true)
      (harness/request-prompt "dap> ")
      "
DAP REPL started. Commands:
  launch <file> [adapter]   — start debugging
  attach <pid> [adapter]    — attach to process
  c/continue, n/next/step, s/step-in, fin/step-out
  p/eval <expr>, bt/backtrace, bp/break <file> <line>
  sessions, vars <ref>, terminate, q/quit
")))

(harness/register-command "dap-repl" "repl-start")
