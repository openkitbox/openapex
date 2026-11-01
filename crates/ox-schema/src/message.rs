//! Session message models, ported from `packages/schema/src/session-message.ts`.
//!
//! Wire notes (verified against upstream):
//!
//! - `SessionMessage.Info` is a tagged union on `type` with these tags:
//!   agent-switched, model-switched, location-switched, user, synthetic,
//!   system, skill, shell, assistant, compaction, idle.
//! - Assistant `content` is a tagged union on `type`: text | reasoning | tool.
//! - Tool `state` is a tagged union on `status`: streaming | running |
//!   completed | error.
//! - Compaction messages are *discriminated by `status`* (running | completed
//!   | failed) in addition to the `type` tag. Serde internally-tagged enums
//!   cannot use two discriminant fields, so the `compaction` variant is a
//!   wrapper over [`Compaction`], whose own `#[serde(tag = "status")]`
//!   flattens into the message object on both encode and decode.
//!
//! Storage split (from `packages/core/src/session/sql.ts` +
//! `history.ts::decodeMessageRow`): the `session_message` table holds the full
//! message JSON in `data` MINUS the `id` and `type` keys, which live in their
//! own columns; decode merges them back: `{ ...data, id, type }`. This module
//! therefore offers [`Message::to_row_data`] / [`Message::from_row_data`].

use serde::{Deserialize, Serialize};

use crate::support::{
    Content, FinishReason, LocationRef, ModelRef, MoneyUsd, Prompt, ProviderContext, Record,
    SessionError, ShellOutput, ShellStatus, SnapshotId, TokenUsage,
};

/// `Session.Message.ProviderState` — opaque provider round-trip record.
pub type ProviderState = Record;

// ---------------------------------------------------------------------------
// Shared time blocks
// ---------------------------------------------------------------------------

/// Base `time` struct (`{ created }`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimeCreated {
    pub created: i64,
}

/// Shell message `time` block (`{ created, completed? }`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellTime {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

/// Assistant tool call `time` block (`{ created, ran?, completed? }`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolTime {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ran: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

/// Assistant message `time` block (`{ created, streamed?, completed? }`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantTime {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streamed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

// ---------------------------------------------------------------------------
// Tool state (tagged union on `status`)
// ---------------------------------------------------------------------------

/// Upstream `Session.Message.ToolState` union.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status")]
pub enum ToolState {
    #[serde(rename = "streaming")]
    Streaming { input: String },
    #[serde(rename = "running")]
    Running { input: Record, metadata: Record },
    #[serde(rename = "completed")]
    Completed {
        input: Record,
        /// Upstream `NonEmptyArray(Content)`; the non-emptiness check is a
        /// validation concern, not wire format.
        content: Vec<Content>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
    },
    #[serde(rename = "error")]
    Error {
        input: Record,
        error: SessionError,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<Content>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
    },
}

// ---------------------------------------------------------------------------
// Assistant content (tagged union on `type`)
// ---------------------------------------------------------------------------

/// Upstream `Session.Message.AssistantContent` union.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AssistantContent {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<ProviderState>,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<ProviderState>,
        #[serde(skip_serializing_if = "Option::is_none")]
        time: Option<ShellTime>,
    },
    #[serde(rename = "tool")]
    Tool {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        executed: Option<bool>,
        #[serde(rename = "providerState", skip_serializing_if = "Option::is_none")]
        provider_state: Option<ProviderState>,
        #[serde(
            rename = "providerResultState",
            skip_serializing_if = "Option::is_none"
        )]
        provider_result_state: Option<ProviderState>,
        state: Box<ToolState>,
        time: ToolTime,
    },
}

// ---------------------------------------------------------------------------
// Assistant snapshot block
// ---------------------------------------------------------------------------

/// Upstream `Session.Message.Assistant` `snapshot` struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<SnapshotId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<SnapshotId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

/// Upstream `Session.Message.Assistant.Retry`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantRetry {
    pub attempt: i64,
    pub at: i64,
    pub error: SessionError,
}

// ---------------------------------------------------------------------------
// Compaction (tagged union on `status`)
// ---------------------------------------------------------------------------

