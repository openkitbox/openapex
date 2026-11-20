//! Ephemeral metadata reported by running tool calls.

use ox_schema::message::Message;
use ox_schema::support::Record;

/// Process-local metadata for tool calls that have not settled yet.
#[derive(Clone, Default)]
pub struct LiveToolMetadata {
    records: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, std::collections::HashMap<String, Record>>,
        >,
    >,
}

impl LiveToolMetadata {
    /// Create an empty live metadata store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the latest metadata for one running tool call.
    pub fn record(&self, assistant_message_id: &str, call_id: &str, metadata: Record) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        records
            .entry(assistant_message_id.to_owned())
            .or_default()
            .insert(call_id.to_owned(), metadata);
    }

    /// Forget metadata after a tool call settles.
    pub fn clear(&self, assistant_message_id: &str, call_id: &str) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(calls) = records.get_mut(assistant_message_id) else {
            return;
        };
        calls.remove(call_id);
        if calls.is_empty() {
            records.remove(assistant_message_id);
        }
    }

    /// Overlay live metadata onto running tool parts in assistant messages.
    pub fn overlay(&self, messages: &mut [Message]) {
        let records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for message in messages {
            let Message::Assistant(assistant) = message else {
                continue;
            };
            for content in &mut assistant.content {
                let ox_schema::message::AssistantContent::Tool { id, state, .. } = content else {
                    continue;
                };
                let ox_schema::message::ToolState::Running { metadata, .. } = state.as_mut() else {
                    continue;
                };
                if let Some(latest) = records
                    .get(assistant.id.as_str())
                    .and_then(|calls| calls.get(id))
                {
                    *metadata = latest.clone();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_schema::message::{
        AssistantContent, AssistantMessage, AssistantTime, Message, ToolState, ToolTime,
    };
    use ox_schema::support::{ModelRef, Record};

    fn message(state: ToolState) -> Message {
        Message::Assistant(Box::new(AssistantMessage {
            id: "msg_1".to_owned(),
            metadata: None,
            time: AssistantTime {
                created: 0,
                streamed: None,
                completed: None,
            },
            agent: "apex".to_owned(),
            model: ModelRef {
                id: "model".to_owned(),
                provider_id: "provider".to_owned(),
                variant: None,
            },
            content: vec![AssistantContent::Tool {
                id: "call_1".to_owned(),
                name: "subagent".to_owned(),
                executed: None,
                provider_state: None,
                provider_result_state: None,
                state: Box::new(state),
                time: ToolTime {
                    created: 0,
                    ran: None,
                    completed: None,
                },
            }],
            snapshot: None,
            finish: None,
            raw_finish: None,
            provider_state: None,
            cost: None,
            tokens: None,
            error: None,
            retry: None,
        }))
    }

    #[test]
    fn latest_metadata_overlays_only_running_tool_parts() {
        let store = LiveToolMetadata::new();
        let mut metadata = Record::new();
        metadata.insert("status".to_owned(), serde_json::json!("old"));
        store.record("msg_1", "call_1", metadata);
        let mut messages = vec![message(ToolState::Running {
            input: Record::new(),
            metadata: Record::new(),
        })];

        store.record(
            "msg_1",
            "call_1",
            [("status".to_owned(), serde_json::json!("latest"))]
                .into_iter()
                .collect(),
        );
        store.overlay(&mut messages);

        let Message::Assistant(assistant) = &messages[0] else {
            panic!("expected assistant message");
        };
        let AssistantContent::Tool { state, .. } = &assistant.content[0] else {
            panic!("expected tool content");
        };
        let ToolState::Running { metadata, .. } = state.as_ref() else {
            panic!("expected running tool");
        };
        assert_eq!(metadata.get("status"), Some(&serde_json::json!("latest")));
    }

    #[test]
    fn settled_tool_is_not_overlaid_and_clear_removes_record() {
        let store = LiveToolMetadata::new();
        store.record(
            "msg_1",
            "call_1",
            [("status".to_owned(), serde_json::json!("latest"))]
                .into_iter()
                .collect(),
        );
        let mut messages = vec![message(ToolState::Completed {
            input: Record::new(),
            content: vec![],
            metadata: None,
        })];
        store.overlay(&mut messages);
        store.clear("msg_1", "call_1");

        let Message::Assistant(assistant) = &messages[0] else {
            panic!("expected assistant message");
        };
        let AssistantContent::Tool { state, .. } = &assistant.content[0] else {
            panic!("expected tool content");
        };
        assert!(matches!(
            state.as_ref(),
            ToolState::Completed { metadata: None, .. }
        ));
    }
}
