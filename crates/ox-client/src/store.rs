//! Headless data layer: event in, state out. No terminal, no IO — this is the
//! part the TUI tests most heavily.
//!
//! Two invariants keep the UI cheap:
//! 1. `items` is ONE ordered sequence (no parallel user/assistant vectors), so
//!    the block list is a 1:1 projection with no positional pairing heuristics.
//! 2. Every mutation bumps `version` and lowers `changed_from`; the UI compares
//!    `(id, version)` in O(1) and only re-wraps the changed tail. Text lives in
//!    an `Arc` so an unchanged block shares the store's buffer (no deep copy).

use std::collections::HashMap;
use std::sync::Arc;

use ox_schema::event::{EventEnvelope, SessionEventData};
use ox_schema::support::TokenUsage;

#[derive(Debug, Clone)]
pub enum Item {
    User { text: Arc<str> },
    Assistant(AssistantItem),
}

#[derive(Debug, Clone, Default)]
pub struct AssistantItem {
    pub id: String,
    /// `Arc<String>` (not `Arc<str>`) so streaming deltas append via
    /// `Arc::make_mut(..).push_str(..)` — amortized O(1) when unique, one
    /// clone when the UI holds a snapshot. `Arc<str>` cannot grow in place.
    pub text: Arc<String>,
    pub reasoning: Arc<String>,
    pub done: bool,
    /// Bumped on every content mutation; the UI's cache key.
    pub version: u64,
    /// Highest `ordinal` applied so far. `None` also skips duplicate ordinal 0.
    pub last_ordinal: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input: i64,
    pub cache_read: i64,
}

#[derive(Debug, Default)]
pub struct DataStore {
    pub session: Option<String>,
    pub items: Vec<Item>,
    assistant_index: HashMap<String, usize>,
    pub running_since: Option<i64>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub ctx: Option<Usage>,
    /// Last durable seq **of the focused session only**.
    pub last_seq: Option<i64>,
    /// First item index whose content changed since the last UI sync.
    pub changed_from: usize,
}

impl DataStore {
    fn ensure_assistant(&mut self, message_id: &str) -> usize {
        if let Some(index) = self.assistant_index.get(message_id) {
            return *index;
        }
        let index = self.items.len();
        self.items.push(Item::Assistant(AssistantItem {
            id: message_id.to_owned(),
            ..Default::default()
        }));
        self.assistant_index.insert(message_id.to_owned(), index);
        self.changed_from = self.changed_from.min(index);
        index
    }

    fn touch(&mut self, index: usize) {
        self.changed_from = self.changed_from.min(index);
    }

    /// Authoritative replacement after a reconnect/reload.
    pub fn replace_items(&mut self, items: Vec<Item>) {
        self.items = items;
        self.assistant_index.clear();
        for (index, item) in self.items.iter().enumerate() {
            if let Item::Assistant(assistant) = item {
                self.assistant_index.insert(assistant.id.clone(), index);
            }
        }
        self.changed_from = 0;
    }

    /// Optimistic local echo of a submitted prompt.
    pub fn push_user(&mut self, text: &str) {
        self.items.push(Item::User {
            text: Arc::from(text),
        });
        let index = self.items.len() - 1;
        self.touch(index);
    }

    /// Take the dirty watermark so the next mutation starts a new one.
    pub fn claim_changed_from(&mut self) -> usize {
        std::mem::replace(&mut self.changed_from, usize::MAX)
    }
}

pub fn reduce(store: &mut DataStore, event: &EventEnvelope<SessionEventData>) {
    if let Some(durable) = &event.durable
        && (store.session.is_none()
            || Some(durable.aggregate_id.as_str()) == store.session.as_deref())
    {
        store.last_seq = Some(durable.seq);
    }

    match &event.data {
        SessionEventData::TextStarted {
            assistant_message_id,
            ..
        } => {
            store.ensure_assistant(assistant_message_id);
        }
        SessionEventData::TextDelta {
            assistant_message_id,
            ordinal,
            delta,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            let Item::Assistant(item) = &mut store.items[index] else {
                return;
            };
            if item.done {
                return; // history is authoritative; replayed deltas must not append
            }
            if item.last_ordinal.is_some_and(|last| *ordinal <= last) {
                return;
            }
            Arc::make_mut(&mut item.text).push_str(delta);
            item.last_ordinal = Some(*ordinal);
            item.version += 1;
            store.touch(index);
        }
        SessionEventData::TextEnded {
            assistant_message_id,
            text,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.text = Arc::new(text.clone());
                item.done = true;
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ReasoningDelta {
            assistant_message_id,
            delta,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                if item.done {
                    return; // history is authoritative; replayed deltas must not append
                }
                Arc::make_mut(&mut item.reasoning).push_str(delta);
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ReasoningEnded {
            assistant_message_id,
            text,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.reasoning = Arc::new(text.clone());
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ExecutionStarted { .. } => {
            store.running_since = Some(event.created);
        }
        SessionEventData::ExecutionSucceeded { .. }
        | SessionEventData::ExecutionFailed { .. }
        | SessionEventData::ExecutionInterrupted { .. } => {
            store.running_since = None;
            if let Some(index) = store
                .items
                .iter()
                .rposition(|item| matches!(item, Item::Assistant(_)))
            {
                if let Item::Assistant(item) = &mut store.items[index] {
                    item.done = true;
                    item.version += 1;
                }
                store.touch(index);
            }
        }
        SessionEventData::SessionModelSelected { model, .. } => {
            store.model = Some(format!("{}/{}", model.provider_id, model.id));
        }
        SessionEventData::SessionAgentSelected { agent, .. } => {
            store.agent = Some(agent.clone());
        }
        SessionEventData::SessionUsageRecorded { tokens, .. }
        | SessionEventData::SessionUsageUpdated { tokens, .. } => {
            store.ctx = Some(usage_of(tokens));
        }
        _ => {}
    }
}

fn usage_of(tokens: &TokenUsage) -> Usage {
    Usage {
        input: tokens.input,
        cache_read: tokens.cache.read,
    }
}
