# dirge workflow plugin
# Drives architect → implementor → review phases automatically
# Inversion of control: harness drives the model, not vice versa

# Declare which hooks this plugin subscribes to. The `workflow-on-*`
# functions below are auto-aliased from these bare names (e.g.
# "on-init" → `workflow-on-init`). The earlier version of this
# plugin only listed four hooks, so `workflow-on-tool-end`,
# `workflow-on-error`, and `workflow-on-complete` were defined but
# never dispatched.
(def hooks ["on-init"
            "on-prompt"
            "on-response"
            "on-tool-start"
            "on-tool-end"
            "on-error"
            "on-complete"])

(var phase :idle)

(defn workflow-on-init [ctx]
  (harness/log (string "workflow loaded (model: " (ctx :model) ")"))
  (set phase :idle)
  nil)

(defn- detect-feature-request [prompt]
  (def patterns ["add a feature" "implement feature" "add support for"
                 "build a" "create a" "add " "feature request"
                 "new feature" "enhancement" "coding task"])
  (var found false)
  (loop [p :in patterns]
    (if (string/find p prompt) (set found true)))
  found)

(defn- architect-prompt [feature]
  (string
    "ARCHITECT MODE — Plan this feature step by step.\n\n"
    "1. First consider code layout — where should this code live?\n"
    "   - Is this a new module, an extension, or a refactor?\n"
    "   - What existing code will this interact with?\n\n"
    "2. Produce a high-level plan as a mermaidjs diagram\n"
    "   - Show the business logic flow\n"
    "   - Show component interactions\n\n"
    "3. Plan file structure (list files to create/modify)\n"
    "   - New files needed\n"
    "   - Existing files to modify\n\n"
    "4. Create function stubs and type signatures\n"
    "   - Interfaces first, implementations second\n\n"
    "5. Write PLAN.md with the full plan\n\n"
    "Feature: " feature))

(defn- implementor-prompt []
  (string
    "IMPLEMENTOR MODE — Follow TDD strictly.\n\n"
    "For each feature described in the plan:\n"
    "1. Write a FAILING test first\n"
    "2. Implement the minimal code to make it pass\n"
    "3. Run tests, verify they pass (green)\n"
    "4. Refactor if the code needs cleaning up\n"
    "5. Move to the next feature\n\n"
    "Do NOT write implementation without tests first.\n"
    "Follow the plan from the architect phase."))

(defn- review-prompt []
  (string
    "REVIEW MODE — Review all changes, find and fix bugs.\n\n"
    "1. Review ALL changes for correctness\n"
    "2. Check for bugs: edge cases, null/undefined handling,\n"
    "   error handling, resource leaks\n"
    "3. Run the full test suite and fix any failures\n"
    "4. Verify TDD was followed (tests exist for all new code)\n"
    "5. Check code style and consistency\n\n"
    "After review, fix any issues found."))

(defn workflow-on-prompt [ctx]
  (let [prompt (ctx :prompt)]
    (if (and (= phase :idle) (detect-feature-request prompt))
      (do
        (harness/log "workflow: feature request detected, entering architect phase")
        (set phase :architect)
        (harness/request-prompt (architect-prompt prompt))
        "Starting architect phase...")
      nil)))

(defn workflow-on-response [ctx]
  (case phase
    :architect
    (do
      (harness/log "workflow: architect complete, entering implementor phase")
      (set phase :implementor)
      (harness/request-prompt (implementor-prompt))
      "Starting implementor phase...")

    :implementor
    (do
      (harness/log "workflow: implementor complete, entering review phase")
      (set phase :review)
      (harness/request-prompt (review-prompt))
      "Starting review phase...")

    :review
    (do
      (harness/log "workflow: review complete, returning to idle")
      (set phase :idle)
      nil)

    nil))

(defn workflow-on-tool-start [ctx]
  (let [tool (ctx :tool)]
    (harness/log (string "tool: " tool))
    nil))

(defn workflow-on-tool-end [ctx]
  nil)

(defn workflow-on-error [ctx]
  (harness/log (string "error: " (ctx :error)))
  nil)

(defn workflow-on-complete [ctx]
  nil)
