//! SQLite session database with FTS5 full-text search.
//!
//! Port of Hermes's `hermes_state.py`. Persists every session
//! transcript in a per-project SQLite database at
//! `.dirge/sessions/state.db`. Schema mirrors Hermes exactly:
//! sessions table + messages table + FTS5 virtual table with
//! content-sync triggers.
//!
//! Design decisions from Hermes preserved:
//! - WAL mode with fallback to DELETE on NFS/SMB
//! - Session splitting via parent_session_id chain
//! - Source tagging (cli, subagent, review-fork)
//! - Schema versioning with migrations
//! - FTS5 content sync triggers for auto-indexing

use rusqlite::{Connection, OpenFlags, params};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

// Used in migrate() to set user_version pragma.
const SCHEMA_VERSION: u32 = 6;

/// Thread-safe snapshot of the most recent `SessionDb::open()` failure.
/// Port of Hermes's `_last_init_error` (hermes_state.py:66-67).
/// Slash-command handlers read this to surface the underlying cause.
static LAST_INIT_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Return the most recent session DB init failure, if any.
/// Port of Hermes's `get_last_init_error()` (hermes_state.py:94-100).
#[allow(dead_code)]
pub fn last_init_error() -> Option<String> {
    LAST_INIT_ERROR.lock().unwrap().clone()
}

fn set_last_init_error(msg: Option<String>) {
    if let Ok(mut guard) = LAST_INIT_ERROR.lock() {
        *guard = msg;
    }
}

