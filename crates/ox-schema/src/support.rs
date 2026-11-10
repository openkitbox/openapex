//! Shared supporting types for the schema layer, ported field-by-field from
//! `packages/schema/src/*` upstream:
//!
//! - `session-error.ts`  -> [`SessionError`]
//! - `tool.ts`           -> [`Content`]
//! - `llm.ts`            -> [`FinishReason`]
//! - `token-usage.ts`    -> [`TokenUsage`]
//! - `model.ts`          -> [`ModelRef`]
//! - `money.ts`          -> [`MoneyUsd`]
//! - `prompt.ts`         -> [`Prompt`], [`FileAttachment`], [`AgentAttachment`], [`SkillAttachment`]
//! - `prompt-input.ts`   -> [`FileInput`]
//! - `location.ts`       -> [`LocationRef`]
//! - `session-fork.ts`   -> [`ForkBoundary`]
//! - `permission.ts`     -> [`Rule`], [`Effect`]
//! - `shell.ts`          -> [`ShellStatus`], [`ShellOutput`], [`ShellInfo`]
//! - `session-metadata.ts` -> [`SessionMetadata`]
//!
//! Wire convention (verified against upstream): upstream's
//! `Schema.optionalKey(...)` means the JSON key may be *absent*; an absent key
//! is never emitted as `null` on encode (encode filters `undefined`). So every
//! optional field here is `Option<T>` with `skip_serializing_if` — no field in
//! the ported unions permits an explicit JSON `null` value.
//!
//! Upstream branded IDs (`Model.ID`, `Agent.ID`, `Snapshot.ID`, ...) are plain
//! `String` here; the brands carry no wire information.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `kv` key for a session's memory participation marker.
/// The engine owns its meaning; fork projection preserves the stored marker.
#[must_use]
pub fn memory_mode_key(session_id: &str) -> String {
    format!("mctx_memory_mode:{session_id}")
}

/// The `kv` key for a session's external-content marker.
/// The engine owns its meaning; fork projection preserves the stored marker.
#[must_use]
pub fn session_taint_key(session_id: &str) -> String {
    format!("mctx_tainted:{session_id}")
}

/// Upstream `Snapshot.ID` (branded string). Plain string on the wire.
pub type SnapshotId = String;

/// `Schema.Finite` — a JSON number on the wire. Upstream stores token counts
/// as integers but cost values may arrive as either an integer (`0`) or a
/// float (`0.0123`), and `serde_json::Number` distinguishes the two. To keep
/// byte-exact round trips we preserve the distinction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Finite {
    Int(i64),
    Float(f64),
}

impl Serialize for Finite {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Finite::Int(n) => serializer.serialize_i64(*n),
            Finite::Float(n) => serializer.serialize_f64(*n),
        }
    }
}

impl<'de> Deserialize<'de> for Finite {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Finite::Int(i))
                } else if let Some(u) = n.as_u64() {
                    Ok(Finite::Int(
                        i64::try_from(u).map_err(|_| serde::de::Error::custom("u64 overflow"))?,
                    ))
                } else {
                    n.as_f64()
                        .map(Finite::Float)
                        .ok_or_else(|| serde::de::Error::custom("invalid finite number"))
                }
            }
            other => Err(serde::de::Error::custom(format!(
                "expected number, got {other}"
            ))),
        }
    }
}

/// `Money.USD` — `Schema.Finite` on the wire.
pub type MoneyUsd = Finite;

/// Opaque string-keyed record (`Schema.Record(String, Unknown)`).
pub type Record = std::collections::BTreeMap<String, Value>;

/// Upstream `SessionMetadata` — host-supplied session annotations, opaque to
/// core (`Schema.Record(String, Json)`).
pub type SessionMetadata = std::collections::BTreeMap<String, Value>;

// ---------------------------------------------------------------------------
// session-error.ts
// ---------------------------------------------------------------------------

/// Upstream `SessionError.Error` (`Session.StructuredError`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<SessionErrorResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionErrorResponse {
    pub body: String,
}

// ---------------------------------------------------------------------------
// tool.ts — Content union
// ---------------------------------------------------------------------------

/// Upstream `Tool.Content` (tagged union on `type`: text | file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Content {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "file")]
    File {
        uri: String,
        mime: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// llm.ts
// ---------------------------------------------------------------------------

/// Upstream `FinishReason` literal union.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FinishReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "tool-calls")]
    ToolCalls,
    #[serde(rename = "content-filter")]
    ContentFilter,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "unknown")]
    Unknown,
}

// ---------------------------------------------------------------------------
// token-usage.ts
// ---------------------------------------------------------------------------

/// Upstream `TokenUsage.Info`, plus the ox-only `estimated` extension.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache: TokenCache,
    /// ox extension (not in upstream `TokenUsage.Info`): `Some(true)` when
    /// `input` is an estimate from the sent request's size because the stream
    /// carried no usage. Absent for provider-reported usage, so those rows
    /// serialize exactly as upstream's shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct TokenCache {
    pub read: i64,
    pub write: i64,
}

