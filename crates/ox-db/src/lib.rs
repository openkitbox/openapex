//! Storage layer: turso (SQLite-compatible, pure Rust, native async).
//!
//! [`Db`] owns the connection, its pragmas and the versioned schema; every
//! domain module owns the SQL for its own tables (see the crate's mctx/api
//! modules) and reaches the driver through [`Db::conn`]. There is no query
//! wrapper layer: the driver's async API is the API.
//!
//! The upstream-compatible table/column names are kept verbatim so the schema
//! stays recognisable against opencode v2's, but ox runs its own database file
//! and does not read or write the Bun server's.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{Mutex, MutexGuard, OwnedSemaphorePermit, Semaphore};
use turso::{Builder, Connection};

/// The SQL driver, re-exported so domain crates can name its types
/// (`ox_db::turso::Row`, …) without declaring a dependency on it.
pub use turso;

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
/// `digest` is the tagged content's FNV-1a hash as hex, so a re-tag can verify
/// identity cheaply (same content → same tag; changed content → the tag stays,
/// the digest is refreshed). `mc_drop` is the
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

/// The retrieval indexes behind `ctx_search` (Task 7), on turso.
///
/// turso indexes a *column* with a Tantivy-backed FTS index
/// (`CREATE INDEX … USING fts (cols)`), and the index follows every INSERT /
/// UPDATE / DELETE by itself. That is why there is no virtual table, no
/// trigger and no backfill-trigger maintenance here: the previous FTS5 design
/// needed nine triggers to keep a shadow table in step, and this needs none.
///
/// Sources are separated by indexing each source table, which replaces FTS5's
/// `UNINDEXED source` column. A compartment's searchable text is not a column
/// (`tiers` is JSON), so this migration adds one derived column, filled by the
/// single writer ([`crate::mctx::compartments`]) and backfilled here for rows
/// that predate it.
///
/// The backfill is an UPDATE, not an INSERT…SELECT: an upgraded database has
/// compartment rows whose new column is the empty default. `json_valid` guards
/// the malformed-JSON case, which raises in turso just as it does in SQLite.
const MIGRATION_V7: &str = r#"
ALTER TABLE mc_compartments ADD COLUMN search_text text NOT NULL DEFAULT '';
UPDATE mc_compartments SET search_text = CASE WHEN json_valid(tiers) THEN trim(
         COALESCE(json_extract(tiers, '$.title'), '') || ' ' ||
         COALESCE(json_extract(tiers, '$.p1'), '') || ' ' ||
         COALESCE(json_extract(tiers, '$.p2'), '') || ' ' ||
         COALESCE(json_extract(tiers, '$.p3'), '') || ' ' ||
         COALESCE(json_extract(tiers, '$.p4'), '')) ELSE '' END;
CREATE INDEX IF NOT EXISTS mc_memory_fts ON mc_memory USING fts (content);
CREATE INDEX IF NOT EXISTS mc_note_fts ON mc_note USING fts (title, body, surface_condition);
CREATE INDEX IF NOT EXISTS mc_compartments_fts ON mc_compartments USING fts (search_text);
"#;