/// SESS-14: scrub credential-shaped tokens from text before it lands in
/// the FTS5 index. Ported from hermes-agent/agent/redact.py (the
/// `_PREFIX_PATTERNS`, `_DB_CONNSTR_RE`, `_URL_USERINFO_RE`,
/// `_AUTH_HEADER_RE`, `_ENV_ASSIGN_RE` patterns) — same coverage as
/// `sandbox::is_sensitive_env_value`, but applied as a *replace* (not
/// a yes/no test) since we still need a searchable, non-secret
/// projection of the message text.
///
/// Raw content stays in `messages.content` / `messages.tool_calls`;
/// only the searchable projection passed to `messages_fts` and
/// `messages_fts_trigram` is redacted. Anyone reading a transcript
/// back out sees the unredacted original.
///
/// Each match is replaced with `<REDACTED>`. Pre-checks gate each
/// regex on a cheap substring so the common no-secret case stays
/// fast (a single line of plain prose pays for the gate misses only).
pub fn redact_for_fts(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    static VENDOR_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
    static URL_USERINFO_RE: OnceLock<Regex> = OnceLock::new();
    static AUTH_HEADER_RE: OnceLock<Regex> = OnceLock::new();
    static ENV_ASSIGN_RE: OnceLock<Regex> = OnceLock::new();
    static JSON_FIELD_RE: OnceLock<Regex> = OnceLock::new();
    static JWT_RE: OnceLock<Regex> = OnceLock::new();

    let mut out: std::borrow::Cow<'_, str> = text.into();

    // Vendor prefix tokens. Same set as
    // sandbox::is_sensitive_env_value — kept in sync deliberately.
    let has_prefix_gate = out.contains("AKIA")
        || out.contains("ghp_")
        || out.contains("github_pat_")
        || out.contains("gho_")
        || out.contains("ghu_")
        || out.contains("ghs_")
        || out.contains("xox")
        || out.contains("sk-")
        || out.contains("sk_live_")
        || out.contains("sk_test_")
        || out.contains("AIza")
        || out.contains("hf_")
        || out.contains("xai-");
    if has_prefix_gate {
        let re = VENDOR_PREFIX_RE.get_or_init(|| {
            Regex::new(
                r"(?x)
                (?:
                      AKIA[0-9A-Z]{16}
                    | ghp_[A-Za-z0-9]{36}
                    | github_pat_[A-Za-z0-9_]{20,}
                    | gho_[A-Za-z0-9]{30,}
                    | ghu_[A-Za-z0-9]{30,}
                    | ghs_[A-Za-z0-9]{30,}
                    | xox[baprs]-[A-Za-z0-9-]{10,}
                    | sk-[A-Za-z0-9_-]{20,}
                    | sk_live_[A-Za-z0-9]{20,}
                    | sk_test_[A-Za-z0-9]{20,}
                    | AIza[A-Za-z0-9_-]{30,}
                    | hf_[A-Za-z0-9]{30,}
                    | xai-[A-Za-z0-9]{30,}
                )
                ",
            )
            .unwrap()
        });
        if re.is_match(&out) {
            out = re.replace_all(&out, "<REDACTED>").into_owned().into();
        }
    }

    // JWTs (3-part eyJ...) — gate on "eyJ" substring.
    if out.contains("eyJ") {
        let re = JWT_RE.get_or_init(|| {
            Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_=-]{4,}").unwrap()
        });
        if re.is_match(&out) {
            out = re.replace_all(&out, "<REDACTED>").into_owned().into();
        }
    }

    // URLs with userinfo: scheme://user:pass@host
    if out.contains("://") {
        let re = URL_USERINFO_RE.get_or_init(|| {
            Regex::new(r"([A-Za-z][A-Za-z0-9+.\-]*://)([^/\s:@]*):([^/\s@]+)@").unwrap()
        });
        if re.is_match(&out) {
            out = re.replace_all(&out, "${1}<REDACTED>@").into_owned().into();
        }
    }

    // Authorization: Bearer <token>
    if out.contains("uthorization") || out.contains("UTHORIZATION") {
        let re = AUTH_HEADER_RE
            .get_or_init(|| Regex::new(r"(?i)(Authorization:\s*Bearer\s+)\S+").unwrap());
        if re.is_match(&out) {
            out = re.replace_all(&out, "${1}<REDACTED>").into_owned().into();
        }
    }

    // KEY=value / TOKEN=value / SECRET=value / PASSWORD=value /
    // CREDENTIAL=value / AUTH=value (env-style)
    if out.contains('=') {
        let re = ENV_ASSIGN_RE.get_or_init(|| {
            Regex::new(
                r#"(?i)([A-Za-z0-9_]*(?:API_?KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH)[A-Za-z0-9_]*\s*=\s*)['"]?[^\s'"&]+"#,
            )
            .unwrap()
        });
        if re.is_match(&out) {
            out = re.replace_all(&out, "${1}<REDACTED>").into_owned().into();
        }
    }

    // JSON-ish fields: "api_key": "value", "token": "value", …
    if out.contains(':') && out.contains('"') {
        let re = JSON_FIELD_RE.get_or_init(|| {
            Regex::new(
                r#"(?i)("(?:api_?key|token|secret|password|access_token|refresh_token|auth_token|bearer)"\s*:\s*)"[^"]+""#,
            )
            .unwrap()
        });
        if re.is_match(&out) {
            out = re
                .replace_all(&out, "${1}\"<REDACTED>\"")
                .into_owned()
                .into();
        }
    }

    out.into_owned()
}

pub struct SessionDb {
    pub(crate) conn: Connection,
}

