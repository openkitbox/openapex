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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ox_client::store::{AssistantItem, DataStore, Item};

    use super::blocks_from_store;
    use crate::ui::timeline::Block;

    #[test]
    fn projects_items_one_to_one_in_order() {
        // DataStore has a private field, so struct-update syntax (FRU) cannot
        // be used here; assign the pub fields on a default instance instead.
        let mut store = DataStore::default();
        store.items = vec![
            Item::User {
                text: Arc::from("a"),
            },
            Item::Assistant(AssistantItem {
                id: "m1".into(),
                text: Arc::from("reply".to_string()),
                done: true,
                ..Default::default()
            }),
            Item::User {
                text: Arc::from("b"),
            },
        ];

        let blocks = blocks_from_store(&store);
        assert_eq!(blocks.len(), 3, "no pairing heuristics, no dropped items");
        assert!(matches!(blocks[0], Block::User { .. }));
        assert!(matches!(blocks[1], Block::Assistant { .. }));
        assert!(matches!(blocks[2], Block::User { .. }));
    }
}
