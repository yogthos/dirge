# Hook definitions for the `turn_timer` plugin. References the vars
# from 00-state.janet — the same Janet env is shared across both
# files because they're in the same plugin directory.
#
# The host aliases bare `on-turn-start` / `on-turn-end` to
# `turn_timer-on-turn-start` / `turn_timer-on-turn-end` after load so
# these hooks survive other plugins also defining bare names.

(defn on-turn-start [ctx]
  (set turn-start-ms (os/time)))

(defn on-turn-end [ctx]
  (let [elapsed (- (os/time) turn-start-ms)]
    (set total-elapsed-ms (+ total-elapsed-ms elapsed))
    (set turn-count (+ turn-count 1))
    (harness/notify
      (string "turn " (get ctx :index) " took " elapsed "s (running total: "
              total-elapsed-ms "s across " turn-count " turns)")
      :info)))
