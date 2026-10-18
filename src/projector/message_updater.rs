//! Message projection port of
//! `packages/core/src/session/message-updater.ts`.
//!
//! Each durable event mutates/appends assistant messages inside the durable
//! transaction. The SQL adapter indirection of the TS original is collapsed to
//! direct rusqlite calls; the projection logic is ported faithfully.

use anyhow::{Context as _, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::id;
use crate::schema::event::{EventEnvelope, SessionEventData, StepFailedFinish};
use crate::schema::message::{
    AssistantContent, AssistantMessage, AssistantRetry, AssistantSnapshot, AssistantTime,
    Compaction, CompactionCompleted, CompactionMessage, IdleOutcome, LocationSwitch, Message,
    TimeCreated, ToolState, ToolTime,
};
use crate::schema::support::{FinishReason, ModelRef, Record};

use super::{apply_usage, insert_message, touch};

pub struct Output {
    /// Whether the event recorded usage (step.ended / step.failed with usage),
    /// triggering a post-commit `session.usage.updated` broadcast.
    pub usage_recorded: bool,
}

// ---------------------------------------------------------------------------
// Assistant row load/store helpers
// ---------------------------------------------------------------------------

/// Decode an assistant `data` JSON row into an `AssistantMessage`.
fn decode_assistant(id: &str, data: &str) -> Result<AssistantMessage> {
    let value: Value = serde_json::from_str(data)?;
    let msg = Message::from_row_data(id, "assistant", value)?;
    match msg {
        Message::Assistant(a) => Ok(*a),
        other => Err(anyhow!("expected assistant row, got {}", other.kind())),
    }
}

/// Load the current assistant message for a session. Port of upstream
/// `getCurrentAssistant`: select the latest assistant row by `seq DESC`, then
/// keep it only if it has no `time.completed` (the filter happens on the
/// decoded message, not in SQL — a completed latest row yields `None`, not an
/// older incomplete row).
fn load_current_assistant(tx: &Connection, session_id: &str) -> Result<Option<AssistantMessage>> {
    let row = tx
        .query_row(
            "SELECT id, data FROM session_message \
             WHERE type = 'assistant' AND session_id = ?1 ORDER BY seq DESC LIMIT 1",
            rusqlite::params![session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let decoded = row
        .map(|(id, data)| decode_assistant(&id, &data))
        .transpose()?;
    Ok(decoded.filter(|msg| msg.time.completed.is_none()))
}

/// Load an assistant message by id.
fn load_assistant(
    tx: &Connection,
    session_id: &str,
    message_id: &str,
) -> Result<Option<AssistantMessage>> {
    let row = tx
        .query_row(
            "SELECT id, data FROM session_message \
             WHERE type = 'assistant' AND id = ?1 AND session_id = ?2",
            rusqlite::params![message_id, session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(id, data)| decode_assistant(&id, &data))
        .transpose()
}

/// Persist an assistant message row (INSERT or UPDATE) with a new seq.
fn store_assistant(
    tx: &Connection,
    session_id: &str,
    seq: i64,
    message_id: &str,
    msg: &AssistantMessage,
) -> Result<()> {
    let data = Message::Assistant(Box::new(msg.clone()))
        .to_row_data()
        .to_string();
    // UPDATE existing; INSERT if absent (mirrors upstream upsert behaviour).
    let n = tx.execute(
        "UPDATE session_message SET seq = ?3, data = ?4, time_updated = ?5
         WHERE id = ?1 AND session_id = ?2",
        rusqlite::params![message_id, session_id, seq, data, msg.time.created],
    )?;
    if n == 0 {
        let m = Message::Assistant(Box::new(msg.clone()));
        insert_message(tx, session_id, seq, &m)?;
    }
    Ok(())
}

/// Decode a compaction `data` JSON row into a `CompactionMessage`.
fn decode_compaction(id: &str, data: &str) -> Result<CompactionMessage> {
    let value: Value = serde_json::from_str(data)?;
    let msg = Message::from_row_data(id, "compaction", value)?;
    match msg {
        Message::Compaction(c) => Ok(c),
        other => Err(anyhow!("expected compaction row, got {}", other.kind())),
    }
}

/// Load the latest compaction message for a session (upstream `getCompaction`).
fn load_latest_compaction(tx: &Connection, session_id: &str) -> Result<Option<CompactionMessage>> {
    let row = tx
        .query_row(
            "SELECT id, data FROM session_message \
             WHERE type = 'compaction' AND session_id = ?1 ORDER BY id DESC LIMIT 1",
            rusqlite::params![session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(id, data)| decode_compaction(&id, &data))
        .transpose()
}

/// Read the current session's location for the `previous` field of a
/// `location-switched` message. Port of upstream `getLocation`.
fn load_location(tx: &Connection, session_id: &str) -> Result<Option<LocationSwitch>> {
    let row = tx
        .query_row(
            "SELECT directory, workspace_id, project_id, path FROM session_v2 WHERE id = ?1",
            rusqlite::params![session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(directory, workspace_id, project_id, subpath)| LocationSwitch {
            location: crate::schema::support::LocationRef {
                directory,
                workspace_id,
            },
            project_id,
            subpath,
        },
    ))
}

/// Read the session row's current `agent` (upstream `getAgent`).
/// `None` covers both a missing row and a NULL column.
fn load_session_agent(tx: &Connection, session_id: &str) -> Result<Option<String>> {
    let raw: Option<Option<String>> = tx
        .query_row(
            "SELECT agent FROM session_v2 WHERE id = ?1",
            rusqlite::params![session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(raw.flatten())
}

/// Read the session row's current `model` JSON (upstream `getModel`).
fn load_session_model(tx: &Connection, session_id: &str) -> Result<Option<ModelRef>> {
    let raw: Option<Option<String>> = tx
        .query_row(
            "SELECT model FROM session_v2 WHERE id = ?1",
            rusqlite::params![session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    raw.flatten()
        .map(|s| Ok(serde_json::from_str(&s)?))
        .transpose()
}

/// Persist a compaction message row (UPDATE existing id, else INSERT).
fn store_compaction(
    tx: &Connection,
    session_id: &str,
    seq: i64,
    msg: &CompactionMessage,
) -> Result<()> {
    let data = Message::Compaction(msg.clone()).to_row_data().to_string();
    let n = tx.execute(
        "UPDATE session_message SET seq = ?3, data = ?4, time_updated = ?5
         WHERE id = ?1 AND session_id = ?2",
        rusqlite::params![msg.id, session_id, seq, data, msg.time.created],
    )?;
    if n == 0 {
        insert_message(tx, session_id, seq, &Message::Compaction(msg.clone()))?;
    }
    Ok(())
}

/// Clear the `retry` block on the current (latest) assistant message.
/// Upstream `clearCurrentRetry`.
fn clear_current_retry(tx: &Connection, session_id: &str, seq: i64) -> Result<()> {
    if let Some(mut current) = load_current_assistant(tx, session_id)?
        && current.retry.is_some()
    {
        current.retry = None;
        store_assistant(tx, session_id, seq, &current.id, &current)?;
    }
    Ok(())
}

/// Mutable access to the assistant message owned by `message_id`, applying
/// `recipe` then persisting. No-op if the row is missing (upstream
/// `updateOwnedAssistant` returns void when not found).
fn update_owned_assistant(
    tx: &Connection,
    session_id: &str,
    seq: i64,
    message_id: &str,
    recipe: impl FnOnce(&mut AssistantMessage),
) -> Result<()> {
    if let Some(mut msg) = load_assistant(tx, session_id, message_id)? {
        recipe(&mut msg);
        store_assistant(tx, session_id, seq, message_id, &msg)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Content helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Main dispatcher
// ---------------------------------------------------------------------------

pub fn run(tx: &Connection, env: &EventEnvelope<SessionEventData>) -> Result<Output> {
    use SessionEventData as E;
    let durable = env.durable.as_ref().context("durable envelope")?;
    let seq = durable.seq;
    let created = env.created;
    let mut usage_recorded = false;

    match &env.data {
        E::SessionMoved {
            session_id,
            payload,
        } => {
            // Append a `location-switched` message recording the new location,
            // with `previous` read from the current session_v2 row.
            let previous = load_location(tx, session_id)?;
            let msg = Message::LocationSwitched {
                id: id::from_event(&env.id),
                metadata: env.metadata.clone(),
                time: TimeCreated { created },
                location: LocationSwitch {
                    location: payload.location.clone(),
                    project_id: Some(payload.project_id.clone()),
                    subpath: payload.subpath.clone(),
                },
                previous,
            };
            insert_message(tx, session_id, seq, &msg)?;
        }

        E::SessionAgentSelected {
            session_id,
            agent,
            previous,
        } => {
            // Append an `agent-switched` message; `previous` falls back to the
            // session row (read before the projector updates it below).
            let previous = match previous {
                Some(agent) => Some(agent.clone()),
                None => load_session_agent(tx, session_id)?,
            };
            let msg = Message::AgentSwitched {
                id: id::from_event(&env.id),
                metadata: env.metadata.clone(),
                time: TimeCreated { created },
                agent: agent.clone(),
                previous,
            };
            insert_message(tx, session_id, seq, &msg)?;
        }

        E::SessionModelSelected {
            session_id,
            model,
            previous,
        } => {
            let previous = match previous {
                Some(model) => Some(model.clone()),
                None => load_session_model(tx, session_id)?,
            };
            let msg = Message::ModelSwitched {
                id: id::from_event(&env.id),
                metadata: env.metadata.clone(),
                time: TimeCreated { created },
                model: model.clone(),
                previous,
            };
            insert_message(tx, session_id, seq, &msg)?;
        }

        E::SessionSynthetic {
            session_id,
            text,
            description,
            metadata,
        } => {
            let msg = Message::Synthetic {
                id: id::from_event(&env.id),
                metadata: metadata.clone(),
                time: TimeCreated { created },
                text: text.clone(),
                description: description.clone(),
            };
            insert_message(tx, session_id, seq, &msg)?;
        }

        E::SessionRetryScheduled {
            session_id,
            assistant_message_id,
            attempt,
            at,
            error,
        } => {
            let mut msg = load_assistant(tx, session_id, assistant_message_id)?
                .ok_or_else(|| anyhow!("no assistant message `{assistant_message_id}`"))?;
            msg.retry = Some(AssistantRetry {
                attempt: *attempt,
                at: *at,
                error: error.clone(),
            });
            store_assistant(tx, session_id, seq, assistant_message_id, &msg)?;
        }

        E::SessionMessageContentUpdated {
            session_id,
            message_id,
            content,
        } => {
            let mut msg = load_assistant(tx, session_id, message_id)?
                .ok_or_else(|| anyhow!("no assistant message `{message_id}`"))?;
            msg.content = content.clone();
            store_assistant(tx, session_id, seq, message_id, &msg)?;
        }

        E::StepStarted {
            session_id,
            assistant_message_id,
            agent,
            model,
            snapshot,
            started,
        } => {
            // Upstream `session.step.started`:
            //  - if the assistant row exists → reset it (agent/model, clear
            //    retry/error/finish/rawFinish/providerState, restart time,
            //    merge snapshot.start);
            //  - else → close any current incomplete assistant (set completed,
            //    clear retry) and append a fresh assistant row.
            let existing = load_assistant(tx, session_id, assistant_message_id)?;
            if let Some(mut msg) = existing {
                msg.agent = agent.clone();
                msg.model = model.clone();
                msg.retry = None;
                msg.error = None;
                msg.finish = None;
                msg.raw_finish = None;
                msg.provider_state = None;
                msg.time.created = *started;
                msg.time.streamed = None;
                msg.time.completed = None;
                if let Some(snap) = snapshot.clone() {
                    msg.snapshot
                        .get_or_insert(AssistantSnapshot {
                            start: None,
                            end: None,
                            files: None,
                        })
                        .start = Some(snap);
                }
                store_assistant(tx, session_id, seq, assistant_message_id, &msg)?;
            } else {
                if let Some(mut current) = load_current_assistant(tx, session_id)? {
                    current.retry = None;
                    current.time.completed = Some(created);
                    store_assistant(tx, session_id, seq, &current.id, &current)?;
                }
                let msg = AssistantMessage {
                    id: assistant_message_id.clone(),
                    metadata: None,
                    time: AssistantTime {
                        created: *started,
                        streamed: None,
                        completed: None,
                    },
                    agent: agent.clone(),
                    model: model.clone(),
                    content: Vec::new(),
                    snapshot: snapshot.clone().map(|s| AssistantSnapshot {
                        start: Some(s),
                        end: None,
                        files: None,
                    }),
                    finish: None,
                    raw_finish: None,
                    provider_state: None,
                    cost: None,
                    tokens: None,
                    error: None,
                    retry: None,
                };
                store_assistant(tx, session_id, seq, assistant_message_id, &msg)?;
            }
            touch(tx, session_id, created)?;
        }

        E::StepStreamed {
            session_id,
            assistant_message_id,
        } => {
            let mut msg = load_assistant(tx, session_id, assistant_message_id)?
                .ok_or_else(|| anyhow!("no assistant message `{assistant_message_id}`"))?;
            msg.time.streamed = Some(created);
            store_assistant(tx, session_id, seq, assistant_message_id, &msg)?;
        }

        E::StepEnded {
            session_id,
            assistant_message_id,
            finish,
            raw_finish,
            provider_state,
            cost,
            tokens,
            snapshot,
            files,
        } => {
            let mut msg = load_assistant(tx, session_id, assistant_message_id)?
                .ok_or_else(|| anyhow!("no assistant message `{assistant_message_id}`"))?;
            msg.finish = Some(*finish);
            msg.raw_finish = raw_finish.clone();
            if provider_state.is_some() {
                msg.provider_state = provider_state.clone();
            }
            msg.cost = Some(*cost);
            msg.tokens = Some(*tokens);
            if snapshot.is_some() || files.is_some() {
                let snap = msg.snapshot.get_or_insert(AssistantSnapshot {
                    start: None,
                    end: None,
                    files: None,
                });
                snap.end = snapshot.clone();
                if files.is_some() {
                    snap.files = files.clone();
                }
            }
            msg.time.completed = Some(created);
            store_assistant(tx, session_id, seq, assistant_message_id, &msg)?;
            apply_usage(tx, session_id, *cost, tokens, Some(created))?;
            usage_recorded = true;
        }

        E::StepFailed {
            session_id,
            assistant_message_id,
            error,
            finish,
            raw_finish,
            provider_state,
            cost,
            tokens,
            snapshot,
            files,
        } => {
            let mut msg = load_assistant(tx, session_id, assistant_message_id)?
                .ok_or_else(|| anyhow!("no assistant message `{assistant_message_id}`"))?;
            msg.time.completed = Some(created);
            // Upstream: `finish = event.data.finish ?? "error"`.
            msg.finish = Some(match finish {
                Some(StepFailedFinish::ContentFilter) => FinishReason::ContentFilter,
                None => FinishReason::Error,
            });
            msg.raw_finish = raw_finish.clone();
            msg.provider_state = provider_state.clone();
            msg.error = Some(error.clone());
            msg.retry = None;
            if let (Some(c), Some(t)) = (cost, tokens) {
                msg.cost = Some(*c);
                msg.tokens = Some(*t);
            }
            if snapshot.is_some() || files.is_some() {
                let snap = msg.snapshot.get_or_insert(AssistantSnapshot {
                    start: None,
                    end: None,
                    files: None,
                });
                snap.end = snapshot.clone();
                if files.is_some() {
                    snap.files = files.clone();
                }
            }
            store_assistant(tx, session_id, seq, assistant_message_id, &msg)?;
            // applyUsage only when both cost and tokens are present; else touch.
            if let (Some(c), Some(t)) = (cost, tokens) {
                apply_usage(tx, session_id, *c, t, Some(created))?;
                usage_recorded = true;
            } else {
                touch(tx, session_id, created)?;
            }
        }

        E::TextStarted {
            session_id,
            assistant_message_id,
            ..
        } => {
            update_owned_assistant(tx, session_id, seq, assistant_message_id, |msg| {
                msg.content.push(AssistantContent::Text {
                    text: String::new(),
                    state: None,
                });
            })?;
        }

        E::TextEnded {
            session_id,
            assistant_message_id,
            text,
            state,
            ..
        } => {
            update_owned_assistant(tx, session_id, seq, assistant_message_id, |msg| {
                // Upstream `latestText`: mutate the last text entry.
                if let Some(content) = msg
                    .content
                    .iter_mut()
                    .rev()
                    .find(|c| matches!(c, AssistantContent::Text { .. }))
                    && let AssistantContent::Text { text: t, state: s } = content
                {
                    *t = text.clone();
                    *s = state.clone();
                }
            })?;
        }

        E::ReasoningStarted {
            session_id,
            assistant_message_id,
            state,
            ..
        } => {
            update_owned_assistant(tx, session_id, seq, assistant_message_id, |msg| {
                msg.content.push(AssistantContent::Reasoning {
                    text: String::new(),
                    state: state.clone(),
                    time: Some(crate::schema::message::ShellTime {
                        created,
                        completed: None,
                    }),
                });
            })?;
        }

        E::ReasoningEnded {
            session_id,
            assistant_message_id,
            text,
            state,
            ..
        } => {
            update_owned_assistant(tx, session_id, seq, assistant_message_id, |msg| {
                // Upstream `latestReasoning`: mutate the last uncompleted
                // reasoning entry.
                if let Some(content) = msg.content.iter_mut().rev().find(|c| {
                    matches!(c, AssistantContent::Reasoning { time, .. } if time.as_ref().map(|t| t.completed.is_none()).unwrap_or(true))
                }) && let AssistantContent::Reasoning { text: t, state: s, time } = content
                {
                    *t = text.clone();
                    let prior_created = time.as_ref().map(|x| x.created).unwrap_or(created);
                    *time = Some(crate::schema::message::ShellTime {
                        created: prior_created,
                        completed: Some(created),
                    });
                    if let Some(st) = state.clone() {
                        *s = Some(st);
                    }
                }
            })?;
        }

        E::ToolInputStarted {
            session_id,
            assistant_message_id,
            id,
            name,
        } => {
            let mut msg = load_assistant(tx, session_id, assistant_message_id)?
                .ok_or_else(|| anyhow!("no assistant message `{assistant_message_id}`"))?;
            msg.content.push(AssistantContent::Tool {
                id: id.clone(),
                name: name.clone(),
                executed: None,
                provider_state: None,
                provider_result_state: None,
                state: Box::new(ToolState::Streaming {
                    input: String::new(),
                }),
                time: ToolTime {
                    created,
                    ran: None,
                    completed: None,
                },
            });
            store_assistant(tx, session_id, seq, assistant_message_id, &msg)?;
        }

        E::ToolInputEnded {
            session_id,
            assistant_message_id,
            id,
            text,
        } => {
            update_owned_assistant(tx, session_id, seq, assistant_message_id, |msg| {
                if let Some(tool) = find_tool_mut(msg, id)
                    && matches!(&*tool.state, ToolState::Streaming { .. })
                {
                    *tool.state = ToolState::Streaming {
                        input: text.clone(),
                    };
                }
            })?;
        }

        E::ToolCalled {
            session_id,
            assistant_message_id,
            id,
            input,
            executed,
            state,
        } => {
            update_owned_assistant(tx, session_id, seq, assistant_message_id, |msg| {
                if let Some(tool) = find_tool_mut(msg, id) {
                    *tool.executed = Some(*executed);
                    *tool.provider_state = state.clone();
                    tool.time.ran = Some(created);
                    *tool.state = ToolState::Running {
                        input: input.clone(),
                        metadata: Record::new(),
                    };
                }
            })?;
        }

        E::ToolSuccess {
            session_id,
            assistant_message_id,
            id,
            content,
            metadata,
            executed,
            result_state,
        } => {
            update_owned_assistant(tx, session_id, seq, assistant_message_id, |msg| {
                if let Some(tool) = find_tool_mut(msg, id)
                    && let ToolState::Running { input, .. } = &*tool.state
                {
                    // Only promote when currently `running`.
                    let input = input.clone();
                    *tool.executed = Some(*executed || (*tool.executed).unwrap_or(false));
                    *tool.provider_result_state = result_state.clone();
                    tool.time.completed = Some(created);
                    *tool.state = ToolState::Completed {
                        input,
                        content: content.clone(),
                        metadata: metadata.clone(),
                    };
                }
            })?;
        }

        E::ToolFailed {
            session_id,
            assistant_message_id,
            id,
            error,
            content,
            metadata,
            executed,
            result_state,
        } => {
            update_owned_assistant(tx, session_id, seq, assistant_message_id, |msg| {
                if let Some(tool) = find_tool_mut(msg, id)
                    && matches!(
                        &*tool.state,
                        ToolState::Streaming { .. } | ToolState::Running { .. }
                    )
                {
                    // Error input: `{}` when the prior input was a raw string,
                    // else the structured input record.
                    let input = match &*tool.state {
                        ToolState::Running { input, .. } => input.clone(),
                        ToolState::Streaming { .. } => Record::new(),
                        ToolState::Completed { input, .. } => input.clone(),
                        ToolState::Error { input, .. } => input.clone(),
                    };
                    *tool.executed = Some(*executed || (*tool.executed).unwrap_or(false));
                    *tool.provider_result_state = result_state.clone();
                    tool.time.completed = Some(created);
                    *tool.state = ToolState::Error {
                        input,
                        error: error.clone(),
                        content: content.clone(),
                        metadata: metadata.clone(),
                    };
                }
            })?;
        }

        E::CompactionStarted {
            session_id,
            reason,
            recent,
            ..
        } => {
            let msg = Message::Compaction(CompactionMessage {
                id: id::from_event(&env.id),
                metadata: None,
                time: TimeCreated { created },
                compaction: Compaction::Running {
                    reason: *reason,
                    summary: String::new(),
                    recent: recent.clone(),
                },
            });
            insert_message(tx, session_id, seq, &msg)?;
        }

        E::CompactionEnded {
            session_id,
            reason,
            model,
            provider_state,
            provider_context,
            text,
            recent,
            cost,
            tokens,
        } => {
            // Upstream: if the latest compaction is `running`, update it to
            // `completed`; otherwise append a fresh completed compaction.
            let current = load_latest_compaction(tx, session_id)?;
            let mut running = match &current {
                Some(c) if matches!(c.compaction, Compaction::Running { .. }) => current,
                _ => None,
            };
            let completed = Compaction::Completed(Box::new(CompactionCompleted {
                reason: *reason,
                model: model.clone(),
                provider_state: provider_state.clone(),
                summary: text.clone(),
                recent: recent.clone(),
                provider_context: provider_context.clone(),
                cost: *cost,
                tokens: *tokens,
            }));
            if let Some(c) = running.as_mut() {
                c.compaction = completed;
                store_compaction(tx, session_id, seq, c)?;
            } else {
                let msg = CompactionMessage {
                    id: id::from_event(&env.id),
                    metadata: None,
                    time: TimeCreated { created },
                    compaction: completed,
                };
                insert_message(tx, session_id, seq, &Message::Compaction(msg))?;
            }
            if let (Some(c), Some(t)) = (cost, tokens) {
                apply_usage(tx, session_id, *c, t, Some(created))?;
                usage_recorded = true;
            }
        }

        E::CompactionFailed {
            session_id,
            reason,
            error,
            cost,
            tokens,
            ..
        } => {
            // Upstream: if the latest compaction is `running`, update it to
            // `failed`; otherwise append a fresh failed compaction.
            let current = load_latest_compaction(tx, session_id)?;
            let mut running = match &current {
                Some(c) if matches!(c.compaction, Compaction::Running { .. }) => current,
                _ => None,
            };
            let failed = Compaction::Failed {
                reason: *reason,
                error: error.clone(),
                cost: *cost,
                tokens: *tokens,
            };
            if let Some(c) = running.as_mut() {
                c.compaction = failed;
                store_compaction(tx, session_id, seq, c)?;
            } else {
                let msg = CompactionMessage {
                    id: id::from_event(&env.id),
                    metadata: None,
                    time: TimeCreated { created },
                    compaction: failed,
                };
                insert_message(tx, session_id, seq, &Message::Compaction(msg))?;
            }
            if let (Some(c), Some(t)) = (cost, tokens) {
                apply_usage(tx, session_id, *c, t, Some(created))?;
                usage_recorded = true;
            }
        }

        E::ExecutionSucceeded { session_id } => {
            clear_current_retry(tx, session_id, seq)?;
            let msg = Message::Idle {
                id: id::from_event(&env.id),
                metadata: None,
                time: TimeCreated { created },
                outcome: IdleOutcome::Succeeded,
                error: None,
            };
            insert_message(tx, session_id, seq, &msg)?;
        }

        E::ExecutionFailed { session_id, error } => {
            clear_current_retry(tx, session_id, seq)?;
            let msg = Message::Idle {
                id: id::from_event(&env.id),
                metadata: None,
                time: TimeCreated { created },
                outcome: IdleOutcome::Failed,
                error: Some(error.clone()),
            };
            insert_message(tx, session_id, seq, &msg)?;
        }

        E::ExecutionInterrupted { session_id, reason } => {
            // Shutdown keeps the execution claim and the resumed drain
            // continues the turn — only clear the retry, no idle message.
            if matches!(reason, crate::schema::event::InterruptReason::Shutdown) {
                clear_current_retry(tx, session_id, seq)?;
            } else {
                clear_current_retry(tx, session_id, seq)?;
                let msg = Message::Idle {
                    id: id::from_event(&env.id),
                    metadata: None,
                    time: TimeCreated { created },
                    outcome: IdleOutcome::Interrupted,
                    error: None,
                };
                insert_message(tx, session_id, seq, &msg)?;
            }
        }

        other => {
            bail!("message_updater: unhandled event `{}`", other.kind());
        }
    }

    Ok(Output { usage_recorded })
}

// ---------------------------------------------------------------------------
// Content mutation helpers
// ---------------------------------------------------------------------------

/// Borrow the tool content entry with id `id`. Returns the boxed `ToolState`
/// plus the sibling mutable fields needed by the tool event handlers.
///
/// Because `AssistantContent::Tool.state` is a `Box<ToolState>`, handlers
/// assign a fresh `ToolState` (the `DerefMut` on `Box` lets them write
/// `tool.state = ...` via the struct below).
fn find_tool_mut<'a>(msg: &'a mut AssistantMessage, id: &str) -> Option<ToolRefMut<'a>> {
    msg.content.iter_mut().find_map(|c| match c {
        AssistantContent::Tool {
            id: cid,
            executed,
            provider_state,
            provider_result_state,
            state: state_box,
            time,
            ..
        } if cid == id => Some(ToolRefMut {
            executed,
            provider_state,
            provider_result_state,
            state: state_box.as_mut(),
            time,
        }),
        _ => None,
    })
}

/// Mutable view over a tool content entry's updatable fields.
struct ToolRefMut<'a> {
    executed: &'a mut Option<bool>,
    provider_state: &'a mut Option<Record>,
    provider_result_state: &'a mut Option<Record>,
    state: &'a mut ToolState,
    time: &'a mut ToolTime,
}
