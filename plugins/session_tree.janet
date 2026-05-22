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

# Register the hook so dirge actually dispatches it. Bare names in
# this vector get auto-aliased to `<stem>-<hook>` (so this resolves
# to `session_tree-on-message-update`) — dirge's hook surface uses
# `on-message-update`, not `on-message`. The earlier version of this
# plugin declared `(def hooks [])` and defined `on-message`, so the
# function was loaded but never fired, leaving /label permanently
# stuck on "no entry yet".
(def hooks ["on-message-update"])

# Track the most-recent entry-id we've seen across on-message-update
# hooks so /label can attach to it without the user typing a uuid.
(var last-entry-id nil)

(defn on-message-update [ctx]
  # Plugin receives the in-progress turn id via ctx; we stash it
  # so the label command has something to target. The dirge hook
  # surface ships `:index` (turn ordinal) and `:partial`; if a
  # future host bumps the context to include `:id`, this picks it
  # up automatically.
  (when-let [id (or (get ctx :id) (get ctx :index))]
    (set last-entry-id (string id))))

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
