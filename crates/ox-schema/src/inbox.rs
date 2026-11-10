//! Session inbox models, ported from `packages/schema/src/session-inbox.ts`.
//!
//! Wire notes:
//!
//! - `SessionInbox.Item` is a tagged union on `type`: user | synthetic |
//!   compaction | move.
//! - `SessionInbox.Info` is an enqueued item: the item fields plus
//!   `{ id, sessionID, time: { created } }`. Modeled as a tagged enum so the
//!   whole shape (including the tag) round-trips in one value.
//! - `Delivery` is the literal steer | queue. On the `session_inbox` table it
//!   lives in its own nullable column (`session_pending.delivery` is nullable;
//!   pending items may have no delivery policy yet).

use serde::{Deserialize, Serialize};

use crate::support::{LocationRef, Prompt, Record};

/// The `metadata` key of a user prompt that carries a one-turn logical model
/// override (`/slim`); the value is a logical model id string.
///
/// The override belongs to that one user request — it rides in the inbox item's
/// metadata so a queued or replayed prompt keeps its model — and never becomes
/// a session-wide model switch.
pub const TURN_MODEL_KEY: &str = "model";

/// Upstream `SessionInbox.Delivery` literal union.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Steer,
    Queue,
}

/// Upstream `Session.Inbox.UserPayload` (`Prompt.fields` + optional metadata).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserPayload {
    #[serde(flatten)]
    pub prompt: Prompt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Record>,
}

/// Upstream `Session.Inbox.SyntheticPayload`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyntheticPayload {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Record>,
}

/// Upstream `Session.Inbox.CompactionPayload` (empty struct on the wire).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionPayload {}

/// Upstream `Session.Inbox.MovePayload` (also the `session.moved` event data).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MovePayload {
    pub location: LocationRef,
    #[serde(rename = "projectID")]
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

/// Upstream `SessionInbox.Item` (tagged union on `type`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Item {
    #[serde(rename = "user")]
    User {
        payload: UserPayload,
        delivery: Delivery,
    },
    #[serde(rename = "synthetic")]
    Synthetic {
        payload: SyntheticPayload,
        delivery: Delivery,
    },
    #[serde(rename = "compaction")]
    Compaction {
        payload: CompactionPayload,
        delivery: Delivery,
    },
    #[serde(rename = "move")]
    Move {
        payload: MovePayload,
        delivery: Delivery,
    },
}

impl Item {
    /// The wire tag for this item.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Item::User { .. } => "user",
            Item::Synthetic { .. } => "synthetic",
            Item::Compaction { .. } => "compaction",
            Item::Move { .. } => "move",
        }
    }

    /// The item's delivery policy.
    #[must_use]
    pub fn delivery(&self) -> Delivery {
        match self {
            Item::User { delivery, .. }
            | Item::Synthetic { delivery, .. }
            | Item::Compaction { delivery, .. }
            | Item::Move { delivery, .. } => *delivery,
        }
    }

    /// The item's payload as a standalone JSON value (the `payload` column of
    /// the `session_inbox` / `data` column of the `session_pending` tables).
    /// Constructed via an untagged-shaped map so a `CompactionPayload` round
    /// trip stays `{}` on the wire.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        use serde_json::Value;
        use serde_json::json;
        match self {
            Item::User { payload, .. } => {
                let mut v = serde_json::to_value(&payload.prompt).unwrap_or(Value::Null);
                if let (Value::Object(map), Some(metadata)) = (&mut v, &payload.metadata) {
                    map.insert("metadata".to_string(), json!(metadata));
                }
                v
            }
            Item::Synthetic { payload, .. } => serde_json::to_value(payload).unwrap_or(Value::Null),
            Item::Compaction { .. } => json!({}),
            Item::Move { payload, .. } => serde_json::to_value(payload).unwrap_or(Value::Null),
        }
    }
}

/// Enqueued marker block: `{ id, sessionID, time: { created } }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnqueuedTime {
    pub created: i64,
}

/// Upstream `SessionInbox.Info`: an [`Item`] plus the enqueued fields.
/// Tagged enum so the whole shape round-trips in one value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Info {
    #[serde(rename = "user")]
    User {
        id: String,
        #[serde(rename = "sessionID")]
        session_id: String,
        time: EnqueuedTime,
        payload: UserPayload,
        delivery: Delivery,
    },
    #[serde(rename = "synthetic")]
    Synthetic {
        id: String,
        #[serde(rename = "sessionID")]
        session_id: String,
        time: EnqueuedTime,
        payload: SyntheticPayload,
        delivery: Delivery,
    },
    #[serde(rename = "compaction")]
    Compaction {
        id: String,
        #[serde(rename = "sessionID")]
        session_id: String,
        time: EnqueuedTime,
        payload: CompactionPayload,
        delivery: Delivery,
    },
    #[serde(rename = "move")]
    Move {
        id: String,
        #[serde(rename = "sessionID")]
        session_id: String,
        time: EnqueuedTime,
        payload: MovePayload,
        delivery: Delivery,
    },
}
