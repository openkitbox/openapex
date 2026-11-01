//! Session event models, ported from `packages/schema/src/session-event.ts`
//! plus the envelope from `packages/schema/src/event.ts`.
//!
//! Wire notes:
//!
//! - Every event on the wire is an envelope:
//!   `{ id, created, type, data, durable?, location?, metadata? }`.
//!   `durable` events carry `durable: { aggregateID, seq, version }`;
//!   ephemeral events never carry it. The `type` tag lives on the envelope,
//!   NOT inside `data`.
//! - [`EventEnvelope`] is the generic bus form (data as `Value`).
//! - [`SessionEventData`] is the typed union over the event `data` payloads,
//!   keyed by the envelope `type` string via serde's internally-tagged-
//!   adjacent representation (`tag = "type"`, `content = "data"`). It is
//!   never put on the wire directly; use
//!   [`SessionEventData::from_parts`] / [`SessionEventData::into_parts`].
//! - Unknown event types deserialize losslessly as
//!   [`SessionEventData::Unknown`], keeping the raw data verbatim.
//!
//! Scope: the event groups the chat path actually emits (Session lifecycle,
//! Execution, Inbox, Step, Text, Reasoning, Tool, RetryScheduled, Synthetic,
//! InstructionsUpdated, MessageContentUpdated). Skill/Shell event groups and
//! the revert group are NOT ported and fall back to `Unknown`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::inbox::{Delivery, Item, MovePayload};
use crate::message::{AssistantContent, ProviderState};
use crate::support::{
    Content, FinishReason, ForkBoundary, LocationRef, ModelRef, MoneyUsd, ProviderContext, Record,
    Rule, SessionError, SessionMetadata, SnapshotId, TokenUsage,
};

// ---------------------------------------------------------------------------
// Instructions sources (batch I / L6: machine-readable source classification)
// ---------------------------------------------------------------------------

/// How one instruction source reaches the context. `global` is the home
/// instruction file, `project` is a session-directory ancestor-chain file
/// (R3 semantics), `config` is a non-file source (skills catalog, MCP block,
/// date, environment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionSourceKind {
    Global,
    Project,
    Config,
}

/// One source behind a `session.instructions.updated` delta, for the
/// projected system row's `instructionSources` metadata: replay/audit tooling
/// classifies sources without string-matching paths. Additive on the wire —
/// consumers that don't know `kind` ignore it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionSource {
    /// The keyed source this entry belongs to (`core/instructions`, ...).
    pub key: String,
    /// Absolute file path; absent for non-file sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<InstructionSourceKind>,
}

// ---------------------------------------------------------------------------
// Envelope (packages/schema/src/event.ts)
// ---------------------------------------------------------------------------

/// Upstream `DurableEnvelope`: position in one aggregate's durable log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DurableEnvelope {
    #[serde(rename = "aggregateID")]
    pub aggregate_id: String,
    pub seq: i64,
    pub version: i64,
}

/// Generic event envelope as it appears on the wire and on the bus.
///
/// `D` defaults to `serde_json::Value`; a typed parse of the same wire bytes
/// is `EventEnvelope<SessionEventData>` obtained via [`SessionEventData`]
/// helpers (not a direct `Deserialize` — the tag is a sibling of `data`, not
/// inside it).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope<D = Value> {
    pub id: String,
    pub created: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: D,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable: Option<DurableEnvelope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Record>,
}

