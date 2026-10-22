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
];

/// All indexes the migration creates: V1's verbatim from upstream
/// (`packages/core/src/database/schema.gen.ts`), plus ox-owned `mc_*` indexes
/// from MIGRATION_V2. Kept next to the schema so the test stays in sync with
/// what we actually create.
#[cfg(test)]
const EXPECTED_INDEXES: &[&str] = &[
    "event_aggregate_seq_idx",
    "event_aggregate_type_seq_idx",
    "event_created_idx",
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
    "mc_historian_run_running_idx",
    "mc_historian_run_session_idx",
    "mc_tag_session_message_idx",
    // The FTS indexes (turso indexes an expression-free column list, and the
    // index is a real `sqlite_master` index, not a shadow table).
    "mc_memory_fts",
    "mc_note_fts",
    "mc_compartments_fts",
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
    "mc_dream_state",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare connection with the same builder settings [`Db`] uses — the same
    /// driver that will migrate the fixture, so a hand-built database is a
    /// real upgrade fixture rather than a foreign one.
    async fn raw(path: &Path) -> Connection {
        let db = Builder::new_local(path.to_str().unwrap())
            .experimental_index_method(true)
            .build()
            .await
            .unwrap();
        db.connect().unwrap()
    }

    /// Build a database as it exists after `version`, by hand.
    async fn build_upto(path: &Path, version: u32) {
        let conn = raw(path).await;
        for &(v, sql) in MIGRATIONS {
            if v <= version {
                conn.execute_batch(sql).await.unwrap();
            }
        }
        conn.pragma_update("user_version", version).await.unwrap();
    }

    async fn query_row(db: &Db, sql: &str) -> turso::Row {
        let conn = db.acquire().await.unwrap();
        let mut stmt = conn.prepare(sql).await.unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        rows.next().await.unwrap().expect("expected one row")
    }

    async fn count(db: &Db, sql: &str) -> i64 {
        query_row(db, sql).await.get::<i64>(0).unwrap()
    }

    /// Seed the `session_v2` row a fixture's `mc_*` rows hang off. Since V13 the
    /// session-scoped magic-context tables are purged when their session is
    /// missing, so a fixture without it reads as an orphan.
    async fn seed_session(conn: &Connection, sid: &str) {
        conn.execute(
            &format!(
                "INSERT INTO session_v2
                   (id, project_id, slug, directory, version, time_created, time_updated)
                 VALUES ('{sid}', 'proj', '{sid}', '/p', '1', 1, 1)"
            ),
            (),
        )
        .await
        .unwrap();
    }

    async fn columns(db: &Db, table: &str) -> Vec<String> {
        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .await
            .unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        let mut names = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            names.push(row.get::<String>(0).unwrap());
        }
        names
    }

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

    /// V11 rewrites NULL root paths to `''`: the TUI's session list filters
    /// `subpath=""`, which upstream answers for root sessions because it
    /// stores roots as `''`, never NULL. Without a `project` table (ox's own
    /// databases) every NULL path is a synthetic-project root.
    #[tokio::test]
    async fn migration_v11_rewrites_null_root_paths_to_empty_strings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade11.db");
        build_upto(&path, 10).await;
        {
            let conn = raw(&path).await;
            for (id, path_col) in [("root", None), ("nested", Some("sub/dir"))] {
                conn.execute(
                    "INSERT INTO session_v2 (id, project_id, slug, directory, path, version, time_created, time_updated)
                     VALUES (?1, 'default', 's', '/p', ?2, 'test', 1, 1)",
                    (id, path_col),
                )
                .await
                .unwrap();
            }
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(
            count(&db, "SELECT count(*) FROM session_v2 WHERE path = ''").await,
            1,
            "the root path becomes the empty string"
        );
        assert_eq!(
            count(&db, "SELECT count(*) FROM session_v2 WHERE path IS NULL").await,
            0,
            "no NULL paths remain"
        );
        let row = query_row(&db, "SELECT path FROM session_v2 WHERE id = 'nested'").await;
        assert_eq!(
            row.get::<Option<String>>(0).unwrap().as_deref(),
            Some("sub/dir"),
            "nested subpaths are untouched"
        );
    }

    /// The version this build migrates to, so a new migration does not have to
    /// be chased through every upgrade test.
    fn latest_version() -> u32 {
        MIGRATIONS.last().expect("at least one migration").0
    }

    /// V13 removes magic-context rows whose session no longer exists: those
    /// six tables carry no foreign key, so deletions made before the projector
    /// learned to clear them left orphans behind (measured on the production
    /// database: 28 `mc_tag` rows).
    #[tokio::test]
    async fn migration_v13_purges_orphaned_magic_context_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orphans.db");
        build_upto(&path, 12).await;
        {
            let conn = raw(&path).await;
            conn.execute(
                "INSERT INTO session_v2
                   (id, project_id, slug, directory, version, time_created, time_updated)
                 VALUES ('ses_live', 'proj', 'slug', '/tmp/ox', '1', 1, 1)",
                (),
            )
            .await
            .unwrap();
            for (id, sid) in [("live", "ses_live"), ("dead", "ses_dead")] {
                for sql in [
                    format!(
                        "INSERT INTO mc_compartments
                           (id, session_id, coverage_end, importance, tiers, created_at)
                         VALUES ('c-{id}', '{sid}', '4', 50, '{{}}', 1)"
                    ),
                    format!(
                        "INSERT INTO mc_tag (session_id, tag, message_id, part_path, digest)
                         VALUES ('{sid}', 1, 'm', 'p', 'd')"
                    ),
                    format!(
                        "INSERT INTO mc_drop (session_id, tag, reason, state, created_at)
                         VALUES ('{sid}', 1, 'r', 'queued', 1)"
                    ),
                    format!(
                        "INSERT INTO mc_historian_run
                           (id, session_id, state, attempt, created_at, updated_at)
                         VALUES ('h-{id}', '{sid}', 'done', 1, 1, 1)"
                    ),
                    format!(
                        "INSERT INTO mc_frozen_state (session_id, state, version)
                         VALUES ('{sid}', '{{}}', 1)"
                    ),
                    format!(
                        "INSERT INTO mc_note
                           (id, session_id, title, body, status, surface_condition,
                            created_at, updated_at)
                         VALUES ('n-{id}', '{sid}', 't', 'b', 'active', NULL, 1, 1)"
                    ),
                ] {
                    conn.execute(sql.as_str(), ()).await.unwrap();
                }
            }
        }

        let upgraded = Db::open(&path).await.unwrap();
        assert_eq!(upgraded.user_version().await.unwrap(), latest_version());
        for table in [
            "mc_compartments",
            "mc_tag",
            "mc_drop",
            "mc_historian_run",
            "mc_frozen_state",
            "mc_note",
        ] {
            assert_eq!(
                count(
                    &upgraded,
                    &format!("SELECT count(*) FROM {table} WHERE session_id = 'ses_dead'")
                )
                .await,
                0,
                "{table}: orphan survived the migration"
            );
            assert_eq!(
                count(
                    &upgraded,
                    &format!("SELECT count(*) FROM {table} WHERE session_id = 'ses_live'")
                )
                .await,
                1,
                "{table}: a live session's row was purged"
            );
        }
    }

    #[tokio::test]
    async fn migration_tracks_user_version_and_is_idempotent() {
        let db = Db::open_in_memory().await.unwrap();
        assert_eq!(db.user_version().await.unwrap(), latest_version());
        // Re-running must be a no-op: version stays, no error.
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();
        assert_eq!(db.user_version().await.unwrap(), latest_version());
    }

    /// The pragmas [`Db::connect`] sets are the ones the storage layer relies
    /// on (WAL for concurrent readers, a busy timeout for writers, foreign keys
    /// for the session cascades).
    #[tokio::test]
    async fn connect_sets_the_expected_pragmas() {
        let db = Db::open_in_memory().await.unwrap();
        assert_eq!(
            query_row(&db, "PRAGMA journal_mode")
                .await
                .get::<String>(0)
                .unwrap(),
            "wal"
        );
        assert_eq!(
            query_row(&db, "PRAGMA busy_timeout")
                .await
                .get::<i64>(0)
                .unwrap(),
            5000
        );
        assert_eq!(
            query_row(&db, "PRAGMA foreign_keys")
                .await
                .get::<i64>(0)
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn mc_migration_creates_tables_and_indexes() {
        let db = Db::open_in_memory().await.unwrap();
        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                  WHERE type IN ('table', 'index') AND name LIKE 'mc%'
                  ORDER BY name",
            )
            .await
            .unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        let mut names = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            names.push(row.get::<String>(0).unwrap());
        }
        // Built from the two schema inventories rather than a second copy of
        // them: a new mc_* table or index is one edit, not two.
        let mut expected: Vec<&str> = EXPECTED_TABLES
            .iter()
            .chain(EXPECTED_INDEXES.iter())
            .copied()
            .filter(|name| name.starts_with("mc"))
            .collect();
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    #[tokio::test]
    async fn migration_v2_upgrades_an_existing_v1_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade.db");
        build_upto(&path, 1).await;
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await.unwrap(), latest_version());
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
        build_upto(&path, 2).await;
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await.unwrap(), latest_version());
        for table in ["mc_tag", "mc_drop"] {
            assert!(
                db.table_exists(table).await.unwrap(),
                "expected table {table} after upgrade"
            );
        }
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='mc_tag_session_message_idx'"
            )
            .await,
            1
        );
        let fresh = Db::open_in_memory().await.unwrap();
        let fresh_conn = fresh.acquire().await.unwrap();
        fresh_conn
            .execute(
                "INSERT INTO mc_drop (session_id, tag, reason, created_at) VALUES ('s', 7, 'r', 0)",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            query_row(
                &fresh,
                "SELECT state FROM mc_drop WHERE session_id = 's' AND tag = 7"
            )
            .await
            .get::<String>(0)
            .unwrap(),
            "queued",
            "state column defaults to 'queued'"
        );
    }

    #[tokio::test]
    async fn migration_v4_upgrades_an_existing_v3_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade4.db");
        build_upto(&path, 3).await;
        {
            let conn = raw(&path).await;
            conn.execute(
                "INSERT INTO mc_memory (id, project, content, category, created_at, updated_at)
                 VALUES ('mem_old', '/p', 'pre-existing fact', 'ARCHITECTURE', 1, 1)",
                (),
            )
            .await
            .unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await.unwrap(), latest_version());
        // The pre-existing row reads back with the 'active' default and no
        // archive reason — the additive columns converge both DB histories.
        let row = query_row(
            &db,
            "SELECT status, archive_reason FROM mc_memory WHERE id = 'mem_old'",
        )
        .await;
        assert_eq!(row.get::<String>(0).unwrap(), "active");
        assert_eq!(row.get::<Option<String>>(1).unwrap(), None);
        // Explicit archived rows round-trip both new columns.
        db.acquire()
            .await
            .unwrap()
            .execute(
                "INSERT INTO mc_memory (id, project, content, category, created_at,
                    updated_at, status, archive_reason)
                 VALUES ('mem_a', '/p', 'gone', 'NAMING', 2, 2, 'archived', 'merged')",
                (),
            )
            .await
            .unwrap();
        let row = query_row(
            &db,
            "SELECT status, archive_reason FROM mc_memory WHERE id = 'mem_a'",
        )
        .await;
        assert_eq!(row.get::<String>(0).unwrap(), "archived");
        assert_eq!(
            row.get::<Option<String>>(1).unwrap().as_deref(),
            Some("merged")
        );
    }

    #[tokio::test]
    async fn migration_v5_upgrades_an_existing_v4_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade5.db");
        build_upto(&path, 4).await;
        {
            let conn = raw(&path).await;
            seed_session(&conn, "ses_1").await;
            conn.execute(
                // A terminal state on purpose: a `running` row would be settled
                // by a later migration and stop being the "defaults preserved"
                // case this test is about.
                "INSERT INTO mc_historian_run (id, session_id, state, chunk_ref, attempt, created_at)
                 VALUES ('hr_old', 'ses_1', 'completed', '0..3', 1, 1)",
                (),
            )
            .await
            .unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await.unwrap(), latest_version());
        let row = query_row(
            &db,
            "SELECT updated_at, error FROM mc_historian_run WHERE id = 'hr_old'",
        )
        .await;
        assert_eq!(row.get::<i64>(0).unwrap(), 0);
        assert_eq!(row.get::<Option<String>>(1).unwrap(), None);
    }

    /// V12 makes "one running run per session" a database constraint. The rows
    /// it settles first are the reason the index can be built at all: a crashed
    /// database may hold two.
    #[tokio::test]
    async fn migration_v12_settles_running_rows_and_unique_indexes_the_claim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade12.db");
        build_upto(&path, 11).await;
        {
            let conn = raw(&path).await;
            seed_session(&conn, "ses_1").await;
            // Two running rows for one session: legal before V12, forbidden after.
            conn.execute(
                "INSERT INTO mc_historian_run (id, session_id, state, chunk_ref, attempt, created_at)
                 VALUES ('hr_a', 'ses_1', 'running', '0..3', 1, 1)",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO mc_historian_run (id, session_id, state, chunk_ref, attempt, created_at)
                 VALUES ('hr_b', 'ses_1', 'running', '0..3', 2, 2)",
                (),
            )
            .await
            .unwrap();
        }

        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await.unwrap(), latest_version());
        let row = query_row(
            &db,
            "SELECT count(*) FROM mc_historian_run WHERE state = 'running'",
        )
        .await;
        assert_eq!(
            row.get::<i64>(0).unwrap(),
            0,
            "both leftovers are settled so the unique index can be built"
        );

        let conn = db.acquire().await.unwrap();
        conn.execute(
            "UPDATE mc_historian_run SET state = 'running' WHERE id = 'hr_a'",
            (),
        )
        .await
        .unwrap();
        let duplicate = conn
            .execute(
                "INSERT INTO mc_historian_run
                     (id, session_id, state, chunk_ref, attempt, created_at, updated_at)
                 VALUES ('hr_c', 'ses_1', 'running', '4..6', 1, 3, 3)",
                (),
            )
            .await;
        assert!(
            duplicate.is_err(),
            "a second running row for one session is refused by the schema"
        );

        // A settled run frees the slot, which is what makes the next fold claimable.
        conn.execute(
            "UPDATE mc_historian_run SET state = 'completed' WHERE id = 'hr_a'",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO mc_historian_run
                 (id, session_id, state, chunk_ref, attempt, created_at, updated_at)
             VALUES ('hr_c', 'ses_1', 'running', '4..6', 1, 3, 3)",
            (),
        )
        .await
        .expect("the slot is free again");
    }

    #[tokio::test]
    async fn migration_v6_moves_the_side_channels_off_the_compartment_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade6.db");
        build_upto(&path, 5).await;
        {
            let conn = raw(&path).await;
            seed_session(&conn, "ses_1").await;
            // A pre-V6 compartment row carries the (neutral) placeholders.
            conn.execute(
                "INSERT INTO mc_compartments
                     (id, session_id, coverage_end, importance, tiers, facts, events, created_at)
                 VALUES ('mc_old', 'ses_1', '4', 50.0, '{}', '{}', '[]', 1)",
                (),
            )
            .await
            .unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        assert_eq!(db.user_version().await.unwrap(), latest_version());
        let compartments = columns(&db, "mc_compartments").await;
        let runs = columns(&db, "mc_historian_run").await;
        assert!(!compartments.iter().any(|c| c == "facts"));
        assert!(!compartments.iter().any(|c| c == "events"));
        assert!(runs.iter().any(|c| c == "facts"));
        assert!(runs.iter().any(|c| c == "events"));
        // The row itself must survive the migration. Decoding it is the
        // engine's job: this crate deliberately does not depend on mctx.
        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare("SELECT coverage_end FROM mc_compartments WHERE id = 'mc_old'")
            .await
            .unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        let row = rows
            .next()
            .await
            .unwrap()
            .expect("the compartment row survives the migration");
        assert_eq!(row.get::<String>(0).unwrap(), "4");
    }

    /// Every schema object a database owns, as `(type, name, sql)` ordered
    /// deterministically.
    async fn schema_objects(db: &Db) -> Vec<(String, String, Option<String>)> {
        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT type, name, sql FROM sqlite_master
                  WHERE name NOT LIKE 'sqlite_%'
                  ORDER BY type, name",
            )
            .await
            .unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            out.push((
                row.get::<String>(0).unwrap(),
                row.get::<String>(1).unwrap(),
                row.get::<Option<String>>(2).unwrap(),
            ));
        }
        out
    }

    #[tokio::test]
    async fn migration_v7_converges_upgraded_and_fresh_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade7.db");
        build_upto(&path, 6).await;
        {
            let conn = raw(&path).await;
            seed_session(&conn, "ses_1").await;
            // Rows written before the search indexes exist, one per source.
            for sql in [
                "INSERT INTO mc_memory (id, project, content, category, created_at, updated_at)
                 VALUES ('1', '/p', 'legacy pre-existing fact', 'ARCHITECTURE', 1, 1)",
                "INSERT INTO mc_note (id, session_id, title, body, status, surface_condition, created_at, updated_at)
                 VALUES ('1', 'ses_1', 'legacy note', 'pre-existing note body', 'active', NULL, 1, 1)",
                "INSERT INTO mc_compartments (id, session_id, coverage_end, importance, tiers, created_at)
                 VALUES ('mc_1', 'ses_1', '4', 50.0, '{\"title\":\"Legacy\",\"p1\":\"pre-existing body\"}', 1)",
            ] {
                conn.execute(sql, ()).await.unwrap();
            }
        }
        let upgraded = Db::open(&path).await.unwrap();
        let fresh = Db::open_in_memory().await.unwrap();
        assert_eq!(upgraded.user_version().await.unwrap(), latest_version());
        // Both histories must create exactly the same objects, SQL included.
        assert_eq!(
            schema_objects(&upgraded).await,
            schema_objects(&fresh).await,
            "upgraded and fresh databases must converge"
        );
        // The backfill is real: each pre-V7 row is searchable through its own
        // source index, and the derived compartment column carries the tiers.
        for sql in [
            "SELECT count(*) FROM mc_memory WHERE fts_match(content, '\"legacy\"')",
            "SELECT count(*) FROM mc_note WHERE fts_match(title, body, '\"legacy\"')",
            "SELECT count(*) FROM mc_compartments WHERE fts_match(search_text, '\"pre-existing\"')",
        ] {
            assert_eq!(count(&upgraded, sql).await, 1, "{sql}");
        }
    }

    /// The search indexes follow writes with no trigger and no second write
    /// path: that is why the FTS5 shadow table and its nine triggers are gone.
    #[tokio::test]
    async fn search_indexes_follow_every_write() {
        let db = Db::open_in_memory().await.unwrap();
        db.acquire()
            .await
            .unwrap()
            .execute(
                "INSERT INTO mc_memory (id, project, content, category, created_at, updated_at)
                 VALUES ('1', '/p', 'narwhal fact', 'CONSTRAINTS', 1, 1)",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM mc_memory WHERE fts_match(content, 'narwhal')"
            )
            .await,
            1
        );
        db.acquire()
            .await
            .unwrap()
            .execute(
                "UPDATE mc_memory SET content = 'axolotl fact' WHERE id = '1'",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM mc_memory WHERE fts_match(content, 'narwhal')"
            )
            .await,
            0,
            "an updated row must leave the index"
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM mc_memory WHERE fts_match(content, 'axolotl')"
            )
            .await,
            1
        );
        db.acquire()
            .await
            .unwrap()
            .execute("DELETE FROM mc_memory WHERE id = '1'", ())
            .await
            .unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM mc_memory WHERE fts_match(content, 'axolotl')"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn migration_v8_converges_upgraded_and_fresh_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade8.db");
        build_upto(&path, 7).await;
        let upgraded = Db::open(&path).await.unwrap();
        let fresh = Db::open_in_memory().await.unwrap();
        assert_eq!(upgraded.user_version().await.unwrap(), latest_version());
        assert_eq!(
            schema_objects(&upgraded).await,
            schema_objects(&fresh).await,
            "upgraded and fresh V8 databases must converge"
        );
        assert!(upgraded.table_exists("mc_dream_state").await.unwrap());
        // The new table's defaults are what the scheduler relies on: a row
        // inserted with only its key reads back as "never run, never failed".
        let upgraded_conn = upgraded.acquire().await.unwrap();
        upgraded_conn
            .execute(
                "INSERT INTO mc_dream_state (project, task) VALUES ('/p', 'verify')",
                (),
            )
            .await
            .unwrap();
        let row = query_row(
            &upgraded,
            "SELECT last_run_at, last_attempt_at, consecutive_failures, rotation_cursor
               FROM mc_dream_state WHERE project = '/p' AND task = 'verify'",
        )
        .await;
        assert_eq!(
            (
                row.get::<Option<i64>>(0).unwrap(),
                row.get::<i64>(1).unwrap(),
                row.get::<i64>(2).unwrap(),
                row.get::<i64>(3).unwrap(),
            ),
            (None, 0, 0, 0)
        );
    }

    /// V9 adds the dreamer's verification watermark; a memory that predates it
    /// must read back as "never verified", which is what makes the verify task
    /// check it at all.
    #[tokio::test]
    async fn migration_v9_adds_an_unverified_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade9.db");
        build_upto(&path, 8).await;
        {
            let conn = raw(&path).await;
            conn.execute(
                "INSERT INTO mc_memory (id, project, content, category, created_at, updated_at)
                 VALUES ('1', '/p', 'a fact', 'ARCHITECTURE', 5, 5)",
                (),
            )
            .await
            .unwrap();
        }
        let upgraded = Db::open(&path).await.unwrap();
        let fresh = Db::open_in_memory().await.unwrap();
        assert_eq!(upgraded.user_version().await.unwrap(), latest_version());
        assert_eq!(
            schema_objects(&upgraded).await,
            schema_objects(&fresh).await,
            "upgraded and fresh V9 databases must converge"
        );
        let row = query_row(
            &upgraded,
            "SELECT verified_at, updated_at FROM mc_memory WHERE id = '1'",
        )
        .await;
        assert_eq!(row.get::<i64>(0).unwrap(), 0, "never verified");
        assert_eq!(
            row.get::<i64>(1).unwrap(),
            5,
            "the write watermark is untouched"
        );
    }

    /// V10 adds the stored-vector columns; an existing row reads back as
    /// "not embedded", which is what makes the first search backfill it.
    #[tokio::test]
    async fn migration_v10_adds_empty_embedding_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade10.db");
        build_upto(&path, 9).await;
        {
            let conn = raw(&path).await;
            conn.execute(
                "INSERT INTO mc_memory (id, project, content, category, created_at, updated_at)
                 VALUES ('1', '/p', 'a fact', 'ARCHITECTURE', 5, 5)",
                (),
            )
            .await
            .unwrap();
        }
        let upgraded = Db::open(&path).await.unwrap();
        let fresh = Db::open_in_memory().await.unwrap();
        assert_eq!(upgraded.user_version().await.unwrap(), latest_version());
        assert_eq!(
            schema_objects(&upgraded).await,
            schema_objects(&fresh).await,
            "upgraded and fresh V10 databases must converge"
        );
        let row = query_row(
            &upgraded,
            "SELECT embedding, embedding_model FROM mc_memory WHERE id = '1'",
        )
        .await;
        assert!(row.get::<Option<Vec<u8>>>(0).unwrap().is_none());
        assert!(row.get::<Option<String>>(1).unwrap().is_none());
    }

    #[tokio::test]
    async fn indexes_exist() {
        let db = Db::open_in_memory().await.unwrap();
        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare(
                // turso materializes its FTS index behind internal
                // `__turso_internal_fts_dir_*` entries; they are the driver's
                // own bookkeeping, not schema ox declares.
                "SELECT name, tbl_name FROM sqlite_master
                  WHERE type='index' AND name NOT LIKE 'sqlite_%'
                    AND name NOT LIKE '__turso_internal%'",
            )
            .await
            .unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        let mut indexes: Vec<(String, String)> = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            indexes.push((row.get::<String>(0).unwrap(), row.get::<String>(1).unwrap()));
        }

        let names: Vec<&str> = indexes.iter().map(|(n, _)| n.as_str()).collect();
        for expected in EXPECTED_INDEXES {
            assert!(names.contains(expected), "missing index {expected}");
        }
        assert_eq!(
            names.len(),
            EXPECTED_INDEXES.len(),
            "unexpected extra indexes: {names:?}"
        );

        // Unique indexes enforce invariants, not just performance.
        for unique in [
            "event_aggregate_seq_idx",
            "session_message_session_seq_idx",
            "session_inbox_session_enqueued_seq_idx",
            "session_pending_session_admitted_seq_idx",
            "session_pending_session_compaction_idx",
        ] {
            let (_, table) = indexes
                .iter()
                .find(|(n, _)| n == unique)
                .unwrap_or_else(|| panic!("missing index {unique}"));
            let conn = db.acquire().await.unwrap();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT \"unique\" FROM pragma_index_list('{table}') WHERE name = '{unique}'"
                ))
                .await
                .unwrap();
            let mut rows = stmt.query(()).await.unwrap();
            let row = rows.next().await.unwrap().expect("index listed");
            assert_eq!(
                row.get::<i64>(0).unwrap(),
                1,
                "index {unique} must be UNIQUE"
            );
        }
    }
}
