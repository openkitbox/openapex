//! In-transaction projector, ported from
//! `packages/core/src/session/projector.ts` (+ `inbox.ts` projections).
//!
//! Every durable event mutates its projection tables inside the same
//! transaction that allocates its seq. On any projector error the caller
//! rolls back, so projections and the event log stay consistent.

pub mod message_updater;

use anyhow::{Result, anyhow, bail};
use ox_db::turso::Connection;
use serde_json::{Value, json};

use ox_schema::event::{EventEnvelope, InterruptReason, SessionEventData};
use ox_schema::inbox::{Delivery, Item};
use ox_schema::message::{IdleOutcome, Message, TimeCreated};
use ox_schema::support::{Finite, ForkBoundary, MoneyUsd, TokenUsage};

/// Secondary effects to run after the durable transaction commits.
pub enum PostCommit {
    /// Broadcast ephemeral `session.usage.updated` with the session's
    /// accumulated usage columns.
    PublishUsage { session_id: String },
}

/// Dispatch a durable event to its projection. `tx` is the open transaction.
/// Returns any post-commit effects to run after COMMIT.
pub async fn project(
    tx: &Connection,
    env: &EventEnvelope<SessionEventData>,
) -> Result<Vec<PostCommit>> {
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
            let n = tx
                .execute(
                    "INSERT OR IGNORE INTO session_v2
                     (id, project_id, workspace_id, parent_id, slug, directory, path,
                      title, agent, model, metadata, permission, version,
                      time_created, time_updated)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14)",
                    (
                        session_id.as_str(),
                        project_id.as_str(),
                        location.workspace_id.as_deref(),
                        parent_id.as_deref(),
                        slug.as_str(),
                        location.directory.as_str(),
                        subpath.as_deref(),
                        title.as_deref(),
                        agent.as_deref(),
                        model_json.as_deref(),
                        meta_json.as_deref(),
                        perm_json.as_deref(),
                        version.as_str(),
                        env.created,
                    ),
                )
                .await?;
            if n == 0 {
                bail!("SessionAlreadyProjected: `{session_id}` already exists");
            }
        }

        E::SessionRenamed { session_id, title } => {
            tx.execute(
                "UPDATE session_v2 SET title = ?2, time_updated = ?3 WHERE id = ?1",
                (session_id.as_str(), title.as_str(), env.created),
            )
            .await?;
        }

        E::SessionDeleted { session_id } => {
            // `session_message`, `session_inbox` and `session_pending` cascade
            // from the `session_v2` delete; these magic-context tables carry no
            // foreign key, so they are cleared here in the same transaction or
            // they leak (and later searches/timelines resurrect a session the
            // user deleted). `mc_note` is session-scoped state — every read
            // filters `session_id = ?` — so it goes with its session, unlike
            // `mc_memory` and `mc_dream_state`, which are project-scoped.
            for table in [
                "mc_compartments",
                "mc_tag",
                "mc_drop",
                "mc_historian_run",
                "mc_frozen_state",
                "mc_note",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE session_id = ?1"),
                    [session_id.as_str()],
                )
                .await?;
            }
            tx.execute(
                "DELETE FROM session_v2 WHERE id = ?1",
                [session_id.as_str()],
            )
            .await?;
        }

        E::ExecutionStarted { session_id } => {
            touch(tx, session_id, env.created).await?;
            project_busy(tx, session_id, env.created).await?;
        }
        E::ExecutionSucceeded { session_id } => {
            message_updater::run(tx, env).await?;
            project_idle(tx, session_id, env.created, IdleOutcome::Succeeded).await?;
        }
        E::ExecutionFailed { session_id, .. } => {
            message_updater::run(tx, env).await?;
            project_idle(tx, session_id, env.created, IdleOutcome::Failed).await?;
        }
        E::ExecutionInterrupted { session_id, reason } => {
            message_updater::run(tx, env).await?;
            // Upstream: shutdown interrupts skip the idle-marker update.
            if *reason != InterruptReason::Shutdown {
                project_idle(tx, session_id, env.created, IdleOutcome::Interrupted).await?;
            }
        }

        E::InboxEnqueued {
            session_id,
            inbox_id,
            item,
        } => {
            project_admitted(tx, session_id, inbox_id, item, durable.seq, env.created).await?;
            touch(tx, session_id, env.created).await?;
        }
        E::InboxDelivered {
            session_id,
            inbox_id,
        } => {
            let item = project_delivered(tx, session_id, inbox_id).await?;
            // Only user/synthetic promote to a message; compaction/move do not.
            if let Some(msg) = promote_to_message(&item, inbox_id, env.created) {
                insert_message(tx, session_id, durable.seq, &msg).await?;
            }
        }
        E::InboxCancelled {
            session_id,
            inbox_id,
        } => {
            project_cancelled(tx, session_id, inbox_id).await?;
        }
        E::InboxDeliveryChanged {
            session_id,
            inbox_id,
            delivery,
        } => {
            project_delivery_changed(tx, session_id, inbox_id, delivery).await?;
        }

        E::SessionUsageRecorded {
            session_id,
            cost,
            tokens,
            ..
        } => {
            apply_usage(tx, session_id, *cost, tokens, None).await?;
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
            message_updater::run(tx, env).await?;
            tx.execute(
                "UPDATE session_v2 SET agent = ?2, time_updated = ?3 WHERE id = ?1",
                (session_id.as_str(), agent.as_str(), env.created),
            )
            .await?;
        }
        E::SessionModelSelected {
            session_id, model, ..
        } => {
            message_updater::run(tx, env).await?;
            tx.execute(
                "UPDATE session_v2 SET model = ?2, time_updated = ?3 WHERE id = ?1",
                (session_id.as_str(), to_json(model)?.as_str(), env.created),
            )
            .await?;
        }
        E::SessionMetadataUpdated {
            session_id,
            metadata,
        } => {
            tx.execute(
                "UPDATE session_v2 SET metadata = ?2, time_updated = ?3 WHERE id = ?1",
                (
                    session_id.as_str(),
                    to_json(metadata)?.as_str(),
                    env.created,
                ),
            )
            .await?;
        }
        E::SessionPermissions {
            session_id,
            permissions,
        } => {
            tx.execute(
                "UPDATE session_v2 SET permission = ?2, time_updated = ?3 WHERE id = ?1",
                (
                    session_id.as_str(),
                    to_json(permissions)?.as_str(),
                    env.created,
                ),
            )
            .await?;
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
                (session_id.as_str(), *idle),
            )
            .await?;
        }
        E::SessionArchived { session_id } => {
            tx.execute(
                "UPDATE session_v2 SET time_archived = ?2, time_updated = ?2 WHERE id = ?1",
                (session_id.as_str(), env.created),
            )
            .await?;
        }
        E::SessionUnarchived { session_id } => {
            tx.execute(
                "UPDATE session_v2 SET time_archived = NULL, time_updated = ?2 WHERE id = ?1",
                (session_id.as_str(), env.created),
            )
            .await?;
        }
        E::SessionForked { .. } => project_fork(tx, env).await?,

        // Message-updater events.
        E::SessionInstructionsUpdated { .. }
        | E::SessionSynthetic { .. }
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
            let out = message_updater::run(tx, env).await?;
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
            message_updater::run(tx, env).await?;
            tx.execute(
                "UPDATE session_v2 SET
                   directory = ?2,
                   path = ?3,
                   project_id = ?4,
                   workspace_id = ?5,
                   time_updated = ?6
                 WHERE id = ?1",
                (
                    session_id.as_str(),
                    payload.location.directory.as_str(),
                    payload.subpath.as_deref(),
                    payload.project_id.as_str(),
                    payload.location.workspace_id.as_deref(),
                    env.created,
                ),
            )
            .await?;
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
pub(crate) async fn touch(tx: &Connection, session_id: &str, created: i64) -> Result<()> {
    tx.execute(
        "UPDATE session_v2 SET time_updated = ?2 WHERE id = ?1",
        (session_id, created),
    )
    .await?;
    Ok(())
}

