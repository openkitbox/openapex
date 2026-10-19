//! SQLite storage layer.
//!
//! Wraps a `tokio_rusqlite::Connection` opened on the configured path and
//! applies the upstream-compatible schema as an idempotent migration. The
//! table/column names MUST match opencode v2's database exactly so that the
//! ox server and the Bun server can read each other's DB files.

use std::path::Path;

use anyhow::Result;
use tokio_rusqlite::Connection;

/// Embedded upstream-compatible schema (opencode v2), migration version 1.
/// Idempotent within itself: every statement uses `IF NOT EXISTS` (so it is
/// safe to apply onto a DB the upstream Bun server already created).
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

/// magic-context tables (ox-owned, `mc_` prefix). JSON payloads are stored
/// as TEXT per project convention. Deliberately no foreign keys: rows are
/// keyed by session id and managed by the mctx layer itself, so deleting a
/// session must not silently depend on cascade order.
const MIGRATION_V2: &str = r#"
CREATE TABLE IF NOT EXISTS mc_frozen_state (
  session_id text PRIMARY KEY,
  state text NOT NULL,
  version integer NOT NULL
);
CREATE TABLE IF NOT EXISTS mc_compartments (
  id text PRIMARY KEY,
  session_id text NOT NULL,
  coverage_end text NOT NULL,
  importance real NOT NULL,
  tiers text NOT NULL,
  facts text NOT NULL,
  events text NOT NULL,
  created_at integer NOT NULL
);
CREATE TABLE IF NOT EXISTS mc_memory (
  id text PRIMARY KEY,
  project text NOT NULL,
  content text NOT NULL,
  category text NOT NULL,
  created_at integer NOT NULL,
  updated_at integer NOT NULL
);
CREATE TABLE IF NOT EXISTS mc_note (
  id text PRIMARY KEY,
  session_id text NOT NULL,
  title text NOT NULL,
  body text NOT NULL,
  status text NOT NULL,
  surface_condition text,
  created_at integer NOT NULL,
  updated_at integer NOT NULL
);
CREATE TABLE IF NOT EXISTS mc_historian_run (
  id text PRIMARY KEY,
  session_id text NOT NULL,
  state text NOT NULL,
  chunk_ref text,
  attempt integer NOT NULL,
  created_at integer NOT NULL
);
CREATE INDEX IF NOT EXISTS mc_compartments_session_idx ON mc_compartments (session_id);
CREATE INDEX IF NOT EXISTS mc_memory_project_idx ON mc_memory (project);
CREATE INDEX IF NOT EXISTS mc_note_session_idx ON mc_note (session_id);
CREATE INDEX IF NOT EXISTS mc_historian_run_session_idx ON mc_historian_run (session_id);
"#;

/// magic-context tag/drop tables (Task 3). `mc_tag` is the durable
/// §N§-tag ↔ message-part identity mapping: tags are per-session, minted in
/// arrival order, and STABLE across passes (the same item keeps its tag).
/// `digest` is the first 16 hex chars of the tagged content's SHA-256, so a
/// re-tag can verify identity cheaply (same content → same tag; changed
/// content → the tag stays, the digest is refreshed). `mc_drop` is the
/// ctx_reduce queue: rows start `state = 'queued'`, and `drain_drops` marks
/// them `'applied'` when the render pipeline consumes them (the rows are
/// kept so `ctx_expand` can report whether content is currently dropped).
const MIGRATION_V3: &str = r#"
CREATE TABLE IF NOT EXISTS mc_tag (
  session_id text NOT NULL,
  tag integer NOT NULL,
  message_id text NOT NULL,
  part_path text NOT NULL,
  digest text NOT NULL,
  PRIMARY KEY (session_id, tag)
);
CREATE TABLE IF NOT EXISTS mc_drop (
  session_id text NOT NULL,
  tag integer NOT NULL,
  reason text NOT NULL,
  state text NOT NULL DEFAULT 'queued',
  created_at integer NOT NULL,
  PRIMARY KEY (session_id, tag)
);
CREATE INDEX IF NOT EXISTS mc_tag_session_message_idx ON mc_tag (session_id, message_id);
"#;

