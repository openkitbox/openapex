//! In-transaction projector, ported from
//! `packages/core/src/session/projector.ts` (+ `inbox.ts` projections).
//!
//! Every durable event mutates its projection tables inside the same
//! transaction that allocates its seq. On any projector error the caller
//! rolls back, so projections and the event log stay consistent.

pub mod message_updater;

use anyhow::{Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::schema::event::{EventEnvelope, InterruptReason, SessionEventData};
use crate::schema::inbox::{Delivery, Item};
use crate::schema::message::{IdleOutcome, Message, TimeCreated};
use crate::schema::support::{Finite, ForkBoundary, MoneyUsd, TokenUsage};

/// Secondary effects to run after the durable transaction commits.
pub enum PostCommit {
    /// Broadcast ephemeral `session.usage.updated` with the session's
    /// accumulated usage columns.
    PublishUsage { session_id: String },
}

/// Dispatch a durable event to its projection. `tx` is the open transaction.
/// Returns any post-commit effects to run after COMMIT.
pub fn project(tx: &Connection, env: &EventEnvelope<SessionEventData>) -> Result<Vec<PostCommit>> {
    use SessionEventData as E;
    let durable = env
        .durable
        .as_ref()
        .ok_or_else(|| anyhow!("projector requires a durable envelope"))?;
    let mut post = Vec::new();

    match &env.data {
        E::SessionCreated {
            session_id,
            project_id,
            location,
            subpath,
            parent_id,
            slug,
            title,
            agent,
            model,
            metadata,
            permissions,
            version,
        } => {
            let model_json = model.as_ref().map(to_json).transpose()?;
            let meta_json = metadata.as_ref().map(to_json).transpose()?;
            let perm_json = permissions.as_ref().map(to_json).transpose()?;
            // Upstream `onConflictDoNothing`; a pre-existing row is a defect.
            let n = tx.execute(
                "INSERT OR IGNORE INTO session_v2
                 (id, project_id, workspace_id, parent_id, slug, directory, path,
                  title, agent, model, metadata, permission, version,
                  time_created, time_updated)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14)",
                rusqlite::params![
                    session_id,
                    project_id,
                    location.workspace_id,
                    parent_id,
                    slug,
                    location.directory,
                    subpath,
                    title,
                    agent,
                    model_json,
                    meta_json,
                    perm_json,
                    version,
                    env.created,
                ],
            )?;
            if n == 0 {
                bail!("SessionAlreadyProjected: `{session_id}` already exists");
            }
        }

        E::SessionRenamed { session_id, title } => {
            tx.execute(
                "UPDATE session_v2 SET title = ?2, time_updated = ?3 WHERE id = ?1",
                rusqlite::params![session_id, title, env.created],
            )?;
        }

        E::SessionDeleted { session_id } => {
            tx.execute(
                "DELETE FROM session_v2 WHERE id = ?1",
                rusqlite::params![session_id],
            )?;
        }

        E::ExecutionStarted { session_id } => {
            touch(tx, session_id, env.created)?;
        }
        E::ExecutionSucceeded { session_id } => {
            message_updater::run(tx, env)?;
            project_idle(tx, session_id, env.created, IdleOutcome::Succeeded)?;
        }
        E::ExecutionFailed { session_id, .. } => {
            message_updater::run(tx, env)?;
            project_idle(tx, session_id, env.created, IdleOutcome::Failed)?;
        }
        E::ExecutionInterrupted { session_id, reason } => {
            message_updater::run(tx, env)?;
            // Upstream: shutdown interrupts skip the idle-marker update.
            if *reason != InterruptReason::Shutdown {
                project_idle(tx, session_id, env.created, IdleOutcome::Interrupted)?;
            }
        }

        E::InboxEnqueued {
            session_id,
            inbox_id,
            item,
        } => {
            project_admitted(tx, session_id, inbox_id, item, durable.seq, env.created)?;
            touch(tx, session_id, env.created)?;
        }
        E::InboxDelivered {
            session_id,
            inbox_id,
        } => {
            let item = project_delivered(tx, session_id, inbox_id)?;
            // Only user/synthetic promote to a message; compaction/move do not.
            if let Some(msg) = promote_to_message(&item, inbox_id, env.created) {
                insert_message(tx, session_id, durable.seq, &msg)?;
            }
        }
        E::InboxCancelled {
            session_id,
            inbox_id,
        } => {
            project_cancelled(tx, session_id, inbox_id)?;
        }
        E::InboxDeliveryChanged {
            session_id,
            inbox_id,
            delivery,
        } => {
            project_delivery_changed(tx, session_id, inbox_id, delivery)?;
        }

        E::SessionUsageRecorded {
            session_id,
            cost,
            tokens,
            ..
        } => {
            apply_usage(tx, session_id, *cost, tokens, None)?;
            post.push(PostCommit::PublishUsage {
                session_id: session_id.clone(),
            });
        }

        // Session-selection / metadata events: append the projected message
        // first (it needs the pre-update row for `previous`), then update the
        // session columns.
        E::SessionAgentSelected {
            session_id, agent, ..
        } => {
            message_updater::run(tx, env)?;
            tx.execute(
                "UPDATE session_v2 SET agent = ?2, time_updated = ?3 WHERE id = ?1",
                rusqlite::params![session_id, agent, env.created],
            )?;
        }
        E::SessionModelSelected {
            session_id, model, ..
        } => {
            message_updater::run(tx, env)?;
            tx.execute(
                "UPDATE session_v2 SET model = ?2, time_updated = ?3 WHERE id = ?1",
                rusqlite::params![session_id, to_json(model)?, env.created],
            )?;
        }
        E::SessionMetadataUpdated {
            session_id,
            metadata,
        } => {
            tx.execute(
                "UPDATE session_v2 SET metadata = ?2, time_updated = ?3 WHERE id = ?1",
                rusqlite::params![session_id, to_json(metadata)?, env.created],
            )?;
        }
        E::SessionPermissions {
            session_id,
            permissions,
        } => {
            tx.execute(
                "UPDATE session_v2 SET permission = ?2, time_updated = ?3 WHERE id = ?1",
                rusqlite::params![session_id, to_json(permissions)?, env.created],
            )?;
        }
        E::SessionViewed { session_id, idle } => {
            // Monotone watermark: a duplicate or stale view never regresses,
            // and a terminal event committing after the viewer's observation
            // keeps the newer idle transition unread. `time_updated` is left
            // unchanged (self-assignment).
            tx.execute(
                "UPDATE session_v2 SET
                   time_viewed = max(?2, coalesce(time_viewed, ?2)),
                   time_updated = time_updated
                 WHERE id = ?1",
                rusqlite::params![session_id, idle],
            )?;
        }
        E::SessionForked { .. } => project_fork(tx, env)?,

        // Message-updater events.
        E::SessionSynthetic { .. }
        | E::SessionRetryScheduled { .. }
        | E::SessionMessageContentUpdated { .. }
        | E::StepStarted { .. }
        | E::StepStreamed { .. }
        | E::StepEnded { .. }
        | E::StepFailed { .. }
        | E::TextStarted { .. }
        | E::TextEnded { .. }
        | E::ReasoningStarted { .. }
        | E::ReasoningEnded { .. }
        | E::ToolInputStarted { .. }
        | E::ToolInputEnded { .. }
        | E::ToolCalled { .. }
        | E::ToolSuccess { .. }
        | E::ToolFailed { .. }
        | E::CompactionStarted { .. }
        | E::CompactionEnded { .. }
        | E::CompactionFailed { .. } => {
            let out = message_updater::run(tx, env)?;
            if out.usage_recorded
                && let Some(sid) = crate::event_bus::session_id_of(&env.data)
            {
                post.push(PostCommit::PublishUsage {
                    session_id: sid.to_string(),
                });
            }
        }

        E::SessionMoved {
            session_id,
            payload,
        } => {
            // Upstream: run the message-updater (appends `location-switched`),
            // then update the session row's directory/path/project/workspace.
            message_updater::run(tx, env)?;
            tx.execute(
                "UPDATE session_v2 SET
                   directory = ?2,
                   path = ?3,
                   project_id = ?4,
                   workspace_id = ?5,
                   time_updated = ?6
                 WHERE id = ?1",
                rusqlite::params![
                    session_id,
                    payload.location.directory,
                    payload.subpath,
                    payload.project_id,
                    payload.location.workspace_id,
                    env.created,
                ],
            )?;
        }
        E::SessionUsageUpdated { .. }
        | E::TextDelta { .. }
        | E::ReasoningDelta { .. }
        | E::ToolInputDelta { .. }
        | E::ToolProgress { .. }
        | E::CompactionDelta { .. } => {}

        E::Unknown { r#type, .. } => {
            bail!("no projector for unknown event type `{}`", r#type);
        }
    }

    Ok(post)
}

// ---------------------------------------------------------------------------
// Session row helpers
// ---------------------------------------------------------------------------

fn to_json<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}

