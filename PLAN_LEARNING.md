# Learning Loop — Per-Project Memory, Skills, and Self-Improvement

Port Hermes-agent's four-layer learning architecture to dirge, adapted for
the coding context. The key difference: Hermes stores memory globally in
`~/.hermes/`; dirge stores it **per-project** in `.dirge/` at the project
root — the directory where `.git/` lives. If you're working in `~/src/foo`,
everything lives under `~/src/foo/.dirge/`. If you're working in
`~/src/bar`, everything lives under `~/src/bar/.dirge/`. Each project gets
its own independent memory, skills, and session history.

## Architecture Overview

Hermes has four layers that work together. We port all of them, adapted:

```
┌─────────────────────────────────────────────────┐
│ Layer 4: Curator (periodic skill maintenance)    │
│   agent/curator.py — lifecycle, consolidation    │
├─────────────────────────────────────────────────┤
│ Layer 3: Skill System (procedural memory)        │
│   tools/skill_manager_tool.py — CRUD + patches   │
│   tools/skill_usage.py — telemetry + provenance  │
├─────────────────────────────────────────────────┤
│ Layer 2: Memory Store (declarative memory)       │
│   tools/memory_tool.py — MEMORY.md, USER.md      │
├─────────────────────────────────────────────────┤
│ Layer 1: Background Review (the learning nudge)  │
│   agent/background_review.py — fork + evaluate   │
├─────────────────────────────────────────────────┤
│ Foundation: Session DB + Search + Compression    │
│   hermes_state.py — SQLite + FTS5                │
│   tools/session_search_tool.py — find past work  │
│   agent/context_compressor.py — long sessions    │
└─────────────────────────────────────────────────┘
```

### Per-Project Storage Layout

```
.dirge/                          # Project root (cf. ~/.hermes/)
├── memory/
│   ├── MEMORY.md                # Project facts, conventions, architecture
│   └── PITFALLS.md              # Things tried and failed (anti-patterns)
├── skills/                      # Procedural knowledge for this project
│   ├── .usage.json              # Telemetry sidecar (skill_usage.py analog)
│   ├── .curator_state           # Curator scheduler state
│   ├── project-build/
│   │   └── SKILL.md             # How to build, test, lint
│   ├── project-architecture/
│   │   ├── SKILL.md             # Module map, invariants, key patterns
│   │   └── references/
│   │       └── dependency-graph.md
│   └── .archive/                # Curator-archived skills (never deleted)
├── sessions/
│   └── state.db                 # SQLite sessions with FTS5
└── config.yaml                  # Optional: per-project dirge config
```

---

## Phase 0 — Per-Project `.dirge/` Storage Infrastructure and Review

**Goal**: establish the `.dirge/` directory as the per-project equivalent
of `~/.hermes/`. Every other phase writes into this tree.

**Reference**: `hermes_constants.py` — `get_hermes_home()` pattern.
Simple: resolve `.dirge/` relative to the project root (where `.git/` lives,
or explicit override via `DIRGE_PROJECT_ROOT` env var).

**Files**:
- `src/extras/memory.rs` — extend `ProjectPaths` or equivalent that resolves
  `.dirge/` from project root
- Helper: `dirge_project_root()` → `PathBuf`

**Design decisions**:
- Project root is detected by walking up from CWD until `.git/` is found
  (same as `get_hermes_home()` walks up to find `.hermes/` markers)
- `DIRGE_PROJECT_ROOT` env var overrides auto-detection
- All `.dirge/` subdirectories are created lazily on first write

**Tests**: project root detection, env var override, lazy directory creation.

**Risk**: zero. Pure path resolution.

We need to review current features like memories to identified ones that would be
logically replaced by this system and flag anything that would be duplicate for
removal after. If the features serve a different purpose then keep them. If they
conflict or duplicate functionality we are introducing then remove or merge functionality
as is sensible.

---

## Phase 1 — SQLite Session Database with FTS5

**Goal**: persist every session transcript in SQLite with full-text search.
This is the foundation that session search (Phase 5) and background review
(Phase 4) depend on. Session transcripts live in the project folder the session is associated with.

**Reference**: `hermes_state.py` (full file, 1174 lines). Key sections:
- Lines 1-15: module docstring — design decisions (WAL mode, FTS5, session
  splitting, source tagging)
- Lines 38-58: `_WAL_INCOMPAT_MARKERS` and WAL fallback logic
- Lines 77-125: `_last_init_error` thread-safe error tracking, `format_session_db_unavailable`
- Lines 128-150: `apply_wal_with_fallback()` — tries WAL, falls back to DELETE on NFS/SMB
- Schema: sessions table + messages table + FTS5 virtual table
  - `sessions`: id, parent_session_id, source, model, provider, started_at,
    last_active, title, message_count, input_tokens, output_tokens, etc.
  - `messages`: id, session_id, role, content, tool_name, tool_calls,
    tool_call_id, timestamp
  - `messages_fts`: FTS5 content table over messages.content
- Session splitting on compression: `parent_session_id` chains
- Source tagging: "cli", "subagent", "review-fork", etc.
- `get_anchored_view()` — FTS5 match + window + bookends (used by session_search)
- `get_messages_around()` — anchored scroll window (used by session_search scroll mode)
- `search_messages()` — FTS5 query with role filter, source exclusion, sort
- `list_sessions_rich()` — recent sessions with metadata