/// The dreamer's per-`(project, task)` schedule state (Task 8). One row per
/// project and task — the same grain as the reference's
/// `task_schedule_state` (`storage-task-schedule.ts`) — holding everything the
/// interval scheduler needs to decide whether a task is due and to report how
/// its last run went:
///
/// - `last_run_at` is the START of the last *successful* run (the reference
///   stamps it from the start so work that lands during a run is not skipped;
///   see [`crate::mctx::dreamer`]). It is NULL until the first success, and a
///   failed or skipped run never moves it.
/// - `last_attempt_at` stamps every started run; together with
///   `consecutive_failures` it is the (simplified) retry bound: past
///   [`crate::mctx::dreamer::MAX_DREAM_FAILURES`] consecutive failures the row
///   is treated as the next interval slot rather than retried on every tick.
/// - `last_status`/`last_error` keep the terminal outcome and the failure
///   class plus message, so a failing task is diagnosable from the DB alone.
/// - `rotation_cursor` is the per-project category cursor for `curate`; it
///   lives here because it must survive restarts (a process-local cursor would
///   re-scan the same categories every boot).
///
/// The primary key is both the identity and the only index a lookup needs.
const MIGRATION_V8: &str = r#"
CREATE TABLE IF NOT EXISTS mc_dream_state (
  project text NOT NULL,
  task text NOT NULL,
  last_run_at integer,
  last_attempt_at integer NOT NULL DEFAULT 0,
  last_status text,
  last_error text,
  consecutive_failures integer NOT NULL DEFAULT 0,
  rotation_cursor integer NOT NULL DEFAULT 0,
  PRIMARY KEY (project, task)
);
"#;

/// The dreamer's own verification watermark (Task 8 follow-up). `updated_at`
/// answers "when was this memory written" and `verified_at` answers "when was
/// this memory last checked against the files it cites" — two different facts
/// that a single column cannot carry: banking a verification used to advance
/// `updated_at`, so a later content edit looked freshly verified, and a memory
/// that had never been checked looked checked.
///
/// `0` means never verified, which is what makes the verify task check a
/// memory's citations the first time it sees it. Additive, so it applies onto
/// any V8-stamped database.
const MIGRATION_V9: &str = r#"
ALTER TABLE mc_memory ADD COLUMN verified_at integer NOT NULL DEFAULT 0;
"#;

/// Stored memory vectors for semantic `ctx_search` (Task 10 follow-up).
///
/// Two columns, no timestamp: a row is "embedded for the current model" when
/// `embedding IS NOT NULL AND embedding_model = <identity>`, so a model change
/// (or a document prefix change, which is part of the identity) re-embeds by
/// the same test that fills a new row. Additive onto any V9 database.
const MIGRATION_V10: &str = r#"
ALTER TABLE mc_memory ADD COLUMN embedding BLOB;
ALTER TABLE mc_memory ADD COLUMN embedding_model TEXT;
"#;

/// The project-root `path` empty-string convention (session-list fix).
///
/// Upstream stores a root session's subpath as `''`, never NULL (`move.ts:89`
/// builds it with `path.relative`, which yields `''` at the root), and the TUI
/// lists sessions with `subpath=""` filters — so a NULL root path makes the
/// session list permanently empty. ox used to write NULL for roots; the
/// rewrite runs in [`Db::migrate`] after the static migrations because it
/// depends on whether a `project` table exists (a shared upstream DB narrows
/// the rewrite to rows whose directory equals the project worktree), and
/// SQLite raises "no such table" at prepare time, so static SQL cannot branch
/// on that.
const MIGRATION_V11: &str = r#""#;

/// One historian run per session, enforced by the database (context-budget
/// follow-up).
///
/// `claim_run` can then be a single `INSERT … ON CONFLICT DO NOTHING RETURNING
/// id`: no read-then-write window exists to lose, so two firings — in one
/// process or two over the same file — cannot both claim, and the constraint
/// survives any future caller that forgets the protocol.
///
/// The `UPDATE` comes first because the index cannot be built over duplicates,
/// and because a `running` row at migration time can only be a leftover: this
/// runs before the process serves anything, so no live run owns it.
const MIGRATION_V12: &str = r#"
UPDATE mc_historian_run
   SET state = 'aborted', error = 'superseded when the schema gained the claim index'
 WHERE state = 'running';
CREATE UNIQUE INDEX IF NOT EXISTS mc_historian_run_running_idx
    ON mc_historian_run (session_id) WHERE state = 'running';
"#;

