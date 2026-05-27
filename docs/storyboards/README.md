# Storyboards

Narrative walkthroughs of user-facing flows in dirge. Each storyboard
describes a scenario step-by-step from the user's perspective and lists
the implementation files / functions involved. They serve two purposes:

1. **Onboarding**: a new contributor can read a storyboard and trace the
   code paths to learn how a feature works end-to-end.
2. **Verification**: when refactoring, walk through each storyboard
   against the current code to confirm the user-visible behavior still
   holds. Failures here indicate either a bug or a stale storyboard.

## Current storyboards

| File | Flow |
|---|---|
| [01-input-and-history.md](01-input-and-history.md) | Typing a message, history navigation, draft preservation |
| [02-permission-ask.md](02-permission-ask.md) | Permission prompt on a `bash` write — Allow once / Allow always / Deny |
| [03-background-task-notification.md](03-background-task-notification.md) | Background task completes; next turn sees a notification without leaking session state into the visible echo |
| [04-compress-with-focus.md](04-compress-with-focus.md) | `/compress <focus>` — Hermes-style focus-topic compaction |
| [05-session-resume-staleness.md](05-session-resume-staleness.md) | `dirge -c` resume with stale `working_dir` / `updated_at` warning |
| [06-webfetch-ssrf-defense.md](06-webfetch-ssrf-defense.md) | Agent tries to fetch a private URL — three-layer SSRF defense fires |
