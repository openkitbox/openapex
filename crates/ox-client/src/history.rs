//! History parsing: a chronological `Vec<Message>` becomes the store's single
//! ordered `Vec<Item>`. Pure function, no IO. Only shell / subagent
//! completion notices are kept from synthetic messages; every other synthetic
//! message stays skipped. Compaction messages become boundary items.
//! `*-switched`, `system` and `skill` messages become row items, and an
//! `idle` error becomes one only when it differs from the error the turn's
//! last step or settled compaction already shows (upstream `rows.ts`
//! `reduceSessionRows` `shownError`).

use std::sync::Arc;

use ox_schema::message::{AssistantContent, Compaction, Message, ToolState as WireToolState};
use ox_schema::support::Content;

use crate::store::{
    AssistantItem, ChildRef, CompactionItem, CompactionStatus, CompletionItem, ErrorInfo, Item,
    ModelChoice, Notice, RetryInfo, ShownError, Switch, ToolPart, ToolState,
};

/// Turn a chronological message list into the store's ordered sequence.
pub fn parse_messages(messages: &[Message]) -> Vec<Item> {
    let mut items = Vec::new();
    let mut shown = ShownError::default();

    for message in messages {
        match message {
            Message::User { prompt, time, .. } => items.push(Item::User {
                text: Arc::from(prompt.text.as_str()),
                created: Some(time.created),
            }),
            Message::Assistant(assistant) => {
                let mut text = String::new();
                let mut reasoning = String::new();
                let mut tools = Vec::new();
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text { text: t, .. } => text.push_str(t),
                        AssistantContent::Reasoning { text: t, .. } => reasoning.push_str(t),
                        AssistantContent::Tool {
                            id, name, state, ..
                        } => {
                            tools.push(tool_part(id, name, state));
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
                    done: true,
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
                ..
            } => {
                if let Some(item) =
                    CompletionItem::from_synthetic(id, metadata.as_ref(), description.as_deref())
                {
                    items.push(Item::Completion(item));
                }
            }
            _ => {}
        }
    }
    items
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

/// Rebuild a live `ToolPart` from the recorded wire state. `Streaming` only
/// carries the raw input text (the summary derives from it as a fallback);
/// the resolved states carry the parsed input record.
fn tool_part(id: &str, name: &str, state: &WireToolState) -> ToolPart {
    let mut child = None;
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
            metadata,
            ..
        } => (
            ToolState::Failed,
            Some(input),
            error.message.clone(),
            String::new(),
            metadata.as_ref(),
        ),
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
