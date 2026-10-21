//! History parsing: a chronological `Vec<Message>` becomes the store's single
//! ordered `Vec<Item>`. Pure function, no IO. Tool/compaction/synthetic
//! messages are out of scope for the MVP and are skipped.

use std::sync::Arc;

use ox_schema::message::{AssistantContent, Message};

use crate::store::{AssistantItem, Item};

/// Turn a chronological message list into the store's ordered sequence.
pub fn parse_messages(messages: &[Message]) -> Vec<Item> {
    let mut items = Vec::new();

    for message in messages {
        match message {
            Message::User { prompt, .. } => items.push(Item::User {
                text: Arc::from(prompt.text.as_str()),
            }),
            Message::Assistant(assistant) => {
                let mut text = String::new();
                let mut reasoning = String::new();
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text { text: t, .. } => text.push_str(t),
                        AssistantContent::Reasoning { text: t, .. } => reasoning.push_str(t),
                        AssistantContent::Tool { .. } => {}
                    }
                }
                if text.is_empty() && reasoning.is_empty() {
                    continue;
                }
                items.push(Item::Assistant(AssistantItem {
                    id: assistant.id.clone(),
                    text: Arc::from(text),
                    reasoning: Arc::from(reasoning),
                    done: true,
                    version: 0,
                    last_ordinal: None,
                }));
            }
            _ => {}
        }
    }
    items
}
