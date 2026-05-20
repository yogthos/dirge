# Session-tree control example (P4d)
#
# Demonstrates the harness/* APIs that mutate the session tree from
# Janet code. Mirrors pi's ctx.setLabel / ctx.fork / ctx.navigateTree /
# ctx.newSession / ctx.switchSession.
#
# All five APIs are queued; the host drains and applies them between
# UI events. No return value — verify side effects via /tree.
#
# Usage:
#   /label <text>     — bookmark the most recent entry on the current
#                        branch. Pass no text to clear it.
#   /fresh            — persist the current session and start a new
#                        one in place. Keeps the model/provider.

(def hooks [])

# Track the most-recent entry-id we've seen across on-message hooks
# so /label can attach to it without the user having to type a uuid.
(var last-entry-id nil)

(defn on-message [ctx]
  # Plugin receives the just-recorded entry id via ctx — we stash it
  # so the label command has something to target. (If your harness
  # passes ids differently, adapt this getter.)
  (when-let [id (get ctx :id)]
    (when (string? id)
      (set last-entry-id id))))

(defn label-handler [args]
  (cond
    (nil? last-entry-id)
      "no entry yet — send a message first"
    (= (length args) 0)
      (do
        (harness/set-label last-entry-id nil)
        "cleared label")
    (do
      (harness/set-label last-entry-id args)
      (string "labeled \"" args "\""))))

(defn fresh-handler [_args]
  # newSession preserves the model + provider; the host wipes
  # messages, tree, and store, then reloads the chat view.
  (harness/new-session)
  "starting fresh session")

(harness/register-command "label" "label-handler")
(harness/register-command "fresh" "fresh-handler")