impl SessionDb {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = match Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        ) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Failed to open session DB at {}: {e}", path.display());
                set_last_init_error(Some(msg.clone()));
                return Err(msg);
            }
        };

        // WAL mode with fallback
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(_) => {}
            Err(e) => {
                let msg = format!(
                    "WAL mode unavailable for {} — falling back to DELETE journal: {e}",
                    path.display()
                );
                tracing::warn!(
                    target: "dirge::session_db",
                    path = %path.display(),
                    "WAL mode unavailable — falling back to DELETE journal"
                );
                set_last_init_error(Some(msg));
                conn.pragma_update(None, "journal_mode", "DELETE")
                    .map_err(|e| format!("Failed to set DELETE journal mode: {e}"))?;
            }
        }

        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| {
                let msg = format!("Failed to enable foreign keys: {e}");
                set_last_init_error(Some(msg.clone()));
                msg
            })?;

        let db = SessionDb { conn };
        db.migrate()?;
        // Clear the error on successful open.
        set_last_init_error(None);
        Ok(db)
    }

    fn migrate(&self) -> Result<(), String> {
        let current: u32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| format!("Failed to read schema version: {e}"))?;

        if current < 1 {
            self.run_migration_v1()?;
        }

        if current < 2 {
            self.run_migration_v2()?;
        }

        if current < 3 {
            self.run_migration_v3()?;
        }

        if current < 4 {
            self.run_migration_v4()?;
        }

        if current < 5 {
            self.run_migration_v5()?;
        }

        if current < 6 {
            self.run_migration_v6()?;
        }

        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| format!("Failed to set schema version: {e}"))?;

        Ok(())
    }

    fn run_migration_v1(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE sessions (
                    id              TEXT PRIMARY KEY,
                    parent_session_id TEXT,
                    source          TEXT NOT NULL DEFAULT 'cli',
                    model           TEXT NOT NULL DEFAULT '',
                    provider        TEXT NOT NULL DEFAULT '',
                    started_at      TEXT NOT NULL,
                    last_active     TEXT NOT NULL,
                    title           TEXT NOT NULL DEFAULT '',
                    message_count   INTEGER NOT NULL DEFAULT 0,
                    input_tokens    INTEGER NOT NULL DEFAULT 0,
                    output_tokens   INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE messages (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id      TEXT NOT NULL REFERENCES sessions(id),
                    role            TEXT NOT NULL,
                    content         TEXT NOT NULL DEFAULT '',
                    tool_name       TEXT,
                    tool_calls      TEXT,
                    tool_call_id    TEXT,
                    timestamp       TEXT NOT NULL
                );

                CREATE INDEX idx_messages_session ON messages(session_id);
                CREATE INDEX idx_messages_role ON messages(session_id, role);

                CREATE VIRTUAL TABLE messages_fts USING fts5(
                    content,
                    content=messages,
                    content_rowid=id
                );

                -- FTS5 content sync triggers — index content + tool_name + tool_calls
                -- so searches for tool names find their messages.
                -- Port of Hermes's FTS_SQL (hermes_state.py:255-278).
                CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;

                CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                END;

                CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                    INSERT INTO messages_fts(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;
                ",
            )
            .map_err(|e| format!("Migration v1 failed: {e}"))?;

        Ok(())
    }

    /// v2: rebuild FTS5 triggers with tool_name/tool_calls in the index
    /// and backfill all existing rows. DBs created by v1 had triggers
    /// that only indexed `new.content` — tool names were invisible to search.
    fn run_migration_v2(&self) -> Result<(), String> {
        // Drop old triggers (IF EXISTS for DBs created after the v1 fix above).
        self.conn
            .execute_batch(
                "
                DROP TRIGGER IF EXISTS messages_ai;
                DROP TRIGGER IF EXISTS messages_ad;
                DROP TRIGGER IF EXISTS messages_au;

                CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;

                CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                END;

                CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                    INSERT INTO messages_fts(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;
                ",
            )
            .map_err(|e| format!("Migration v2 triggers failed: {e}"))?;

        // Backfill: delete stale v1 content entries, then re-insert
        // with the composite content + tool_name + tool_calls formula.
        // External-content FTS5 tables don't auto-rebuild with a new
        // formula — the trigger controls what content is indexed.
        self.conn
            .execute("DELETE FROM messages_fts", [])
            .map_err(|e| format!("Migration v2 delete failed: {e}"))?;

        self.conn
            .execute(
                "INSERT INTO messages_fts(rowid, content)
                 SELECT id,
                        COALESCE(content, '') || ' ' ||
                        COALESCE(tool_name, '') || ' ' ||
                        COALESCE(tool_calls, '')
                 FROM messages",
                [],
            )
            .map_err(|e| format!("Migration v2 backfill failed: {e}"))?;

        Ok(())
    }

    /// v3: add trigram FTS5 table for CJK/substring search.
    /// Port of Hermes's FTS_TRIGRAM_SQL (hermes_state.py:284-308).
    /// The default unicode61 tokenizer splits CJK characters into
    /// individual tokens, breaking phrase matching. The trigram
    /// tokenizer creates overlapping 3-character sequences so
    /// substring queries work natively for any script.
    fn run_migration_v3(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_trigram USING fts5(
                    content,
                    tokenize='trigram'
                );

                CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_insert AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;

                CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_delete AFTER DELETE ON messages BEGIN
                    DELETE FROM messages_fts_trigram WHERE rowid = old.id;
                END;

                CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_update AFTER UPDATE ON messages BEGIN
                    DELETE FROM messages_fts_trigram WHERE rowid = old.id;
                    INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                        new.id,
                        COALESCE(new.content, '') || ' ' ||
                        COALESCE(new.tool_name, '') || ' ' ||
                        COALESCE(new.tool_calls, '')
                    );
                END;
                ",
            )
            .map_err(|e| format!("Migration v3 failed: {e}"))?;

        // Backfill trigram index from existing messages.
        self.conn
            .execute(
                "INSERT INTO messages_fts_trigram(rowid, content)
                 SELECT id,
                        COALESCE(content, '') || ' ' ||
                        COALESCE(tool_name, '') || ' ' ||
                        COALESCE(tool_calls, '')
                 FROM messages
                 WHERE id NOT IN (SELECT rowid FROM messages_fts_trigram)",
                [],
            )
            .map_err(|e| format!("Migration v3 backfill failed: {e}"))?;

        Ok(())
    }

    /// v4: add session lifecycle + cost-tracking columns.
    /// Port of Hermes's sessions schema (hermes_state.py:190-222).
    fn run_migration_v4(&self) -> Result<(), String> {
        for col in &[
            "ended_at TEXT",
            "end_reason TEXT",
            "tool_call_count INTEGER DEFAULT 0",
            "api_call_count INTEGER DEFAULT 0",
        ] {
            if let Err(e) = self
                .conn
                .execute(&format!("ALTER TABLE sessions ADD COLUMN {col}"), [])
            {
                // Duplicate column name is harmless — the column
                // already exists from a partial previous migration.
                if !e.to_string().contains("duplicate column name") {
                    return Err(format!("Migration v4 failed on {col}: {e}"));
                }
            }
        }
        Ok(())
    }

    /// v6: SESS-14 — drop the auto-INSERT / auto-UPDATE FTS triggers so
    /// the application can redact secrets before they land in the
    /// full-text index. The raw text stays in `messages.content` /
    /// `messages.tool_calls`, but `messages_fts` and
    /// `messages_fts_trigram` only receive a redacted projection
    /// supplied by `insert_message`.
    ///
    /// AFTER DELETE triggers stay in place — purging from the FTS
    /// table on a row delete doesn't need any redaction.
    ///
    /// Backfill: re-insert the existing row contents into both FTS
    /// tables after passing them through `redact_for_fts`. Existing
    /// indexes were built from raw content; without this step a
    /// search would still hit pre-v6 secrets.
    fn run_migration_v6(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                DROP TRIGGER IF EXISTS messages_ai;
                DROP TRIGGER IF EXISTS messages_au;
                DROP TRIGGER IF EXISTS messages_fts_trigram_insert;
                DROP TRIGGER IF EXISTS messages_fts_trigram_update;
                ",
            )
            .map_err(|e| format!("Migration v6 trigger drop failed: {e}"))?;

        // Backfill: clear both indexes then re-insert with redacted
        // content row-by-row so the redactor runs on each row.
        self.conn
            .execute("DELETE FROM messages_fts", [])
            .map_err(|e| format!("Migration v6 clear fts failed: {e}"))?;
        self.conn
            .execute("DELETE FROM messages_fts_trigram", [])
            .map_err(|e| format!("Migration v6 clear trigram failed: {e}"))?;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, COALESCE(content, ''), COALESCE(tool_name, ''), COALESCE(tool_calls, '')
                 FROM messages",
            )
            .map_err(|e| format!("Migration v6 select failed: {e}"))?;

        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map_err(|e| format!("Migration v6 query failed: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        for (id, content, tool_name, tool_calls) in rows {
            let combined = format!("{content} {tool_name} {tool_calls}");
            let redacted = redact_for_fts(&combined);
            self.conn
                .execute(
                    "INSERT INTO messages_fts(rowid, content) VALUES (?1, ?2)",
                    params![id, redacted],
                )
                .map_err(|e| format!("Migration v6 fts backfill failed at row {id}: {e}"))?;
            self.conn
                .execute(
                    "INSERT INTO messages_fts_trigram(rowid, content) VALUES (?1, ?2)",
                    params![id, redacted],
                )
                .map_err(|e| format!("Migration v6 trigram backfill failed at row {id}: {e}"))?;
        }
        Ok(())
    }

    /// v5: add message detail columns.
    /// Port of Hermes's messages schema (hermes_state.py:224-242).
    fn run_migration_v5(&self) -> Result<(), String> {
        for col in &["token_count INTEGER", "finish_reason TEXT"] {
            if let Err(e) = self
                .conn
                .execute(&format!("ALTER TABLE messages ADD COLUMN {col}"), [])
            {
                if !e.to_string().contains("duplicate column name") {
                    return Err(format!("Migration v5 failed on {col}: {e}"));
                }
            }
        }
        Ok(())
    }

    pub fn insert_session(
        &self,
        id: &str,
        source: &str,
        model: &str,
        provider: &str,
        started_at: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO sessions (id, source, model, provider, started_at, last_active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![id, source, model, provider, started_at],
            )
            .map_err(|e| format!("Failed to insert session: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        tool_name: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        timestamp: &str,
    ) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO messages (session_id, role, content, tool_name, tool_calls, tool_call_id, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![session_id, role, content, tool_name, tool_calls, tool_call_id, timestamp],
            )
            .map_err(|e| format!("Failed to insert message: {e}"))?;

        let row_id = self.conn.last_insert_rowid();

        // SESS-14: redact secrets before they reach the FTS5 index.
        // The auto-insert triggers were dropped in v6 so we own this
        // path explicitly. Raw text stays in `messages` (so callers
        // re-reading a transcript see the original content); only
        // the searchable projection is scrubbed.
        let combined = format!(
            "{} {} {}",
            content,
            tool_name.unwrap_or(""),
            tool_calls.unwrap_or(""),
        );
        let redacted = redact_for_fts(&combined);

        self.conn
            .execute(
                "INSERT INTO messages_fts(rowid, content) VALUES (?1, ?2)",
                params![row_id, redacted],
            )
            .map_err(|e| format!("Failed to insert into messages_fts: {e}"))?;
        self.conn
            .execute(
                "INSERT INTO messages_fts_trigram(rowid, content) VALUES (?1, ?2)",
                params![row_id, redacted],
            )
            .map_err(|e| format!("Failed to insert into messages_fts_trigram: {e}"))?;

        self.conn
            .execute(
                "UPDATE sessions SET message_count = message_count + 1, last_active = ?1 WHERE id = ?2",
                params![timestamp, session_id],
            )
            .map_err(|e| format!("Failed to update session message count: {e}"))?;

        Ok(row_id)
    }
}

