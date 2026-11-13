//! In-process event bus with in-transaction projection.
//!
//! Faithful port of upstream `packages/core/src/bus.ts` `commitDurableEvent`:
//! allocate the next per-aggregate sequence, run the projector in the same
//! transaction, upsert the `event_sequence` watermark, then insert the event
//! row — all atomically. Ephemeral events skip persistence and only broadcast.
//!
//! Upstream uses Effect's `Effect.transaction` keyed by aggregate; here we
//! serialize per-aggregate with a `DashMap<String, Arc<Mutex<()>>>` guard held
//! across the whole `db.conn().call(...)` (which runs on a dedicated SQLite
//! thread, so the lock is the only concurrency point).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::projector::{PostCommit, project};
use ox_db::Db;
use ox_schema::event::{DurableEnvelope, EventEnvelope, SessionEventData};
use ox_schema::id;
use ox_schema::support::{Finite, LocationRef, TokenUsage};

/// Broadcast capacity. Subscribers that fall behind get `Lagged`.
const BROADCAST_CAPACITY: usize = 1024;

/// Ephemeral kinds (upstream `EphemeralEvent` union): never persisted, never
/// assigned a durable seq, only broadcast.
fn is_ephemeral(kind: &str) -> bool {
    matches!(
        kind,
        "session.text.delta"
            | "session.reasoning.delta"
            | "session.tool.input.delta"
            | "session.tool.progress"
            | "session.usage.updated"
            | "session.compaction.delta"
    )
}

/// How often the replay-log sweep runs. Six hours is frequent enough to keep a
/// week's window tight and rare enough to be invisible.
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Rows deleted per transaction, so a large backlog drains without holding the
/// writer gate for the whole sweep.
const PRUNE_BATCH: u64 = 5_000;

/// Upper bound on the configured window, purely so the arithmetic cannot
/// overflow into a window that would delete everything.
const MAX_RETENTION_DAYS: u64 = 3_650;

/// Epoch-millisecond cutoff below which replay events may be dropped.
/// `days == 0` keeps everything, which is upstream's behavior.
pub(crate) fn retention_cutoff(now_ms: i64, days: u64) -> Option<i64> {
    if days == 0 {
        return None;
    }
    // A nonsensical `retention_days` must not wrap into a negative window,
    // which would delete every event: clamp to a decade before multiplying.
    let days = days.min(MAX_RETENTION_DAYS) as i64;
    Some(now_ms - days * 86_400_000)
}

/// Delete replay events created before `cutoff_ms`, returning how many went.
/// The `event_sequence` watermark is untouched: sequence numbers must stay
/// monotonic even when their rows are gone (a reconnecting client simply sees
/// fewer rows after its `after` cursor).
///
/// Two deliberate properties: each batch commits through [`Db::begin`], so the
/// sweep shares the writer gate with the projector instead of racing it, and
/// the work is chunked so one sweep cannot hold that gate for the length of a
/// full-table delete.
pub(crate) async fn prune_events_before(db: &Db, cutoff_ms: i64) -> Result<u64> {
    prune_events_before_in_batches(db, cutoff_ms, PRUNE_BATCH).await
}

/// The sweep itself, with the batch size injected so a test can cross a batch
/// boundary without inserting thousands of rows. Termination relies on the
/// delete being exact: a short batch means the predicate set is exhausted.
async fn prune_events_before_in_batches(db: &Db, cutoff_ms: i64, batch_size: u64) -> Result<u64> {
    let mut pruned = 0u64;
    loop {
        let tx = db.begin().await?;
        let batch = tx
            .execute(
                "DELETE FROM event WHERE rowid IN
                   (SELECT rowid FROM event WHERE created < ?1 LIMIT ?2)",
                (cutoff_ms, batch_size as i64),
            )
            .await
            .context("prune replay events")?;
        tx.commit().await.context("commit replay prune")?;
        pruned += batch;
        if batch < batch_size {
            return Ok(pruned);
        }
    }
}

