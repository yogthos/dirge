# nREPL plugin hooks.
#
# on-init: tries to auto-connect by reading .nrepl-port from cwd.
# before-agent-start: injects skill instructions for nrepl_eval tool.

(defn on-init [ctx]
  (def cwd (harness/get-cwd))
  (def port-file (string cwd "/.nrepl-port"))
  (if-let [port-str (try (string/trim (slurp port-file)) ([_] nil))]
    (do
      (harness/log (string "nrepl: found .nrepl-port → " port-str))
      # Janet `try` takes ONE body form, so multi-step bodies must be
      # wrapped in `(do ...)`.
      (try
        (do
          (def status (nrepl-connect nrepl-host port-str))
          (harness/notify (string "[nrepl] " status) :info)
          (harness/log (string "nrepl: " status)))
        ([err]
         (harness/notify
           (string "[nrepl] auto-connect failed: " err) :warn))))
    (harness/log "nrepl: no .nrepl-port found, skipping auto-connect"))
  nil)

(def nrepl-skill-prompt
  (string
    "\n"
    "## nREPL Evaluation\n"
    "\n"
    "You have access to an nREPL-connected Clojure REPL via the `nrepl_eval` tool. "
    "The REPL session persists across evaluations — state, vars, and loaded namespaces "
    "survive as long as the nREPL server keeps running.\n"
    "\n"
    "### When to use nrepl_eval\n"
    "- Verify that edited Clojure files compile and load correctly\n"
    "- Test function behavior interactively\n"
    "- Check runtime state (vars, namespaces, system components)\n"
    "- Debug code by evaluating expressions\n"
    "- Require or reload namespaces after making changes\n"
    "- Run tests from the REPL\n"
    "\n"
    "### How to connect\n"
    "Use the `/nrepl-connect [host] [port]` slash command. "
    "If no port is given, reads from `.nrepl-port` in the project root. "
    "Check connection status with `/nrepl-status`.\n"
    "\n"
    "### Common patterns\n"
    "\n"
    "**Require a namespace (always use :reload to pick up changes):**\n"
    "```\n"
    "(require '[my.namespace :as ns] :reload)\n"
    "```\n"
    "\n"
    "**Test a function after requiring:**\n"
    "```\n"
    "(ns/my-function arg1 arg2)\n"
    "```\n"
    "\n"
    "**Check if a file compiles:**\n"
    "```\n"
    "(require 'my.namespace :reload)\n"
    "```\n"
    "\n"
    "**Run a test:**\n"
    "```\n"
    "(require '[clojure.test :refer [run-tests]])\n"
    "(run-tests 'my.namespace-test)\n"
    "```\n"
    "\n"
    "**Inspect runtime state:**\n"
    "```\n"
    "(keys (ns-publics 'my.namespace))\n"
    "(*print-length*)\n"
    "```\n"
    "\n"
    "**Multiple expressions in one call:**\n"
    "```\n"
    "(def x 10)\n"
    "(* x 2)\n"
    "(+ x 5)\n"
    "```\n"
    "\n"
    "### Important notes\n"
    "- **Automatic delimiter repair:** Unbalanced parentheses are auto-repaired before eval. "
    "If you see a repair notice, your code had mismatched delimiters.\n"
    "- **Session persists:** State survives across tool calls until the nREPL server restarts.\n"
    "- **Always :reload:** When requiring namespaces after editing, use `:reload` to pick up changes.\n"
    "- **Connection management:** Use `/nrepl-status` to check connectivity, "
    "`/nrepl-disconnect` to close, `/nrepl-connect` to reconnect.\n"))

(defn before-agent-start [ctx]
  (harness/append-system-prompt nrepl-skill-prompt)
  nil)