/// magic-context memory lifecycle columns (Task 4). `mc_memory` gains the
/// archive lifecycle the reference has: archived rows stay `get`-able and
/// their `archive_reason` is kept (`"merged"` marks merge losers). Purely
/// additive (`ALTER TABLE ADD COLUMN`), so it applies onto any V3-stamped
/// database; fresh databases get the columns via V2 + this migration in
/// sequence — V2 itself stays byte-identical because it is already stamped
/// into existing databases.
const MIGRATION_V4: &str = r#"
ALTER TABLE mc_memory ADD COLUMN status text NOT NULL DEFAULT 'active';
ALTER TABLE mc_memory ADD COLUMN archive_reason text;
"#;

/// magic-context historian run lifecycle columns (Task 6a). `mc_historian_run`
/// gains the settle half the historian's durable "host" needs: `updated_at`
/// stamps the terminal transition (a run's duration is `updated_at -
/// created_at`, which no other column records) and `error` keeps the failure
/// text when a run settles `failed` (or is aborted at startup). Purely additive
/// (`ALTER TABLE ADD COLUMN`), so it applies onto any V4-stamped database; V4
/// itself stays byte-identical because it is already stamped.
const MIGRATION_V5: &str = r#"
ALTER TABLE mc_historian_run ADD COLUMN updated_at integer NOT NULL DEFAULT 0;
ALTER TABLE mc_historian_run ADD COLUMN error text;
"#;

/// magic-context side-channel columns (Task 6b). `mc_compartments` carried
/// `facts`/`events` columns from Task 1, but a compartment has no such fields in
/// the reference (`mc-store/src/lib.rs:583`) and the historian emits both
/// **once per run**, not per compartment: they are the run's memory-promotion
/// channel, so they belong on the run row that produced them. V6 therefore drops
/// the two dead columns and gives the run row the pair. The drops are safe on
/// any existing database: nothing ever wrote anything but the neutral `{}`/`[]`
/// placeholders into them.
///
/// `mc_compartments` is never empty in practice, so this is a real column drop,
/// not a table rewrite: SQLite drops a column in place when it is not indexed
/// and no view or partial index references it (neither column is).
const MIGRATION_V6: &str = r#"
ALTER TABLE mc_compartments DROP COLUMN facts;
ALTER TABLE mc_compartments DROP COLUMN events;
ALTER TABLE mc_historian_run ADD COLUMN facts text;
ALTER TABLE mc_historian_run ADD COLUMN events text;
"#;

/// Versioned migrations, applied in order. `PRAGMA user_version` tracks how
/// far the database has been migrated; each migration runs in a transaction
/// that bumps `user_version` to its own version number.
const MIGRATIONS: &[(u32, &str)] = &[
    (1, MIGRATION_V1),
    (2, MIGRATION_V2),
    (3, MIGRATION_V3),
    (4, MIGRATION_V4),
    (5, MIGRATION_V5),
    (6, MIGRATION_V6),
];