/// Sweep the replay log once now and then every [`PRUNE_INTERVAL`]. `days == 0`
/// disables retention entirely (upstream keeps every event forever); the sweep
/// is best-effort — a failure is logged and retried on the next tick.
///
/// Returns the sweep's join handle — `None` when retention is off — so the
/// caller awaits the last pass instead of detaching it. `stop` is checked at
/// the sleep boundary only: a prune transaction already started is committed
/// rather than dropped mid-flight.
#[must_use]
pub fn spawn_pruner(
    db: Db,
    days: u64,
    stop: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if days == 0 {
        return None;
    }
    Some(tokio::spawn(async move {
        loop {
            if let Some(cutoff) = retention_cutoff(now_millis(), days) {
                match prune_events_before(&db, cutoff).await {
                    Ok(0) => {}
                    Ok(pruned) => tracing::info!(pruned, days, "pruned replay events"),
                    Err(error) => {
                        tracing::warn!(error = %error, "pruning replay events failed");
                    }
                }
            }
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(PRUNE_INTERVAL) => {}
            }
        }
    }))
}

/// Event version for the persisted `type` suffix `{kind}@v{version}`.
/// Upstream marks `session.deleted`, `session.tool.success`,
/// `session.tool.failed`, `session.instructions.updated`, `session.forked`
/// as v2; everything else is v1.
fn durable_version(kind: &str) -> u32 {
    match kind {
        "session.deleted"
        | "session.tool.success"
        | "session.tool.failed"
        | "session.instructions.updated"
        | "session.forked" => 2,
        _ => 1,
    }
}

/// Current time in millis (id.rs's `now_millis` is `pub(crate)`).
fn now_millis() -> i64 {
    id::now_millis()
}

/// An event bus with per-aggregate durable sequencing and broadcast.
#[derive(Clone)]
pub struct EventBus {
    db: Db,
    tx: broadcast::Sender<EventEnvelope>,
    /// Per-aggregate serialization for durable commit.
    locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Monotonic counter so `id::id("evt")` is strictly ascending within a
    /// process even when multiple events share the same millisecond. Shared
    /// across clones so every publisher draws from one sequence.
    counter: Arc<AtomicU64>,
}

impl EventBus {
    #[must_use]
    pub fn new(db: Db) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            db,
            tx,
            locks: DashMap::new(),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Subscribe to the broadcast stream of all committed events.
    ///
    /// Envelopes are broadcast in the generic wire form
    /// (`EventEnvelope<Value>`); call `.parse_typed()` to recover the typed
    /// `SessionEventData`.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// Broadcast an ephemeral event outside the typed session-event union —
    /// upstream's `ephemeral()` events (`form.created`, `form.replied`,
    /// `form.cancelled`). Like the session deltas they are never persisted and
    /// never sequenced: only live subscribers see them.
    ///
    /// `directory` mirrors upstream's `location` stamp (v2.1.x TUI clients
    /// drop `form.*` frames that carry no location).
    pub fn broadcast_ephemeral(&self, kind: &str, data: Value, directory: Option<&str>) {
        let env = EventEnvelope {
            id: self.next_evt_id(),
            created: now_millis(),
            kind: kind.to_owned(),
            data,
            durable: None,
            location: directory.map(|directory| LocationRef {
                directory: directory.to_owned(),
                workspace_id: None,
            }),
            metadata: None,
        };
        // Ignore SendError: no subscribers is fine.
        let _ = self.tx.send(env);
    }

    /// Publish an event. Durable events commit via in-transaction projection;
    /// ephemeral events only broadcast. The envelope is moved to the
    /// broadcast channel (never cloned): callers observe the event through
    /// the bus, not through the return value.
    pub async fn publish(&self, data: SessionEventData) -> Result<()> {
        self.publish_durable(data, true, None, None)
            .await
            .map(|_| ())
    }

    /// [`Self::publish`] with a caller-supplied envelope id, so a client can
    /// correlate the event it caused (upstream `bus.publish(.., { id })`).
    /// Ephemeral kinds ignore the id and get a fresh one.
    pub async fn publish_as(&self, data: SessionEventData, event_id: String) -> Result<()> {
        self.publish_durable(data, true, Some(event_id), None)
            .await
            .map(|_| ())
    }

