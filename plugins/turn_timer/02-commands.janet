# Slash command for the `turn_timer` plugin. Loaded last (per
# alphabetical order) so the var refs resolve cleanly.

(defn timer-stats-handler [_args]
  (if (= turn-count 0)
    "no turns recorded yet"
    (string "turns: " turn-count
            " | total elapsed: " total-elapsed-ms "s"
            " | avg: " (/ total-elapsed-ms turn-count) "s")))

(harness/register-command "timer-stats" "timer-stats-handler")