/// `time_updated = event.created`.
pub(crate) fn touch(tx: &Connection, session_id: &str, created: i64) -> Result<()> {
    tx.execute(
        "UPDATE session_v2 SET time_updated = ?2 WHERE id = ?1",
        rusqlite::params![session_id, created],
    )?;
    Ok(())
}

/// projectIdle: `time_idle = max(created, coalesce(time_idle + 1, created))`,
/// set `idle_outcome`, leave `time_updated` unchanged (self-assignment).
fn project_idle(
    tx: &Connection,
    session_id: &str,
    created: i64,
    outcome: IdleOutcome,
) -> Result<()> {
    let outcome_str = match outcome {
        IdleOutcome::Succeeded => "succeeded",
        IdleOutcome::Failed => "failed",
        IdleOutcome::Interrupted => "interrupted",
    };
    tx.execute(
        "UPDATE session_v2 SET
           time_idle = max(?2, coalesce(time_idle + 1, ?2)),
           idle_outcome = ?3,
           time_updated = time_updated
         WHERE id = ?1",
        rusqlite::params![session_id, created, outcome_str],
    )?;
    Ok(())
}

/// Accumulate cost/tokens. `time_updated` is set only when `Some`.
pub(crate) fn apply_usage(
    tx: &Connection,
    session_id: &str,
    cost: MoneyUsd,
    tokens: &TokenUsage,
    time_updated: Option<i64>,
) -> Result<()> {
    if let Some(t) = time_updated {
        tx.execute(
            "UPDATE session_v2 SET
               cost = cost + ?2,
               tokens_input = tokens_input + ?3,
               tokens_output = tokens_output + ?4,
               tokens_reasoning = tokens_reasoning + ?5,
               tokens_cache_read = tokens_cache_read + ?6,
               tokens_cache_write = tokens_cache_write + ?7,
               time_updated = ?8
             WHERE id = ?1",
            rusqlite::params![
                session_id,
                finite_f64(cost),
                tokens.input,
                tokens.output,
                tokens.reasoning,
                tokens.cache.read,
                tokens.cache.write,
                t,
            ],
        )?;
    } else {
        tx.execute(
            "UPDATE session_v2 SET
               cost = cost + ?2,
               tokens_input = tokens_input + ?3,
               tokens_output = tokens_output + ?4,
               tokens_reasoning = tokens_reasoning + ?5,
               tokens_cache_read = tokens_cache_read + ?6,
               tokens_cache_write = tokens_cache_write + ?7
             WHERE id = ?1",
            rusqlite::params![
                session_id,
                finite_f64(cost),
                tokens.input,
                tokens.output,
                tokens.reasoning,
                tokens.cache.read,
                tokens.cache.write,
            ],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Inbox projections (packages/core/src/session/inbox.ts)
// ---------------------------------------------------------------------------

/// projectAdmitted: INSERT session_inbox. If a `session_message` already has
/// the id → LifecycleConflict. INSERT OR IGNORE with 0 rows ⇒ Err.
fn project_admitted(
    tx: &Connection,
    session_id: &str,
    inbox_id: &str,
    item: &Item,
    seq: i64,
    created: i64,
) -> Result<()> {
    let exists: Option<String> = tx
        .query_row(
            "SELECT id FROM session_message WHERE id = ?1",
            rusqlite::params![inbox_id],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_some() {
        bail!("LifecycleConflict: a session_message already exists for `{inbox_id}`");
    }

    let n = tx.execute(
        "INSERT OR IGNORE INTO session_inbox
         (id, session_id, type, payload, delivery, enqueued_seq, time_created)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            inbox_id,
            session_id,
            item.kind(),
            item.payload().to_string(),
            delivery_str(item.delivery()),
            seq,
            created,
        ],
    )?;
    if n == 0 {
        bail!("inbox item `{inbox_id}` already admitted");
    }
    Ok(())
}

/// projectDelivered: DELETE ... RETURNING; missing row ⇒ Err.
fn project_delivered(tx: &Connection, session_id: &str, inbox_id: &str) -> Result<Item> {
    let row = tx
        .query_row(
            "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2
             RETURNING type, payload, delivery",
            rusqlite::params![inbox_id, session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let (kind, payload, delivery) = row
        .ok_or_else(|| anyhow!("inbox.delivered: no row `{inbox_id}` in session `{session_id}`"))?;
    let payload_value: Value = serde_json::from_str(&payload)?;
    decode_item(&kind, payload_value, &delivery)
}

/// projectCancelled: DELETE only if delivery IN ('queue','steer'); missing ⇒ Err.
fn project_cancelled(tx: &Connection, session_id: &str, inbox_id: &str) -> Result<()> {
    let n = tx.execute(
        "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2
         AND delivery IN ('queue','steer')",
        rusqlite::params![inbox_id, session_id],
    )?;
    if n == 0 {
        bail!("inbox.cancelled: no cancellable row `{inbox_id}` in session `{session_id}`");
    }
    Ok(())
}

/// projectDeliveryChanged: UPDATE delivery with a from-guard (from = opposite
/// of to). 0 rows ⇒ Err.
fn project_delivery_changed(
    tx: &Connection,
    session_id: &str,
    inbox_id: &str,
    to: &Delivery,
) -> Result<()> {
    let to_str = delivery_str(*to);
    let from_str = opposite_delivery(to_str);
    let n = tx.execute(
        "UPDATE session_inbox SET delivery = ?4
         WHERE id = ?1 AND session_id = ?2 AND delivery = ?3",
        rusqlite::params![inbox_id, session_id, from_str, to_str],
    )?;
    if n == 0 {
        bail!("inbox.delivery.changed: no row `{inbox_id}` with delivery `{from_str}`");
    }
    Ok(())
}

fn delivery_str(d: Delivery) -> &'static str {
    match d {
        Delivery::Steer => "steer",
        Delivery::Queue => "queue",
    }
}

fn opposite_delivery(d: &str) -> &'static str {
    if d == "queue" { "steer" } else { "queue" }
}

/// Convert a `Finite` money value to an f64 for SQL binding.
fn finite_f64(v: Finite) -> f64 {
    match v {
        Finite::Int(n) => n as f64,
        Finite::Float(f) => f,
    }
}

/// Reconstruct an [`Item`] from its session_inbox row parts.
fn decode_item(kind: &str, payload: Value, delivery: &str) -> Result<Item> {
    let delivery = match delivery {
        "steer" => Delivery::Steer,
        "queue" => Delivery::Queue,
        other => bail!("unknown inbox delivery `{other}`"),
    };
    let adapter = json!({ "type": kind, "payload": payload, "delivery": delivery });
    Ok(serde_json::from_value(adapter)?)
}

/// Promote a delivered user/synthetic inbox item to its message. The message
/// reuses the inbox id. Compaction and move items do not produce a message.
fn promote_to_message(item: &Item, inbox_id: &str, created: i64) -> Option<Message> {
    match item {
        Item::User { payload, .. } => Some(Message::User {
            id: inbox_id.to_string(),
            metadata: payload.metadata.clone(),
            time: TimeCreated { created },
            prompt: payload.prompt.clone(),
        }),
        Item::Synthetic { payload, .. } => Some(Message::Synthetic {
            id: inbox_id.to_string(),
            metadata: payload.metadata.clone(),
            time: TimeCreated { created },
            text: payload.text.clone(),
            description: payload.description.clone(),
        }),
        Item::Compaction { .. } | Item::Move { .. } => None,
    }
}

/// Insert a message row, mirroring upstream `insertMessage` (seq = event seq).
pub(crate) fn insert_message(
    tx: &Connection,
    session_id: &str,
    seq: i64,
    message: &Message,
) -> Result<()> {
    let data = message.to_row_data().to_string();
    tx.execute(
        "INSERT INTO session_message
         (id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1,?2,?3,?4,?5,?5,?6)",
        rusqlite::params![
            message.id(),
            session_id,
            message.kind(),
            seq,
            message.time_created(),
            data,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fork projection (packages/core/src/session/projector.ts projectFork)
// ---------------------------------------------------------------------------

/// Columns copied verbatim from the parent row (`model`, `metadata`,
/// `permission` are TEXT blobs of JSON, so no re-encoding is needed).
type ParentRow = (
    String,         // project_id
    Option<String>, // workspace_id
    String,         // directory
    Option<String>, // path
    Option<String>, // title
    Option<String>, // agent
    Option<String>, // model
    Option<String>, // metadata
    Option<String>, // permission
    String,         // version
);

/// `forkTitle`: append ` (fork #N)` to the parent's title, incrementing an
/// existing counter. `None` stays `None` (no title to fork).
fn fork_title(title: Option<&str>) -> Option<String> {
    let value = title?;
    if let Some(open) = value.rfind(" (fork #")
        && value.ends_with(')')
    {
        let head = &value[..open];
        let digits = &value[open + " (fork #".len()..value.len() - 1];
        if !digits.is_empty()
            && digits.chars().all(|c| c.is_ascii_digit())
            && let Ok(n) = digits.parse::<u64>()
        {
            return Some(format!("{head} (fork #{})", n + 1));
        }
    }
    Some(format!("{value} (fork #1)"))
}

/// projectFork: insert the forked session row, copy the parent's settled
/// history up to the boundary, and reserve the child's durable sequence.
fn project_fork(tx: &Connection, env: &EventEnvelope<SessionEventData>) -> Result<()> {
    let SessionEventData::SessionForked {
        session_id,
        parent_id,
        boundary,
        ..
    } = &env.data
    else {
        bail!("project_fork called with {}", env.kind);
    };

    let parent: Option<ParentRow> = tx
        .query_row(
            "SELECT project_id, workspace_id, directory, path, title, agent,
                    model, metadata, permission, version
             FROM session_v2 WHERE id = ?1",
            rusqlite::params![parent_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()?;
    let (
        project_id,
        workspace_id,
        directory,
        path,
        title,
        agent,
        model,
        metadata,
        permission,
        version,
    ) = parent.ok_or_else(|| anyhow!("Fork parent session not found: {parent_id}"))?;

    let boundary_message_id = match boundary {
        ForkBoundary::Before { message_id } | ForkBoundary::Through { message_id } => message_id,
    };
    let boundary_seq: Option<i64> = tx
        .query_row(
            "SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2",
            rusqlite::params![parent_id, boundary_message_id],
            |row| row.get(0),
        )
        .optional()?;
    let boundary_seq = boundary_seq
        .ok_or_else(|| anyhow!("Fork boundary message not found: {boundary_message_id}"))?;

    // Last message seq included in the copy ("before" is exclusive).
    let copied_seq: Option<i64> = tx
        .query_row(
            if matches!(boundary, ForkBoundary::Before { .. }) {
                "SELECT seq FROM session_message WHERE session_id = ?1 AND seq < ?2
                 ORDER BY seq DESC LIMIT 1"
            } else {
                "SELECT seq FROM session_message WHERE session_id = ?1 AND seq <= ?2
                 ORDER BY seq DESC LIMIT 1"
            },
            rusqlite::params![parent_id, boundary_seq],
            |row| row.get(0),
        )
        .optional()?;

    // Forked row: new slug, cost/tokens zeroed (column defaults), location and
    // settings inherited from the parent. Upstream `.onConflictDoNothing()`
    // maps to `INSERT OR IGNORE`; a pre-existing row is a defect.
    let n = tx.execute(
        "INSERT OR IGNORE INTO session_v2
         (id, project_id, workspace_id, parent_id, fork_session_id, fork_boundary,
          slug, directory, path, title, agent, model, metadata, permission, version,
          time_created, time_updated)
         VALUES (?1,?2,?3,NULL,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?15)",
        rusqlite::params![
            session_id,
            project_id,
            workspace_id,
            parent_id,
            to_json(boundary)?,
            crate::slug::create(),
            directory,
            path,
            fork_title(title.as_deref()),
            agent,
            model,
            metadata,
            permission,
            version,
            env.created,
        ],
    )?;
    if n == 0 {
        bail!("SessionAlreadyProjected: `{session_id}` already exists");
    }

    // TODO(instructions): `instructionEntries` / `instructions` snapshots need
    // the instruction tables, which are out of scope for this task.

    let Some(copied_seq) = copied_seq else {
        return Ok(());
    };

    // Copy settled history: rows up to `copied_seq`, skipping active
    // projections (running assistant/shell/compaction stay on the parent).
    const FORK_BATCH_SIZE: usize = 500;
    let msg_prefix = crate::id::from_event(&env.id);
    let mut cursor: i64 = -1;
    loop {
        let mut stmt = tx.prepare(
            "SELECT id, type, seq, time_created, time_updated, data
             FROM session_message
             WHERE session_id = ?1 AND seq > ?2 AND seq <= ?3
               AND (type != 'assistant'
                    OR json_extract(data, '$.time.completed') IS NOT NULL)
               AND (type != 'shell'
                    OR json_extract(data, '$.status') != 'running')
               AND (type != 'compaction'
                    OR json_extract(data, '$.status') != 'running')
             ORDER BY seq ASC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![parent_id, cursor, copied_seq, FORK_BATCH_SIZE as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )?;
        let mut batch = Vec::new();
        for row in rows {
            batch.push(row?);
        }
        if batch.is_empty() {
            break;
        }
        cursor = batch
            .last()
            .map(|(_, _, seq, _, _, _)| *seq)
            .ok_or_else(|| anyhow!("fork copy: empty batch"))?;
        drop(stmt);
        for (_old_id, kind, seq, time_created, time_updated, data) in &batch {
            tx.execute(
                "INSERT INTO session_message
                 (id, session_id, type, seq, time_created, time_updated, data)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    format!("{msg_prefix}_{seq}"),
                    session_id,
                    kind,
                    seq,
                    time_created,
                    time_updated,
                    data,
                ],
            )?;
        }
    }

    // reserveSequence: the forked event already sits at seq 0 (allocated by
    // the durable commit); never regress it.
    tx.execute(
        "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)
         ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, excluded.seq)",
        rusqlite::params![session_id, copied_seq],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::schema::event::DurableEnvelope;
    use crate::schema::event::SessionEventData;
    use crate::schema::support::ModelRef;
    use rusqlite::Connection;

    fn model() -> ModelRef {
        ModelRef {
            id: "m".to_owned(),
            provider_id: "p".to_owned(),
            variant: None,
        }
    }

    fn envelope(seq: i64, data: SessionEventData) -> EventEnvelope<SessionEventData> {
        EventEnvelope {
            id: format!("evt_{seq}"),
            created: seq * 10,
            kind: "session.test".to_owned(),
            data,
            durable: Some(DurableEnvelope {
                aggregate_id: "ses_close".to_owned(),
                seq,
                version: 1,
            }),
            location: None,
            metadata: None,
        }
    }

    fn assistant_row(tx: &Connection, id: &str) -> (i64, Option<i64>, Option<i64>) {
        tx.query_row(
            "SELECT seq, time_created, json_extract(data, '$.time.completed')
             FROM session_message WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("assistant row")
    }

    /// `StepStarted` with an incomplete current assistant closes that row in
    /// place: the closed row keeps its original `seq` (history position and
    /// the UNIQUE(session_id, seq) index) while the event's seq belongs to
    /// the fresh assistant appended after it.
    #[tokio::test]
    async fn step_started_closes_stale_assistant_in_place() {
        let db = Db::open_in_memory().await.expect("db");
        db.conn()
            .call(|conn| -> rusqlite::Result<()> {
                conn.execute(
                    "INSERT INTO session_v2 (id, project_id, slug, directory, version, time_created, time_updated)
                     VALUES ('ses_close', 'p', 's', '/tmp', 'v', 0, 0)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("session row");

        // Turn 1: a step starts (assistant msg_a) and never completes —
        // the crash / failed-step scenario.
        let first = envelope(
            1,
            SessionEventData::StepStarted {
                session_id: "ses_close".to_owned(),
                assistant_message_id: "msg_a".to_owned(),
                agent: "build".to_owned(),
                model: model(),
                snapshot: None,
                started: 100,
            },
        );
        db.conn()
            .call(move |conn| {
                let tx = conn.transaction()?;
                project(&tx, &first).expect("first step started");
                tx.commit()
            })
            .await
            .expect("turn 1");

        // Turn 2: another step starts — msg_a must close in place.
        let second = envelope(
            2,
            SessionEventData::StepStarted {
                session_id: "ses_close".to_owned(),
                assistant_message_id: "msg_b".to_owned(),
                agent: "build".to_owned(),
                model: model(),
                snapshot: None,
                started: 200,
            },
        );
        db.conn()
            .call(move |conn| {
                let tx = conn.transaction()?;
                project(&tx, &second).expect("second step started");
                tx.commit()
            })
            .await
            .expect("turn 2");

        let (seq_a, created_a, completed_a) = db
            .conn()
            .call(|conn| Ok(assistant_row(conn, "msg_a")) as rusqlite::Result<_>)
            .await
            .expect("row a");
        let (seq_b, _, _) = db
            .conn()
            .call(|conn| Ok(assistant_row(conn, "msg_b")) as rusqlite::Result<_>)
            .await
            .expect("row b");
        assert_eq!(seq_a, 1_i64, "the closed row keeps its original seq");
        assert_eq!(
            created_a,
            Some(100_i64),
            "the closed row keeps its creation"
        );
        // Completed carries the closing event's created time, not the new
        // step's `started`.
        assert_eq!(
            completed_a,
            Some(20_i64),
            "the closed row is marked completed"
        );
        assert_eq!(seq_b, 2_i64, "the fresh assistant owns the event's seq");
        // History order is unchanged: the closed assistant still sorts first.
        let order = db
            .conn()
            .call(|conn| -> rusqlite::Result<Vec<String>> {
                Ok(conn
                    .prepare("SELECT id FROM session_message WHERE session_id = 'ses_close' ORDER BY seq")
                    .expect("stmt")
                    .query_map([], |row| row.get::<_, String>(0))
                    .expect("rows")
                    .collect::<rusqlite::Result<Vec<String>>>()
                    .expect("collect"))
            })
            .await
            .expect("order");
        assert_eq!(order, vec!["msg_a".to_owned(), "msg_b".to_owned()]);
    }
}