    /// Publish an event without broadcasting it to live subscribers.
    ///
    /// Durable-event semantics only: an ephemeral kind has no durable form,
    /// and through `publish_quiet` it still broadcasts — broadcasting is the
    /// only existence an ephemeral event has.
    ///
    /// The durable write path is identical to [`Self::publish`]: persistence
    /// and projection happen in full — the projector runs in the same
    /// transaction, the per-aggregate `event_sequence` watermark advances and
    /// the `event` row commits — so sequence numbers stay contiguous. The
    /// single difference: after COMMIT the envelope is not sent on the
    /// broadcast channel, so active SSE subscribers receive nothing, while a
    /// reconnecting client replaying the event log still sees the event
    /// (deliberate: sequence integrity is never sacrificed for silence).
    pub async fn publish_quiet(&self, data: SessionEventData) -> Result<()> {
        self.publish_durable(data, false, None, None)
            .await
            .map(|_| ())
    }

    /// Publish an idle checkpoint only if history has not changed since its
    /// input was read and no blocking compaction owns a running row. The guard,
    /// projection and event append share the existing write transaction.
    pub async fn publish_quiet_checkpoint(
        &self,
        data: SessionEventData,
        history_version: i64,
    ) -> Result<bool> {
        if !matches!(
            &data,
            SessionEventData::CompactionEnded {
                reason: ox_schema::message::CompactionReason::Idle,
                cost: None,
                tokens: None,
                ..
            }
        ) {
            return Err(anyhow!(
                "quiet checkpoint requires an idle compaction without usage"
            ));
        }
        self.publish_durable(data, false, None, Some(history_version))
            .await
    }