/// Upstream `Execution.Started` projection: `time_busy = created`, with
/// `time_updated` left unchanged (self-assignment). While the execution runs,
/// `time_idle` still holds the previous execution's end.
async fn project_busy(tx: &Connection, session_id: &str, created: i64) -> Result<()> {
    tx.execute(
        "UPDATE session_v2 SET time_busy = ?2, time_updated = time_updated WHERE id = ?1",
        (session_id, created),
    )
    .await?;
    Ok(())
}

/// projectIdle: `time_idle = max(created, coalesce(time_idle + 1, created))`,
/// set `idle_outcome`, leave `time_updated` unchanged (self-assignment).
async fn project_idle(
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
        (session_id, created, outcome_str),
    )
    .await?;
    Ok(())
}

/// Accumulate cost/tokens. `time_updated` is set only when `Some`.
pub(crate) async fn apply_usage(
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
            (
                session_id,
                finite_f64(cost),
                tokens.input,
                tokens.output,
                tokens.reasoning,
                tokens.cache.read,
                tokens.cache.write,
                t,
            ),
        )
        .await?;
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
            (
                session_id,
                finite_f64(cost),
                tokens.input,
                tokens.output,
                tokens.reasoning,
                tokens.cache.read,
                tokens.cache.write,
            ),
        )
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Inbox projections (packages/core/src/session/inbox.ts)
// ---------------------------------------------------------------------------