/// V13: purge magic-context rows whose session no longer exists. The six
/// `mc_*` tables that carry a `session_id` and no foreign key were never
/// reached by the cascade from `session_v2`; deletions made before the
/// projector cleared them explicitly left orphans behind (28 `mc_tag` rows in
/// the production database). `mc_memory` is project-scoped and
/// `mc_dream_state` is (project, task)-scoped, so neither is touched.
///
/// The `event` index is part of the same retention pass: `event.created` is the
/// only key the replay-log sweep filters on, and without it the sweep scans the
/// whole log (114 MB in the production database).
const MIGRATION_V13: &str = r#"
DELETE FROM mc_compartments WHERE session_id NOT IN (SELECT id FROM session_v2);
DELETE FROM mc_tag WHERE session_id NOT IN (SELECT id FROM session_v2);
DELETE FROM mc_drop WHERE session_id NOT IN (SELECT id FROM session_v2);
DELETE FROM mc_historian_run WHERE session_id NOT IN (SELECT id FROM session_v2);
DELETE FROM mc_frozen_state WHERE session_id NOT IN (SELECT id FROM session_v2);
DELETE FROM mc_note WHERE session_id NOT IN (SELECT id FROM session_v2);
CREATE INDEX IF NOT EXISTS event_created_idx ON event (created);
"#;

/// V14: the memory selection weight (`importance`), fed by the dreamer's
/// classify task (see `.ox/plans/ox-context-management-backlog.md`, M1/M2).
///
/// The pinboard is a fixed prefix paid on every request, so which memories get
/// in is a budget decision; `importance` is the first sort key of that
/// decision (`ox-engine::mctx::memory::select_memories`). `0` is the
/// pre-classify value: every row ties, and selection falls back to the last
/// touch (`max(verified_at, updated_at)`). Purely additive, so it applies onto
/// any V13-stamped database.
const MIGRATION_V14: &str = r#"
ALTER TABLE mc_memory ADD COLUMN importance real NOT NULL DEFAULT 0;
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
    (7, MIGRATION_V7),
    (8, MIGRATION_V8),
    (9, MIGRATION_V9),
    (10, MIGRATION_V10),
    (11, MIGRATION_V11),
    (12, MIGRATION_V12),
    (13, MIGRATION_V13),
    (14, MIGRATION_V14),
];

/// Async handle to the database.
#[derive(Clone)]
pub struct Db {
    database: turso::Database,
    /// Idle connections, ready to hand out. A pooled [`Connection`] serves one
    /// operation at a time: turso refuses concurrent use of a connection
    /// ("concurrent use forbidden" — a non-blocking check in its per-connection
    /// guard), so a connection cannot be shared the way a mutex-guarded
    /// `sqlite3*` can.
    idle: Arc<Mutex<Vec<Connection>>>,
    /// Caps live connections; each carries its own page cache.
    permits: Arc<Semaphore>,
    /// The ephemeral database file behind [`Db::open_in_memory`], deleted when
    /// the handle goes away.
    ephemeral: Option<Arc<EphemeralFile>>,
    /// One writer at a time. ox's transactions are short, and serializing them
    /// here keeps `BEGIN IMMEDIATE` from ever meeting a busy database.
    write_gate: Arc<Mutex<()>>,
}

/// Most connections ox will hold open at once.
const MAX_CONNECTIONS: usize = 8;

/// Connections kept for reuse after a burst. The rest are closed, releasing
/// their page caches: a single-user server's steady state is one reader plus
/// the occasional writer, and each idle connection costs memory (the reason
/// this server exists).
const MAX_IDLE: usize = 2;