    /// Shared publish path. `fanout == false` suppresses only the post-commit
    /// broadcast of the published envelope itself; the post-commit secondary
    /// effects below are shared verbatim (they fire only when the projector
    /// reports recorded usage, which no quiet caller produces). `event_id`
    /// overrides the generated id of a durable envelope.
    async fn publish_durable(
        &self,
        data: SessionEventData,
        fanout: bool,
        event_id: Option<String>,
        checkpoint_version: Option<i64>,
    ) -> Result<bool> {
        let kind = data.kind().to_string();
        let created = now_millis();

        if is_ephemeral(&kind) {
            let env = EventEnvelope {
                id: self.next_evt_id(),
                created,
                kind,
                data,
                durable: None,
                location: None,
                metadata: None,
            };
            // Ignore SendError: no subscribers is fine.
            let _ = self.tx.send(env.into_generic());
            return Ok(true);
        }

        let aggregate_id = session_id_of(&data)
            .ok_or_else(|| anyhow!("durable event `{kind}` has no sessionID field"))?
            .to_string();

        // Per-aggregate serialization across the whole DB call.
        let lock = self
            .locks
            .entry(aggregate_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let env_id = event_id.unwrap_or_else(|| self.next_evt_id());
        let version = durable_version(&kind) as i64;
        let data_value = data.to_data();
        let type_str = format!("{kind}@v{version}");

        // One transaction, projector included: the event log and the read
        // model it feeds must not be able to disagree.
        let tx = self.db.begin().await?;

        if let Some(expected) = checkpoint_version {
            let unchanged = {
                let mut rows = tx.query(
                    "SELECT COALESCE((SELECT seq FROM event_sequence WHERE aggregate_id = ?1), -1) = ?2
                       AND EXISTS (SELECT 1 FROM session_v2 WHERE id = ?1)
                       AND NOT EXISTS (SELECT 1 FROM session_message
                         WHERE session_id = ?1 AND type = 'compaction'
                           AND json_extract(data, '$.status') = 'running')",
                    (aggregate_id.as_str(), expected),
                ).await?;
                rows.next()
                    .await?
                    .context("checkpoint guard returned no row")?
                    .get::<i64>(0)?
                    != 0
            };
            if !unchanged {
                tx.rollback()
                    .await
                    .context("discard stale quiet checkpoint")?;
                return Ok(false);
            }
        }

        let latest = {
            let mut stmt = tx
                .prepare("SELECT seq FROM event_sequence WHERE aggregate_id = ?1")
                .await?;
            let mut rows = stmt.query([aggregate_id.as_str()]).await?;
            match rows.next().await? {
                Some(row) => Some(row.get::<i64>(0)?),
                None => None,
            }
        };
        let seq = latest.map_or(0, |s| s + 1);

        // Built once, with the payload moved in: `project` borrows it, the
        // INSERT below uses the originals (`env_id`/`aggregate_id` — only
        // their clones went into the envelope), and after COMMIT the
        // envelope moves to the broadcast channel. No clone of `data` or
        // `kind` anywhere on this path.
        let env = EventEnvelope {
            id: env_id.clone(),
            created,
            kind,
            data,
            durable: Some(DurableEnvelope {
                aggregate_id: aggregate_id.clone(),
                seq,
                version,
            }),
            location: None,
            metadata: None,
        };

        let post_commit = project(&tx, &env).await?;

        tx.execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)
             ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, excluded.seq)",
            (aggregate_id.as_str(), seq),
        )
        .await
        .context("commit durable event")?;

        tx.execute(
            "INSERT INTO event (id, aggregate_id, seq, created, type, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                env_id.as_str(),
                aggregate_id.as_str(),
                seq,
                created,
                type_str.as_str(),
                data_value.to_string(),
            ),
        )
        .await
        .context("commit durable event")?;

        tx.commit().await.context("commit durable event")?;

        if fanout {
            let _ = self.tx.send(env.into_generic());
        }

        // Post-commit secondary effects (ephemeral usage broadcasts).
        for action in post_commit {
            match action {
                PostCommit::PublishUsage { session_id } => {
                    if let Ok((cost, tokens)) = self.read_usage(&session_id).await {
                        let usage = SessionEventData::SessionUsageUpdated {
                            session_id,
                            cost,
                            tokens,
                        };
                        let uenv = EventEnvelope {
                            id: self.next_evt_id(),
                            created: now_millis(),
                            kind: usage.kind().to_string(),
                            data: usage,
                            durable: None,
                            location: None,
                            metadata: None,
                        };
                        let _ = self.tx.send(uenv.into_generic());
                    }
                }
            }
        }

        Ok(true)
    }

    /// The database handle (read-only fixtures in tests).
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Read the accumulated usage columns for a session (post-commit).
    async fn read_usage(&self, session_id: &str) -> Result<(Finite, TokenUsage)> {
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT cost, tokens_input, tokens_output, tokens_reasoning, \
                 tokens_cache_read, tokens_cache_write FROM session_v2 WHERE id = ?1",
            )
            .await
            .context("read session usage")?;
        let mut rows = stmt
            .query([session_id])
            .await
            .context("read session usage")?;
        let row = rows
            .next()
            .await
            .context("read session usage")?
            .ok_or_else(|| anyhow!("session {session_id} has no row"))?;
        Ok((
            Finite::Float(row.get::<f64>(0)?),
            TokenUsage {
                input: row.get::<i64>(1)?,
                output: row.get::<i64>(2)?,
                reasoning: row.get::<i64>(3)?,
                cache: ox_schema::support::TokenCache {
                    read: row.get::<i64>(4)?,
                    write: row.get::<i64>(5)?,
                },
                estimated: None,
                reasoning_reported: None,
            },
        ))
    }

    /// Replay the durable event log for an aggregate, ordered by seq, at most
    /// `limit` envelopes. Callers page by resuming from the last envelope's
    /// `durable.seq`; a bounded page keeps a reconnect replay's peak memory at
    /// one page instead of the whole log (which grows between retention sweeps).
    /// `i64::MAX` means unbounded.
    pub async fn event_log(
        &self,
        aggregate_id: &str,
        after: i64,
        limit: i64,
    ) -> Result<Vec<EventEnvelope<Value>>> {
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, aggregate_id, seq, created, type, data FROM event \
                 WHERE aggregate_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
            )
            .await
            .context("read event log")?;
        let mut rows = stmt
            .query((aggregate_id, after, limit))
            .await
            .context("read event log")?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.context("read event log")? {
            out.push((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<i64>(2)?,
                row.get::<i64>(3)?,
                row.get::<String>(4)?,
                row.get::<String>(5)?,
            ));
        }
        let rows = out;

        let mut result = Vec::with_capacity(rows.len());
        for (id, aggregate_id, seq, created, type_str, data_str) in rows {
            let (kind, version) = split_versioned_type(&type_str);
            let data_value: Value = serde_json::from_str(&data_str).context("decode event data")?;
            result.push(EventEnvelope {
                id,
                created,
                kind,
                data: data_value,
                durable: Some(DurableEnvelope {
                    aggregate_id,
                    seq,
                    version: version as i64,
                }),
                location: None,
                metadata: None,
            });
        }
        Ok(result)
    }

    /// Purge an aggregate's durable log: the `event_sequence` watermark and
    /// all `event` rows (upstream `Bus.remove`, called by `session.remove`).
    pub async fn remove(&self, aggregate_id: &str) -> Result<()> {
        let tx = self.db.begin().await?;
        tx.execute(
            "DELETE FROM event_sequence WHERE aggregate_id = ?1",
            [aggregate_id],
        )
        .await
        .context("remove aggregate event log")?;
        tx.execute("DELETE FROM event WHERE aggregate_id = ?1", [aggregate_id])
            .await
            .context("remove aggregate event log")?;
        tx.commit().await.context("remove aggregate event log")?;
        self.locks.remove(aggregate_id);
        Ok(())
    }

    fn next_evt_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{}_{n}", id::id("evt"))
    }
}

