use std::sync::Arc;

use ox_client::store::{DataStore, Item};

use crate::ui::timeline::Block;

/// 1:1 projection of the store's ordered items. No pairing and no reordering:
/// ordering is owned by `DataStore::items`, and text is shared via `Arc` so an
/// unchanged block costs a pointer clone rather than a string copy.
pub fn blocks_from_store(store: &DataStore) -> Vec<Block> {
    store
        .items
        .iter()
        .map(|item| match item {
            Item::User { text } => Block::User {
                text: Arc::clone(text),
            },
            Item::Assistant(assistant) => Block::Assistant {
                id: assistant.id.clone(),
                version: assistant.version,
                text: Arc::clone(&assistant.text),
                reasoning: Arc::clone(&assistant.reasoning),
                done: assistant.done,
            },
        })
        .collect()
}
