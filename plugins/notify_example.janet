# Plugin notify example
#
# Demonstrates (harness/notify msg [level]) — fire-and-forget messages
# the host displays as colored chat lines. Levels: :info (default,
# grey), :warn (yellow), :error (red).

(def hooks ["on-init" "on-tool-end"])

(defn on-init [ctx]
  (harness/notify (string "notify_example loaded; cwd=" (ctx :cwd)) :info)
  nil)

(defn on-tool-end [ctx]
  # Warn the user whenever a tool reports a non-zero exit code or other
  # failure marker in its output. Cheap heuristic; refine for your case.
  (let [output (ctx :output)]
    (when (and output
               (or (string/find "error:" output)
                   (string/find "FAILED" output)))
      (harness/notify
        (string "tool '" (ctx :tool) "' reported an error")
        :warn)))
  nil)
