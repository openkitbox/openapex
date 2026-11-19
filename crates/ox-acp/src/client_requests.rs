//! Negotiated ACP client capabilities and owned client interactions.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CreateElicitationRequest, ElicitationAcceptAction, ElicitationAction, ElicitationContentValue,
    ElicitationFormMode, ElicitationSchema, ElicitationSessionScope, PermissionOption,
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, Error};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::internal;

#[derive(Clone, Default)]
pub(crate) struct ClientRequests {
    capabilities: Arc<Mutex<agent_client_protocol::v1::schema::ClientCapabilities>>,
}

impl ClientRequests {
    pub(crate) fn set_capabilities(
        &self,
        capabilities: agent_client_protocol::v1::schema::ClientCapabilities,
    ) {
        if let Ok(mut stored) = self.capabilities.lock() {
            *stored = capabilities;
        }
    }

    pub(crate) fn supports_form_elicitation(&self) -> bool {
        self.capabilities
            .lock()
            .ok()
            .and_then(|stored| stored.elicitation.clone())
            .is_some_and(|capabilities| capabilities.supports_form())
    }

    pub(crate) fn supports_compaction_updates(&self) -> bool {
        self.capabilities
            .lock()
            .ok()
            .and_then(|stored| stored.session.clone())
            .and_then(|session| session.compaction)
            .is_some()
    }

    pub(crate) fn supports_notices(&self) -> bool {
        self.capabilities
            .lock()
            .ok()
            .and_then(|stored| stored.session.clone())
            .and_then(|session| session.notices)
            .is_some()
    }

    pub(crate) fn allows_update(
        &self,
        update: &agent_client_protocol::v1::schema::SessionUpdate,
    ) -> bool {
        if matches!(
            update,
            agent_client_protocol::v1::schema::SessionUpdate::CompactionUpdate(_)
                | agent_client_protocol::v1::schema::SessionUpdate::CompactionSummaryChunk(_)
        ) {
            return self.supports_compaction_updates();
        }
        if matches!(
            update,
            agent_client_protocol::v1::schema::SessionUpdate::Notice(_)
        ) {
            return self.supports_notices();
        }
        true
    }

    pub(crate) fn request_token(&self) -> CancellationToken {
        CancellationToken::new()
    }

