# Storyboard 05 — Session resume with staleness warning

## Scenario

Last week the user was working in `~/src/api-gateway` on a refactor.
They quit dirge mid-task. Today they `cd ~/src/billing-service` and run
`dirge -c` (resume last session). The session WILL load, but the user
gets a loud stderr warning that:

1. The session's `working_dir` is `~/src/api-gateway` but cwd is
   `~/src/billing-service` — tool results captured against the OLD
   tree (file reads, git logs, bash output) may not match reality.
2. The session is 9 days old. Captured state has had time to drift.

## What the user sees

```
$ cd ~/src/billing-service
$ dirge -c

warning: resumed session was created in "/Users/yogthos/src/api-gateway",
current cwd is "/Users/yogthos/src/billing-service". Tool results
captured against the old tree may be stale.

warning: resumed session is 216 hours old. Captured tool results
(read/git/bash) may no longer reflect the current state of the
working tree.

[session log appears]
<dirge> Last session was about refactoring the JWT middleware.
        Let me know what you want to do next…
```

The agent loads normally; the warnings are informational. The user
can still issue prompts — but they know to verify any "I already
read file X" claims from the prior session.

## Code trace

### Step 1 — `dirge -c` resolves the most-recent session

- `src/main.rs` handles `--continue` / `-c` by calling
  `session::storage::find_recent_sessions(1)` and picking the
  newest. (The exact path varies — could also be an exact id via
  `--session <id>`.)
- `load_session` parses the JSON, runs schema migrations, and
  populates `session.loaded_mtime` for the concurrent-writer check.

### Step 2 — Staleness warning fires

- `src/main.rs` `warn_on_stale_resume(&session)`:
  ```rust
  fn warn_on_stale_resume(session: &session::Session) {
      let cwd = std::env::current_dir().ok();
      let session_wd = session.working_dir.as_str();
      if !session_wd.is_empty()
          && let Some(cwd) = &cwd
          && cwd.to_string_lossy() != session_wd
      {
          eprintln!(
              "warning: resumed session was created in {:?}, current cwd is {:?}. \
               Tool results captured against the old tree may be stale.",
              session_wd,
              cwd.display().to_string(),
          );
      }
      if let Ok(updated) = chrono::DateTime::parse_from_rfc3339(session.updated_at.as_str()) {
          let age = chrono::Utc::now().signed_duration_since(updated.with_timezone(&chrono::Utc));
          if age.num_hours() >= 24 {
              eprintln!(
                  "warning: resumed session is {} hours old. …",
                  age.num_hours(),
              );
          }
      }
  }
  ```
- Both warnings are independent — either or both can fire.
- Threshold is 24 hours. Anything fresher is silent (typical
  daily-resume workflow shouldn't be noisy).

### Step 3 — Normal session render proceeds

- `render_session` (`src/ui/events.rs:30`) prints the banner, the
  compaction count (if any), and walks `session.messages` printing
  each as `<you>` / `<dirge>` / `<sys>`.
- The user can immediately start typing.

## Cross-references

- **SESS-8** — warning on stale resume: `src/main.rs::warn_on_stale_resume`
  (added this round; modeled on Hermes's session-staleness check
  but adapted for dirge's simpler session model).
- **SESS-15** — `loaded_from_newer_version` flag refuses save on
  downgrade. NOT used here (this is upgrade-or-equal version, not
  downgrade), but worth knowing the related guard exists.

## Edge cases verified

- **First-time use (`-c` with no prior session)**:
  `find_recent_sessions(1)` returns empty → no resume happens →
  `warn_on_stale_resume` is never called.
- **Same cwd resume**: `cwd.to_string_lossy() != session_wd` is
  false → no working-dir warning. The age warning may still fire.
- **Sub-24h age**: `age.num_hours() < 24` → no age warning. Working-
  dir mismatch can still fire independently.
- **Corrupted `updated_at`**: `parse_from_rfc3339` returns `Err`
  → the age warning is silently skipped (defensive — a malformed
  timestamp shouldn't be a hard failure).
- **Empty `working_dir`** (very old session pre-dating the field):
  `!session_wd.is_empty()` gate skips the comparison.