/// Upstream compaction `reason` literal: auto | manual. `idle` is ox's own
/// addition (plan D5): the silent background fold's checkpoint rows carry it
/// on the wire as `"idle"`; it never rides the live SSE stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Auto,
    Manual,
    Idle,
}

/// Upstream `Session.Message.Compaction` union (discriminated by `status`;
/// the message-level `type: "compaction"` tag lives on the enclosing
/// [`Message`] variant and flattens into this struct on the wire).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status")]
pub enum Compaction {
    #[serde(rename = "running")]
    Running {
        reason: CompactionReason,
        summary: String,
        recent: String,
    },
    #[serde(rename = "completed")]
    Completed(Box<CompactionCompleted>),
    #[serde(rename = "failed")]
    Failed {
        reason: CompactionReason,
        error: SessionError,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<MoneyUsd>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
    },
}

/// Payload of the `completed` compaction variant, boxed to keep the
/// [`Compaction`] enum size uniform across variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionCompleted {
    pub reason: CompactionReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(rename = "providerState", skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<ProviderState>,
    pub summary: String,
    pub recent: String,
    #[serde(rename = "providerContext", skip_serializing_if = "Option::is_none")]
    pub provider_context: Option<ProviderContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<MoneyUsd>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
}

/// Wrapper for the `compaction` message variant. Field order on encode
/// follows the upstream struct order (`type` tag, then base fields, then the
/// flattened compaction fields including `status`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionMessage {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Record>,
    pub time: TimeCreated,
    #[serde(flatten)]
    pub compaction: Compaction,
}

impl CompactionMessage {
    /// Encode for the `session_message.data` column without re-wrapping the
    /// value in [`Message`]; byte-identical to
    /// `Message::Compaction(msg).to_row_data()`.
    #[must_use]
    pub fn row_data(&self) -> serde_json::Value {
        row_data_of(self)
    }
}

// ---------------------------------------------------------------------------
// Location-switch blocks
// ---------------------------------------------------------------------------

/// The `location` / `previous` structs of a `location-switched` message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocationSwitch {
    pub location: LocationRef,
    #[serde(rename = "projectID", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

// ---------------------------------------------------------------------------
// Message union (tagged on `type`)
// ---------------------------------------------------------------------------

/// Upstream `SessionMessage.Info` — the full tagged union.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "agent-switched")]
    AgentSwitched {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        time: TimeCreated,
        agent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<String>,
    },
    #[serde(rename = "model-switched")]
    ModelSwitched {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        time: TimeCreated,
        model: ModelRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<ModelRef>,
    },
    #[serde(rename = "location-switched")]
    LocationSwitched {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        time: TimeCreated,
        #[serde(flatten)]
        location: LocationSwitch,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<LocationSwitch>,
    },
    #[serde(rename = "user")]
    User {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        time: TimeCreated,
        #[serde(flatten)]
        prompt: Prompt,
    },
    #[serde(rename = "synthetic")]
    Synthetic {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        time: TimeCreated,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    #[serde(rename = "system")]
    System {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        time: TimeCreated,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    #[serde(rename = "skill")]
    Skill {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        time: TimeCreated,
        skill: String,
        name: String,
        text: String,
    },
    #[serde(rename = "shell")]
    Shell {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        #[serde(rename = "shellID")]
        shell_id: String,
        command: String,
        status: ShellStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<ShellOutput>,
        /// Overrides the base `time` block: adds an optional `completed`.
        time: ShellTime,
    },
    #[serde(rename = "assistant")]
    Assistant(Box<AssistantMessage>),
    #[serde(rename = "compaction")]
    Compaction(CompactionMessage),
    #[serde(rename = "idle")]
    Idle {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        time: TimeCreated,
        outcome: IdleOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<SessionError>,
    },
}

/// Payload of the `assistant` message variant, boxed to keep the [`Message`]
/// enum size uniform (assistant messages carry the heaviest field set).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessage {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Record>,
    pub time: AssistantTime,
    pub agent: String,
    pub model: ModelRef,
    pub content: Vec<AssistantContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<AssistantSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<FinishReason>,
    #[serde(rename = "rawFinish", skip_serializing_if = "Option::is_none")]
    pub raw_finish: Option<String>,
    #[serde(rename = "providerState", skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<ProviderState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<MoneyUsd>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<SessionError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<AssistantRetry>,
}

