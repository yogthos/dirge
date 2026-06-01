# Backpressured plugin — slash commands.
#
# /backpressured [goal]    — engage the loop (optionally kick off a goal)
# /backpressured-stop      — disengage
# /backpressured-status    — show state + detected project checks

(defn backpressured-cmd [args]
  (set bp-active true)
  (def goal (string/trim args))
  (if (= goal "")
    (string "▶ backpressured loop engaged.\n"
            "Describe your goal and I'll drive it through plan → implement "
            "→ verify → ship, with checks every iteration.")
    (do
      # Best-effort kickoff: queue the goal as the next turn. If the host
      # doesn't pick it up from a command, the loop is still engaged —
      # just send the goal as your next message.
      (harness/request-prompt goal)
      "▶ backpressured loop engaged — driving the goal now.")))

(defn backpressured-stop-cmd [_args]
  (set bp-active false)
  "backpressured loop disengaged.")

(defn backpressured-status-cmd [_args]
  (string "backpressured: " (if bp-active "ENGAGED" "off") "\n\n"
          (bp-detect-checks)))

(harness/register-command "backpressured" "backpressured-cmd")
(harness/register-command "backpressured-stop" "backpressured-stop-cmd")
(harness/register-command "backpressured-status" "backpressured-status-cmd")
