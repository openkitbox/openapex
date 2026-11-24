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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Asked, Replied, Reply, Request, SavedInfo, Source, new_request_id, new_saved_id};

    #[test]
    fn request_wire_shape_matches_upstream() {
        let raw = json!({
            "id": "per_123",
            "sessionID": "ses_123",
            "action": "shell",
            "resources": ["git status"],
            "save": ["git status"],
            "metadata": {"parentID": "ses_root"},
            "source": {"type": "tool", "messageID": "msg_1", "id": "call_1"},
            "message": "why"
        });
        let request: Request = serde_json::from_value(raw.clone()).expect("valid V2 request");
        assert_eq!(
            serde_json::to_value(&request).expect("serialize request"),
            raw
        );
        assert_eq!(request.session_id, "ses_123");
        assert_eq!(request.action, "shell");
        assert_eq!(request.resources, ["git status"]);
        assert!(matches!(request.source, Some(Source::Tool { .. })));
    }

    #[test]
    fn source_has_no_call_id() {
        assert!(
            serde_json::from_value::<Source>(json!({
                "type": "tool",
                "messageID": "msg_1",
                "id": "call_1",
                "callID": "extra"
            }))
            .is_err(),
            "upstream Source has only type, messageID and id"
        );
    }

    #[test]
    fn asked_and_replied_event_data_match_upstream() {
        let asked_raw = json!({
            "id": "per_1",
            "sessionID": "ses_1",
            "action": "edit",
            "resources": ["/tmp/file"],
            "source": {"type": "tool", "messageID": "msg_1", "id": "call_1"}
        });
        let asked: Asked = serde_json::from_value(asked_raw.clone()).expect("valid asked data");
        assert_eq!(asked.id, "per_1");
        assert_eq!(
            serde_json::to_value(&asked).expect("serialize asked"),
            asked_raw
        );

        let replied_raw = json!({
            "sessionID": "ses_1",
            "requestID": "per_1",
            "reply": "always"
        });
        let replied: Replied =
            serde_json::from_value(replied_raw.clone()).expect("valid replied data");
        assert_eq!(replied.reply, Reply::Always);
        assert_eq!(
            serde_json::to_value(&replied).expect("serialize replied"),
            replied_raw
        );
    }

    #[test]
    fn saved_info_matches_upstream() {
        let raw = json!({
            "id": "psv_1",
            "projectID": "prj_1",
            "action": "shell",
            "resource": "git status",
            "time": {"created": 1_700_000_000_000_i64, "updated": 1_700_000_001_000_i64}
        });
        let saved: SavedInfo = serde_json::from_value(raw.clone()).expect("valid saved info");
        assert_eq!(serde_json::to_value(&saved).expect("serialize saved"), raw);
        assert_eq!(saved.time.created, 1_700_000_000_000);
    }

    #[test]
    fn ids_use_upstream_prefixes() {
        assert!(new_request_id().starts_with("per_"));
        assert!(new_saved_id().starts_with("psv_"));
    }
}