/// projectAdmitted: INSERT session_inbox. If a `session_message` already has
/// the id → LifecycleConflict. INSERT OR IGNORE with 0 rows ⇒ Err.
async fn project_admitted(
    tx: &Connection,
    session_id: &str,
    inbox_id: &str,
    item: &Item,
    seq: i64,
    created: i64,
) -> Result<()> {
    let mut stmt = tx
        .prepare("SELECT id FROM session_message WHERE id = ?1")
        .await?;
    let exists = ox_db::optional(&mut stmt, [inbox_id]).await?;
    if exists.is_some() {
        bail!("LifecycleConflict: a session_message already exists for `{inbox_id}`");
    }

    let n = tx
        .execute(
            "INSERT OR IGNORE INTO session_inbox
             (id, session_id, type, payload, delivery, enqueued_seq, time_created)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            (
                inbox_id,
                session_id,
                item.kind(),
                item.payload().to_string(),
                delivery_str(item.delivery()),
                seq,
                created,
            ),
        )
        .await?;
    if n == 0 {
        bail!("inbox item `{inbox_id}` already admitted");
    }
    Ok(())
}

/// projectDelivered: DELETE ... RETURNING; missing row ⇒ Err.
async fn project_delivered(tx: &Connection, session_id: &str, inbox_id: &str) -> Result<Item> {
    // `RETURNING` only produces rows through `query`; a one-shot `execute`
    // rejects them as `unexpected row during execution`.
    let mut stmt = tx
        .prepare(
            "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2
             RETURNING type, payload, delivery",
        )
        .await?;
    let Some(row) = ox_db::optional(&mut stmt, (inbox_id, session_id)).await? else {
        return Err(anyhow!(
            "inbox.delivered: no row `{inbox_id}` in session `{session_id}`"
        ));
    };
    let kind = row.get::<String>(0)?;
    let payload = row.get::<String>(1)?;
    let delivery = row.get::<String>(2)?;
    let payload_value: Value = serde_json::from_str(&payload)?;
    decode_item(&kind, payload_value, &delivery)
}

/// projectCancelled: DELETE only if delivery IN ('queue','steer'); missing ⇒ Err.
async fn project_cancelled(tx: &Connection, session_id: &str, inbox_id: &str) -> Result<()> {
    let n = tx
        .execute(
            "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2
             AND delivery IN ('queue','steer')",
            (inbox_id, session_id),
        )
        .await?;
    if n == 0 {
        bail!("inbox.cancelled: no cancellable row `{inbox_id}` in session `{session_id}`");
    }
    Ok(())
}

