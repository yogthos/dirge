# Per-turn timing example
#
# Demonstrates the P3 streaming-hook events:
#   on-turn-start     — fires at the start of each LLM call cycle.
#   on-message-update — fires every ~16 streamed tokens with the
#                       accumulated turn text so far.
#   on-turn-end       — fires after the turn's tool results return
#                       (or after the final assistant message),
#                       carrying the full turn text.
#
# This plugin notifies the user how long each turn took, which is
# useful for debugging slow tool chains.

(def hooks ["on-turn-start" "on-turn-end"])

# Map of turn index -> start time (epoch ms).
(var turn-starts @{})

(defn on-turn-start [ctx]
  (let [idx (ctx :index)]
    (put turn-starts idx (os/time)))
  nil)

(defn on-turn-end [ctx]
  (let [idx (ctx :index)
        started (get turn-starts idx)
        msg-len (length (ctx :message))]
    (when started
      (let [elapsed (- (os/time) started)]
        (harness/notify
          (string "turn " idx " took " elapsed "s, "
                  msg-len " chars output")
          :info))
      (put turn-starts idx nil)))
  nil)
