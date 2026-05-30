# nREPL LLM-callable tool.
#
# Registers `nrepl_eval` so the agent can evaluate Clojure code
# on the connected nREPL server. The tool accepts:
#   {"code": "<clojure expression>"}
#
# Returns the evaluation result, stdout, stderr, and current ns.

(defn nrepl-eval-tool-handler [args]
  # args is the raw JSON string from the LLM, e.g. {"code": "(+ 1 2)"}
  (def code (json-extract-string args "code"))
  (if (or (nil? code) (= code ""))
    (string "nrepl_eval error: missing or empty \"code\" in args: " args)
    (if (not nrepl-connected)
      "nrepl_eval error: not connected to an nREPL server. Use /nrepl-connect first, or ensure the project has an .nrepl-port file."
      (try
        (do
          (def result (nrepl-eval code))
          (def out (result :out))
          (def err (result :err))
          (def value (result :result))
          (def ns (result :ns))
          (def repaired (result :repaired))
          (def parts @[])
          (when (not= out "")
            (array/push parts (string ";; stdout:\n" out)))
          (when (not= err "")
            (array/push parts (string ";; stderr:\n" err)))
          (when (not= value "")
            (array/push parts value))
          (when (not= ns "")
            (array/push parts (string ";; current ns: " ns)))
          (when (not= repaired nil)
            (array/push parts (string ";; note: auto-repaired unbalanced delimiters:\n;; " repaired)))
          (if (> (length parts) 0)
            (string/join parts "\n")
            "nil"))
        ([err]
         (string "nrepl_eval error: " err))))))

(harness/register-tool
  "nrepl_eval"
  (string
    "Evaluate a Clojure expression on the connected nREPL server. "
    "Use this to run Clojure/Script code, inspect vars, run tests, "
    "or explore a Clojure project's runtime state. The nREPL must be "
    "connected first via the /nrepl-connect slash command or by "
    "starting a Clojure REPL in the project (which writes .nrepl-port). "
    "Returns the evaluation result (value), stdout, stderr, and current ns.")
  "nREPL Eval"
  "{\"type\":\"object\",\"properties\":{\"code\":{\"type\":\"string\",\"description\":\"Clojure expression to evaluate\"}},\"required\":[\"code\"]}"
  "nrepl-eval-tool-handler"
  :parallel)
