# Response inspector example
#
# Demonstrates the `on-response` hook — the host calls it after every
# LLM response with `(ctx :response)` set to the full assistant
# message text. This hook is the natural place to:
#
#   1. Inspect the response for patterns and POST NOTIFICATIONS via
#      `harness/notify` (chat-visible, fire-and-forget).
#   2. Return a string to be APPENDED to the system prompt on the
#      next turn — useful for steering subsequent responses based on
#      patterns in this one (e.g. "the model is being terse, ask it
#      to be more thorough next time").
#   3. Inspect tool calls embedded in the response by piping through
#      `on-tool-end` (which fires PER tool call and is the right
#      place for `harness/replace-result` if you want to rewrite the
#      tool output the LLM sees).
#
# This example posts a notification when the response contains a
# code block, and gently nudges the model to add more comments if
# the previous response had unannotated code.

(def hooks [])

(var last-had-code-block false)

(defn response_inspector-on-response [ctx]
  (let [text (or (ctx :response) "")]
    # 1. Notification when a response includes a code block.
    (when (string/find "```" text)
      (harness/notify "agent included a code block" :info)
      (set last-had-code-block true))

    # 2. Steering string: if the prior response had code but no
    #    `;` or `//` comment lines, append a system-prompt hint for
    #    the next turn asking for more annotations.
    (if (and last-had-code-block
             (string/find "```" text)
             (not (or (string/find ";; " text)
                      (string/find "// " text)
                      (string/find "# " text))))
      (do
        (set last-had-code-block false)
        # The returned string is appended to the system prompt
        # injection for the next turn.
        (string "The previous response included code but no inline "
                "comments. When writing code, briefly annotate the "
                "*why* of non-obvious lines (one short comment per "
                "non-trivial block)."))
      (do
        (set last-had-code-block false)
        nil))))
