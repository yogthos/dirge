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

const SCHEMA_VERSION: u32 = 1;

pub struct SessionDb {
    pub(crate) conn: Connection,
}

impl SessionDb {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|e| format!("Failed to open session DB: {e}"))?;

        // WAL mode with fallback
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(
                    target: "dirge::session_db",
                    path = %path.display(),
                    "WAL mode unavailable — falling back to DELETE journal"
                );
                conn.pragma_update(None, "journal_mode", "DELETE")
                    .map_err(|e| format!("Failed to set DELETE journal mode: {e}"))?;
            }
        }

        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| format!("Failed to enable foreign keys: {e}"))?;

        let db = SessionDb { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), String> {
        let current: u32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| format!("Failed to read schema version: {e}"))?;

        if current < 1 {
            self.run_migration_v1()?;
            self.conn
                .pragma_update(None, "user_version", 1)
                .map_err(|e| format!("Failed to set schema version: {e}"))?;
        }

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

                -- FTS5 content sync triggers — auto-update on insert/update/delete
                CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
                END;

                CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content);
                END;

                CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content);
                    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
                END;
                ",
            )
            .map_err(|e| format!("Migration v1 failed: {e}"))?;

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

        self.conn
            .execute(
                "UPDATE sessions SET message_count = message_count + 1, last_active = ?1 WHERE id = ?2",
                params![timestamp, session_id],
            )
            .map_err(|e| format!("Failed to update session message count: {e}"))?;

        Ok(self.conn.last_insert_rowid())
    }
}

pub struct SearchResult {
    pub session_id: String,
    pub content: String,
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
            let refs: Vec<&dyn rusqlite::types::ToSql> =
                sources.iter().map(|s| s as &dyn rusqlite::types::ToSql).collect();
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

    pub fn set_parent_session(&self, session_id: &str, parent_id: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE sessions SET parent_session_id = ?1 WHERE id = ?2",
                params![parent_id, session_id],
            )
            .map_err(|e| format!("Failed to set parent session: {e}"))?;
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
            .query_map(
                params![session_id, before + 1 + after, offset],
                |row| {
                    Ok(AnchorMessage {
                        id: row.get(0)?,
                        role: row.get(1)?,
                        content: row.get(2)?,
                        timestamp: row.get(3)?,
                    })
                },
            )
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
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    static DB_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_db() -> (SessionDb, std::path::PathBuf) {
        let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dirge-session-db-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let db = SessionDb::open(&path).unwrap();
        (db, dir)
    }

    #[test]
    fn create_and_read_session() {
        let (db, _dir) = temp_db();
        db.insert_session("sess-1", "cli", "claude-opus", "anthropic", "2025-01-15T10:00:00Z")
            .unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM sessions WHERE id = 'sess-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn insert_message_and_fts5_search() {
        let (db, _dir) = temp_db();
        db.insert_session("sess-1", "cli", "claude-opus", "anthropic", "2025-01-15T10:00:00Z")
            .unwrap();

        db.insert_message(
            "sess-1",
            "user",
            "how do we handle database migrations",
            None,
            None,
            None,
            "2025-01-15T10:01:00Z",
        )
        .unwrap();

        let results = db
            .search_messages("database migrations", None)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("database migrations"));
    }

    #[test]
    fn list_sessions_returns_recent() {
        let (db, _dir) = temp_db();
        db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();
        db.insert_session("sess-2", "subagent", "claude-sonnet", "anthropic", "2025-01-15T11:00:00Z")
            .unwrap();

        let sessions = db.list_sessions_rich(None).unwrap();
        assert_eq!(sessions.len(), 2);
        // Most recent first.
        assert_eq!(sessions[0].id, "sess-2");
        assert_eq!(sessions[1].id, "sess-1");
    }

    #[test]
    fn list_sessions_excludes_source() {
        let (db, _dir) = temp_db();
        db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();
        db.insert_session("sess-2", "review-fork", "claude-sonnet", "anthropic", "2025-01-15T11:00:00Z")
            .unwrap();

        let sessions = db.list_sessions_rich(Some(&["review-fork"])).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "sess-1");
    }

    #[test]
    fn session_split_parent_chain() {
        let (db, _dir) = temp_db();
        db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();

        // Split: child session points to parent.
        db.insert_session("sess-2", "cli", "gpt-5", "openai", "2025-01-15T11:00:00Z")
            .unwrap();
        db.set_parent_session("sess-2", "sess-1").unwrap();

        let parent: String = db
            .conn
            .query_row(
                "SELECT parent_session_id FROM sessions WHERE id = 'sess-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, "sess-1");
    }

    #[test]
    fn fts5_search_with_role_filter() {
        let (db, _dir) = temp_db();
        db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();

        db.insert_message(
            "sess-1", "user", "how do we build this", None, None, None, "2025-01-15T10:01:00Z",
        )
        .unwrap();
        db.insert_message(
            "sess-1", "assistant", "run cargo build", None, None, None, "2025-01-15T10:02:00Z",
        )
        .unwrap();

        let results = db.search_messages("build", Some("user")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].role, "user");
    }

    #[test]
    fn anchored_view_returns_window_around_match() {
        let (db, _dir) = temp_db();
        db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();

        // Insert 10 messages.
        for i in 0..10 {
            db.insert_message(
                "sess-1",
                if i % 2 == 0 { "user" } else { "assistant" },
                &format!("message {}", i),
                None,
                None,
                None,
                &format!("2025-01-15T10:{:02}:00Z", i),
            )
            .unwrap();
        }

        // Anchor on message 5.
        let view = db.get_anchored_view("sess-1", 5, 2).unwrap();

        // Window should have 5 messages: anchor + 2 before + 2 after.
        assert_eq!(view.messages.len(), 5);
        assert_eq!(view.anchor_index, 2);
        assert_eq!(view.before, 2);
        assert_eq!(view.after, 2);
    }

    #[test]
    fn resolve_parent_walks_lineage() {
        let (db, _dir) = temp_db();
        db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();
        db.insert_session("sess-2", "cli", "gpt-5", "openai", "2025-01-15T11:00:00Z")
            .unwrap();
        db.insert_session("sess-3", "cli", "gpt-5", "openai", "2025-01-15T12:00:00Z")
            .unwrap();

        db.set_parent_session("sess-2", "sess-1").unwrap();
        db.set_parent_session("sess-3", "sess-2").unwrap();

        assert_eq!(db.resolve_parent("sess-3").unwrap(), "sess-1");
        assert_eq!(db.resolve_parent("sess-2").unwrap(), "sess-1");
        assert_eq!(db.resolve_parent("sess-1").unwrap(), "sess-1");
    }
}