/// Split `"{kind}@v{N}"` into `(kind, N)`. Absent suffix ⇒ v1.
fn split_versioned_type(t: &str) -> (String, u32) {
    if let Some(idx) = t.rfind("@v") {
        let (k, v) = t.split_at(idx);
        if let Ok(n) = v[2..].parse::<u32>() {
            return (k.to_string(), n);
        }
    }
    (t.to_string(), 1)
}

/// Extract the session id (aggregate id) from any session-scoped event.
pub(crate) fn session_id_of(data: &SessionEventData) -> Option<&str> {
    use SessionEventData as E;
    Some(match data {
        E::SessionCreated { session_id, .. }
        | E::SessionRenamed { session_id, .. }
        | E::SessionDeleted { session_id }
        | E::SessionForked { session_id, .. }
        | E::SessionAgentSelected { session_id, .. }
        | E::SessionModelSelected { session_id, .. }
        | E::SessionMetadataUpdated { session_id, .. }
        | E::SessionPermissions { session_id, .. }
        | E::SessionViewed { session_id, .. }
        | E::SessionArchived { session_id, .. }
        | E::SessionUnarchived { session_id, .. }
        | E::SessionUsageRecorded { session_id, .. }
        | E::SessionUsageUpdated { session_id, .. }
        | E::SessionInstructionsUpdated { session_id, .. }
        | E::SessionSynthetic { session_id, .. }
        | E::SessionShellStarted { session_id, .. }
        | E::SessionShellEnded { session_id, .. }
        | E::SessionRetryScheduled { session_id, .. }
        | E::SessionMessageContentUpdated { session_id, .. }
        | E::SessionMoved { session_id, .. }
        | E::ExecutionStarted { session_id }
        | E::ExecutionSucceeded { session_id }
        | E::ExecutionFailed { session_id, .. }
        | E::ExecutionInterrupted { session_id, .. }
        | E::InboxEnqueued { session_id, .. }
        | E::InboxDelivered { session_id, .. }
        | E::InboxCancelled { session_id, .. }
        | E::InboxDeliveryChanged { session_id, .. }
        | E::StepStarted { session_id, .. }
        | E::StepStreamed { session_id, .. }
        | E::StepEnded { session_id, .. }
        | E::StepFailed { session_id, .. }
        | E::TextStarted { session_id, .. }
        | E::TextDelta { session_id, .. }
        | E::TextEnded { session_id, .. }
        | E::ReasoningStarted { session_id, .. }
        | E::ReasoningDelta { session_id, .. }
        | E::ReasoningEnded { session_id, .. }
        | E::ToolInputStarted { session_id, .. }
        | E::ToolInputDelta { session_id, .. }
        | E::ToolInputEnded { session_id, .. }
        | E::ToolCalled { session_id, .. }
        | E::ToolProgress { session_id, .. }
        | E::ToolSuccess { session_id, .. }
        | E::ToolFailed { session_id, .. }
        | E::CompactionStarted { session_id, .. }
        | E::CompactionDelta { session_id, .. }
        | E::CompactionEnded { session_id, .. }
        | E::CompactionFailed { session_id, .. } => session_id,
        E::Unknown { .. } => return None,
    })
}