impl EventEnvelope<Value> {
    /// Parse the envelope's `data` into the typed event union based on the
    /// `type` string. Unknown types come back as [`SessionEventData::Unknown`]
    /// with the raw data preserved.
    #[must_use]
    pub fn parse_typed(&self) -> EventEnvelope<SessionEventData> {
        EventEnvelope {
            id: self.id.clone(),
            created: self.created,
            kind: self.kind.clone(),
            data: SessionEventData::from_parts(&self.kind, self.data.clone()),
            durable: self.durable.clone(),
            location: self.location.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

impl EventEnvelope<SessionEventData> {
    /// Lower a typed envelope back to the generic wire form.
    #[must_use]
    pub fn into_generic(self) -> EventEnvelope<Value> {
        EventEnvelope {
            id: self.id,
            created: self.created,
            kind: self.kind,
            data: self.data.into_data(),
            durable: self.durable,
            location: self.location,
            metadata: self.metadata,
        }
    }
}

/// Upstream `session.usage.recorded` `source` literal: title | compaction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UsageSource {
    Title,
    Compaction,
}

/// Upstream `Execution.Interrupted` `reason` literal union.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InterruptReason {
    User,
    Shutdown,
    Superseded,
    Inactivity,
}

/// Upstream compaction reason literal: auto | manual (shared with the
/// message schema).
pub use crate::message::CompactionReason;

/// Upstream `Step.Failed` `finish` literal (currently only content-filter).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StepFailedFinish {
    #[serde(rename = "content-filter")]
    ContentFilter,
}

// ---------------------------------------------------------------------------
// Typed event data union
// ---------------------------------------------------------------------------