**What to port faithfully**:
1. **Schema**: exact same table structure. The FTS5 content sync triggers are
   critical — they auto-update the FTS index on message insert/update/delete.
2. **WAL mode with fallback**: try `PRAGMA journal_mode=WAL`, fall back to
   DELETE with a single WARNING logged (deduplicated per db path). This is
   load-bearing for concurrent access (background review fork + main session).
3. **Session splitting**: when context compression happens (Phase 7), the
   session splits. New session gets `parent_session_id` pointing to old.
   Session search deduplicates by lineage root.
4. **Source tagging**: every session gets a `source` field. "cli" for
   interactive, "subagent" for delegate_task, "review-fork" for background
   review. Session search excludes "review-fork" by default.
5. **Thread safety**: `rusqlite` with `SQLITE_OPEN_FULL_MUTEX` or connection
   pooling. The background review fork writes to the same DB.
6. **Error surface**: `SessionDb::open()` returns `Result<Self, Error>` with
   rich context (like `_last_init_error`). Callers get actionable messages
   ("state.db may be on NFS/SMB").

**Files**:
- `src/extras/session_db.rs` — `SessionDb` struct, schema migration,
  `insert_session`, `insert_message`, `search_messages`, `get_anchored_view`,
  `get_messages_around`, `list_sessions_rich`

**Schema versioning**: like Hermes's `SCHEMA_VERSION = 13`, include a
`schema_version` pragma. On open, if version < current, run migrations.
This lets us evolve the schema across dirge versions without breaking
existing `.dirge/sessions/state.db` files.

**Tests**:
- Session create → read back matches
- Message insert → FTS5 search finds it
- Session split → `parent_session_id` chain
- WAL fallback on tempfs (or mock)
- Concurrent read/write (two connections to same DB)
- Schema migration from v0 → vN

**Risk**: medium. SQLite in Rust via `rusqlite` with `bundled` feature is
well-trodden. The FTS5 content sync triggers are the trickiest part — must
match Hermes's exact trigger definitions.

---

## Phase 2 — Per-Project Memory Store

**Goal**: bounded, file-backed declarative memory for the project. Two stores:
`MEMORY.md` (project facts) and `PITFALLS.md` (anti-patterns). The agent
reads these at session start and can write to them during the session.

**Reference**: `tools/memory_tool.py` (full file, 690 lines). Key sections:
- Lines 1-24: module docstring — § delimiter, char limits, frozen snapshot pattern
- Lines 56-60: `get_memory_dir()` — profile-aware path resolution
- Lines 68-106: `_MEMORY_THREAT_PATTERNS` + `_scan_memory_content()` —
  injection/exfiltration scanning
- Lines 108-136: `_drift_error()` — external modification detection
- Lines 138-155: `MemoryStore.__init__()` — two parallel states (live + frozen snapshot)
- Lines 157-173: `load_from_disk()` — reads files, captures snapshot, deduplicates
- Lines 175-210: `_file_lock()` — `fcntl.flock` / `msvcrt.locking` with `.lock` file
- Lines 219-235: `_reload_target()` — re-read under lock, detect external drift
- Lines 264-312: `add()` — append entry, char budget check, injection scan, duplicate reject
- Lines 314-372: `replace()` — substring match, ambiguity detection, budget check
- Lines 374-408: `remove()` — substring match with ambiguity detection
- Lines 410-459: `format_for_system_prompt()` — returns frozen snapshot, never live state
- Lines 460-480: `_read_file()` — § split, handles missing/corrupt files
- Lines 482-535: `_detect_external_drift()` — two signal detection (round-trip mismatch
  + entry-size overflow)
- Lines 537-567: `_write_file()` — atomic tempfile + rename (NOT truncate+flock)
- Lines 619-668: `MEMORY_SCHEMA` — the tool description that trains the model WHEN to save

**Key design decisions to port faithfully**:

1. **§ delimiter** (ENTRY_DELIMITER = `"\n§\n"`). Entries are separated by
   this exact sequence. Content within entries can contain "§" characters
   — the delimiter is the full `\n§\n` sequence. Do NOT use a simpler
   delimiter; it will break on real content.

2. **Frozen snapshot at session start**. On `load_from_disk()`, the store
   captures a snapshot of what the files looked like. This snapshot goes into
   the system prompt and NEVER changes mid-session. Mid-session writes go to
   disk immediately but don't touch the prompt. This preserves the LLM's
   prefix cache across all turns. THE LIVE STATE AND THE SNAPSHOT ARE TWO
   DIFFERENT THINGS. Tool responses always reflect live state; system prompt
   always reflects frozen snapshot.

3. **Char limits, not token limits**. Hermes uses `memory_char_limit = 2200`
   and `user_char_limit = 1375`. Dirge should use similar bounds (slightly
   larger since project context is broader). Char counts are model-independent
   and avoid the complexity of token estimation.

4. **File locking**. `fcntl.flock(LOCK_EX)` on a separate `.lock` file.
   The memory file itself is atomically replaced via `tempfile.mkstemp` +
   `os.replace`, so readers always see a complete file. The lock only
   serializes writers. On platforms without `fcntl` (Windows), use `msvcrt`.