pub struct SearchResult {
    pub session_id: String,
    pub content: String,
    #[allow(dead_code)] // populated from SQL, not yet read by consumers
    pub role: String,
    pub timestamp: String,
}

pub struct SessionSummary {
    pub id: String,
    pub source: String,
    pub model: String,
    pub title: String,
    pub started_at: String,
    pub last_active: String,
    pub message_count: i64,
}

impl SessionDb {
    pub fn list_sessions_rich(
        &self,
        exclude_sources: Option<&[&str]>,
    ) -> Result<Vec<SessionSummary>, String> {
        fn map_row(row: &rusqlite::Row) -> rusqlite::Result<SessionSummary> {
            Ok(SessionSummary {
                id: row.get(0)?,
                source: row.get(1)?,
                model: row.get(2)?,
                title: row.get(3)?,
                started_at: row.get(4)?,
                last_active: row.get(5)?,
                message_count: row.get(6)?,
            })
        }

        let (sql, has_exclude) = if exclude_sources.is_some_and(|s| !s.is_empty()) {
            let placeholders: Vec<String> = (0..exclude_sources.as_ref().unwrap().len())
                .map(|i| format!("?{}", i + 1))
                .collect();
            (
                format!(
                    "SELECT id, source, model, title, started_at, last_active, message_count
                     FROM sessions
                     WHERE source NOT IN ({})
                     ORDER BY last_active DESC
                     LIMIT 50",
                    placeholders.join(", ")
                ),
                true,
            )
        } else {
            (
                "SELECT id, source, model, title, started_at, last_active, message_count
                 FROM sessions
                 ORDER BY last_active DESC
                 LIMIT 50"
                    .to_string(),
                false,
            )
        };

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| format!("Failed to prepare list sessions: {e}"))?;