// ---------------------------------------------------------------------------
// model.ts — Model.Ref only (Model.Info is out of scope for task 2)
// ---------------------------------------------------------------------------

/// Upstream `Model.Ref`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRef {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// The product-reserved logical model id for cheap auxiliary work: session
/// title generation, `explore` subagents, and the one-turn `/slim` override.
///
/// It is a logical id, so the wire model behind it is decided by `[models.slim]`
/// routes or by a catalogue entry named `slim` — never by a value hardcoded
/// here. Callers that cannot resolve it fall back to their own model.
pub const SLIM: &str = "slim";

// ---------------------------------------------------------------------------
// prompt.ts
// ---------------------------------------------------------------------------

/// Upstream `Prompt` — the fields are embedded into `SessionMessage.User` and
/// `SessionInbox.UserPayload` (`...Prompt.fields`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Prompt {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileAttachment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentAttachment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<SkillAttachment>>,
}

/// Upstream `Prompt.FileSource` (tagged union on `type`: inline | uri).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum FileSource {
    #[serde(rename = "inline")]
    Inline,
    #[serde(rename = "uri")]
    Uri { uri: String },
}

/// Upstream `PromptMention`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptMention {
    pub start: i64,
    pub end: i64,
    pub text: String,
}

/// Upstream `PromptInput.FileAttachment` — the request-side shape of a prompt
/// file (`session.prompt` body). Distinct from the stored [`FileAttachment`]:
/// the server materializes `uri` into `data`/`mime`/`source` at admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileInput {
    pub uri: String,
    /// Where the bytes came from, for a `data:` upload whose original file
    /// the server may not be able to reopen (upstream 2.2.3). Descriptive
    /// only: the server never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<FileSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<PromptMention>,
}

/// Upstream `Prompt.FileAttachment`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileAttachment {
    /// Base64 payload. `Arc<str>` because the same payload is shared by the
    /// inbox item, the event and the projected message row instead of being
    /// deep-cloned at each hop (a 20 MiB image is ~27 MB of base64).
    pub data: Arc<str>,
    pub mime: String,
    pub source: FileSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<PromptMention>,
}

/// Upstream `Prompt.AgentAttachment`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentAttachment {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<PromptMention>,
}

/// Upstream `Prompt.SkillAttachment`.
///
/// Upstream resolves the id at admission (`SessionPrompt.prepare` ->
/// `Skill.prepare`), and ox does the same in its prompt handler, so a stored
/// payload always carries `name` and `text`. `text` stays optional because
/// upstream's duplicate-id branch emits `{id, name, mention}` with no body —
/// the body is injected once, on the first occurrence. Upstream's prompt
/// *input* only carries `id` (plus an optional `mention`), so `name: None`
/// means "not resolved" (a payload from ox always has one), and neither field
/// is ever emitted as `null` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillAttachment {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<PromptMention>,
}

// ---------------------------------------------------------------------------
// location.ts — Location.Ref
// ---------------------------------------------------------------------------

/// Upstream `Location.Ref`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocationRef {
    pub directory: String,
    #[serde(rename = "workspaceID", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

// ---------------------------------------------------------------------------
// session-fork.ts
// ---------------------------------------------------------------------------

/// Upstream `SessionFork.Boundary` (tagged union on `type`: before | through).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ForkBoundary {
    #[serde(rename = "before")]
    Before {
        #[serde(rename = "messageID")]
        message_id: String,
    },
    #[serde(rename = "through")]
    Through {
        #[serde(rename = "messageID")]
        message_id: String,
    },
}

// ---------------------------------------------------------------------------
// permission.ts — Rule / Effect
// ---------------------------------------------------------------------------

/// Upstream `Permission.Effect` literal union.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

/// Upstream `Permission.Rule` (`Permission.Ruleset` is `Vec<Rule>`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
}

// ---------------------------------------------------------------------------
// shell.ts — Status / Output / Info
// ---------------------------------------------------------------------------

/// Upstream `Shell.Status` literal union.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShellStatus {
    Running,
    Exited,
    Timeout,
    Killed,
}

/// Upstream `Shell.Output`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellOutput {
    pub output: String,
    pub cursor: i64,
    pub size: i64,
    pub truncated: bool,
}

/// Upstream `Shell.Info`: the wire shape of a live shell session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellInfo {
    pub id: String,
    pub status: ShellStatus,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub metadata: Record,
    pub time: ShellInfoTime,
}

/// Epoch milliseconds of `Shell.Info.time`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellInfoTime {
    pub started: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

// ---------------------------------------------------------------------------
// session-provider-context.ts — referenced by compaction events/messages
// ---------------------------------------------------------------------------

/// Upstream `SessionProviderContext.Provenance`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderProvenance {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub provider: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    pub route: String,
    pub protocol: String,
    pub endpoint: String,
}

/// Upstream `SessionProviderContext.Info`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderContext {
    pub version: u8, // upstream: Literal(1)
    pub provenance: ProviderProvenance,
    pub messages: Value,
}
