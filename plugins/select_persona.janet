# Picker dialog example
#
# Demonstrates (harness/select title [options]) — a blocking picker
# dialog. Returns the chosen option string, or nil on Escape.
#
# Registers a /persona command that asks the user which response style
# to apply, then rewrites the next user prompt to instruct the model
# accordingly via harness/replace-prompt.

(def hooks ["on-prompt"])

(var chosen-persona nil)

(defn pick-persona [_args]
  (let [picked (harness/select
                 "persona"
                 ["concise" "verbose" "socratic" "pirate"])]
    (set chosen-persona picked)
    (if picked
      (string "persona set to '" picked "'")
      "no persona selected (cancelled)")))

(harness/register-command "persona" "pick-persona")

(defn on-prompt [ctx]
  (when chosen-persona
    (harness/replace-prompt
      (string "[" chosen-persona " persona] " (ctx :prompt))))
  nil)
