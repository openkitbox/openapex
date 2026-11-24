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