        let results: Vec<SessionSummary> = if has_exclude {
            let sources = exclude_sources.unwrap();
            let refs: Vec<&dyn rusqlite::types::ToSql> = sources
                .iter()
                .map(|s| s as &dyn rusqlite::types::ToSql)
                .collect();
            stmt.query_map(rusqlite::params_from_iter(refs.iter()), map_row)
                .map_err(|e| format!("Failed to list sessions: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map([], map_row)
                .map_err(|e| format!("Failed to list sessions: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        };

        Ok(results)
    }

    pub fn search_messages(
        &self,
        query: &str,
        role_filter: Option<&str>,
    ) -> Result<Vec<SearchResult>, String> {
        fn map_row(row: &rusqlite::Row) -> rusqlite::Result<SearchResult> {
            Ok(SearchResult {
                session_id: row.get(0)?,
                content: row.get(1)?,
                role: row.get(2)?,
                timestamp: row.get(3)?,
            })
        }

        let (sql, has_role) = if role_filter.is_some() {
            (
                "SELECT m.session_id, m.content, m.role, m.timestamp
                 FROM messages_fts f
                 JOIN messages m ON f.rowid = m.id
                 WHERE messages_fts MATCH ?1 AND m.role = ?2
                 ORDER BY rank
                 LIMIT 50",
                true,
            )
        } else {
            (
                "SELECT m.session_id, m.content, m.role, m.timestamp
                 FROM messages_fts f
                 JOIN messages m ON f.rowid = m.id
                 WHERE messages_fts MATCH ?1
                 ORDER BY rank
                 LIMIT 50",
                false,
            )
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("Failed to prepare search: {e}"))?;

        let results: Vec<SearchResult> = if has_role {
            stmt.query_map(params![query, role_filter.unwrap()], map_row)
                .map_err(|e| format!("FTS5 search failed: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map(params![query], map_row)
                .map_err(|e| format!("FTS5 search failed: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        };

        Ok(results)
    }

    /// Search messages via the trigram FTS5 index (CJK/substring queries).
    /// The trigram tokenizer creates overlapping 3-character sequences,
    /// making substring matching work natively for any script.
    /// Port of Hermes's trigram search path (hermes_state.py:2245-2350).
    pub fn search_messages_trigram(
        &self,
        query: &str,
        role_filter: Option<&str>,
    ) -> Result<Vec<SearchResult>, String> {
        fn map_row(row: &rusqlite::Row) -> rusqlite::Result<SearchResult> {
            Ok(SearchResult {
                session_id: row.get(0)?,
                content: row.get(1)?,
                role: row.get(2)?,
                timestamp: row.get(3)?,
            })
        }

        let (sql, has_role) = if role_filter.is_some() {
            (
                "SELECT m.session_id, m.content, m.role, m.timestamp
                 FROM messages_fts_trigram f
                 JOIN messages m ON f.rowid = m.id
                 WHERE messages_fts_trigram MATCH ?1 AND m.role = ?2
                 ORDER BY rank
                 LIMIT 50",
                true,
            )
        } else {
            (
                "SELECT m.session_id, m.content, m.role, m.timestamp
                 FROM messages_fts_trigram f
                 JOIN messages m ON f.rowid = m.id
                 WHERE messages_fts_trigram MATCH ?1
                 ORDER BY rank
                 LIMIT 50",
                false,
            )
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("Failed to prepare trigram search: {e}"))?;

        let results: Vec<SearchResult> = if has_role {
            stmt.query_map(params![query, role_filter.unwrap()], map_row)
                .map_err(|e| format!("Trigram FTS5 search failed: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map(params![query], map_row)
                .map_err(|e| format!("Trigram FTS5 search failed: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        };

        Ok(results)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_parent_session(&self, session_id: &str, parent_id: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE sessions SET parent_session_id = ?1 WHERE id = ?2",
                params![parent_id, session_id],
            )
            .map_err(|e| format!("Failed to set parent session: {e}"))?;
        Ok(())
    }

    /// Mark a session as ended with the given reason.
    /// No-ops when the session is already ended — the first end_reason
    /// wins (compression splits keep their end_reason).
    /// Port of Hermes's `end_session()` (hermes_state.py:732-748).
    ///
    /// Mark a session as ended with the given reason.
    /// No-ops when the session is already ended — the first end_reason
    /// wins (compression splits keep their end_reason).
    pub fn end_session(&self, session_id: &str, end_reason: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE sessions SET ended_at = ?1, end_reason = ?2 WHERE id = ?3 AND ended_at IS NULL",
                params![chrono::Utc::now().to_rfc3339(), end_reason, session_id],
            )
            .map_err(|e| format!("Failed to end session: {e}"))?;
        Ok(())
    }

    pub fn resolve_parent(&self, session_id: &str) -> Result<String, String> {
        let mut current = session_id.to_string();
        // Walk the parent chain up to root (max 100 hops to prevent
        // infinite loops on corrupted data).
        for _ in 0..100 {
            let parent: Option<String> = self
                .conn
                .query_row(
                    "SELECT parent_session_id FROM sessions WHERE id = ?1",
                    params![current],
                    |row| row.get(0),
                )
                .ok()
                .and_then(|p: Option<String>| p);
            match parent {
                Some(p) if !p.is_empty() => current = p,
                _ => break,
            }
        }
        Ok(current)
    }
}

pub struct AnchorView {
    pub messages: Vec<AnchorMessage>,
    pub anchor_index: usize,
    pub before: usize,
    pub after: usize,
}

pub struct AnchorMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

impl SessionDb {
    pub fn get_anchored_view(
        &self,
        session_id: &str,
        anchor_message_id: i64,
        window: usize,
    ) -> Result<AnchorView, String> {
        // Get the anchor's position (row number within the session).
        let anchor_row: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND id <= ?2",
                params![session_id, anchor_message_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to find anchor position: {e}"))?;

        let total: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to count messages: {e}"))?;

        let before = window.min(anchor_row.saturating_sub(1) as usize);
        let after = window.min((total - anchor_row).max(0) as usize);
        let offset = (anchor_row - before as i64 - 1).max(0);

        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, role, content, timestamp
                 FROM messages
                 WHERE session_id = ?1
                 ORDER BY id
                 LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| format!("Failed to prepare anchored view: {e}"))?;

        let messages: Vec<AnchorMessage> = stmt
            .query_map(params![session_id, before + 1 + after, offset], |row| {
                Ok(AnchorMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    timestamp: row.get(3)?,
                })
            })
            .map_err(|e| format!("Failed to query anchored view: {e}"))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(AnchorView {
            messages,
            anchor_index: before,
            before,
            after,
        })
    }
}

#[cfg(test)]
#[path = "session_db_tests.rs"]
mod tests;
