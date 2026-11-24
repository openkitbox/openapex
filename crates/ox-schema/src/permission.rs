//! Wire types from upstream `packages/schema/src/permission.ts` and
//! `permission-saved.ts`. The event data shapes are the payloads carried by the
//! ephemeral `permission.asked` / `permission.replied` events.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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

/// Upstream `Permission.Source`: a union of one tool variant, with no `callID`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    Tool {
        #[serde(rename = "messageID")]
        message_id: String,
        id: String,
    },
}

/// Upstream `Permission.Reply`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Reply {
    Once,
    Always,
    Reject,
}

/// Full `Permission.Request.fields` as carried by the `permission.asked` data
/// (including the request `id` the TUI reads from the event payload).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Asked {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub action: String,
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Upstream `Permission.Request` (`id` plus the `Asked` fields).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub action: String,
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Upstream `permission.replied` event data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Replied {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "requestID")]
    pub request_id: String,
    pub reply: Reply,
}

/// Upstream `Permission.Event` group: the two ephemeral event data shapes.
pub mod event {
    pub use super::{Asked, Replied};
}

/// Upstream `PermissionSaved.Info` (`time` is UTC milliseconds).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedInfo {
    pub id: String,
    #[serde(rename = "projectID")]
    pub project_id: String,
    pub action: String,
    pub resource: String,
    pub time: SavedTime,
}

/// `PermissionSaved.Info.time`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedTime {
    pub created: i64,
    pub updated: i64,
}

/// Upstream `Permission.ID.create` (`per_` prefix).
#[must_use]
pub fn new_request_id() -> String {
    crate::id::id("per")
}

/// Upstream `PermissionSaved.ID.create` (`psv_` prefix).
#[must_use]
pub fn new_saved_id() -> String {
    crate::id::id("psv")
}
