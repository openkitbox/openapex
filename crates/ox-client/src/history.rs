//! History parsing: a chronological `Vec<Message>` becomes the store's single
//! ordered `Vec<Item>`. Pure function, no IO. Shell / subagent completion
//! notices and every described synthetic message (a historian fold notice)
//! become rows; a synthetic without a description stays skipped, and so does a
//! compaction still `running` (its settled states become boundary items).
//! `*-switched`, `system` and `skill` messages become row items, and an
//! `idle` error becomes one only when it differs from the error the turn's
//! last step or settled compaction already shows (upstream `rows.ts`
//! `reduceSessionRows` `shownError`).
//!
//! A turn's start is an [`Item::Turn`], emitted at the input that begins the
//! turn: a `synthetic` message, described or not, or a `user` message that
//! begins one. A `user` message is its own row, and it begins a turn only when
//! none is open — a prompt typed while a turn runs is persisted mid-turn, and
//! taking it as a boundary would date the rest of the turn from the steer.
//! Which input begins a turn follows upstream
//! `packages/util/src/session-turn.ts` `inputIndex`: the first input after the
//! previous `idle` marker, or — in a history that has no markers at all
//! (`legacy`) — the nearest preceding input, so every input starts a turn. That
//! file is outside the sparse `.ox/reference/opencode` checkout (only
//! `packages/tui` is there); reach it with
//! `git show HEAD:packages/util/src/session-turn.ts` in that repository.
//!
//! Markers are not always present in a marked history either: the projector
//! writes `idle` on a turn that succeeded, failed or was interrupted, but not
//! on a `Shutdown` interrupt (`crates/ox-core/src/projector/message_updater.rs`
//! `ExecutionInterrupted` only clears the retry), so a hard kill mid-turn
//! leaves the turn open in the persisted history. Every later prompt then reads
//! as a steer, and the turn stays dated from the prompt that opened it, until
//! the next `idle`. That is the upstream rule, not a parsing bug.
//!
//! An assistant row is `done` once its message has `time.completed`. A history
//! read mid-turn therefore leaves the streaming message open; whether a turn is
//! running at all is not in the messages. [`DataStore::replace_items`] closes
//! every open row except, while a turn runs, the newest assistant row of the
//! latest turn; an earlier open row (one a `Shutdown` interrupt left behind) is
//! closed even mid-turn.
//!
//! [`DataStore::replace_items`]: crate::store::DataStore::replace_items
//!
//! `Item::from_synthetic` is the one classification point for synthetics,
//! shared with the live reducer.

use std::sync::Arc;

use ox_schema::message::{
    AssistantContent, Compaction, Message, ToolState as WireToolState, ToolTime,
};
use ox_schema::support::{Content, FileAttachment};

use crate::store::{
    AssistantItem, ChildRef, CompactionItem, CompactionStatus, ErrorInfo, Item, MediaMeta,
    MediaSource, ModelChoice, Notice, RetryInfo, ShownError, Switch, ToolPart, ToolState,
    tool_media,
};

