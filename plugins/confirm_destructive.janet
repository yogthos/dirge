# Confirm-before-destructive-bash example
#
# Demonstrates (harness/confirm title question) — a blocking yes/no
# dialog the host renders inline in the chat. Janet pauses on the
# worker thread while waiting for the answer; the UI is free during.
#
# Use cases: gate dangerous bash commands, double-check force pushes,
# require explicit consent before touching credentials, etc.

(def hooks ["on-tool-start"])

(def danger-patterns ["rm -rf" "sudo " "git push --force" ":(){:|:&};:"])

(defn- danger? [command]
  (var hit false)
  (loop [p :in danger-patterns]
    (when (string/find p command) (set hit true)))
  hit)

(defn on-tool-start [ctx]
  # Only gate bash. args is a JSON string; we look for `"command":"..."`
  # via a lightweight scan — good enough for the built-in bash tool.
  (when (= (ctx :tool) "bash")
    (let [args (ctx :args)
          marker "\"command\""
          mp (string/find marker args)]
      (when mp
        (let [after (string/slice args (+ mp (length marker)))
              q1 (string/find "\"" after)]
          (when q1
            (let [rest (string/slice after (+ q1 1))
                  q2 (string/find "\"" rest)]
              (when q2
                (let [cmd (string/slice rest 0 q2)]
                  (when (danger? cmd)
                    (let [ok (harness/confirm
                               "danger"
                               (string "run '" cmd "'?"))]
                      (when (not ok)
                        (harness/block "user denied"))))))))))))
  nil)