/// Typed `data` payloads for the ported session events. The serde
/// representation is `{"type": <envelope type>, "data": <payload>}` — an
/// adapter shape used by [`SessionEventData::from_parts`] /
/// [`SessionEventData::into_parts`]; it never appears on the wire as-is.
///
/// Unknown event types (including the un-ported groups: skill/shell events
/// and `session.revert.*`) deserialize losslessly as
/// [`SessionEventData::Unknown`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum SessionEventData {
    // -- Session lifecycle ---------------------------------------------------
    #[serde(rename = "session.created")]
    SessionCreated {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "projectID")]
        project_id: String,
        location: LocationRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
        #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        slug: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<SessionMetadata>,
        #[serde(skip_serializing_if = "Option::is_none")]
        permissions: Option<Vec<Rule>>,
        version: String,
    },
    #[serde(rename = "session.renamed")]
    SessionRenamed {
        #[serde(rename = "sessionID")]
        session_id: String,
        title: String,
    },
    #[serde(rename = "session.deleted")]
    SessionDeleted {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.forked")]
    SessionForked {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "parentID")]
        parent_id: String,
        boundary: ForkBoundary,
        /// TODO(task>2): upstream `Instruction.Values` — opaque for now.
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        /// TODO(task>2): upstream `InstructionEntry.Snapshot` — opaque for now.
        #[serde(rename = "instructionEntries", skip_serializing_if = "Option::is_none")]
        instruction_entries: Option<Value>,
    },
    #[serde(rename = "session.usage.recorded")]
    SessionUsageRecorded {
        #[serde(rename = "sessionID")]
        session_id: String,
        source: UsageSource,
        cost: MoneyUsd,
        tokens: TokenUsage,
    },
    #[serde(rename = "session.usage.updated")]
    SessionUsageUpdated {
        #[serde(rename = "sessionID")]
        session_id: String,
        cost: MoneyUsd,
        tokens: TokenUsage,
    },
    /// Upstream `SessionEvent.InstructionsUpdated`: a keyed-instruction delta.
    /// `text` absent = bookkeeping only (no transcript row, mirroring the
    /// upstream updater). `delta` maps source key → hash (`"removed"` =
    /// removed, upstream `Instruction.removed`). Metadata rides on the
    /// envelope only; the projector builds the row metadata from `delta`.
    /// `sources` is the ox-specific machine-readable classification behind
    /// the row's `instructionSources` metadata (batch I): empty on pre-batch
    /// events, which replay with key strings only.
    #[serde(rename = "session.instructions.updated")]
    SessionInstructionsUpdated {
        #[serde(rename = "sessionID")]
        session_id: String,
        delta: Record,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sources: Vec<InstructionSource>,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    #[serde(rename = "session.synthetic")]
    SessionSynthetic {
        #[serde(rename = "sessionID")]
        session_id: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
    },
    #[serde(rename = "session.retry.scheduled")]
    SessionRetryScheduled {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        attempt: i64,
        at: i64,
        error: SessionError,
    },
    #[serde(rename = "session.message.content.updated")]
    SessionMessageContentUpdated {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "messageID")]
        message_id: String,
        content: Vec<AssistantContent>,
    },
    #[serde(rename = "session.moved")]
    SessionMoved {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(flatten)]
        payload: MovePayload,
    },
    #[serde(rename = "session.agent.selected")]
    SessionAgentSelected {
        #[serde(rename = "sessionID")]
        session_id: String,
        agent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<String>,
    },
    #[serde(rename = "session.model.selected")]
    SessionModelSelected {
        #[serde(rename = "sessionID")]
        session_id: String,
        model: ModelRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<ModelRef>,
    },
    #[serde(rename = "session.metadata.updated")]
    SessionMetadataUpdated {
        #[serde(rename = "sessionID")]
        session_id: String,
        metadata: SessionMetadata,
    },
    #[serde(rename = "session.permissions")]
    SessionPermissions {
        #[serde(rename = "sessionID")]
        session_id: String,
        permissions: Vec<Rule>,
    },
    #[serde(rename = "session.viewed")]
    SessionViewed {
        #[serde(rename = "sessionID")]
        session_id: String,
        /// Epoch-millisecond idle watermark the viewer observed.
        idle: i64,
    },
    /// Archive the session: it stays stored but drops out of the archived
    /// filter of `session.list` (upstream `SessionEvent.Archived`).
    #[serde(rename = "session.archived")]
    SessionArchived {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    /// Restore an archived session (upstream `SessionEvent.Unarchived`).
    #[serde(rename = "session.unarchived")]
    SessionUnarchived {
        #[serde(rename = "sessionID")]
        session_id: String,
    },

    // -- Execution ------------------------------------------------------------
    #[serde(rename = "session.execution.started")]
    ExecutionStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.execution.succeeded")]
    ExecutionSucceeded {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.execution.failed")]
    ExecutionFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        error: SessionError,
    },
    #[serde(rename = "session.execution.interrupted")]
    ExecutionInterrupted {
        #[serde(rename = "sessionID")]
        session_id: String,
        reason: InterruptReason,
    },

    // -- Inbox ----------------------------------------------------------------
    #[serde(rename = "session.inbox.enqueued")]
    InboxEnqueued {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "inboxID")]
        inbox_id: String,
        item: Item,
    },
    #[serde(rename = "session.inbox.delivered")]
    InboxDelivered {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "inboxID")]
        inbox_id: String,
    },
    #[serde(rename = "session.inbox.cancelled")]
    InboxCancelled {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "inboxID")]
        inbox_id: String,
    },
    #[serde(rename = "session.inbox.delivery.changed")]
    InboxDeliveryChanged {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "inboxID")]
        inbox_id: String,
        delivery: Delivery,
    },

    // -- Step -----------------------------------------------------------------
    #[serde(rename = "session.step.started")]
    StepStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        agent: String,
        model: ModelRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<SnapshotId>,
        /// Request dispatch time, before waiting for provider output.
        started: i64,
    },
    #[serde(rename = "session.step.streamed")]
    StepStreamed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
    },
    #[serde(rename = "session.step.ended")]
    StepEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        finish: FinishReason,
        #[serde(rename = "rawFinish", skip_serializing_if = "Option::is_none")]
        raw_finish: Option<String>,
        #[serde(rename = "providerState", skip_serializing_if = "Option::is_none")]
        provider_state: Option<ProviderState>,
        cost: MoneyUsd,
        tokens: TokenUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<SnapshotId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        files: Option<Vec<String>>,
    },
    #[serde(rename = "session.step.failed")]
    StepFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        error: SessionError,
        #[serde(skip_serializing_if = "Option::is_none")]
        finish: Option<StepFailedFinish>,
        #[serde(rename = "rawFinish", skip_serializing_if = "Option::is_none")]
        raw_finish: Option<String>,
        #[serde(rename = "providerState", skip_serializing_if = "Option::is_none")]
        provider_state: Option<ProviderState>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<MoneyUsd>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<SnapshotId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        files: Option<Vec<String>>,
    },

    // -- Text -----------------------------------------------------------------
    #[serde(rename = "session.text.started")]
    TextStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: i64,
    },
    #[serde(rename = "session.text.delta")]
    TextDelta {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: i64,
        delta: String,
    },
    #[serde(rename = "session.text.ended")]
    TextEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: i64,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<ProviderState>,
    },

    // -- Reasoning --------------------------------------------------------------
    #[serde(rename = "session.reasoning.started")]
    ReasoningStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<ProviderState>,
    },
    #[serde(rename = "session.reasoning.delta")]
    ReasoningDelta {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: i64,
        delta: String,
    },
    #[serde(rename = "session.reasoning.ended")]
    ReasoningEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        ordinal: i64,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<ProviderState>,
    },

    // -- Tool -------------------------------------------------------------------
    #[serde(rename = "session.tool.input.started")]
    ToolInputStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        name: String,
    },
    #[serde(rename = "session.tool.input.delta")]
    ToolInputDelta {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        delta: String,
    },
    #[serde(rename = "session.tool.input.ended")]
    ToolInputEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        text: String,
    },
    #[serde(rename = "session.tool.called")]
    ToolCalled {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        input: Record,
        executed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<ProviderState>,
    },
    #[serde(rename = "session.tool.progress")]
    ToolProgress {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        metadata: Record,
    },
    #[serde(rename = "session.tool.success")]
    ToolSuccess {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        content: Vec<Content>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        executed: bool,
        #[serde(rename = "resultState", skip_serializing_if = "Option::is_none")]
        result_state: Option<ProviderState>,
    },
    #[serde(rename = "session.tool.failed")]
    ToolFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        assistant_message_id: String,
        id: String,
        error: SessionError,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<Content>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Record>,
        executed: bool,
        #[serde(rename = "resultState", skip_serializing_if = "Option::is_none")]
        result_state: Option<ProviderState>,
    },

    // -- Compaction events --------------------------------------------------------
    #[serde(rename = "session.compaction.started")]
    CompactionStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        reason: CompactionReason,
        recent: String,
        #[serde(rename = "inputID", skip_serializing_if = "Option::is_none")]
        input_id: Option<String>,
    },
    #[serde(rename = "session.compaction.delta")]
    CompactionDelta {
        #[serde(rename = "sessionID")]
        session_id: String,
        text: String,
    },
    #[serde(rename = "session.compaction.ended")]
    CompactionEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        reason: CompactionReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(rename = "providerState", skip_serializing_if = "Option::is_none")]
        provider_state: Option<ProviderState>,
        #[serde(rename = "providerContext", skip_serializing_if = "Option::is_none")]
        provider_context: Option<ProviderContext>,
        text: String,
        recent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<MoneyUsd>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
    },
    #[serde(rename = "session.compaction.failed")]
    CompactionFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        reason: CompactionReason,
        error: SessionError,
        #[serde(rename = "inputID", skip_serializing_if = "Option::is_none")]
        input_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<MoneyUsd>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokenUsage>,
    },

    /// Any event type not in the ported set: kept verbatim (type string plus
    /// raw data) so no information is lost when reading upstream databases
    /// or streams.
    #[serde(untagged)]
    Unknown {
        /// The original envelope `type` string, captured because serde's
        /// internally-tagged matching cannot preserve an unmatched tag.
        r#type: String,
        data: Value,
    },
}