impl Db {
    /// Open (creating if needed) the database at `path` and apply migrations.
    /// Parent directories are created automatically.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // turso takes the path as a `&str`; a lossy conversion would silently
        // open a different file, so a non-UTF-8 path is an error.
        let Some(path_str) = path.to_str() else {
            anyhow::bail!("database path is not valid UTF-8: {}", path.display());
        };
        Self::open_path(path_str).await
    }

    /// Open a throwaway database (for tests and ephemeral runs).
    ///
    /// A file rather than `:memory:`: a turso in-memory database is one
    /// database *per connection*, so a pool of connections would each see an
    /// empty look-alike. The directory is removed when the handle drops.
    pub async fn open_in_memory() -> Result<Self> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ox-db-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        let ephemeral = Arc::new(EphemeralFile(dir.clone()));
        let mut db = Self::open_path(dir.join("ox.db").to_str().unwrap_or_default()).await?;
        db.ephemeral = Some(ephemeral);
        Ok(db)
    }

    async fn open_path(path: &str) -> Result<Self> {
        // `experimental_index_method` is required for `CREATE INDEX … USING
        // fts`, which is how the search indexes are declared.
        let database = Builder::new_local(path)
            .experimental_index_method(true)
            .build()
            .await?;
        let db = Self {
            database,
            idle: Arc::new(Mutex::new(Vec::new())),
            permits: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            ephemeral: None,
            write_gate: Arc::new(Mutex::new(())),
        };
        db.migrate().await?;
        Ok(db)
    }

    /// Borrow a connection for the duration of one operation.
    ///
    /// The guard derefs to [`Connection`] and returns the connection to the
    /// pool on drop; holding it longer than a single operation is what makes
    /// concurrent use impossible.
    pub async fn acquire(&self) -> Result<Conn<'_>> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("connection pool closed"))?;
        let idle = {
            let mut idle = self.idle.lock().await;
            idle.pop()
        };
        let conn = match idle {
            Some(conn) => conn,
            None => self.connect().await?,
        };
        // A transaction left open by a writer that failed mid-way must not be
        // handed to the next user: statements would run inside it and either
        // commit with the next COMMIT or vanish with a later rollback.
        if !conn.is_autocommit()? {
            // `execute`, not `query`: turso answers a non-returning statement
            // sent through `query` with success and does nothing.
            conn.execute("ROLLBACK", ()).await?;
        }
        Ok(Conn {
            db: self,
            conn: Some(conn),
            _permit: permit,
        })
    }

    /// One fresh connection with ox's pragmas. WAL is persistent in the file,
    /// but the busy timeout and foreign keys are per connection, so every
    /// connection sets them.
    async fn connect(&self) -> Result<Connection> {
        let conn = self.database.connect()?;
        conn.pragma_update("journal_mode", "'wal'").await?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update("foreign_keys", "ON").await?;
        Ok(conn)
    }

    /// Begin a write transaction, waiting for any other writer.
    pub async fn begin(&self) -> Result<WriteTx<'_>> {
        let gate = self.write_gate.lock().await;
        let conn = self.acquire().await?;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        Ok(WriteTx { conn, _gate: gate })
    }

    /// Apply pending migrations in version order. Each migration runs in a
    /// transaction that bumps `PRAGMA user_version`, so re-running is a no-op
    /// once the database is current. After the static migrations, the V11
    /// data rewrite runs (its SQL depends on whether a `project` table
    /// exists, which static migration SQL cannot branch on).
    pub async fn migrate(&self) -> Result<()> {
        let current = self.user_version().await?;
        for &(version, sql) in MIGRATIONS {
            if version <= current {
                continue;
            }
            let tx = self.begin().await?;
            tx.execute_batch(sql).await?;
            tx.pragma_update("user_version", version).await?;
            tx.commit().await?;
            tracing::info!(version, "applied db migration");
        }
        if current < 11 {
            self.backfill_root_paths().await?;
        }
        Ok(())
    }

    /// MIGRATION_V11's data rewrite: NULL root paths become `''` (see the
    /// constant's doc comment). Idempotent and bounded by the same
    /// `user_version < 11` gate as the static migrations.
    async fn backfill_root_paths(&self) -> Result<()> {
        let tx = self.begin().await?;
        // With a shared upstream `project` table, only a directory equal to
        // its project's worktree is a root; without one, every row's directory
        // is the synthetic project's root by construction. A subpath that
        // genuinely nests keeps its NULL (upstream also stores nested paths,
        // never NULL, so this branch is defensive for malformed rows only).
        if self.table_exists("project").await.unwrap_or(false) {
            tx.execute(
                "UPDATE session_v2 SET path = ''
                 WHERE path IS NULL
                   AND directory = (SELECT worktree FROM project WHERE project.id = session_v2.project_id)",
                (),
            )
            .await?;
        } else {
            tx.execute("UPDATE session_v2 SET path = '' WHERE path IS NULL", ())
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Current `PRAGMA user_version` (migration level).
    async fn user_version(&self) -> Result<u32> {
        let conn = self.acquire().await?;
        let mut stmt = conn.prepare("PRAGMA user_version").await?;
        let mut rows = stmt.query(()).await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<i64>(0)? as u32),
            None => Ok(0),
        }
    }

    /// Check whether a table exists.
    pub async fn table_exists(&self, name: &'static str) -> Result<bool> {
        let conn = self.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?1")
            .await?;
        let mut rows = stmt.query([name]).await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<i64>(0)? > 0),
            None => Ok(false),
        }
    }
}

