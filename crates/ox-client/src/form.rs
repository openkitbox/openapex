//! Wire mirror of upstream `Form.*` (`packages/schema/src/form.ts`) as the ox
//! server speaks it (`ox-tools/src/form.rs`), plus the pure reducer for the
//! `form.created` / `form.replied` / `form.cancelled` events.
//!
//! Only what the TUI renders is modelled: validation keywords (`format`,
//! `pattern`, `minLength`, …), `hidden` and `when` are ignored by serde.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One selectable option (`Form.Option`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A form field (`Form.Field`, tagged on `type`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FormField {
    String {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: Option<bool>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        default: Option<String>,
        #[serde(default)]
        options: Option<Vec<FormOption>>,
        /// Free-form input allowed beside the options.
        #[serde(default)]
        custom: Option<bool>,
    },
    Number {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: Option<bool>,
        #[serde(default)]
        minimum: Option<f64>,
        #[serde(default)]
        maximum: Option<f64>,
        #[serde(default)]
        default: Option<f64>,
    },
    Integer {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: Option<bool>,
        #[serde(default)]
        minimum: Option<f64>,
        #[serde(default)]
        maximum: Option<f64>,
        #[serde(default)]
        default: Option<f64>,
    },
    Boolean {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: Option<bool>,
        #[serde(default)]
        default: Option<bool>,
    },
    Multiselect {
        key: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: Option<bool>,
        options: Vec<FormOption>,
        #[serde(default)]
        custom: Option<bool>,
        #[serde(default)]
        default: Option<Vec<String>>,
    },
    External {
        key: String,
        url: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
    },
    /// Any `type` this client does not know. Serde cannot keep the payload of
    /// an `other` variant, so it carries no key: the form still parses and the
    /// field is shown read-only instead of dropping the whole `Form.Info`.
    #[serde(other)]
    Unsupported,
}

impl FormField {
    /// The answer-map key this field fills; empty for [`FormField::Unsupported`],
    /// which never contributes an answer.
    pub fn key(&self) -> &str {
        match self {
            FormField::String { key, .. }
            | FormField::Number { key, .. }
            | FormField::Integer { key, .. }
            | FormField::Boolean { key, .. }
            | FormField::Multiselect { key, .. }
            | FormField::External { key, .. } => key,
            FormField::Unsupported => "",
        }
    }
}

/// `Form.Info`: a pending form as listed by REST and carried by `form.created`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FormInfo {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub title: String,
    #[serde(default)]
    pub metadata: Option<Value>,
    pub fields: Vec<FormField>,
}

/// `Form.Value`: one answered field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FormValue {
    String(String),
    Number(f64),
    Boolean(bool),
    Multiple(Vec<String>),
}

/// `Form.Answer`: field key → value.
pub type Answer = HashMap<String, FormValue>;

/// Fold one `form.*` event into the pending list. `session` is the focused
/// session; `None` accepts every owner (the store has not been focused yet).
/// Returns `false` for event types this reducer does not own.
///
/// - `form.created` appends the form unless it belongs to another session or
///   its id is already pending (replays and reconnects re-deliver frames).
///   A payload that does not parse is dropped (and logged).
/// - `form.replied` / `form.cancelled` remove by id; the frames carry the
///   owning session but the id alone is authoritative.
pub fn apply_event(
    pending: &mut Vec<FormInfo>,
    session: Option<&str>,
    kind: &str,
    data: &Value,
) -> bool {
    match kind {
        "form.created" => {
            match FormInfo::deserialize(&data["form"]) {
                Ok(form)
                    if session.is_none_or(|session| session == form.session_id)
                        && !pending.iter().any(|known| known.id == form.id) =>
                {
                    pending.push(form);
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "dropping unparsable form.created"),
            }
            true
        }
        "form.replied" | "form.cancelled" => {
            if let Some(id) = data["id"].as_str() {
                pending.retain(|form| form.id != id);
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_unknown_field_type_parses_as_unsupported() {
        let form = FormInfo::deserialize(&json!({
            "id": "frm_1", "sessionID": "ses_1", "title": "t",
            "fields": [
                {"type": "date", "key": "when", "format": "iso"},
                {"type": "boolean", "key": "ok"}
            ]
        }))
        .expect("tolerant parse");
        assert_eq!(form.fields[0], FormField::Unsupported);
        assert_eq!(form.fields[0].key(), "");
        assert_eq!(form.fields[1].key(), "ok");
    }

    #[test]
    fn a_form_with_an_unknown_field_type_is_still_pending() {
        let mut pending = Vec::new();
        let data = json!({"form": {
            "id": "frm_1", "sessionID": "ses_1", "title": "t",
            "fields": [{"type": "date", "key": "when"}]
        }});
        assert!(apply_event(
            &mut pending,
            Some("ses_1"),
            "form.created",
            &data
        ));
        assert_eq!(pending.len(), 1);
    }
}