    pub(crate) async fn request_permission(
        &self,
        http: &reqwest::Client,
        base: &str,
        form: &Value,
        connection: &ConnectionTo<Client>,
        cancellation: &CancellationToken,
        directory: &str,
    ) -> Result<PermissionResult, Error> {
        let (session_id, form_id) = form_identity(form)?;
        let field = form
            .pointer("/fields")
            .and_then(Value::as_array)
            .and_then(|fields| {
                fields.iter().find(|field| {
                    field.get("type").and_then(Value::as_str) == Some("string")
                        && field.get("key").and_then(Value::as_str) == Some("decision")
                })
            })
            .ok_or_else(|| internal("unsupported form interaction"))?;
        let options = field
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| internal("permission form has no options"))?
            .iter()
            .filter_map(|option| {
                Some(PermissionOption::new(
                    option.get("value")?.as_str()?.to_owned(),
                    option.get("label")?.as_str()?.to_owned(),
                    permission_kind(option.get("value")?.as_str()?),
                ))
            })
            .collect::<Vec<_>>();
        if options.is_empty() {
            return Err(internal("permission form has no valid options"));
        }
        let request = RequestPermissionRequest::new(
            SessionId::new(session_id.clone()),
            ToolCallUpdate::new(
                ToolCallId::new(form_id.clone()),
                ToolCallUpdateFields::new()
                    .title("Permission required")
                    .status(ToolCallStatus::Pending),
            ),
            options,
        );
        let sent = connection.send_request(request);
        let cancellation_handle = sent.cancellation_handle();
        let response = tokio::select! {
            result = sent.block_task() => result?,
            () = cancellation.cancelled() => {
                let _ = cancellation_handle.cancel();
                settle_form_cancel(http, base, &session_id, &form_id, directory).await?;
                return Ok(PermissionResult::Cancelled);
            }
        };
        match response.outcome {
            RequestPermissionOutcome::Cancelled => {
                settle_form_cancel(http, base, &session_id, &form_id, directory).await?;
                Ok(PermissionResult::Cancelled)
            }
            RequestPermissionOutcome::Selected(selected) => {
                let value = selected.option_id.0.as_ref();
                settle_form_answer(http, base, &session_id, &form_id, directory, json!(value))
                    .await?;
                Ok(PermissionResult::Selected(value.to_owned()))
            }
            _ => {
                settle_form_cancel(http, base, &session_id, &form_id, directory).await?;
                Err(internal("unsupported permission response"))
            }
        }
    }

    pub(crate) async fn request_elicitation(
        &self,
        http: &reqwest::Client,
        base: &str,
        form: &Value,
        connection: &ConnectionTo<Client>,
        cancellation: &CancellationToken,
        directory: &str,
    ) -> Result<ElicitationResult, Error> {
        let (session_id, form_id) = form_identity(form)?;
        if !self.supports_form_elicitation() {
            settle_form_cancel(http, base, &session_id, &form_id, directory).await?;
            return Err(internal("client does not support elicitation"));
        }
        let scope = ElicitationSessionScope::new(SessionId::new(session_id.clone()));
        let schema = ElicitationSchema::new().property(
            "answer",
            agent_client_protocol::v1::schema::StringPropertySchema::new(),
            true,
        );
        let request = CreateElicitationRequest::new(
            ElicitationFormMode::new(scope, schema),
            form.pointer("/title")
                .and_then(Value::as_str)
                .unwrap_or("Input required"),
        );
        let sent = connection.send_request(request);
        let cancellation_handle = sent.cancellation_handle();
        let response = tokio::select! {
            result = sent.block_task() => result?,
            () = cancellation.cancelled() => {
                let _ = cancellation_handle.cancel();
                settle_form_cancel(http, base, &session_id, &form_id, directory).await?;
                return Ok(ElicitationResult::Cancelled);
            }
        };
        match response.action {
            ElicitationAction::Accept(ElicitationAcceptAction {
                content: Some(content),
                ..
            }) => {
                let answer = elicitation_answer(&content)?;
                settle_form_answer(http, base, &session_id, &form_id, directory, answer).await?;
                Ok(ElicitationResult::Accepted(content))
            }
            ElicitationAction::Accept(ElicitationAcceptAction { content: None, .. })
            | ElicitationAction::Decline => {
                settle_form_cancel(http, base, &session_id, &form_id, directory).await?;
                Ok(ElicitationResult::Declined)
            }
            ElicitationAction::Cancel => {
                settle_form_cancel(http, base, &session_id, &form_id, directory).await?;
                Ok(ElicitationResult::Cancelled)
            }
            _ => {
                settle_form_cancel(http, base, &session_id, &form_id, directory).await?;
                Err(internal("unsupported elicitation response"))
            }
        }
    }

    pub(crate) async fn settle_form_for_task(
        http: &reqwest::Client,
        base: &str,
        form: &Value,
        directory: &str,
    ) -> Result<(), Error> {
        let (session_id, form_id) = form_identity(form)?;
        settle_form_cancel(http, base, &session_id, &form_id, directory).await
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PermissionResult {
    Selected(String),
    Cancelled,
}

#[derive(Debug, PartialEq)]
pub(crate) enum ElicitationResult {
    Accepted(BTreeMap<String, ElicitationContentValue>),
    Declined,
    Cancelled,
}

fn form_identity(form: &Value) -> Result<(String, String), Error> {
    let form_id = form
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("form has no id"))?;
    let session_id = form
        .get("sessionID")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("form has no session ID"))?;
    Ok((session_id.to_owned(), form_id.to_owned()))
}