/// All indexes the migration creates: V1's verbatim from upstream
/// (`packages/core/src/database/schema.gen.ts`), plus ox-owned `mc_*` indexes
/// from MIGRATION_V2. Kept next to the schema so the test stays in sync with
/// what we actually create.
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
    "mc_compartments_session_idx",
    "mc_memory_project_idx",
    "mc_note_session_idx",
    "mc_historian_run_session_idx",
    "mc_tag_session_message_idx",
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
    "mc_frozen_state",
    "mc_compartments",
    "mc_memory",
    "mc_note",
    "mc_historian_run",
    "mc_tag",
    "mc_drop",
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

    /// Open an in-memory database (for tests and ephemeral runs).
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

    /// Apply pending migrations in version order. Each migration runs in a
    /// transaction and bumps `PRAGMA user_version`, so re-running is a no-op
    /// once the database is current.
    pub async fn migrate(&self) -> Result<()> {
        self.conn
            .call(|c| -> rusqlite::Result<()> {
                let current: u32 = c.pragma_query_value(None, "user_version", |row| row.get(0))?;
                for &(version, sql) in MIGRATIONS {
                    if version <= current {
                        continue;
                    }
                    let tx = c.transaction()?;
                    tx.execute_batch(sql)?;
                    tx.pragma_update(None, "user_version", version)?;
                    tx.commit()?;
                    tracing::info!(version, "applied db migration");
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Access the underlying connection for queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Current `PRAGMA user_version` (migration level).
    #[cfg(test)]
    async fn user_version(&self) -> u32 {
        self.conn
            .call(|c| -> rusqlite::Result<u32> {
                c.pragma_query_value(None, "user_version", |row| row.get(0))
            })
            .await
            .unwrap()
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
    async fn migration_tracks_user_version_and_is_idempotent() {
        let db = Db::open_in_memory().await.unwrap();
        assert_eq!(db.user_version().await, 6);
        // Re-running must be a no-op: version stays, no error.
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();
        assert_eq!(db.user_version().await, 6);
    }

    #[tokio::test]
    async fn mc_migration_creates_tables_and_indexes() {
        let db = Db::open_in_memory().await.unwrap();
        let names = db
            .conn()
            .call(|c| -> rusqlite::Result<Vec<String>> {
                let mut stmt = c.prepare(
                    "SELECT name FROM sqlite_master WHERE name LIKE 'mc\\_%' ESCAPE '\\' ORDER BY name",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect()
            })
            .await
            .unwrap();
        assert_eq!(
            names,
            vec![
                "mc_compartments",
                "mc_compartments_session_idx",
                "mc_drop",
                "mc_frozen_state",
                "mc_historian_run",
                "mc_historian_run_session_idx",
                "mc_memory",
                "mc_memory_project_idx",
                "mc_note",
                "mc_note_session_idx",
                "mc_tag",
                "mc_tag_session_message_idx",
            ]
        );
    }

    #[tokio::test]
    async fn migration_v2_upgrades_an_existing_v1_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade.db");
        // Build a v1 database by hand: apply only MIGRATION_V1 and stamp it.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await, 6);
        for table in [
            "mc_frozen_state",
            "mc_compartments",
            "mc_memory",
            "mc_note",
            "mc_historian_run",
        ] {
            assert!(
                db.table_exists(table).await.unwrap(),
                "expected table {table} after upgrade"
            );
        }
    }

    #[tokio::test]
    async fn migration_v3_upgrades_an_existing_v2_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade3.db");
        // Build a v2 database by hand: apply V1+V2 and stamp version 2.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(MIGRATION_V2).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await, 6);
        for table in ["mc_tag", "mc_drop"] {
            assert!(
                db.table_exists(table).await.unwrap(),
                "expected table {table} after upgrade"
            );
        }
        // The mc_drop default state and the tag lookup index survive the
        // upgrade path exactly as on a fresh database.
        let indexes = db
            .conn()
            .call(|c| -> rusqlite::Result<i64> {
                c.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='mc_tag_session_message_idx'",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(indexes, 1);
        let db2 = Db::open_in_memory().await.unwrap();
        db2.conn()
            .call(|c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO mc_drop (session_id, tag, reason, created_at) VALUES ('s', 7, 'r', 0)",
                    [],
                )?;
                let state: String = c.query_row(
                    "SELECT state FROM mc_drop WHERE session_id = 's' AND tag = 7",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(state, "queued", "state column defaults to 'queued'");
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn migration_v4_upgrades_an_existing_v3_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade4.db");
        // Build a v3 database by hand: apply V1..V3 and stamp version 3.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(MIGRATION_V2).unwrap();
            conn.execute_batch(MIGRATION_V3).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
            // A pre-V4 row: no lifecycle columns yet.
            conn.execute(
                "INSERT INTO mc_memory (id, project, content, category, created_at, updated_at)
                 VALUES ('mem_old', '/p', 'pre-existing fact', 'ARCHITECTURE', 1, 1)",
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await, 6);
        // The pre-existing row reads back with the 'active' default and no
        // archive reason — the additive columns converge both DB histories.
        let (status, reason): (String, Option<String>) = db
            .conn()
            .call(|c| {
                c.query_row(
                    "SELECT status, archive_reason FROM mc_memory WHERE id = 'mem_old'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .await
            .unwrap();
        assert_eq!(status, "active");
        assert_eq!(reason, None);
        // Explicit archived rows round-trip both new columns.
        db.conn()
            .call(|c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO mc_memory (id, project, content, category, created_at,
                        updated_at, status, archive_reason)
                     VALUES ('mem_a', '/p', 'gone', 'NAMING', 2, 2, 'archived', 'merged')",
                    [],
                )?;
                let (status, reason): (String, Option<String>) = c.query_row(
                    "SELECT status, archive_reason FROM mc_memory WHERE id = 'mem_a'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(status, "archived");
                assert_eq!(reason.as_deref(), Some("merged"));
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn migration_v5_upgrades_an_existing_v4_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade5.db");
        // Build a v4 database by hand: apply V1..V4 and stamp version 4.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(MIGRATION_V2).unwrap();
            conn.execute_batch(MIGRATION_V3).unwrap();
            conn.execute_batch(MIGRATION_V4).unwrap();
            conn.pragma_update(None, "user_version", 4).unwrap();
            // A pre-V5 historian run row: no settle columns yet.
            conn.execute(
                "INSERT INTO mc_historian_run (id, session_id, state, chunk_ref, attempt, created_at)
                 VALUES ('hr_old', 'ses_1', 'running', '0..3', 1, 1)",
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await, 6);
        // The pre-existing row reads back with the additive defaults.
        let (updated_at, error): (i64, Option<String>) = db
            .conn()
            .call(|c| {
                c.query_row(
                    "SELECT updated_at, error FROM mc_historian_run WHERE id = 'hr_old'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .await
            .unwrap();
        assert_eq!(updated_at, 0);
        assert_eq!(error, None);
    }

    #[tokio::test]
    async fn migration_v6_moves_the_side_channels_off_the_compartment_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade6.db");
        // Build a v5 database by hand: apply V1..V5 and stamp version 5.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(MIGRATION_V2).unwrap();
            conn.execute_batch(MIGRATION_V3).unwrap();
            conn.execute_batch(MIGRATION_V4).unwrap();
            conn.execute_batch(MIGRATION_V5).unwrap();
            conn.pragma_update(None, "user_version", 5).unwrap();
            // A pre-V6 compartment row carries the (neutral) placeholders.
            conn.execute(
                "INSERT INTO mc_compartments
                     (id, session_id, coverage_end, importance, tiers, facts, events, created_at)
                 VALUES ('mc_old', 'ses_1', '4', 50.0, '{}', '{}', '[]', 1)",
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await, 6);
        let (compartments, runs): (Vec<String>, Vec<String>) = db
            .conn()
            .call(|c| -> rusqlite::Result<(Vec<String>, Vec<String>)> {
                let mut stmt =
                    c.prepare("SELECT name FROM pragma_table_info('mc_compartments')")?;
                let compartments = stmt
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                let mut stmt =
                    c.prepare("SELECT name FROM pragma_table_info('mc_historian_run')")?;
                let runs = stmt
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok((compartments, runs))
            })
            .await
            .unwrap();
        // The compartment row's side channels are gone; the run row owns them.
        // The surviving row still loads (it only ever held placeholders).
        assert!(!compartments.iter().any(|c| c == "facts"));
        assert!(!compartments.iter().any(|c| c == "events"));
        assert!(runs.iter().any(|c| c == "facts"));
        assert!(runs.iter().any(|c| c == "events"));
        let compartments = crate::mctx::compartments::load(&db, "ses_1").await.unwrap();
        assert_eq!(compartments.len(), 1);
        assert_eq!(compartments[0].coverage_end, "4");
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