/// Removes a throwaway database's directory on drop.
struct EphemeralFile(std::path::PathBuf);

impl Drop for EphemeralFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A pooled connection, borrowed for one operation.
pub struct Conn<'a> {
    db: &'a Db,
    conn: Option<Connection>,
    _permit: OwnedSemaphorePermit,
}

impl std::ops::Deref for Conn<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection present until drop")
    }
}

impl Drop for Conn<'_> {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        // A writer that failed mid-transaction leaves it open. Settle it before
        // the connection is reused or released: an open write transaction holds
        // the database's write lock, and the next writer would wait out its
        // busy timeout. Rollback is async, so it runs on a task; without a
        // runtime the connection is simply dropped, and closing it rolls back.
        let open_transaction = matches!(conn.is_autocommit(), Ok(false));
        if open_transaction && let Ok(handle) = tokio::runtime::Handle::try_current() {
            let idle = Arc::clone(&self.db.idle);
            handle.spawn(async move {
                let _ = conn.execute("ROLLBACK", ()).await;
                let mut idle = idle.lock().await;
                if idle.len() < MAX_IDLE {
                    idle.push(conn);
                }
            });
            return;
        }
        // `try_lock` is enough: an idle slot is an optimisation.
        if let Ok(mut idle) = self.db.idle.try_lock()
            && idle.len() < MAX_IDLE
        {
            idle.push(conn);
        }
    }
}

/// A write transaction: a pooled connection plus the write gate.
///
/// Held by one task for the transaction's whole life, so prepared statements
/// inside it are the only place prepared *writes* may live — outside a
/// transaction they would be sharing a connection that another task may borrow
/// at any moment.
pub struct WriteTx<'a> {
    conn: Conn<'a>,
    _gate: MutexGuard<'a, ()>,
}

impl WriteTx<'_> {
    /// Commit and release the connection.
    pub async fn commit(self) -> Result<()> {
        self.conn.execute("COMMIT", ()).await?;
        Ok(())
    }

    /// Roll back and release the connection.
    pub async fn rollback(self) -> Result<()> {
        self.conn.execute("ROLLBACK", ()).await?;
        Ok(())
    }
}

impl std::ops::Deref for WriteTx<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

/// Read an optional row from a prepared statement.
///
/// turso reports "no rows" as an error variant rather than an empty stream, and
/// 33 rusqlite call sites used `OptionalExtension::optional` for exactly this
/// distinction. Keeping it in one place is what makes that distinction
/// impossible to get wrong: a real error must not be read as an absent row.
pub async fn optional(
    stmt: &mut turso::Statement,
    params: impl turso::IntoParams,
) -> Result<Option<turso::Row>> {
    match stmt.query_row(params).await {
        Ok(row) => Ok(Some(row)),
        Err(turso::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
