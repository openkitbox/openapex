//! Negotiated ACP client capabilities and owned client interactions.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CreateElicitationRequest, ElicitationAcceptAction, ElicitationAction, ElicitationContentValue,
    ElicitationFormMode, ElicitationSchema, ElicitationSessionScope, PermissionOption,
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, Error};
use ox_schema::permission::Asked;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::internal;

pub(crate) struct PermissionCall<'a> {
    pub(crate) asked: &'a Asked,
    pub(crate) client_session: &'a str,
    pub(crate) connection: &'a ConnectionTo<Client>,
    pub(crate) cancellation: &'a CancellationToken,
    pub(crate) directory: &'a str,
    pub(crate) settled: &'a Notify,
}

#[derive(Clone, Default)]
pub(crate) struct ClientRequests {
    capabilities: Arc<Mutex<agent_client_protocol::v1::schema::ClientCapabilities>>,
    pending_permissions: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
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

    pub(crate) fn begin_permission(&self, request_id: &str) -> Option<Arc<Notify>> {
        let mut pending = self
            .pending_permissions
            .lock()
            .expect("permission registry poisoned");
        if pending.contains_key(request_id) {
            return None;
        }
        let notification = Arc::new(Notify::new());
        pending.insert(request_id.to_owned(), notification.clone());
        Some(notification)
    }

    pub(crate) fn finish_permission(&self, request_id: &str) {
        self.pending_permissions
            .lock()
            .expect("permission registry poisoned")
            .remove(request_id);
    }

    pub(crate) fn cancel_permission(&self, request_id: &str) -> bool {
        let notification = self
            .pending_permissions
            .lock()
            .expect("permission registry poisoned")
            .remove(request_id);
        let Some(notification) = notification else {
            return false;
        };
        notification.notify_one();
        true
    }

    pub(crate) async fn request_permission(
        &self,
        http: &reqwest::Client,
        base: &str,
        call: PermissionCall<'_>,
    ) -> Result<PermissionResult, Error> {
        let PermissionCall {
            asked,
            client_session,
            connection,
            cancellation,
            directory,
            settled,
        } = call;
        let tool_call_id = asked
            .source
            .as_ref()
            .and_then(|source| serde_json::to_value(source).ok())
            .and_then(|source| source.get("id").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| asked.id.clone());
        let mut fields = ToolCallUpdateFields::new()
            .title(asked.message.as_deref().unwrap_or(&asked.action))
            .status(ToolCallStatus::Pending)
            .locations(permission_locations(&asked.resources));
        if let Some(metadata) = &asked.metadata {
            fields = fields.raw_input(Value::Object(metadata.clone()));
        }
        let request = RequestPermissionRequest::new(
            SessionId::new(client_session),
            ToolCallUpdate::new(ToolCallId::new(tool_call_id), fields),
            permission_options(asked.save.as_deref()),
        );
        let sent = connection.send_request(request);
        let cancellation_handle = sent.cancellation_handle();
        let response = tokio::select! {
            result = sent.block_task() => {
                match result {
                    Ok(response) => response,
                    Err(error) => {
                        let _ = cancellation_handle.cancel();
                        reply_permission(http, base, asked, directory, "reject", None).await?;
                        return Err(error);
                    }
                }
            }
            () = cancellation.cancelled() => {
                let _ = cancellation_handle.cancel();
                reply_permission(http, base, asked, directory, "reject", None).await?;
                return Ok(PermissionResult::Cancelled);
            }
            () = settled.notified() => {
                let _ = cancellation_handle.cancel();
                reply_permission(http, base, asked, directory, "reject", None).await?;
                return Ok(PermissionResult::Cancelled);
            }
        };
        match response.outcome {
            RequestPermissionOutcome::Cancelled => {
                reply_permission(http, base, asked, directory, "reject", None).await?;
                Ok(PermissionResult::Cancelled)
            }
            RequestPermissionOutcome::Selected(selected) => {
                let option = selected.option_id.0.as_ref();
                let decision = match option {
                    "once" => "once",
                    "always" => "always",
                    _ => "reject",
                };
                let message = if decision == "reject" {
                    selected
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                } else {
                    None
                };
                reply_permission(http, base, asked, directory, decision, message.as_deref())
                    .await?;
                Ok(PermissionResult::Selected(decision.to_owned()))
            }
            _ => {
                reply_permission(http, base, asked, directory, "reject", None).await?;
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

fn permission_locations(resources: &[String]) -> Vec<ToolCallLocation> {
    resources
        .iter()
        .filter(|resource| Path::new(resource).is_absolute())
        .map(ToolCallLocation::new)
        .collect()
}

fn permission_options(save: Option<&[String]>) -> Vec<PermissionOption> {
    let mut options = vec![PermissionOption::new(
        "once",
        "Allow once",
        PermissionOptionKind::AllowOnce,
    )];
    if save.is_some_and(|save| !save.is_empty()) {
        options.push(PermissionOption::new(
            "always",
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ));
    }
    options.push(PermissionOption::new(
        "reject",
        "Reject",
        PermissionOptionKind::RejectOnce,
    ));
    options
}

async fn reply_permission(
    http: &reqwest::Client,
    base: &str,
    asked: &Asked,
    directory: &str,
    decision: &str,
    message: Option<&str>,
) -> Result<(), Error> {
    let mut body = json!({"decision": decision});
    if let Some(message) = message {
        body["message"] = json!(message);
    }
    let response = http
        .post(format!(
            "{base}/api/session/{}/permission/{}/reply",
            asked.session_id, asked.id
        ))
        .header("x-opencode-directory", directory)
        .json(&body)
        .send()
        .await
        .map_err(internal)?;
    if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    Err(internal(format!(
        "permission reply returned {}",
        response.status()
    )))
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