/// Turn a chronological message list into the store's ordered sequence.
pub fn parse_messages(messages: &[Message]) -> Vec<Item> {
    let mut items = Vec::new();
    let mut shown = ShownError::default();
    // A turn starts at its first input: upstream `session-turn.ts`
    // `inputIndex` (reach it as the module doc says) returns the first
    // `user`/`synthetic` after the previous `idle` marker. A history with no
    // markers at all predates them, and there the nearest preceding input
    // starts the turn instead, so every input does.
    let marked = messages
        .iter()
        .any(|message| matches!(message, Message::Idle { .. }));
    let mut turn_open = false;

    for message in messages {
        match message {
            Message::User {
                id, prompt, time, ..
            } => {
                // The row carries the prompt time the projector recorded, which
                // is exact. It opens a turn when none is open — a prompt
                // persisted mid-turn is a steer, and the wait for that turn is
                // what upstream's `inputIndex` skips. An unmarked history has no
                // marker to keep one open, so there it always opens one.
                let starts_turn = !marked || !turn_open;
                turn_open = true;
                let message: Arc<str> = Arc::from(id.as_str());
                items.push(Item::User {
                    text: Arc::from(prompt.text.as_str()),
                    files: prompt_media(prompt.files.as_deref(), &message),
                    created: Some(time.created),
                    starts_turn,
                    pending: None,
                });
            }
            Message::Assistant(assistant) => {
                let mut text = String::new();
                let mut reasoning = String::new();
                let mut tools = Vec::new();
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text { text: t, .. } => text.push_str(t),
                        AssistantContent::Reasoning { text: t, .. } => reasoning.push_str(t),
                        AssistantContent::Tool {
                            id,
                            name,
                            state,
                            time,
                            ..
                        } => {
                            tools.push(tool_part(id, name, state, time, &assistant.id));
                        }
                    }
                }
                let error = assistant.error.as_ref().map(ErrorInfo::from);
                shown.show(error.clone());
                let retry = assistant.retry.as_ref().map(|retry| RetryInfo {
                    attempt: retry.attempt,
                    at: retry.at,
                    message: Arc::from(retry.error.message.as_str()),
                });
                if text.is_empty()
                    && reasoning.is_empty()
                    && tools.is_empty()
                    && error.is_none()
                    && retry.is_none()
                {
                    continue;
                }
                items.push(Item::Assistant(Box::new(AssistantItem {
                    id: assistant.id.clone(),
                    text: Arc::from(text),
                    reasoning: Arc::from(reasoning),
                    reasoning_ms: reasoning_ms(&assistant.content),
                    // The projector stamps `time.completed` only when the
                    // message ends (step ended or failed, or a stale attempt
                    // closed), so a message still streaming loads open and
                    // keeps taking live deltas. Upstream reads the same stamp
                    // (`message-parts.tsx` `message.time.completed`).
                    done: assistant.time.completed.is_some(),
                    version: 0,
                    last_ordinal: None,
                    tools,
                    // Per-turn usage: the footer shows turn duration and tok/s.
                    // `streamed − created` is upstream's provider-active window;
                    // the whole turn would understate throughput on tool-heavy
                    // messages.
                    output_tokens: assistant
                        .tokens
                        .as_ref()
                        .map(|tokens| tokens.output + tokens.reasoning)
                        .unwrap_or(0),
                    active_ms: assistant
                        .time
                        .streamed
                        .or(assistant.time.completed)
                        .map(|end| (end - assistant.time.created).max(0))
                        .unwrap_or(0),
                    turn_started_ms: Some(assistant.time.created),
                    turn_done_ms: assistant.time.completed,
                    agent: Some(assistant.agent.clone()),
                    model: Some(format!(
                        "{}/{}",
                        assistant.model.provider_id, assistant.model.id
                    )),
                    finish: assistant.finish,
                    context_tokens: assistant
                        .tokens
                        .as_ref()
                        .map(crate::store::total_tokens)
                        .unwrap_or(0),
                    cost_usd: assistant
                        .cost
                        .as_ref()
                        .map(crate::store::money_usd)
                        .unwrap_or(0.0),
                    error,
                    retry,
                    ..Default::default()
                })));
            }
            Message::Compaction(message) => {
                match &message.compaction {
                    Compaction::Running { .. } => {}
                    Compaction::Failed { error, .. } => shown.show(Some(ErrorInfo::from(error))),
                    Compaction::Completed(_) => shown.show(None),
                }
                items.push(Item::Compaction(Box::new(compaction_item(message))));
            }
            Message::Idle { error, .. } => {
                // The turn is over: the next input starts a new one.
                turn_open = false;
                if let Some(error) = shown.end_turn(error.as_ref().map(ErrorInfo::from)) {
                    items.push(Item::Error(error));
                }
            }
            Message::AgentSwitched {
                agent, previous, ..
            } => items.push(Item::Switch(Switch::Agent {
                agent: Arc::from(agent.as_str()),
                previous: previous.as_deref().map(Arc::from),
            })),
            Message::ModelSwitched {
                model, previous, ..
            } => items.push(Item::Switch(Switch::Model {
                model: ModelChoice::from(model),
                previous: previous.as_ref().map(ModelChoice::from),
            })),
            Message::LocationSwitched { location, .. } => {
                items.push(Item::Switch(Switch::Location {
                    directory: Arc::from(location.location.directory.as_str()),
                }));
            }
            Message::System { description, .. } => items.push(Item::Notice(Notice::System {
                description: description.as_deref().map(Arc::from),
            })),
            Message::Skill { name, .. } => items.push(Item::Notice(Notice::Skill {
                name: Arc::from(name.as_str()),
            })),
            Message::Synthetic {
                id,
                metadata,
                description,
                time,
                ..
            } => {
                // A synthetic is a turn input even when it renders nothing (a
                // repeat or todo reminder, an output-limit or incomplete-stream
                // continuation), so the turn item goes in before the row.
                if !marked || !turn_open {
                    turn_open = true;
                    items.push(Item::Turn {
                        created: Some(time.created),
                    });
                }
                if let Some(item) =
                    Item::from_synthetic(id, metadata.as_ref(), description.as_deref())
                {
                    items.push(item);
                }
            }
            _ => {}
        }
    }
    items
}

