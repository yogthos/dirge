# dap-watch — expression watchpoints with auto-refresh via DAP Janet FFI
#
# At every tool-end event, checks if a DAP session is active and
# then evaluates all registered watch expressions via `dap/eval`.
# Prints results inline as notifications.
#
# Usage:
#   /dap-watch add counter.value      Start watching an expression
#   /dap-watch add "x + y"
#   /dap-watch add "len(items)"
#   /dap-watch list                   List all watches
#   /dap-watch remove 0               Remove watch #0
#   /dap-watch clear                  Remove all watches
#   /dap-watch once "x"               One-shot eval, no register

(def hooks ["on-tool-end"])

# ── State ────────────────────────────────────────────────────────────

(var watch-exprs @[])  # list of expression strings

# ── Hook — evaluate watches on tool-end ──────────────────────────────

(defn on-tool-end [ctx]
  (when (and (dap/session-active?) (not (empty? watch-exprs)))
    (def status-str (dap/sessions))
    (when (and status-str (string/find "\"stopped\"" status-str))
      (var output "")
      (for i 0 (length watch-exprs)
        (def expr (get watch-exprs i))
        (def val (dap/eval expr))
        (if val
          (set output (string output "[" i "] " expr " = " val "\n"))
          (set output (string output "[" i "] " expr " = <failed>\n"))))
      (when (not (empty? output))
        (harness/notify (string "WATCH:\n" output) :info)))))

# ── Slash commands ──────────────────────────────────────────────────

(defn watch-cmd [args]
  (def parts (string/split " " args))
  (def sub (get parts 0))
  (def rest (string/join (array/slice parts 1) " "))

  (match sub
    "add" (do
      (when (empty? rest)
        (break "usage: /dap-watch add <expression>"))
      (array/push watch-exprs rest)
      (string "added watch #" (- (length watch-exprs) 1) ": " rest))

    "remove" (do
      (when (empty? rest)
        (break "usage: /dap-watch remove <index>"))
      (def idx (math/parse-int rest))
      (if (or (< idx 0) (>= idx (length watch-exprs)))
        (break (string "invalid index " idx " (0.." (- (length watch-exprs) 1) ")")))
      (def removed (get watch-exprs idx))
      (set watch-exprs (array/remove watch-exprs idx))
      (string "removed: " removed))

    "list" (do
      (if (empty? watch-exprs)
        "no watches"
        (do
          (var out "watches:\n")
          (for i 0 (length watch-exprs)
            (set out (string out "  [" i "] " (get watch-exprs i) "\n")))
          out)))

    "clear" (do
      (set watch-exprs @[])
      "all watches cleared")

    "once" (do
      (when (empty? rest)
        (break "usage: /dap-watch once <expression>"))
      (if (not (dap/session-active?))
        (break "no active DAP session"))
      (def val (dap/eval rest))
      (or val "eval failed"))

    (string "unknown: " sub " — try add, remove, list, clear, once")))

(harness/register-command "dap-watch" "watch-cmd")