/// Take the `data` member out of an internally-tagged adapter object without
/// cloning the subtree (`{"type": .., "data": ..}` → the `data` value).
fn take_data(mut adapter: Value) -> Value {
    match &mut adapter {
        Value::Object(map) => map.remove("data").unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

impl SessionEventData {
    /// Parse a typed event from the envelope parts (`type` string + `data`).
    ///
    /// A *known* event type whose data payload does not match the schema
    /// falls back to [`SessionEventData::Unknown`] (verbatim), mirroring
    /// upstream's lenient `schemaCodec` bus decode. Only non-object data or
    /// other structural failures produce an error.
    #[must_use]
    pub fn from_parts(kind: &str, data: Value) -> Self {
        let adapter = serde_json::json!({ "type": kind, "data": data });
        serde_json::from_value(adapter).unwrap_or_else(|_| SessionEventData::Unknown {
            r#type: kind.to_string(),
            data,
        })
    }

    /// The envelope `type` string for this event.
    ///
    /// Invariant: a typed value's `kind()` always equals the envelope `type`
    /// it was parsed from. Because [`SessionEventData::from_parts`] uses an
    /// internally-tagged representation with an `untagged` `Unknown`
    /// fallback, an *unknown* type string is captured as
    /// [`SessionEventData::Unknown`] together with the tag, so the invariant
    /// holds even for unknown events.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            SessionEventData::SessionCreated { .. } => "session.created",
            SessionEventData::SessionRenamed { .. } => "session.renamed",
            SessionEventData::SessionDeleted { .. } => "session.deleted",
            SessionEventData::SessionForked { .. } => "session.forked",
            SessionEventData::SessionUsageRecorded { .. } => "session.usage.recorded",
            SessionEventData::SessionUsageUpdated { .. } => "session.usage.updated",
            SessionEventData::SessionInstructionsUpdated { .. } => "session.instructions.updated",
            SessionEventData::SessionSynthetic { .. } => "session.synthetic",
            SessionEventData::SessionRetryScheduled { .. } => "session.retry.scheduled",
            SessionEventData::SessionMessageContentUpdated { .. } => {
                "session.message.content.updated"
            }
            SessionEventData::SessionMoved { .. } => "session.moved",
            SessionEventData::SessionAgentSelected { .. } => "session.agent.selected",
            SessionEventData::SessionModelSelected { .. } => "session.model.selected",
            SessionEventData::SessionMetadataUpdated { .. } => "session.metadata.updated",
            SessionEventData::SessionPermissions { .. } => "session.permissions",
            SessionEventData::SessionViewed { .. } => "session.viewed",
            SessionEventData::SessionArchived { .. } => "session.archived",
            SessionEventData::SessionUnarchived { .. } => "session.unarchived",
            SessionEventData::ExecutionStarted { .. } => "session.execution.started",
            SessionEventData::ExecutionSucceeded { .. } => "session.execution.succeeded",
            SessionEventData::ExecutionFailed { .. } => "session.execution.failed",
            SessionEventData::ExecutionInterrupted { .. } => "session.execution.interrupted",
            SessionEventData::InboxEnqueued { .. } => "session.inbox.enqueued",
            SessionEventData::InboxDelivered { .. } => "session.inbox.delivered",
            SessionEventData::InboxCancelled { .. } => "session.inbox.cancelled",
            SessionEventData::InboxDeliveryChanged { .. } => "session.inbox.delivery.changed",
            SessionEventData::StepStarted { .. } => "session.step.started",
            SessionEventData::StepStreamed { .. } => "session.step.streamed",
            SessionEventData::StepEnded { .. } => "session.step.ended",
            SessionEventData::StepFailed { .. } => "session.step.failed",
            SessionEventData::TextStarted { .. } => "session.text.started",
            SessionEventData::TextDelta { .. } => "session.text.delta",
            SessionEventData::TextEnded { .. } => "session.text.ended",
            SessionEventData::ReasoningStarted { .. } => "session.reasoning.started",
            SessionEventData::ReasoningDelta { .. } => "session.reasoning.delta",
            SessionEventData::ReasoningEnded { .. } => "session.reasoning.ended",
            SessionEventData::ToolInputStarted { .. } => "session.tool.input.started",
            SessionEventData::ToolInputDelta { .. } => "session.tool.input.delta",
            SessionEventData::ToolInputEnded { .. } => "session.tool.input.ended",
            SessionEventData::ToolCalled { .. } => "session.tool.called",
            SessionEventData::ToolProgress { .. } => "session.tool.progress",
            SessionEventData::ToolSuccess { .. } => "session.tool.success",
            SessionEventData::ToolFailed { .. } => "session.tool.failed",
            SessionEventData::CompactionStarted { .. } => "session.compaction.started",
            SessionEventData::CompactionDelta { .. } => "session.compaction.delta",
            SessionEventData::CompactionEnded { .. } => "session.compaction.ended",
            SessionEventData::CompactionFailed { .. } => "session.compaction.failed",
            SessionEventData::Unknown { r#type, .. } => r#type,
        }
    }

    /// Extract the raw `data` payload.
    #[must_use]
    pub fn into_data(self) -> Value {
        if let SessionEventData::Unknown { data, .. } = self {
            return data;
        }
        let adapter = serde_json::to_value(&self).unwrap_or(Value::Null);
        take_data(adapter)
    }

    /// The borrowed twin of [`Self::into_data`]: publishers that keep the
    /// typed event (the envelope) need the `data` subtree without giving up
    /// ownership. Same output, no clone of the union.
    #[must_use]
    pub fn to_data(&self) -> Value {
        if let SessionEventData::Unknown { data, .. } = self {
            return data.clone();
        }
        let adapter = serde_json::to_value(self).unwrap_or(Value::Null);
        take_data(adapter)
    }

    /// The session this event belongs to, when the variant carries it.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        match self {
            SessionEventData::SessionCreated { session_id, .. }
            | SessionEventData::SessionRenamed { session_id, .. }
            | SessionEventData::SessionDeleted { session_id, .. }
            | SessionEventData::SessionForked { session_id, .. }
            | SessionEventData::SessionUsageRecorded { session_id, .. }
            | SessionEventData::SessionUsageUpdated { session_id, .. }
            | SessionEventData::SessionInstructionsUpdated { session_id, .. }
            | SessionEventData::SessionSynthetic { session_id, .. }
            | SessionEventData::SessionRetryScheduled { session_id, .. }
            | SessionEventData::SessionMessageContentUpdated { session_id, .. }
            | SessionEventData::SessionMoved { session_id, .. }
            | SessionEventData::SessionAgentSelected { session_id, .. }
            | SessionEventData::SessionModelSelected { session_id, .. }
            | SessionEventData::SessionMetadataUpdated { session_id, .. }
            | SessionEventData::SessionPermissions { session_id, .. }
            | SessionEventData::SessionViewed { session_id, .. }
            | SessionEventData::SessionArchived { session_id, .. }
            | SessionEventData::SessionUnarchived { session_id, .. }
            | SessionEventData::ExecutionStarted { session_id, .. }
            | SessionEventData::ExecutionSucceeded { session_id, .. }
            | SessionEventData::ExecutionFailed { session_id, .. }
            | SessionEventData::ExecutionInterrupted { session_id, .. }
            | SessionEventData::InboxEnqueued { session_id, .. }
            | SessionEventData::InboxDelivered { session_id, .. }
            | SessionEventData::InboxCancelled { session_id, .. }
            | SessionEventData::InboxDeliveryChanged { session_id, .. }
            | SessionEventData::StepStarted { session_id, .. }
            | SessionEventData::StepStreamed { session_id, .. }
            | SessionEventData::StepEnded { session_id, .. }
            | SessionEventData::StepFailed { session_id, .. }
            | SessionEventData::TextStarted { session_id, .. }
            | SessionEventData::TextDelta { session_id, .. }
            | SessionEventData::TextEnded { session_id, .. }
            | SessionEventData::ReasoningStarted { session_id, .. }
            | SessionEventData::ReasoningDelta { session_id, .. }
            | SessionEventData::ReasoningEnded { session_id, .. }
            | SessionEventData::ToolInputStarted { session_id, .. }
            | SessionEventData::ToolInputDelta { session_id, .. }
            | SessionEventData::ToolInputEnded { session_id, .. }
            | SessionEventData::ToolCalled { session_id, .. }
            | SessionEventData::ToolProgress { session_id, .. }
            | SessionEventData::ToolSuccess { session_id, .. }
            | SessionEventData::ToolFailed { session_id, .. }
            | SessionEventData::CompactionStarted { session_id, .. }
            | SessionEventData::CompactionDelta { session_id, .. }
            | SessionEventData::CompactionEnded { session_id, .. }
            | SessionEventData::CompactionFailed { session_id, .. } => Some(session_id),
            SessionEventData::Unknown { .. } => None,
        }
    }
}