/// Milliseconds the provider spent thinking, summed over the message's
/// `reasoning` parts. Each part records `time.created → time.completed`, the
/// same window the live `reasoning.started → reasoning.ended` path times, and
/// the live reducer adds those windows up the same way — so a history-loaded
/// and a live row show the same duration. A part without a completion stamp
/// contributes nothing; when no part is timed the result is `None`, and the row
/// renders no suffix.
fn reasoning_ms(content: &[AssistantContent]) -> Option<u64> {
    content
        .iter()
        .filter_map(|content| {
            let AssistantContent::Reasoning { time, .. } = content else {
                return None;
            };
            let time = time.as_ref()?;
            let completed = time.completed?;
            Some((completed - time.created).max(0) as u64)
        })
        .reduce(|total, part| total + part)
}

fn compaction_item(message: &ox_schema::message::CompactionMessage) -> CompactionItem {
    let (status, text, cancelled, tokens) = match &message.compaction {
        Compaction::Running { summary, .. } => {
            (CompactionStatus::Running, summary.clone(), false, None)
        }
        Compaction::Completed(done) => (
            CompactionStatus::Completed,
            done.summary.clone(),
            false,
            done.tokens,
        ),
        Compaction::Failed { error, tokens, .. } => (
            CompactionStatus::Failed,
            error.message.clone(),
            error.kind == "aborted",
            *tokens,
        ),
    };
    CompactionItem {
        id: message.id.clone(),
        status,
        text: Arc::new(text),
        cancelled,
        tokens,
        version: 0,
    }
}

/// The prompt's attachments, as metadata only (`Item::User.files`): every
/// entry, images and non-images alike, because the UI labels the latter
/// `file`/`dir` without fetching anything. `index` is the entry's own position
/// in `files` — that is what the media endpoint reads — and the base64
/// payload is never touched.
#[must_use]
fn prompt_media(files: Option<&[FileAttachment]>, message: &Arc<str>) -> Arc<[MediaMeta]> {
    let Some(files) = files else {
        return Arc::default();
    };
    let media: Vec<MediaMeta> = files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let index = u32::try_from(index).ok()?;
            Some(MediaMeta::new(
                file.name.as_deref(),
                &file.mime,
                MediaSource::Prompt {
                    message: Arc::clone(message),
                    index,
                },
            ))
        })
        .collect();
    media.into()
}

/// Rebuild a live `ToolPart` from the recorded wire state. `Streaming` only
/// carries the raw input text (the summary derives from it as a fallback);
/// the resolved states carry the parsed input record. `time` dates the call
/// (`ran`, else `created`) once it is past streaming.
fn tool_part(
    id: &str,
    name: &str,
    state: &WireToolState,
    time: &ToolTime,
    message: &str,
) -> ToolPart {
    let started_ms = match state {
        WireToolState::Streaming { .. } => None,
        _ => Some(time.ran.unwrap_or(time.created)),
    };
    let mut child = None;
    let mut media = None;
    let (state, record, output, input_text, metadata): (
        ToolState,
        Option<&_>,
        String,
        String,
        Option<&_>,
    ) = match state {
        WireToolState::Streaming { input } => {
            (ToolState::Input, None, String::new(), input.clone(), None)
        }
        WireToolState::Running { input, .. } => (
            ToolState::Running,
            Some(input),
            String::new(),
            String::new(),
            None,
        ),
        WireToolState::Completed {
            input,
            content,
            metadata,
        } => {
            child = ChildRef::from_metadata(name, metadata.as_ref());
            media = Some(tool_media(content, message, id));
            (
                ToolState::Success,
                Some(input),
                join_text(content),
                String::new(),
                metadata.as_ref(),
            )
        }
        WireToolState::Error {
            input,
            error,
            content,
            metadata,
        } => {
            media = content
                .as_deref()
                .map(|content| tool_media(content, message, id));
            (
                ToolState::Failed,
                Some(input),
                error.message.clone(),
                String::new(),
                metadata.as_ref(),
            )
        }
    };
    let mut part = ToolPart {
        id: id.to_owned(),
        tool: name.to_owned(),
        state,
        summary: record
            .map(|record| crate::store::tool_summary(name, record))
            .unwrap_or_default(),
        output: Arc::from(output),
        version: 0,
        input_text: Arc::from(input_text),
        todos: record
            .map(|record| crate::store::todos_of(name, record))
            .unwrap_or_default(),
        child,
        input: record
            .map(|record| Arc::new(record.clone()))
            .unwrap_or_default(),
        metadata: metadata.map(|metadata| Arc::new(metadata.clone())),
        media: media.unwrap_or_default(),
        started_ms,
    };
    if part.todos.is_empty() {
        part.todos = crate::store::todos_from_input_text(name, &part.input_text);
    }
    if part.summary.is_empty() {
        part.summary = crate::store::summary_from_input_text(&part.tool, &part.input_text);
    }
    part
}

fn join_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            Content::File { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