impl AssistantMessage {
    /// Encode for the `session_message.data` column without re-wrapping the
    /// value in [`Message`] (which would require a full clone); byte-identical
    /// to `Message::Assistant(Box::new(msg.clone())).to_row_data()`.
    #[must_use]
    pub fn row_data(&self) -> serde_json::Value {
        row_data_of(self)
    }
}

/// Serialize a concrete message variant into the `session_message.data`
/// shape: the variant's own JSON minus `id`. The `type` tag lives on the
/// [`Message`] union (`#[serde(tag = "type")]`), so a concrete variant
/// never carries it.
fn row_data_of<T: Serialize>(message: &T) -> serde_json::Value {
    let mut value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("id");
    }
    value
}

/// Upstream `Session.Message.Idle` `outcome` literal union.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IdleOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

impl Message {
    /// The wire tag for this message (matches the upstream `type` literal).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Message::AgentSwitched { .. } => "agent-switched",
            Message::ModelSwitched { .. } => "model-switched",
            Message::LocationSwitched { .. } => "location-switched",
            Message::User { .. } => "user",
            Message::Synthetic { .. } => "synthetic",
            Message::System { .. } => "system",
            Message::Skill { .. } => "skill",
            Message::Shell { .. } => "shell",
            Message::Assistant { .. } => "assistant",
            Message::Compaction(_) => "compaction",
            Message::Idle { .. } => "idle",
        }
    }

    /// The message id.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Message::AgentSwitched { id, .. }
            | Message::ModelSwitched { id, .. }
            | Message::LocationSwitched { id, .. }
            | Message::User { id, .. }
            | Message::Synthetic { id, .. }
            | Message::System { id, .. }
            | Message::Skill { id, .. }
            | Message::Shell { id, .. }
            | Message::Idle { id, .. } => id,
            Message::Assistant(msg) => &msg.id,
            Message::Compaction(msg) => &msg.id,
        }
    }

    /// The `time.created` value (also the `session_message.time_created`
    /// column).
    #[must_use]
    pub fn time_created(&self) -> i64 {
        match self {
            Message::AgentSwitched { time, .. }
            | Message::ModelSwitched { time, .. }
            | Message::LocationSwitched { time, .. }
            | Message::User { time, .. }
            | Message::Synthetic { time, .. }
            | Message::System { time, .. }
            | Message::Skill { time, .. }
            | Message::Idle { time, .. } => time.created,
            Message::Shell { time, .. } => time.created,
            Message::Assistant(msg) => msg.time.created,
            Message::Compaction(msg) => msg.time.created,
        }
    }

    /// Encode for the `session_message.data` column: the full message JSON
    /// minus `id` and `type` (upstream
    /// `DistributiveOmit<Info.Encoded, "type" | "id">`).
    #[must_use]
    pub fn to_row_data(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("id");
            map.remove("type");
        }
        value
    }

    /// Merge the two `session_message` columns back into the `data` payload:
    /// `decode({ ...row.data, id: row.id, type: row.type })`. The result is the
    /// row shape the tagger addresses (it keys on `id` and switches on `type`),
    /// so a caller that overlays before decoding merges here first.
    pub fn merge_row_columns(id: &str, kind: &str, data: &mut serde_json::Value) {
        if let serde_json::Value::Object(map) = data {
            map.insert("id".to_owned(), serde_json::Value::String(id.to_owned()));
            map.insert(
                "type".to_owned(),
                serde_json::Value::String(kind.to_owned()),
            );
        }
    }

    /// Decode a payload that already carries `id`/`type` (the output of
    /// [`Self::merge_row_columns`]).
    pub fn from_merged_row(data: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(data)
    }

    /// Decode a `session_message` row, merging the column values back into
    /// the `data` JSON exactly like upstream
    /// (`decode({ ...row.data, id: row.id, type: row.type })`).
    pub fn from_row_data(
        id: &str,
        kind: &str,
        data: serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        let mut merged = data;
        Self::merge_row_columns(id, kind, &mut merged);
        Self::from_merged_row(merged)
    }
}
