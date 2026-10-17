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
-- Indexes verified against upstream packages/core/src/database/schema.gen.ts.
CREATE UNIQUE INDEX IF NOT EXISTS event_aggregate_seq_idx ON event (aggregate_id, seq);
CREATE INDEX IF NOT EXISTS event_aggregate_type_seq_idx ON event (aggregate_id, type, seq);
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
CREATE INDEX IF NOT EXISTS session_v2_workspace_idx ON session_v2 (workspace_id);
CREATE INDEX IF NOT EXISTS session_v2_parent_idx ON session_v2 (parent_id);
CREATE INDEX IF NOT EXISTS session_v2_time_suspended_idx ON session_v2 (time_suspended) WHERE time_suspended IS NOT NULL;
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
CREATE INDEX IF NOT EXISTS session_message_session_type_seq_idx ON session_message (session_id, type, seq);
CREATE INDEX IF NOT EXISTS session_message_session_time_created_id_idx ON session_message (session_id, time_created, id);
CREATE INDEX IF NOT EXISTS session_message_time_created_idx ON session_message (time_created);
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
CREATE INDEX IF NOT EXISTS session_inbox_session_delivery_seq_idx ON session_inbox (session_id, delivery, enqueued_seq);
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
CREATE INDEX IF NOT EXISTS session_pending_session_delivery_seq_idx ON session_pending (session_id, delivery, admitted_seq);
CREATE UNIQUE INDEX IF NOT EXISTS session_pending_session_compaction_idx ON session_pending (session_id) WHERE type = 'compaction';
"#;

/// All indexes the migration creates, verbatim from upstream
/// (`packages/core/src/database/schema.gen.ts`). Kept next to the schema so
/// the test stays in sync with what we actually create.
#[cfg(test)]
const EXPECTED_INDEXES: &[&str] = &[
    "event_aggregate_seq_idx",
    "event_aggregate_type_seq_idx",
    "session_v2_project_idx",
    "session_v2_workspace_idx",
    "session_v2_parent_idx",
    "session_v2_time_suspended_idx",
    "session_message_session_seq_idx",
    "session_message_session_type_seq_idx",
    "session_message_session_time_created_id_idx",
    "session_message_time_created_idx",
    "session_inbox_session_enqueued_seq_idx",
    "session_inbox_session_delivery_seq_idx",
    "session_pending_session_admitted_seq_idx",
    "session_pending_session_delivery_seq_idx",
    "session_pending_session_compaction_idx",
];

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
        // (name, is_unique) per index, via sqlite_master + pragma_index_list.
        let indexes = db
            .conn()
            .call(|c| -> rusqlite::Result<Vec<(String, bool)>> {
                let mut stmt = c.prepare(
                    "SELECT name, tbl_name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'",
                )?;
                let rows: Vec<(String, String)> = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut out = Vec::new();
                for (name, table) in rows {
                    let mut s = c.prepare(&format!(
                        "SELECT \"unique\" FROM pragma_index_list('{table}') WHERE name = '{name}'"
                    ))?;
                    let unique: bool = s.query_row([], |row| row.get(0))?;
                    out.push((name, unique));
                }
                Ok(out)
            })
            .await
            .unwrap();

        let names: Vec<&str> = indexes.iter().map(|(n, _)| n.as_str()).collect();
        for expected in EXPECTED_INDEXES {
            assert!(names.contains(expected), "missing index {expected}");
        }
        assert_eq!(
            names.len(),
            EXPECTED_INDEXES.len(),
            "unexpected extra indexes: {names:?}"
        );

        // Unique indexes enforce shared-DB invariants, not just performance.
        for unique in [
            "event_aggregate_seq_idx",
            "session_message_session_seq_idx",
            "session_inbox_session_enqueued_seq_idx",
            "session_pending_session_admitted_seq_idx",
            "session_pending_session_compaction_idx",
        ] {
            let (_, is_unique) = indexes
                .iter()
                .find(|(n, _)| n == unique)
                .unwrap_or_else(|| panic!("missing index {unique}"));
            assert!(is_unique, "index {unique} must be UNIQUE");
        }
    }
}