/// projectDeliveryChanged: UPDATE delivery with a from-guard (from = opposite
/// of to). 0 rows ⇒ Err.
async fn project_delivery_changed(
    tx: &Connection,
    session_id: &str,
    inbox_id: &str,
    to: &Delivery,
) -> Result<()> {
    let to_str = delivery_str(*to);
    let from_str = opposite_delivery(to_str);
    let n = tx
        .execute(
            "UPDATE session_inbox SET delivery = ?4
             WHERE id = ?1 AND session_id = ?2 AND delivery = ?3",
            (inbox_id, session_id, from_str, to_str),
        )
        .await?;
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
pub(crate) async fn insert_message(
    tx: &Connection,
    session_id: &str,
    seq: i64,
    message: &Message,
) -> Result<()> {
    let data = message.to_row_data().to_string();
    insert_message_row(
        tx,
        session_id,
        seq,
        message.id(),
        message.kind(),
        message.time_created(),
        &data,
    )
    .await
}

/// Insert a message row from values the caller already has, so a caller
/// holding a concrete variant can avoid re-wrapping it in [`Message`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_message_row(
    tx: &Connection,
    session_id: &str,
    seq: i64,
    id: &str,
    kind: &str,
    time_created: i64,
    data: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO session_message
         (id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1,?2,?3,?4,?5,?5,?6)",
        (id, session_id, kind, seq, time_created, data),
    )
    .await?;
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
async fn project_fork(tx: &Connection, env: &EventEnvelope<SessionEventData>) -> Result<()> {
    let SessionEventData::SessionForked {
        session_id,
        parent_id,
        boundary,
        ..
    } = &env.data
    else {
        bail!("project_fork called with {}", env.kind);
    };

    let mut stmt = tx
        .prepare(
            "SELECT project_id, workspace_id, directory, path, title, agent,
                    model, metadata, permission, version
             FROM session_v2 WHERE id = ?1",
        )
        .await?;
    let parent: Option<ParentRow> = ox_db::optional(&mut stmt, [parent_id.as_str()])
        .await?
        .map(|row| {
            Ok::<ParentRow, anyhow::Error>((
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
        })
        .transpose()?;
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
    let mut stmt = tx
        .prepare("SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2")
        .await?;
    let boundary_seq: Option<i64> = ox_db::optional(
        &mut stmt,
        (parent_id.as_str(), boundary_message_id.as_str()),
    )
    .await?
    .map(|row| row.get(0))
    .transpose()?;
    let boundary_seq = boundary_seq
        .ok_or_else(|| anyhow!("Fork boundary message not found: {boundary_message_id}"))?;

    // Last message seq included in the copy ("before" is exclusive).
    let mut stmt = tx
        .prepare(if matches!(boundary, ForkBoundary::Before { .. }) {
            "SELECT seq FROM session_message WHERE session_id = ?1 AND seq < ?2
             ORDER BY seq DESC LIMIT 1"
        } else {
            "SELECT seq FROM session_message WHERE session_id = ?1 AND seq <= ?2
             ORDER BY seq DESC LIMIT 1"
        })
        .await?;
    let copied_seq: Option<i64> = ox_db::optional(&mut stmt, (parent_id.as_str(), boundary_seq))
        .await?
        .map(|row| row.get(0))
        .transpose()?;

    // Forked row: new slug, cost/tokens zeroed (column defaults), location and
    // settings inherited from the parent. Upstream `.onConflictDoNothing()`
    // maps to `INSERT OR IGNORE`; a pre-existing row is a defect.
    let n = tx
        .execute(
            "INSERT OR IGNORE INTO session_v2
             (id, project_id, workspace_id, parent_id, fork_session_id, fork_boundary,
              slug, directory, path, title, agent, model, metadata, permission, version,
              time_created, time_updated)
             VALUES (?1,?2,?3,NULL,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?15)",
            (
                session_id.as_str(),
                project_id,
                workspace_id,
                parent_id.as_str(),
                to_json(boundary)?,
                ox_schema::slug::create(),
                directory,
                path,
                fork_title(title.as_deref()),
                agent,
                model,
                metadata,
                permission,
                version,
                env.created,
            ),
        )
        .await?;
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
    let msg_prefix = ox_schema::id::from_event(&env.id);
    let mut cursor: i64 = -1;
    loop {
        let mut stmt = tx
            .prepare(
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
            )
            .await?;
        let mut rows = stmt
            .query((
                parent_id.as_str(),
                cursor,
                copied_seq,
                FORK_BATCH_SIZE as i64,
            ))
            .await?;
        let mut batch = Vec::new();
        while let Some(row) = rows.next().await? {
            batch.push((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<i64>(2)?,
                row.get::<i64>(3)?,
                row.get::<i64>(4)?,
                row.get::<String>(5)?,
            ));
        }
        if batch.is_empty() {
            break;
        }
        cursor = batch
            .last()
            .map(|(_, _, seq, _, _, _)| *seq)
            .ok_or_else(|| anyhow!("fork copy: empty batch"))?;
        for (_old_id, kind, seq, time_created, time_updated, data) in &batch {
            tx.execute(
                "INSERT INTO session_message
                 (id, session_id, type, seq, time_created, time_updated, data)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                (
                    format!("{msg_prefix}_{seq}"),
                    session_id.as_str(),
                    kind.as_str(),
                    *seq,
                    *time_created,
                    *time_updated,
                    data.as_str(),
                ),
            )
            .await?;
        }
    }

    // Carry the parent's folded history into the child. A compartment is a
    // summary of an ordinal range of the session's OWN messages, and the copy
    // above preserves the parent's seq numbers — so a compartment whose
    // coverage ends inside the copied range still describes exactly the rows
    // the child owns. A compartment reaching past `copied_seq` summarizes
    // messages the child does not have, so it stays behind: a fork may land
    // inside a compartment, and the child must never inherit a summary of
    // content it cannot see.
    //
    // Row ids are minted per row in the table's own scheme (`mc_…`, as the
    // historian mints them) rather than derived from the forking event the way
    // the message ids above are: `mc_compartments.id` is an opaque text handle,
    // and deriving it from the coverage end would let a parent whose rows end
    // at the same ordinal collide on the child's primary key. Minting per row
    // is why this is a batched read/write loop instead of one `INSERT … SELECT`;
    // the batch bound matches the message copy.
    //
    // Deliberately NOT copied: `mc_frozen_state` (a cache the child rebuilds on
    // its first pass), `mc_tag` / `mc_drop` (overlays that address messages by
    // id, and those ids are fresh in the child), `mc_historian_run` (the
    // parent's run history, not state), and `mc_note` (the parent's
    // session-scoped intentions).
    let mut cursor: i64 = 0;
    loop {
        let mut stmt = tx
            .prepare(
                "SELECT coverage_end, importance, tiers, search_text, created_at
                 FROM mc_compartments
                 WHERE session_id = ?1
                 ORDER BY created_at ASC, id ASC
                 LIMIT ?2 OFFSET ?3",
            )
            .await?;
        let mut rows = stmt
            .query((parent_id.as_str(), FORK_BATCH_SIZE as i64, cursor))
            .await?;
        let mut batch = Vec::new();
        while let Some(row) = rows.next().await? {
            batch.push((
                row.get::<String>(0)?,
                row.get::<f64>(1)?,
                row.get::<String>(2)?,
                row.get::<String>(3)?,
                row.get::<i64>(4)?,
            ));
        }
        if batch.is_empty() {
            break;
        }
        cursor += batch.len() as i64;
        for (coverage_end, importance, tiers, search_text, created_at) in &batch {
            // Fail closed: a coverage end that does not parse cannot be proven
            // inside the copied range, so its compartment is left behind
            // (`Compartment::end_ordinal` reads it the same way).
            let Ok(end_ordinal) = coverage_end.trim().parse::<i64>() else {
                continue;
            };
            if end_ordinal > copied_seq {
                continue;
            }
            tx.execute(
                "INSERT INTO mc_compartments
                 (id, session_id, coverage_end, importance, tiers, search_text, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                (
                    ox_schema::id::id("mc"),
                    session_id.as_str(),
                    coverage_end.as_str(),
                    *importance,
                    tiers.as_str(),
                    search_text.as_str(),
                    *created_at,
                ),
            )
            .await?;
        }
    }

    // reserveSequence: the forked event already sits at seq 0 (allocated by
    // the durable commit); never regress it.
    tx.execute(
        "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)
         ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, excluded.seq)",
        (session_id.as_str(), copied_seq),
    )
    .await?;
    Ok(())
}
