# nREPL slash commands.
#
# /nrepl-connect [host] [port]  — connect to nREPL (default: 127.0.0.1:<.nrepl-port>)
# /nrepl-disconnect             — close connection
# /nrepl-eval <code>            — evaluate code and show result
# /nrepl-status                 — show connection status
# /nrepl-timeout <seconds>      — set per-eval timeout (default 120s)
# /nrepl-interrupt              — interrupt a long-running eval

(defn nrepl-connect-cmd [args]
  (def parts (filter (fn [p] (not= p "")) (string/split " " args)))
  (def host (if (> (length parts) 0) (get parts 0) "127.0.0.1"))
  (def port
    (if (> (length parts) 1)
      (get parts 1)
      (do
        (def cwd (harness/get-cwd))
        (def port-file (string cwd "/.nrepl-port"))
        (if-let [p (try (string/trim (slurp port-file)) ([_] nil))]
          p
          (error "no port specified and no .nrepl-port found")))))
  (nrepl-connect host port))

(defn nrepl-disconnect-cmd [_args]
  (nrepl-disconnect))

(defn nrepl-eval-cmd [args]
  (if (not nrepl-connected)
    "not connected to nREPL — use /nrepl-connect first"
    (try
      (do
        (def result (nrepl-eval args))
        (def out (result :out))
        (def err (result :err))
        (def value (result :result))
        (def parts @[])
        (when (not= out "")
          (array/push parts (string "stdout:\n" out)))
        (when (not= err "")
          (array/push parts (string "stderr:\n" err)))
        (when (not= value "")
          (array/push parts (string "=> " value)))
        (if (> (length parts) 0)
          (string/join parts "\n")
          "nil"))
      ([err]
       (string "eval error: " err)))))

(defn nrepl-status-cmd [_args]
  (nrepl-status))

(defn nrepl-timeout-cmd [args]
  (def secs (scan-number args))
  (if secs
    (do
      (set nrepl-eval-timeout secs)
      (string "nREPL eval timeout set to " secs "s"))
    (string "current nREPL eval timeout: " nrepl-eval-timeout "s (use /nrepl-timeout <seconds> to change)")))

(defn nrepl-interrupt-cmd [_args]
  (if nrepl-connected
    (do
      (nrepl-interrupt)
      "sent interrupt to nREPL server")
    "not connected to nREPL"))

(harness/register-command "nrepl-connect" "nrepl-connect-cmd")
(harness/register-command "nrepl-disconnect" "nrepl-disconnect-cmd")
(harness/register-command "nrepl-eval" "nrepl-eval-cmd")
(harness/register-command "nrepl-status" "nrepl-status-cmd")
(harness/register-command "nrepl-timeout" "nrepl-timeout-cmd")
(harness/register-command "nrepl-interrupt" "nrepl-interrupt-cmd")