5. **Injection scanning**. Before accepting any new content, scan for:
   - Invisible unicode characters (zero-width spaces, bidi markers)
   - Prompt injection patterns ("ignore previous instructions", "you are now")
   - Exfiltration patterns (curl with secrets, cat of .env files)
   - SSH backdoor patterns
   This matters because memory content is injected into the system prompt.

6. **Drift detection**. Before mutating, re-read the file under lock and
   verify the on-disk content round-trips through the parser/serializer.
   If an external tool (manual edit, patch tool, concurrent session) wrote
   content that doesn't round-trip, snapshot the file to `.bak.<ts>` and
   REFUSE the mutation. This prevents silent data loss (Hermes issue #26045).

7. **Atomic writes**. Write to tempfile in same directory, `fsync`, then
   `rename()` over the target. Never `open("w")` + truncate — that creates
   a race where readers see an empty file. Previous Hermes implementation
   had this bug; the fix is in `_write_file()` at lines 537-567.

8. **Substring matching for replace/remove**. No IDs. The agent provides an
   `old_text` substring. The store finds the entry containing it. If multiple
   entries match with different content, return error with previews. If
   multiple matches with identical content (duplicates), operate on the first.

9. **Deduplication on load**. `load_from_disk()` runs `list(dict.fromkeys(entries))`
   before capturing the snapshot. This means loading `["a", "a", "b"]` produces
   `["a", "b"]`. The dedup only affects the in-memory representation; the file
   on disk keeps whatever was written.

**Dirge-specific adaptations**:

- **MEMORY.md** stores project facts: build commands, test runners, project
  conventions, architecture patterns, library quirks, discovered invariants.
  Hermes's MEMORY.md stores environment facts and tool quirks; dirge's stores
  codebase-specific knowledge.

- **PITFALLS.md** (new, dirge-only) stores anti-patterns: "don't use async
  in this module because X", "the mock for Y breaks if you Z", "tests in
  this directory need the `--feature=foo` flag". This is critical for a
  coding agent — knowing what NOT to do is as valuable as knowing what to do.

- No USER.md. Hermes tracks user preferences globally; dirge stores user
  preferences for this specific project in MEMORY.md under a "user preferences"
  entry.

- The system prompt injection for memory should appear in dirge's AGENTS.md
  or the system prompt template, clearly fenced.

**Files**:
- `src/extras/memory_store.rs` — `MemoryStore` struct with `load_from_disk`,
  `add`, `replace`, `remove`, `format_for_system_prompt`, `entries_for`
- `src/agent/tools/memory_tool.rs` — tool registration with the schema

**Tests**:
- Add entry → read back → entry present
- Duplicate add → rejected
- Char limit exceeded → rejected with usage stats
- Replace by substring → content updated
- Ambiguous replace (multiple different matches) → error with previews
- Remove → entry gone
- Frozen snapshot unchanged after mid-session write
- Injection scan blocks dangerous content
- Drift detection catches external modification
- Concurrent write (two stores to same file) → lock serializes
- File corruption recovery → graceful degradation

**Risk**: low for the store logic (well-specified by Hermes). Medium for
getting the Rust ownership right (frozen snapshot vs live state borrows).

---

## Phase 3 — Skill System for Coding Patterns

**Goal**: procedural memory for the project. Skills capture "how to do this
class of task for this specific codebase." The agent creates and improves
them through experience.

**Reference**:
- `tools/skill_manager_tool.py` (1034 lines) — CRUD operations
- `tools/skill_usage.py` (608 lines) — telemetry + lifecycle
- `tools/skill_provenance.py` — distinguishes shipped from agent-created
- `tools/fuzzy_match.py` — fuzzy matching for patches
- `tools/patch_parser.py` — patch file parsing
- `tools/skills_guard.py` — security scanning of skills code
- `skills/software-development/test-driven-development/SKILL.md` — example skill format

### Skill Format (from Hermes)

```yaml
---
name: project-build
description: "Build and test commands for this specific project"
version: 1.0.0
metadata:
  dirge:
    tags: [build, test, rust, cargo]
    related_skills: [project-architecture]
---

# Project Build

## Overview
...

## Build Commands
...

## Test Commands
...

## Pitfalls
...
```

### CRUD Operations (port from `skill_manager_tool.py`)

1. **create**: validate name (lowercase, hyphens, max 64 chars), validate
   frontmatter (YAML with required `name` + `description`), validate content
   size (max 100K chars ≈ 36K tokens), check for name collisions, create
   skill directory, write SKILL.md atomically, run security scan, roll back
   on scan failure.

2. **edit**: full SKILL.md rewrite. Back up original, write new, validate
   frontmatter, roll back on scan failure.

3. **patch**: targeted find-and-replace within SKILL.md or supporting file.
   Uses `fuzzy_match.py` — handles whitespace normalization, indentation
   differences, escape sequences, block-anchor matching. Returns match count.
   Validates frontmatter integrity after patch. Rolls back on scan failure.

4. **delete**: remove skill directory. Check pinned guard (pinned skills
   refuse deletion). Support `absorbed_into` parameter for consolidation
   intent. Clean up empty category directories.

5. **write_file**: add/overwrite supporting file under `references/`,
   `templates/`, `scripts/`, or `assets/`. Validate path traversal prevention.
   Back up original, write new, roll back on scan failure.

6. **remove_file**: remove a supporting file. Clean up empty subdirectories.

### Usage Tracking (port from `skill_usage.py`)

A `.usage.json` sidecar file in the skills directory, keyed by skill name.
Each record tracks:
- `created_by`: "agent" or null (only agent-created skills are curator-managed)
- `use_count`, `view_count`, `patch_count`: activity counters
- `last_used_at`, `last_viewed_at`, `last_patched_at`: ISO timestamps
- `created_at`: when the record was first created
- `state`: "active", "stale", or "archived"
- `pinned`: boolean (pinned skills are exempt from lifecycle transitions)
- `archived_at`: when the skill was archived

Key design decisions:
- **Sidecar, not frontmatter**. Keeps operational telemetry out of user-authored
  SKILL.md content. Uses atomic writes via tempfile + os.replace.
- **Provenance filter**: only skills with `created_by == "agent"` are eligible
  for curator management. Bundled/shipped skills are tracked for usage stats
  but never auto-archived.
- **File locking**: `fcntl.flock` on `.usage.json.lock` for read-modify-write
  safety across concurrent processes.
- **All counter bumps are best-effort**: failures log at DEBUG and return
  silently. A broken sidecar never breaks the underlying tool call.

### Provenance (port from `skill_provenance.py`)

Two tiers:
1. **Bundled skills** (shipped with dirge): `project-build`, `project-architecture`,
   `project-conventions`. These are seeded into `.dirge/skills/` on first use.
   Never auto-archived, never deleted by curator. Can be patched by the agent
   when they become outdated.

2. **Agent-created skills**: created by the background review fork or explicitly
   by the agent. These are curator-managed: they can be marked stale, archived,
   or consolidated. Marked by `created_by: "agent"` in `.usage.json`.

The bundled manifest (Hermes's `.bundled_manifest`) can be a simple file listing
the names of shipped skills. Skills whose names appear in this list are bundled.

### Fuzzy Matching for Patches (port from `tools/fuzzy_match.py`)

The `fuzzy_find_and_replace()` function handles:
- Whitespace normalization (tabs ↔ spaces, trailing whitespace)
- Indentation differences (varying leading whitespace depth)
- Escape sequences (JSON-escaped strings vs literal)
- Block-anchor matching (when the target string is ambiguous, surrounding
  context lines disambiguate)

This is critical because LLM-generated `old_string` values often have minor
formatting mismatches with the actual file content. Without fuzzy matching,
patch operations fail frequently and the agent has to retry.

**Dirge-specific adaptations**:

- Skills are stored in `.dirge/skills/` (per-project), not `~/.hermes/skills/`.
- Optionally load from `~/.dirge/skills/` for cross-project patterns that
  apply everywhere (e.g., "rust-best-practices", "git-workflow").
- The built-in skills (`project-build`, `project-architecture`,
  `project-conventions`) are seeded with sensible defaults but designed to
  be patched/improved by the agent as it learns.
- The skill loading mechanism should look in `.dirge/skills/` first, then
  fall back to `~/.dirge/skills/` for global skills, then to built-in
  defaults shipped in the binary.

**Files**:
- `src/extras/skills/mod.rs` — skill discovery, loading, format validation
- `src/extras/skills/manager.rs` — CRUD operations (`create`, `edit`, `patch`,
  `delete`, `write_file`, `remove_file`)
- `src/extras/skills/usage.rs` — telemetry sidecar (`.usage.json`)
- `src/extras/skills/provenance.rs` — bundled vs agent-created distinction
- `src/extras/skills/fuzzy_match.rs` — fuzzy find-and-replace
- `src/extras/skills/guard.rs` — security scanning (code injection detection)
- `src/agent/tools/skill_tools.rs` — tool registration (`skill_manage`,
  `skill_view`, `skills_list`)

**Tests**:
- Create skill → read back → content matches
- Patch skill with fuzzy matching → handles whitespace differences
- Delete skill → directory removed, usage record dropped
- Agent-created skill tracked in usage → curator-eligible
- Bundled skill tracked in usage → not curator-eligible
- Pinned skill refuses delete but accepts patch
- Archive → skill moves to `.archive/`, state set to "archived"
- Restore → skill moves back, state set to "active"
- Concurrent usage writes → lock serializes
- Security scan blocks malicious script in skill

**Risk**: medium for the full CRUD + fuzzy matching. Low for the basic
operations. The fuzzy matching port needs careful attention — it's the
most subtle part.

---

## Phase 4 — Background Review at Session End

**Goal**: after every session, fork the agent with limited tools (memory +
skill only) and ask it to evaluate what was learned about the project. This
is the intake valve — the mechanism by which the agent's experience becomes
persistent knowledge.

**Reference**: `agent/background_review.py` (593 lines). Key sections:
- Lines 1-17: module docstring — daemon thread, forked AIAgent, tool whitelist
- Lines 34-43: `_MEMORY_REVIEW_PROMPT` — what to save to memory
- Lines 45-148: `_SKILL_REVIEW_PROMPT` — the detailed skill review prompt
  (class-level skills, preference order for updates, signals to look for)
- Lines 150-158: `_COMBINED_REVIEW_PROMPT` — reviews both memory and skills
  in one pass
- Lines 160-593: `spawn_background_review()` — the fork-and-review logic

### Prerequisite

Need to update configuration system to support a separate model for this in config.json

currently we have
{ 
  "provider": "deepseek",
  "model": "deepseek-v4-pro",
  ...
}

we'll want something like

currently we have
{ 
  "providers": 
  {
   "deepseek" {
      "base_url": <optional>
      "model": "deepseek-v4-pro"
   },
   "glm" {
      "nase+url": <optional>
      "model": "glm-5.1"
   }   
  },
  "provider": "deepseek"
  "review_provider": "glm"  
  ...
}

### How Hermes's Background Review Works

1. After every turn, `run_conversation()` calls `spawn_background_review()`.
2. A daemon thread forks the agent: same provider (if no review_provider specified), model, base_url, credentials,
   and cached system prompt (so it hits the same prefix cache).
3. The fork's tool set is limited to `memory` + `skill_manage` + `skills_list`
   + `skill_view`. Everything else is denied at runtime.
4. The fork receives a snapshot of the conversation and one of the review
   prompts as its user message.
5. The fork runs autonomously — writes go directly to memory and skill stores.
6. The main conversation continues unaffected. The daemon thread runs to
   completion or is abandoned on process exit.

### Dirge Adaptation

Hermes runs this after EVERY turn. For a coding agent, that's too aggressive
— most turns are "read file, write code, run test" and don't produce new
architectural insights. Dirge should run background review:

1. **At session end** (always): review the full session transcript.
2. **After significant milestones** (optional, configurable): successful build
   after a series of failures, a bug fix, a new test suite passing.
3. Significant task completion such as implementing a feature   

The review prompts need to be coding-specific:

```
REVIEW_PROMPT (coding context):

Review the session above and consider what we learned about this project.

**MEMORY**: project facts, conventions, architecture.
- What build/test commands were discovered or confirmed?
- What naming conventions, file layout patterns, or import styles were used?
- What architecture patterns emerged (how modules relate, DI approach,
  error handling style)?
- What library quirks or tool behaviors were discovered?
- Were there any user corrections about how things should be done?

**PITFALLS**: anti-patterns and things to avoid.
- Was something tried and failed? Capture what was attempted and WHY it failed.
- Were there environment-specific issues that need documentation?
- Were there test fixtures or mocks that behaved unexpectedly?

**SKILLS**: procedural improvements.
- Did a skill that was loaded turn out wrong, outdated, or missing steps?
  PATCH IT NOW.
- Did a non-trivial technique, workaround, or debugging path emerge?
- Did the user correct your style, approach, or workflow? Embed the lesson.

Preference order for skills:
1. UPDATE a currently-loaded skill (the one in play)
2. UPDATE an existing umbrella skill
3. ADD a support file under an existing umbrella
4. CREATE a new class-level skill

"Nothing to save." is valid but should not be the default. Most coding
sessions produce at least one learning.
```

### Implementation

Key design decisions from Hermes to keep:
1. **Fork, don't inline**. The review runs in a separate agent instance so
   it never touches the main conversation's prompt cache or tool state.
2. **Tool whitelist**. The fork only gets `memory` + `skill_manage` +
   `skill_view` + `skills_list`. No file tools, no shell, no browser.
3. **Same credentials**. The fork inherits the parent's provider/model/auth
   so it works without additional configuration.
4. **Daemon thread** (or `tokio::spawn`). Fire-and-forget; don't block the
   main session. If the fork fails, log and move on.
5. **Frozen conversation snapshot**. The fork receives a copy of the session
   messages, not a live reference. The main session may continue while the
   fork is running.

**Files**:
- `src/agent/review.rs` — `spawn_background_review()` function
- `src/agent/runner.rs` — call `spawn_background_review()` at session end

**Tests**:
- Review fork creates a memory entry from session content
- Review fork patches a skill that was loaded during the session
- Review fork creates a new skill from a discovered workflow
- Tool whitelist enforcement: fork cannot call file/shell tools
- Fork failure does not affect main session
- Concurrent review + main session writes to memory → lock serializes

**Risk**: medium. The fork mechanism needs a lightweight agent instance that
can make LLM calls independently. Dirge's current architecture with rig may
need adaptation to support forked agents. This phase depends on the agent
loop port (PLAN.md) being complete enough to create independent agent
instances.

---

## Phase 5 — Session Search Tool

**Goal**: give the agent the ability to search its own past sessions on this
project. "How did we solve the database migration issue last month?" becomes
answerable without consuming the entire past transcript.

**Reference**: `tools/session_search_tool.py` (602 lines). Key sections:
- Lines 1-30: module docstring — three shape design (discovery, scroll, browse)
- Lines 42-64: `_format_timestamp()` — human-readable timestamps
- Lines 67-86: `_resolve_to_parent()` — walk `parent_session_id` chain to root
- Lines 89-107: `_shape_message()` — slim message for tool response
- Lines 110-150: `_list_recent_sessions()` — browse shape
- Lines 153-274: `_scroll()` — anchored window with lineage rebinding
- Lines 277-375: `_discover()` — FTS5 search with dedup + bookends + window
- Lines 378-450: `session_search()` — main entry point, mode inference
- Lines 462-579: `SESSION_SEARCH_SCHEMA` — tool description

### Three Calling Shapes

1. **DISCOVERY** — pass `query`. Runs FTS5, dedupes hits by session lineage
   (same compression chain → one result), returns top N sessions each with:
   - `snippet`: FTS5-highlighted match excerpt
   - `bookend_start`: first 3 user+assistant messages (the goal/kickoff)
   - `messages`: ±5 messages around the FTS5 match, anchor flagged
   - `bookend_end`: last 3 user+assistant messages (resolution/decisions)
   - `match_message_id`, `messages_before`, `messages_after`

2. **SCROLL** — pass `session_id` + `around_message_id`. Returns a window of
   ±N messages centered on the anchor. No FTS5, no bookends. To scroll
   forward, re-anchor on the last message id in the window. To scroll
   backward, re-anchor on the first. Boundary overlap provides orientation.

3. **BROWSE** — no args. Returns recent sessions chronologically with titles,
   previews, timestamps. Use when the user asks "what was I working on."

### Key Design Decisions to Port Faithfully

1. **No LLM cost**. All three shapes are pure DB queries. No summarization,
   no embedding search, no LLM calls. This is important because session
   search is used DURING a session (it's a tool the agent calls), and
   every token counts.

2. **Lineage deduplication**. When sessions split due to compression (Phase 7),
   the new session has a `parent_session_id` pointing to the old one. Search
   deduplicates by lineage root — `_resolve_to_parent()` walks the chain to
   the original session. Without this, a 5-compression session shows up as
   6 separate search results.

3. **Lineage rebinding in scroll**. If the caller passes a `session_id` that's
   a parent, but the `around_message_id` actually lives in a child session
   (because the message was created after compression split), the scroll
   handler silently rebinds to the child session. This is a usability fix —
   without it, the agent gets "message not found" and has to retry.

4. **Source exclusion**. Sessions with `source = "review-fork"` are excluded
   from browse and search by default. These are background review runs and
   are noise for the user.

5. **Current session exclusion**. The active session's lineage is excluded
   from search results — those messages are already in context.

6. **FTS5 syntax**: AND is the default, `OR` for broader recall, quoted
   phrases for exact match, `NOT` for exclusion, `*` for prefix wildcards.

**Dirge-specific adaptations**:

- The tool is called `session_search` matching Hermes's name.
- Session metadata (model, provider) is less relevant for dirge — focus on
  what was done, what files were touched, what decisions were made.
- The tool description should emphasize coding-specific use cases: "how did
  we handle the database migration", "what was the fix for the test flake",
  "where did we leave the authentication refactor."

**Files**:
- `src/extras/session_search.rs` — `session_search()` function with three
  shapes, FTS5 query building, lineage resolution
- `src/agent/tools/session_search_tool.rs` — tool registration

**Tests**:
- Discovery: FTS5 search returns sessions with bookends and window
- Discovery: deduplication by lineage (same root → one result)
- Scroll: anchored window returns correct messages
- Scroll: lineage rebinding (parent session_id + child message id)
- Browse: recent sessions exclude review-fork sources
- Current session excluded from results
- FTS5 boolean syntax: AND, OR, NOT, quoted phrases

**Risk**: medium. Depends on Phase 1 (session DB). The FTS5 query construction
is the trickiest part — FTS5 has its own SQL syntax that differs from
standard SQLite.

---

## Phase 6 — Curator (Background Skill Maintenance)

**Goal**: periodically review and maintain agent-created skills. Transition
stale skills to archive, consolidate overlapping skills, keep the skill
library healthy.

**Reference**: `agent/curator.py` (1224 lines). Key sections:
- Lines 1-20: module docstring — responsibilities, strict invariants
- Lines 56-59: default config values (7 day interval, 2h idle, 30d stale, 90d archive)
- Lines 62-125: `.curator_state` — persistent scheduler state (JSON file)
- Lines 128-183: config access — `curator.*` from config.yaml
- Lines 186-249: idle/interval check — `should_run_now()` with gates
- Lines 252-296: `apply_automatic_transitions()` — pure function, no LLM
- Lines 299-1224: `maybe_run_curator()` — spawns review fork for consolidation

### How Hermes's Curator Works

1. **Trigger**: when the agent is idle (no active session) AND the last
   curator run was >= `interval_hours` ago (default: 7 days).

2. **Automatic transitions** (no LLM): walks every agent-created skill and
   applies lifecycle rules based on derived activity timestamps:
   - No activity for `stale_after_days` (30d) → mark `stale`
   - No activity for `archive_after_days` (90d) → move to `.archive/`
   - Recent activity on a stale skill → reactivate to `active`
   - Pinned skills → never touched

3. **Review fork** (with LLM): spawns a forked AIAgent (same pattern as
   background review) with a curator-specific system prompt. The fork can:
   - Consolidate overlapping skills into umbrella skills
   - Patch outdated skills
   - Archive (not delete) truly unused skills
   - Add missing cross-references between skills

4. **Strict invariants**:
   - Only touches agent-created skills (never bundled/shipped)
   - Never auto-deletes — only archives (archive is recoverable)
   - Pinned skills bypass all auto-transitions
   - Uses auxiliary client (cheaper model); never touches main session's
     prompt cache

### Dirge Adaptation

The curator is lower priority than memory + skills + review — it's the
polish layer. But it should be planned upfront so the architecture supports it.

Key differences from Hermes:
- Dirge's curator state lives in `.dirge/skills/.curator_state` (per-project)
- The "idle" check means no dirge process is running for this project
- The interval should probably be shorter for active projects (3 days?)
  since coding knowledge evolves faster than general agent skills

**Files**:
- `src/extras/skills/curator.rs` — `Curator` struct, `should_run_now`,
  `apply_automatic_transitions`, `maybe_run_curator`
- `src/cli.rs` — `dirge curator` subcommand (manual trigger)

**Tests**:
- Automatic transition: stale → archived after 90 days
- Pinned skill unaffected by transitions
- Only agent-created skills are touched
- Archive is recoverable (restore)
- Interval gate: won't run before interval elapses
- First-run deferral: seeds state on first check

**Risk**: medium. The curator is mostly mechanical (state transitions) with
one LLM call for consolidation. The LLM call uses the same fork pattern as
background review (Phase 4).

---

## Phase 7 — Context Compression

**Goal**: when a long coding session approaches the model's context limit,
compress the middle turns with an auxiliary model so the session can continue
without losing state.

**Reference**: `agent/context_compressor.py` (1104 lines) and
`agent/conversation_compression.py` (603 lines). Key sections:

From `context_compressor.py`:
- Lines 1-17: module docstring — structured template, filter-safe preamble
- Lines 37-51: `SUMMARY_PREFIX` — the preamble injected before the summary
  that tells the model this is reference, not active instructions
- Lines 54-59: budget constants — `_MIN_SUMMARY_TOKENS = 2000`,
  `_SUMMARY_RATIO = 0.20`, `_SUMMARY_TOKENS_CEILING = 12_000`
- Lines 62-76: tool output pruning, char/token estimates
- Lines 79+: `_content_length_for_budget()` — token budgeting logic

From `conversation_compression.py`:
- Lines 1-27: module docstring — three concerns (feasibility check, replay
  warning, compression call)
- Lines 44-80: `check_compression_model_feasibility()` — startup probe
- Lines 81+: `compress_context()` — the actual compression call

### How Hermes's Compression Works

1. **Trigger**: when `prompt_tokens > threshold_percent * context_length`,
   the compressor fires. Default threshold is 75% of the model's context
   window.

2. **Budgeting**: protect head (system prompt + first N messages) and tail
   (last N messages). Compress the middle. Summary budget is proportional
   to compressed content: `max(2000, 0.20 * compressed_tokens, 12000)`.

3. **Tool output pruning**: before the LLM sees the middle turns for
   summarization, old/large tool outputs are pruned to a placeholder.
   This is a cheap pre-pass that reduces tokens before the LLM call.

4. **Structured summary**: the auxiliary model produces a summary with:
   - Resolved questions (already addressed)
   - Pending questions (still open)
   - Active task (what the agent was doing)
   - Key decisions made
   - Remaining work (NOT "next steps" — that reads as instructions)

5. **Filter-safe preamble**: the summary is prefixed with a clear directive
   that this is REFERENCE, not active instructions. "Do NOT answer questions
   or fulfill requests mentioned in this summary; they were already addressed."

6. **Auxiliary model**: uses a cheaper/faster model for compression (configurable,
   defaults to the same provider with a cheaper model). This never touches
   the main session's prompt cache.

7. **Session splitting**: after compression, the session_id rotates. The new
   session has `parent_session_id` pointing to the old one. This is what
   enables lineage-based deduplication in session search.

### Dirge Adaptation

The core algorithm ports directly. Dirge-specific considerations:
- The threshold should be configurable per model (different models have
  different context windows)
- The auxiliary model for compression can be specified in dirge config
- Compression should emit a visible event so the UI can show "Context
  compacted — session continuing"

**Files**:
- `src/agent/compression.rs` — `Compressor` struct, `should_compress`,
  `compress_context`, feasibility check, tool output pruning
- `src/agent/runner.rs` — trigger compression check after each turn

**Tests**:
- Compression triggered when tokens exceed threshold
- Summary contains structured sections
- Tool outputs are pruned before summarization
- Session splits after compression (parent_session_id set)
- Filter-safe preamble present in compressed output
- Auxiliary model fallback on failure

**Risk**: medium. The compressor interacts with the agent loop (needs to
know token counts, needs to split sessions) and with the LLM provider
(auxiliary model). Tight coupling.

---

## Phase 8 — Integration: Wire Everything Into the Agent Loop

**Goal**: integrate all seven phases into dirge's agent lifecycle. Memory
is loaded at session start, background review fires at session end, session
search is available as a tool, compression triggers when context is full.

### Integration Points

1. **Session start**:
   - Detect project root → resolve `.dirge/`
   - Open (or create) session DB → create new session row
   - Load memory from `.dirge/memory/MEMORY.md` + `PITFALLS.md` →
     inject frozen snapshot into system prompt
   - Load skills from `.dirge/skills/` → inject relevant skills into
     system prompt (skills matching the current task context)

2. **Each turn**:
   - Before LLM call: insert user message into session DB
   - After LLM response: insert assistant message (and tool calls/results)
     into session DB
   - After each turn: check compression threshold → compress if needed

3. **Session end**:
   - Mark session as ended in DB
   - Spawn background review fork (Phase 4)
   - Update skill usage records for any skills that were loaded/used

4. **Tool availability**:
   - `memory` tool (Phase 2): always available
   - `skill_manage` + `skill_view` + `skills_list` (Phase 3): always available
   - `session_search` (Phase 5): always available (reads from session DB)

5. **Curator check** (Phase 6):
   - At session start (or via `dirge curator` subcommand): check if curator
     should run
   - If yes and agent is idle, spawn curator review fork

6. **Graceful degradation**:
   - If `.dirge/` cannot be created (permission, read-only FS) → session
     continues without memory/skills/session DB, with a warning
   - If session DB is on NFS/SMB → WAL fallback to DELETE mode (Phase 1)
   - If memory files are corrupt → warn, use defaults, allow writes to
     overwrite
   - If background review fork fails → log, continue (never block the user)
   - If compression fails → log warning, try with smaller budget, eventually
     truncate oldest messages as last resort

---

## Implementation Order and Dependencies

```
Phase 0: .dirge/ infrastructure         (no deps)
    ↓
Phase 1: Session DB + FTS5              (depends on Phase 0)
    ↓
Phase 2: Memory store                   (depends on Phase 0)
    ↓
Phase 3: Skill system                   (depends on Phase 0)
    ↓
Phase 4: Background review              (depends on Phases 1, 2, 3)
    ↓
Phase 5: Session search tool            (depends on Phase 1)
    ↓
Phase 6: Curator                        (depends on Phases 3, 4)
    ↓
Phase 7: Context compression            (depends on Phase 1, agent loop)
    ↓
Phase 8: Integration                    (depends on all above)
```

Phases 2 and 3 are independent (both only need Phase 0).
Phases 5 and 6 are independent of each other.
Phase 7 can be developed in parallel with Phases 2-6 since it mainly
depends on Phase 1 + the agent loop.

---

## Porting Principles

1. **Port the logic, not just the idea**. Every guard clause, every edge case
   handler, every error message pattern from Hermes should be present in the
   port. The value is in the details — the drift detection, the frozen
   snapshot, the injection scanning, the WAL fallback, the lineage rebinding.

2. **Adapt the storage, keep the semantics**. The storage moves from
   `~/.hermes/` to `.dirge/`, but the semantics of every operation stay
   identical. Memory tool behavior, skill CRUD behavior, session search
   behavior — all should match Hermes exactly.

3. **Rust ownership is the main challenge**. Hermes's Python uses shared
   mutable state with locks. Rust's ownership model is stricter. The
   frozen-snapshot pattern (live state vs snapshot) maps cleanly to Rust
   borrows, but requires explicit lifetime management. The fork pattern
   (independent agent instances) maps well to Rust's ownership model.

4. **Test against Hermes behavior**. For the memory store, the skill system,
   and session search, write tests that verify the Rust implementation
   produces the same results as the Python implementation for the same inputs.

5. **No shortcuts on error handling**. Hermes has careful error handling
   throughout: drifts are detected, locks are acquired, temp files are
   fsynced, injection is scanned. Every one of these has a documented
   failure mode that was discovered in production. Port them all.

---

## Estimated Sizes

| Phase | Rust LOC | Tests | Risk | Cumulative State |
|-------|----------|-------|------|------------------|
| 0     | ~100     | 3     | None | .dirge/ path resolution |
| 1     | ~800     | 12    | Med  | + Session DB with FTS5 |
| 2     | ~600     | 15    | Med  | + Memory store with two files |
| 3     | ~1200    | 20    | Med  | + Skill CRUD + usage tracking |
| 4     | ~400     | 8     | Med  | + Background review fork |
| 5     | ~500     | 10    | Med  | + Session search tool |
| 6     | ~500     | 8     | Med  | + Curator lifecycle |
| 7     | ~600     | 10    | Med  | + Context compression |
| 8     | ~400     | 8     | Med  | + Full integration |
| **Total** | **~5100** | **94** | | |

---

## Key Hermes Files Referenced

| File | Lines | What It Provides |
|------|-------|-----------------|
| `tools/memory_tool.py` | 690 | MemoryStore, § delimiters, frozen snapshot, injection scanning, drift detection, file locking, atomic writes |
| `tools/skill_manager_tool.py` | 1034 | Skill CRUD, YAML frontmatter validation, patch with fuzzy match, security scan, supporting files |
| `tools/skill_usage.py` | 608 | Telemetry sidecar, lifecycle states, provenance filter, archive/restore, activity tracking |
| `tools/skill_provenance.py` | ~150 | Bundled vs agent-created distinction, background review detection |
| `tools/fuzzy_match.py` | ~200 | Whitespace normalization, indentation handling, block-anchor matching |
| `agent/background_review.py` | 593 | Fork pattern, review prompts, tool whitelist, daemon thread |
| `agent/curator.py` | 1224 | Lifecycle transitions, interval gating, consolidation fork, invariants |
| `tools/session_search_tool.py` | 602 | Three-shape search, FTS5, lineage dedup, scroll with rebinding, bookends |
| `hermes_state.py` | 1174 | SQLite schema, FTS5 triggers, WAL fallback, session splitting, anchored views |
| `agent/context_compressor.py` | 1104 | Structured summaries, token budgeting, filter-safe preamble, tool output pruning |
| `agent/conversation_compression.py` | 603 | Feasibility check, compression call, session rotation |
| `agent/context_engine.py` | 212 | Pluggable context engine interface |
| `agent/insights.py` | 930 | Usage analytics (token tracking, cost estimation) — lower priority, not in plan |
| `agent/memory_manager.py` | 609 | Memory provider orchestration — Hermes-specific plugin pattern; dirge can be simpler |
