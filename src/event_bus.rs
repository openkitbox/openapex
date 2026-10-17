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

use anyhow::{Context as _, Result, anyhow};
use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::db::Db;
use crate::id;
use crate::projector::{PostCommit, project};
use crate::schema::event::{DurableEnvelope, EventEnvelope, SessionEventData};
use crate::schema::support::{Finite, TokenUsage};

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
pub struct EventBus {
    db: Db,
    tx: broadcast::Sender<EventEnvelope>,
    /// Per-aggregate serialization for durable commit.
    locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Monotonic counter so `id::id("evt")` is strictly ascending within a
    /// process even when multiple events share the same millisecond.
    counter: AtomicU64,
}

impl EventBus {
    pub fn new(db: Db) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            db,
            tx,
            locks: DashMap::new(),
            counter: AtomicU64::new(0),
        }
    }

    /// Subscribe to the broadcast stream of all committed events.
    ///
    /// Envelopes are broadcast in the generic wire form
    /// (`EventEnvelope<Value>`); call `.parse_typed()` to recover the typed
    /// `SessionEventData`.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// Publish an event. Durable events commit via in-transaction projection;
    /// ephemeral events only broadcast. Returns the constructed (typed)
    /// envelope.
    pub async fn publish(&self, data: SessionEventData) -> Result<EventEnvelope<SessionEventData>> {
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
            let _ = self.tx.send(env.clone().into_generic());
            return Ok(env);
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

        let env_id = self.next_evt_id();
        let version = durable_version(&kind) as i64;
        let data_value = data.clone().into_data();
        let kind_for_tx = kind.clone();
        let data_for_tx = data.clone();
        let env_id_for_tx = env_id.clone();
        let agg = aggregate_id.clone();
        let db = self.db.conn().clone();

        let (seq, post_commit) = db
            .call(move |conn| {
                let tx = conn.transaction()?;

                let latest: Option<i64> = tx
                    .query_row(
                        "SELECT seq FROM event_sequence WHERE aggregate_id = ?1",
                        rusqlite::params![agg],
                        |row| row.get(0),
                    )
                    .ok();
                let seq = latest.map_or(0, |s| s + 1);

                let env = EventEnvelope {
                    id: env_id_for_tx.clone(),
                    created,
                    kind: kind_for_tx.clone(),
                    data: data_for_tx.clone(),
                    durable: Some(DurableEnvelope {
                        aggregate_id: agg.clone(),
                        seq,
                        version,
                    }),
                    location: None,
                    metadata: None,
                };

                // Run the projector in the same transaction. On error the
                // transaction rolls back and the error propagates.
                let post = project(&tx, &env).map_err(to_sql_err)?;

                tx.execute(
                    "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)
                     ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, excluded.seq)",
                    rusqlite::params![agg, seq],
                )?;

                let type_str = format!("{kind_for_tx}@v{version}");
                tx.execute(
                    "INSERT INTO event (id, aggregate_id, seq, created, type, data)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        env_id_for_tx,
                        agg,
                        seq,
                        created,
                        type_str,
                        data_value.to_string(),
                    ],
                )?;

                tx.commit()?;
                Ok::<(i64, Vec<PostCommit>), rusqlite::Error>((seq, post))
            })
            .await
            .context("commit durable event")?;

        let env = EventEnvelope {
            id: env_id,
            created,
            kind,
            data,
            durable: Some(DurableEnvelope {
                aggregate_id,
                seq,
                version,
            }),
            location: None,
            metadata: None,
        };
        let _ = self.tx.send(env.clone().into_generic());

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

        Ok(env)
    }

    /// Read the accumulated usage columns for a session (post-commit).
    async fn read_usage(&self, session_id: &str) -> Result<(Finite, TokenUsage)> {
        let sid = session_id.to_string();
        let db = self.db.conn().clone();
        let out = db
            .call(move |conn| {
                conn.query_row(
                    "SELECT cost, tokens_input, tokens_output, tokens_reasoning, \
                     tokens_cache_read, tokens_cache_write FROM session_v2 WHERE id = ?1",
                    rusqlite::params![sid],
                    |row| {
                        Ok((
                            Finite::Float(row.get::<_, f64>(0)?),
                            TokenUsage {
                                input: row.get(1)?,
                                output: row.get(2)?,
                                reasoning: row.get(3)?,
                                cache: crate::schema::support::TokenCache {
                                    read: row.get(4)?,
                                    write: row.get(5)?,
                                },
                            },
                        ))
                    },
                )
            })
            .await
            .context("read session usage")?;
        Ok(out)
    }

    /// Replay the durable event log for an aggregate, ordered by seq.
    pub async fn event_log(
        &self,
        aggregate_id: &str,
        after: i64,
    ) -> Result<Vec<EventEnvelope<Value>>> {
        let agg = aggregate_id.to_string();
        let db = self.db.conn().clone();
        let rows = db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, aggregate_id, seq, created, type, data FROM event \
                     WHERE aggregate_id = ?1 AND seq > ?2 ORDER BY seq ASC",
                )?;
                let out = stmt
                    .query_map(rusqlite::params![agg, after], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok::<Vec<_>, rusqlite::Error>(out)
            })
            .await
            .context("read event log")?;

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

    /// Access the underlying connection (tests / read-only helpers).
    pub fn db_conn(&self) -> &tokio_rusqlite::Connection {
        self.db.conn()
    }

    fn next_evt_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{}_{n}", id::id("evt"))
    }
}

/// Map an anyhow projector error into a rusqlite error so the transaction
/// rolls back and the failure propagates out of `db.conn().call`.
fn to_sql_err(e: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.to_string())))
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
        | E::SessionUsageRecorded { session_id, .. }
        | E::SessionUsageUpdated { session_id, .. }
        | E::SessionSynthetic { session_id, .. }
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
