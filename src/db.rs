//! SQLite storage layer.
//!
//! Wraps a `tokio_rusqlite::Connection` opened on the configured path and
//! applies the upstream-compatible schema as an idempotent migration. The
//! table/column names MUST match opencode v2's database exactly so that the
//! ox server and the Bun server can read each other's DB files.

use std::path::Path;

use anyhow::Result;
use tokio_rusqlite::Connection;

/// Embedded upstream-compatible schema (opencode v2). Idempotent: every
/// statement uses `IF NOT EXISTS`.
const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS event_sequence (
  aggregate_id text PRIMARY KEY,
  seq integer NOT NULL,
  owner_id text
);
CREATE TABLE IF NOT EXISTS event (
  id text PRIMARY KEY,
  aggregate_id text NOT NULL,
  seq integer NOT NULL,
  created integer DEFAULT 0 NOT NULL,
  type text NOT NULL,
  data text NOT NULL,
  FOREIGN KEY (aggregate_id) REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS kv (
  key text PRIMARY KEY,
  value text NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL
);
CREATE TABLE IF NOT EXISTS session_v2 (
  id text PRIMARY KEY,
  project_id text NOT NULL,
  workspace_id text,
  parent_id text,
  fork_session_id text,
  fork_boundary text,
  slug text NOT NULL,
  directory text NOT NULL,
  path text,
  title text,
  version text NOT NULL,
  share_url text,
  summary_additions integer,
  summary_deletions integer,
  summary_files integer,
  summary_diffs text,
  metadata text,
  cost real NOT NULL DEFAULT 0,
  tokens_input integer NOT NULL DEFAULT 0,
  tokens_output integer NOT NULL DEFAULT 0,
  tokens_reasoning integer NOT NULL DEFAULT 0,
  tokens_cache_read integer NOT NULL DEFAULT 0,
  tokens_cache_write integer NOT NULL DEFAULT 0,
  revert text,
  permission text,
  agent text,
  model text,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  time_idle integer,
  time_viewed integer,
  idle_outcome text,
  time_compacting integer,
  time_archived integer,
  time_suspended integer,
  resume_attempts integer NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS session_v2_project_idx ON session_v2 (project_id);
CREATE TABLE IF NOT EXISTS session_message (
  id text PRIMARY KEY,
  session_id text NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type text NOT NULL,
  seq integer NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  data text NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS session_message_session_seq_idx ON session_message (session_id, seq);
CREATE TABLE IF NOT EXISTS session_inbox (
  id text PRIMARY KEY,
  session_id text NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type text NOT NULL,
  payload text NOT NULL,
  delivery text NOT NULL,
  enqueued_seq integer NOT NULL,
  time_created integer NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS session_inbox_session_enqueued_seq_idx ON session_inbox (session_id, enqueued_seq);
CREATE TABLE IF NOT EXISTS session_pending (
  id text PRIMARY KEY,
  session_id text NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type text NOT NULL,
  data text NOT NULL,
  delivery text,
  admitted_seq integer NOT NULL,
  time_created integer NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS session_pending_session_admitted_seq_idx ON session_pending (session_id, admitted_seq);
"#;

/// Tables created by the embedded migration, in creation order.
#[cfg(test)]
const EXPECTED_TABLES: &[&str] = &[
    "event_sequence",
    "event",
    "kv",
    "session_v2",
    "session_message",
    "session_inbox",
    "session_pending",
];

/// Async handle to the SQLite database.
#[derive(Clone)]
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database at `path` and apply migrations.
    /// Parent directories are created automatically.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).await?;
        conn.call(|c| -> rusqlite::Result<()> {
            c.pragma_update(None, "journal_mode", "WAL")?;
            c.pragma_update(None, "busy_timeout", 5000)?;
            c.pragma_update(None, "foreign_keys", "ON")?;
            Ok(())
        })
        .await?;
        let db = Self { conn };
        db.migrate().await?;
        Ok(db)
    }

    /// Open an in-memory database (for tests).
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().await?;
        conn.call(|c| -> rusqlite::Result<()> {
            c.pragma_update(None, "journal_mode", "WAL")?;
            c.pragma_update(None, "busy_timeout", 5000)?;
            c.pragma_update(None, "foreign_keys", "ON")?;
            Ok(())
        })
        .await?;
        let db = Self { conn };
        db.migrate().await?;
        Ok(db)
    }

    /// Apply the embedded schema. Idempotent.
    pub async fn migrate(&self) -> Result<()> {
        self.conn
            .call(|c| -> rusqlite::Result<()> {
                c.execute_batch(MIGRATION_V1)?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Access the underlying connection for queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Check whether a table exists (used for graceful degradation when
    /// sharing a DB file with the upstream server).
    pub async fn table_exists(&self, name: &'static str) -> Result<bool> {
        let exists = self
            .conn
            .call(move |c| -> rusqlite::Result<bool> {
                let mut stmt =
                    c.prepare("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1")?;
                let count: i64 = stmt.query_row([name], |row| row.get(0))?;
                Ok(count > 0)
            })
            .await?;
        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_db_has_all_tables() {
        let db = Db::open_in_memory().await.unwrap();
        for table in EXPECTED_TABLES {
            assert!(
                db.table_exists(table).await.unwrap(),
                "expected table {table} to exist"
            );
        }
        assert!(!db.table_exists("no_such_table").await.unwrap());
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let db = Db::open_in_memory().await.unwrap();
        // Running the migration a second time must not error.
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();
    }

    #[tokio::test]
    async fn indexes_exist() {
        let db = Db::open_in_memory().await.unwrap();
        let names = db
            .conn()
            .call(|c| -> rusqlite::Result<Vec<String>> {
                let mut stmt = c
                    .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'")?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .await
            .unwrap();
        for expected in [
            "session_v2_project_idx",
            "session_message_session_seq_idx",
            "session_inbox_session_enqueued_seq_idx",
            "session_pending_session_admitted_seq_idx",
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "missing index {expected}"
            );
        }
    }
}