fn elicitation_answer(content: &BTreeMap<String, ElicitationContentValue>) -> Result<Value, Error> {
    let mut answer = serde_json::Map::new();
    for (key, value) in content {
        let value = match value {
            ElicitationContentValue::String(value) => Value::String(value.clone()),
            ElicitationContentValue::Integer(value) => json!(value),
            ElicitationContentValue::Number(value) if value.is_finite() => json!(value),
            ElicitationContentValue::Number(_) => {
                return Err(internal("elicitation answer contains a non-finite number"));
            }
            ElicitationContentValue::Boolean(value) => Value::Bool(*value),
            ElicitationContentValue::StringArray(values) => json!(values),
            _ => return Err(internal("unsupported elicitation answer value")),
        };
        answer.insert(key.clone(), value);
    }
    Ok(Value::Object(answer))
}

fn permission_kind(value: &str) -> PermissionOptionKind {
    match value {
        "once" => PermissionOptionKind::AllowOnce,
        "session" => PermissionOptionKind::AllowAlways,
        _ => PermissionOptionKind::RejectOnce,
    }
}

async fn settle_form_cancel(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
    form_id: &str,
    directory: &str,
) -> Result<(), Error> {
    settle_form(http, base, session_id, form_id, directory, None).await
}

async fn settle_form_answer(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
    form_id: &str,
    directory: &str,
    answer: Value,
) -> Result<(), Error> {
    settle_form(http, base, session_id, form_id, directory, Some(answer)).await
}

async fn settle_form(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
    form_id: &str,
    directory: &str,
    answer: Option<Value>,
) -> Result<(), Error> {
    let url = format!("{base}/api/session/{session_id}/form/{form_id}");
    let response = match answer {
        Some(answer) => {
            http.post(format!("{url}/reply"))
                .header("x-opencode-directory", directory)
                .json(&json!({"answer": answer}))
                .send()
                .await
        }
        None => {
            http.delete(format!("{url}?message=cancelled"))
                .header("x-opencode-directory", directory)
                .send()
                .await
        }
    }
    .map_err(internal)?;
    if !response.status().is_success() {
        return Err(internal(format!(
            "form settlement returned {}",
            response.status()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::ClientRequests;
    use agent_client_protocol::v1::schema::{
        ClientCapabilities, ElicitationCapabilities, ElicitationFormCapabilities,
    };

    #[test]
    fn negotiated_capabilities_gate_elicitation_modes() {
        let requests = ClientRequests::default();
        assert!(!requests.supports_form_elicitation());
        requests.set_capabilities(
            ClientCapabilities::new().elicitation(
                ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
            ),
        );
        assert!(requests.supports_form_elicitation());
    }

    #[test]
    fn elicitation_answers_are_direct_form_fields() {
        let content = BTreeMap::from([
            (
                "q0".to_owned(),
                agent_client_protocol::v1::schema::ElicitationContentValue::String("Ada".into()),
            ),
            (
                "q1".to_owned(),
                agent_client_protocol::v1::schema::ElicitationContentValue::StringArray(vec![
                    "Rust".into(),
                ]),
            ),
            (
                "q2".to_owned(),
                agent_client_protocol::v1::schema::ElicitationContentValue::Boolean(true),
            ),
        ]);
        let answer = super::elicitation_answer(&content).expect("valid elicitation answer");
        let decoded: std::collections::HashMap<String, ox_tools::form::FormValue> =
            serde_json::from_value(answer).expect("ox form answer decoding");
        assert!(
            matches!(decoded.get("q0"), Some(ox_tools::form::FormValue::String(value)) if value == "Ada")
        );
        assert!(
            matches!(decoded.get("q1"), Some(ox_tools::form::FormValue::Multiple(values)) if values == &["Rust"])
        );
        assert!(matches!(
            decoded.get("q2"),
            Some(ox_tools::form::FormValue::Boolean(true))
        ));
    }
}
