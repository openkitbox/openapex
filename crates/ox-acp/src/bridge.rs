//! The bridge: state + behavior of the `ox acp` translator.
//!
//! One [`Bridge`] per stdio connection maps ACP sessions to ox sessions
//! (identity mapping: the ox id IS the ACP id) and turns each `session/prompt`
//! into a subscribed-then-POSTed turn whose SSE frames stream out as ACP
//! updates until a turn-level terminator arrives.

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, Implementation,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
};
use ox_schema::permission::Asked;
use ox_schema::support::ModelRef;
use serde::Deserialize;
use serde_json::{Value, json};

use super::client_requests;
use super::client_requests::PermissionCall;
use super::config_options::{Catalog, Change};
use super::internal;
use super::session::{SessionInfo, Sessions, Turn, setup_directory, validate_id};
use super::stream::{self, Frame};
use super::translate;
use super::wire_error;

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

/// How a turn ended: a stop reason for the editor, or ox's error verbatim.
enum TurnEnd {
    Settle(StopReason),
    Failed(String),
}

/// Shared bridge state: cheap to clone into every ACP handler.
#[derive(Clone)]
pub struct Bridge {
    pub(super) http: reqwest::Client,
    pub(super) base: Arc<str>,
    pub(super) sessions: Sessions,
    pub(super) client_requests: super::client_requests::ClientRequests,
}

impl Bridge {
    pub fn new(http: reqwest::Client, base: String) -> Self {
        Self {
            http,
            base: base.into(),
            sessions: Sessions::default(),
            client_requests: super::client_requests::ClientRequests::default(),
        }
    }

    /// `initialize` — echo the negotiated protocol version and advertise the
    /// v1 surface (load, image + embedded context prompts, session lifecycle;
    /// no MCP).
    pub fn initialize(&self, request: InitializeRequest) -> InitializeResponse {
        self.client_requests
            .set_capabilities(request.client_capabilities);
        // Only ACP v1 is implemented; answering V1 (never echoing a draft v2)
        // tells a newer client to fall back.
        InitializeResponse::new(ProtocolVersion::V1)
            .agent_info(Implementation::new("ox", env!("CARGO_PKG_VERSION")))
            .agent_capabilities(
                AgentCapabilities::new()
                    .load_session(true)
                    .session_capabilities(super::lifecycle::capabilities())
                    .prompt_capabilities(
                        PromptCapabilities::new().image(true).embedded_context(true),
                    ),
            )
    }

    /// `session/new` — create the ox session scoped to the editor's `cwd`.
    pub async fn new_session(
        &self,
        request: NewSessionRequest,
    ) -> Result<NewSessionResponse, acp::Error> {
        let directory = setup_directory(
            &request.cwd,
            &request.additional_directories,
            &request.mcp_servers,
            request.system_prompt.as_ref(),
        )?;
        let request = self
            .http
            .post(format!("{}/api/session", self.base))
            .json(&json!({}));
        let info: SessionInfo = Self::send_data(request, &directory).await?;
        validate_id(&info.id)?;
        let directory = info.directory_header()?;
        let (catalog, model) = self.configuration(&info, &directory).await?;
        let response = NewSessionResponse::new(info.id.clone())
            .modes(catalog.modes(info.agent.as_deref()))
            .config_options(catalog.options(model.as_ref(), info.agent.as_deref()));
        self.sessions.bind(info.id, directory)?;
        Ok(response)
    }

    /// `session/load` — the editor reopened a thread: verify the session,
    /// replay its transcript as ACP updates, then answer with the mode state.
    /// The replay reads through `message.list`, which applies the mctx
    /// dropped-content overlay, so the editor sees the same transcript the
    /// model saw.
    pub async fn load_session(
        &self,
        request: LoadSessionRequest,
        connection: &acp::v1::ConnectionTo<acp::Client>,
    ) -> Result<LoadSessionResponse, acp::Error> {
        let directory = setup_directory(
            &request.cwd,
            &request.additional_directories,
            &request.mcp_servers,
            request.system_prompt.as_ref(),
        )?;
        let restored = self
            .restore_session(&request.session_id, &directory, Some(connection))
            .await?;
        Ok(LoadSessionResponse::new()
            .modes(restored.modes)
            .config_options(restored.options))
    }

    pub(super) async fn replay(
        &self,
        session: &SessionId,
        directory: &str,
        connection: &acp::v1::ConnectionTo<acp::Client>,
    ) -> Result<(), acp::Error> {
        // Walk every page (the cursor embeds the order, so later pages send
        // only `limit` + `cursor` — src/api/session.rs rejects the pair).
        let mut cursor: Option<String> = None;
        let mut first_page = true;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > 500 {
                return Err(internal("message history exceeds 500 pages"));
            }
            let mut url = format!("{}/api/session/{}/message?limit=100", self.base, session.0);
            if let Some(cursor) = &cursor {
                url.push_str("&cursor=");
                url.extend(url::form_urlencoded::byte_serialize(cursor.as_bytes()));
            } else if first_page {
                url.push_str("&order=asc");
            }
            let page: Value = Self::send(self.http.get(url), directory)
                .await?
                .json()
                .await
                .map_err(internal)?;
            first_page = false;
            let messages = page
                .get("data")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            for message in messages {
                for update in translate::replay_message(message) {
                    let notification = SessionNotification::new(session.clone(), update);
                    connection
                        .send_notification(notification)
                        .map_err(internal)?;
                }
            }
            cursor = page
                .pointer("/cursor/next")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() || messages.is_empty() {
                break;
            }
        }

        Ok(())
    }

    pub(super) async fn send(
        request: reqwest::RequestBuilder,
        directory: &str,
    ) -> Result<reqwest::Response, acp::Error> {
        Self::checked(
            request
                .header("x-opencode-directory", directory)
                .send()
                .await
                .map_err(internal)?,
        )
        .await
    }

    pub(super) async fn checked(
        response: reqwest::Response,
    ) -> Result<reqwest::Response, acp::Error> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.map_err(internal)?;
            return Err(wire_error(status.as_u16(), &body));
        }
        Ok(response)
    }

    pub(super) async fn send_data<T: serde::de::DeserializeOwned>(
        request: reqwest::RequestBuilder,
        directory: &str,
    ) -> Result<T, acp::Error> {
        let envelope: Data<T> = Self::send(request, directory)
            .await?
            .json()
            .await
            .map_err(internal)?;
        Ok(envelope.data)
    }

    pub(super) async fn session_info(
        &self,
        id: &str,
        directory: &str,
    ) -> Result<SessionInfo, acp::Error> {
        validate_id(id)?;
        let info: SessionInfo = Self::send_data(
            self.http.get(format!("{}/api/session/{id}", self.base)),
            directory,
        )
        .await?;
        if info.id != id {
            return Err(internal("ox returned a different session"));
        }
        Ok(info)
    }

    async fn catalog(&self, directory: &str) -> Result<Catalog, acp::Error> {
        let (agents, models) = tokio::try_join!(
            Self::send_data(self.http.get(format!("{}/api/agent", self.base)), directory),
            Self::send_data(self.http.get(format!("{}/api/model", self.base)), directory),
        )?;
        Ok(Catalog { agents, models })
    }

    async fn effective_model(
        &self,
        info: &SessionInfo,
        directory: &str,
    ) -> Result<Option<ModelRef>, acp::Error> {
        if let Some(model) = &info.model {
            return Ok(Some(model.clone()));
        }
        Self::send_data(
            self.http.get(format!("{}/api/model/default", self.base)),
            directory,
        )
        .await
    }

    pub(super) async fn configuration(
        &self,
        info: &SessionInfo,
        directory: &str,
    ) -> Result<(Catalog, Option<ModelRef>), acp::Error> {
        // Listing can refresh the catalog; model/default reads its snapshot.
        let catalog = self.catalog(directory).await?;
        let model = self.effective_model(info, directory).await?;
        Ok((catalog, model))
    }

    /// `available_commands_update`: push ox's command catalogue (currently
    /// empty — emitted anyway so slash-command support lights up when ox
    /// grows one).
    pub(crate) async fn send_available_commands(
        &self,
        session: &SessionId,
        connection: &acp::v1::ConnectionTo<acp::Client>,
    ) -> Result<(), acp::Error> {
        let mut commands = Vec::new();
        let Ok(directory) = self.sessions.directory(&session.0) else {
            return Ok(());
        };
        let request = self.http.get(format!("{}/api/command", self.base));
        match Self::send_data::<Vec<Value>>(request, &directory).await {
            Ok(data) => {
                for command in &data {
                    if let (Some(name), Some(description)) = (
                        command.get("name").and_then(Value::as_str),
                        command.get("description").and_then(Value::as_str),
                    ) {
                        commands.push(AvailableCommand::new(name, description));
                    }
                }
            }
            Err(error) => return Err(error),
        }
        let update = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands));
        let notification = SessionNotification::new(session.clone(), update);
        connection
            .send_notification(notification)
            .map_err(internal)?;
        Ok(())
    }

    /// `session/set_mode` — switch the same ox agent as the mode config option.
    pub async fn set_session_mode(
        &self,
        request: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse, acp::Error> {
        let session = &request.session_id.0;
        let directory = self.sessions.directory(session)?;
        let agents = Self::send_data(
            self.http.get(format!("{}/api/agent", self.base)),
            &directory,
        )
        .await?;
        let catalog = Catalog {
            agents,
            models: Vec::new(),
        };
        let agent = catalog.agent(&request.mode_id.0)?;
        self.apply_change(session, &directory, Change::Agent(agent))
            .await?;
        Ok(SetSessionModeResponse::new())
    }

    pub async fn set_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, acp::Error> {
        let value = request
            .value
            .as_value_id()
            .ok_or_else(|| acp::Error::invalid_params().data("expected a select value"))?;
        let session = &request.session_id.0;
        let directory = self.sessions.directory(session)?;
        let info = self.session_info(session, &directory).await?;
        let directory = info.directory_header()?;
        let (catalog, model) = self.configuration(&info, &directory).await?;
        let change = catalog.resolve(model.as_ref(), &request.config_id.0, &value.0)?;
        self.apply_change(session, &directory, change).await?;
        let info = self.session_info(session, &directory).await?;
        let model = self.effective_model(&info, &directory).await?;
        Ok(SetSessionConfigOptionResponse::new(
            catalog.options(model.as_ref(), info.agent.as_deref()),
        ))
    }

    async fn apply_change(
        &self,
        session: &str,
        directory: &str,
        change: Change<'_>,
    ) -> Result<(), acp::Error> {
        let (endpoint, payload) = match change {
            Change::Agent(agent) => ("agent", json!({"agent": agent})),
            Change::Model(model) => ("model", json!({"model": model})),
        };
        let request = self
            .http
            .post(format!("{}/api/session/{session}/{endpoint}", self.base))
            .json(&payload);
        Self::send(request, directory).await?;
        Ok(())
    }

    /// `session/cancel` — wake the in-flight turn and interrupt ox. The turn
    /// loop sees the interrupt frame and answers `StopReason::Cancelled`.
    pub async fn cancel(&self, session_id: &SessionId) {
        let session = &session_id.0;
        if let Ok(directory) = self.sessions.cancel(session)
            && let Err(error) = self.post_interrupt(session, &directory).await
        {
            tracing::warn!(session = %session, error = %error, "ACP cancel interrupt failed");
        }
    }

    /// Run one prompt turn: subscribe → POST → translate → settle.
    ///
    /// `Err` carries ox's HTTP error verbatim for the JSON-RPC error reply.
    pub(crate) async fn run_prompt(
        &self,
        request: &PromptRequest,
        connection: &acp::v1::ConnectionTo<acp::Client>,
    ) -> Result<StopReason, acp::Error> {
        let session = &request.session_id.0;
        let directory = self.sessions.directory(session)?;
        let turn = self.sessions.begin_turn(session)?;
        let _finished = turn.finished.clone().drop_guard();
        let turn_token = self.client_requests.request_token();
        let _turn_cleanup = turn_token.clone().drop_guard();
        let result = self
            .drive_turn(session, &directory, request, connection, &turn, &turn_token)
            .await;
        turn_token.cancel();
        result
    }

    /// The turn loop: subscribe first (the no-dup replay/live contract makes
    /// the ordering lossless), then POST, then translate frames until a
    /// turn-level terminator. A dropped stream reconnects once with the last
    /// durable seq (the server's documented repair path); ephemeral deltas
    /// may gap across a reconnect, matching upstream's volatile contract.
    async fn drive_turn(
        &self,
        session: &str,
        directory: &str,
        request: &PromptRequest,
        connection: &acp::v1::ConnectionTo<acp::Client>,
        turn: &Turn,
        turn_token: &tokio_util::sync::CancellationToken,
    ) -> Result<StopReason, acp::Error> {
        let mut stream = {
            let _admission_done = turn.admission_done.clone().drop_guard();
            let stream = stream::open(&self.http, &self.base, Some(session), None).await?;
            self.post_prompt(session, directory, request).await?;
            stream
        };

        let mut tool_names = translate::ToolNames::new();
        let mut compaction_ids = translate::CompactionIds::new();
        let mut last_seq: Option<i64> = None;
        let mut reconnects = 0usize;
        let mut interrupt_requested = false;

        loop {
            tokio::select! {
                    _ = turn.detached.cancelled(), if !interrupt_requested => {
                        // The cancel handler also POSTs interrupt; doing it here
                        // too keeps the ordering deterministic for this turn.
                        interrupt_requested = true;
                        self.post_interrupt(session, directory).await?;
                    }
                    _ = turn.cancel.notified(), if !interrupt_requested => {
                        interrupt_requested = true;
                        self.post_interrupt(session, directory).await?;
                    }
                    frame = stream.next() => match frame {
                        Err(error) => {
                            return Err(internal(format!("event stream frame failed: {error}")));
                        }
                        Ok(None) => {
                            reconnects += 1;
                            if reconnects > 2 {
                                tracing::warn!(session, "event stream ended twice mid-turn");
                                return Err(internal("event stream ended before a terminal turn event"));
                            }
                            tracing::info!(session, reconnects, "event stream ended; reconnecting");
                            // Reconnect through the documented replay path when a
                            // durable cursor is known; otherwise fall back to the
                            // live-only global stream.
                            stream = stream::open(&self.http, &self.base, Some(session), last_seq).await?;
                        }
                        Ok(Some(frame)) => {
                            // A reply settled elsewhere (including a 300s server
                            // timeout) withdraws the matching ACP dialog. Handle
                            // it before `accept_frame`: child replies carry only
                            // their own session ID, not the parent's.
                            if frame.kind == "permission.replied" {
                                if let Some(request_id) = frame
                                    .data
                                    .get("requestID")
                                    .and_then(Value::as_str)
                                {
                                    self.client_requests.cancel_permission(request_id);
                                }
                                continue;
                            }
                            if !accept_frame(&mut last_seq, &frame, session) {
                                continue;
                            }
                            if frame.kind == "permission.asked" {
                                match serde_json::from_value::<Asked>(frame.data.clone()) {
                                    Ok(asked) => {
                                        let Some(settled) =
                                            self.client_requests.begin_permission(&asked.id)
                                        else {
                                            continue;
                                        };
                                        let requests = self.client_requests.clone();
                                        let http = self.http.clone();
                                        let base = self.base.clone();
                                        let connection_for_task = connection.clone();
                                        let directory_for_task = directory.to_owned();
                                        let cancellation = turn_token.clone();
                                        let requested = asked.clone();
                                        let client_session = session.to_owned();
                                        // Use a runtime task, not `ConnectionTo::spawn`:
                                        // an editor disconnect must not abort the
                                        // fail-closed rejection POST.
                                        tokio::spawn(async move {
                                            let result = requests
                                                .request_permission(
                                                    &http,
                                                    &base,
                                                    PermissionCall {
                                                        asked: &requested,
                                                        client_session: &client_session,
                                                        connection: &connection_for_task,
                                                        cancellation: &cancellation,
                                                        directory: &directory_for_task,
                                                        settled: &settled,
                                                    },
                                                )
                                                .await;
                                            requests.finish_permission(&requested.id);
                                            if let Err(error) = result {
                                                tracing::debug!(error = %error, "ACP permission request failed closed");
                                            }
                                            Ok::<(), acp::Error>(())
                                        });
                                    }
                                    Err(error) => {
                                        tracing::warn!(error = %error, "invalid permission.asked event");
                                    }
                                }
                            }
                            if frame.kind == "form.created"
                                && frame.data.pointer("/form/fields").is_none()
                            {
                                let form = frame.data.get("form").cloned().unwrap_or(Value::Null);
                                if !self.client_requests.supports_form_elicitation() {
                                    if let Err(error) = client_requests::ClientRequests::settle_form_for_task(
                                        &self.http,
                                        &self.base,
                                        &form,
                                        directory,
                                    ).await {
                                        tracing::warn!(error = %error, "unsupported elicitation form cleanup failed");
                                    }
                                    continue;
                                }
                                let requests = self.client_requests.clone();
                                let http = self.http.clone();
                                let base = self.base.clone();
                                let connection_for_task = connection.clone();
                                let directory = directory.to_owned();
                                let cancellation = turn_token.clone();
                                let form_for_cleanup = form.clone();
                                let http_for_cleanup = http.clone();
                                let base_for_cleanup = base.clone();
                                let directory_for_cleanup = directory.clone();
                                let spawn_result = connection.clone().spawn(async move {
                                    let result = requests
                                        .request_elicitation(
                                            &http,
                                            &base,
                                            &form,
                                            &connection_for_task,
                                            &cancellation,
                                            &directory,
                                        )
                                        .await;
                                    if let Err(error) = result {
                                        cleanup_failed_form(
                                            &http,
                                            &base,
                                            &form,
                                            &directory,
                                            &error,
                                            "ACP elicitation request failed closed",
                                        )
                                        .await;
                                    }
                                    Ok(())
                                });
                                if let Err(error) = spawn_result {
                                    cleanup_failed_form(
                                        &http_for_cleanup,
                                        &base_for_cleanup,
                                        &form_for_cleanup,
                                        &directory_for_cleanup,
                                        &error,
                                        "ACP elicitation task failed to spawn",
                                    )
                                    .await;
                                }
                            }
            match self.consume(
                                &frame,
                                &mut tool_names,
                                &mut compaction_ids,
                                request,
                                connection,
                            )? {
                                Some(TurnEnd::Settle(stop)) => return Ok(stop),
                                Some(TurnEnd::Failed(message)) => {
                                    return Err(acp::Error::internal_error().data(message));
                                }
                                None => {}
                            }
                        }
                    }
                }
        }
    }

    /// Translate one frame; returns `Some(end)` on a turn terminator.
    fn consume(
        &self,
        frame: &Frame,
        tool_names: &mut translate::ToolNames,
        compaction_ids: &mut translate::CompactionIds,
        request: &PromptRequest,
        connection: &acp::v1::ConnectionTo<acp::Client>,
    ) -> Result<Option<TurnEnd>, acp::Error> {
        if frame.kind == "session.tool.input.started" {
            translate::learn_tool_name(tool_names, &frame.data);
        }
        for update in translate::translate_with_compactions(
            &frame.kind,
            &frame.data,
            tool_names,
            compaction_ids,
        ) {
            if !self.client_requests.allows_update(&update) {
                continue;
            }
            let notification = SessionNotification::new(request.session_id.clone(), update);
            connection
                .send_notification(notification)
                .map_err(internal)?;
        }
        Ok(match frame.kind.as_str() {
            "session.execution.succeeded" => Some(TurnEnd::Settle(StopReason::EndTurn)),
            // ox's turn-level failure carries the provider/tool error; it goes
            // back to the editor verbatim as a JSON-RPC error.
            "session.execution.failed" => Some(TurnEnd::Failed(
                frame
                    .data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("ox turn failed")
                    .to_owned(),
            )),
            "session.execution.interrupted" => Some(TurnEnd::Settle(StopReason::Cancelled)),
            _ => None,
        })
    }

    async fn post_prompt(
        &self,
        session: &str,
        directory: &str,
        request: &PromptRequest,
    ) -> Result<(), acp::Error> {
        let body = prompt_body(request)?;
        let response = self
            .http
            .post(format!("{}/api/session/{}/prompt", self.base, session))
            .header("x-opencode-directory", directory)
            .json(&body)
            .send()
            .await
            .map_err(internal)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.map_err(internal)?;
            return Err(wire_error(status.as_u16(), &text));
        }
        Ok(())
    }

    async fn post_interrupt(&self, session: &str, directory: &str) -> Result<(), acp::Error> {
        let url = format!("{}/api/session/{}/interrupt", self.base, session);
        Self::send(self.http.post(url), directory).await?;
        Ok(())
    }
}

async fn cleanup_failed_form(
    http: &reqwest::Client,
    base: &str,
    form: &Value,
    directory: &str,
    error: &acp::Error,
    message: &'static str,
) {
    if let Err(settle_error) =
        client_requests::ClientRequests::settle_form_for_task(http, base, form, directory).await
    {
        tracing::warn!(error = %settle_error, %message, "elicitation form cleanup failed");
    }
    tracing::debug!(error = %error, %message);
}

/// Translate an ACP prompt into ox's `PromptBody` JSON (`src/api/session.rs`):
/// text blocks concatenate into `text`; images become upstream `PromptInput`
/// file inputs `{ "uri": "data:<mime>;base64,<data>" }` (the server
/// materializes them). Resource links are rejected because ox cannot safely
/// materialize arbitrary ACP URIs without downloading or changing content.
fn prompt_body(request: &PromptRequest) -> Result<Value, acp::Error> {
    let mut text = String::new();
    let mut files = Vec::new();
    for block in &request.prompt {
        match super::content::prompt_block(block)? {
            super::content::PromptBlock::Text(value) => text.push_str(value),
            super::content::PromptBlock::Image { data, mime_type } => {
                files.push(json!({
                    "uri": format!("data:{mime_type};base64,{data}"),
                }));
            }
            super::content::PromptBlock::ResourceLink => {
                return Err(internal("resource link prompts are not supported by ox"));
            }
            super::content::PromptBlock::EmbeddedText(value) => {
                text.push_str(value);
                text.push('\n');
            }
        }
    }
    let mut body = json!({ "text": text });
    if !files.is_empty() {
        body["files"] = json!(files);
    }
    Ok(body)
}

fn accept_frame(last_seq: &mut Option<i64>, frame: &Frame, session: &str) -> bool {
    // The live-only subscription is global: other sessions' traffic passes
    // through and is dropped before it can advance the reconnect cursor.
    let frame_session = frame
        .data
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| {
            frame
                .data
                .pointer("/form/sessionID")
                .and_then(Value::as_str)
        });
    // V2 child asks carry the child session ID and the calling family's ID in
    // `metadata.parentID`; dropping them would hang the parent's approval.
    let child_of = frame.kind == "permission.asked"
        && frame
            .data
            .pointer("/metadata/parentID")
            .and_then(Value::as_str)
            == Some(session);
    if frame_session != Some(session) && !child_of {
        return false;
    }
    match (frame.seq, frame.aggregate_id.as_deref()) {
        (Some(seq), Some(aggregate_id)) if aggregate_id == session => *last_seq = Some(seq),
        (Some(_), _) | (None, Some(_)) => return false,
        (None, None) => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::{Context, Poll};

    use salvo::conn::Listener as _;
    use salvo::prelude::{Depot, Json, Request, Response, Router, Server, StatusCode};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use super::{
        Bridge, PromptRequest, Value, accept_frame, acp, cleanup_failed_form, internal, json,
        prompt_body,
    };
    use crate::stream::Frame;
    use acp::v1::schema::{ContentBlock, ImageContent, TextContent};

    #[test]
    fn reconnect_cursor_ignores_other_sessions() {
        let mut last_seq = None;
        let other_session = Frame {
            seq: Some(99),
            aggregate_id: None,
            kind: "session.updated".to_owned(),
            data: json!({"sessionID": "other"}),
        };
        let target_session = Frame {
            seq: Some(7),
            aggregate_id: Some("target".to_owned()),
            kind: "session.updated".to_owned(),
            data: json!({"sessionID": "target"}),
        };

        assert!(!accept_frame(&mut last_seq, &other_session, "target"));
        assert_eq!(last_seq, None);

        let wrong_aggregate = Frame {
            seq: Some(8),
            aggregate_id: Some("other".to_owned()),
            kind: "session.updated".to_owned(),
            data: json!({"sessionID": "target"}),
        };
        assert!(!accept_frame(&mut last_seq, &wrong_aggregate, "target"));
        assert_eq!(last_seq, None);

        assert!(accept_frame(&mut last_seq, &target_session, "target"));
        assert_eq!(last_seq, Some(7));

        for (kind, data) in [
            (
                "session.text.delta",
                json!({"sessionID": "target", "delta": "hi"}),
            ),
            (
                "session.reasoning.delta",
                json!({"sessionID": "target", "delta": "thinking"}),
            ),
            ("form.created", json!({"form": {"sessionID": "target"}})),
        ] {
            let ephemeral = Frame {
                seq: None,
                aggregate_id: None,
                kind: kind.to_owned(),
                data,
            };
            assert!(accept_frame(&mut last_seq, &ephemeral, "target"));
        }
        let missing_aggregate = Frame {
            seq: Some(9),
            aggregate_id: None,
            kind: "session.updated".to_owned(),
            data: json!({"sessionID": "target"}),
        };
        assert!(!accept_frame(&mut last_seq, &missing_aggregate, "target"));
        assert_eq!(last_seq, Some(7));

        let aggregate_without_sequence = Frame {
            seq: None,
            aggregate_id: Some("target".to_owned()),
            kind: "session.updated".to_owned(),
            data: json!({"sessionID": "target"}),
        };
        assert!(!accept_frame(
            &mut last_seq,
            &aggregate_without_sequence,
            "target"
        ));
        assert_eq!(last_seq, Some(7));
    }

    struct State {
        selection: Mutex<Value>,
        writes: Mutex<Vec<Value>>,
    }

    #[salvo::handler]
    async fn session(req: &Request, depot: &mut Depot, res: &mut Response) {
        let state = depot.get_typed::<Arc<State>>().expect("mock state");
        if req.method() == salvo::http::Method::POST {
            assert_eq!(req.headers()["x-opencode-directory"], "/requested");
            state.writes.lock().expect("mock writes").push(json!("new"));
        } else {
            assert_eq!(req.headers()["x-opencode-directory"], "/work");
        }
        res.render(Json(
            json!({"data": *state.selection.lock().expect("mock selection")}),
        ));
    }

    #[salvo::handler]
    async fn list_sessions(req: &Request, res: &mut Response) {
        assert_eq!(req.headers()["x-opencode-directory"], "/work");
        let cursor = req.uri().query().unwrap_or_default().contains("cursor=");
        if cursor {
            res.render(Json(json!({
                "data": [{"id": "ses_two", "location": {"directory": "/work"}, "title": "Second", "time": {"updated": 1700000001000i64}}],
                "cursor": {"next": null}
            })));
        } else {
            res.render(Json(json!({
                "data": [{"id": "ses_one", "location": {"directory": "/work"}, "title": "First", "time": {"updated": 1700000000000i64}}],
                "cursor": {"next": "page-two"}
            })));
        }
    }

    #[salvo::handler]
    async fn interrupt(res: &mut Response) {
        res.status_code(StatusCode::NO_CONTENT);
    }

    #[salvo::handler]
    async fn wait(res: &mut Response) {
        res.status_code(StatusCode::NO_CONTENT);
    }

    struct DeleteState {
        attempts: AtomicUsize,
    }

    #[salvo::handler]
    async fn remove_session(req: &Request, depot: &mut Depot, res: &mut Response) {
        let state = depot.get_typed::<Arc<DeleteState>>().expect("delete state");
        assert_eq!(req.method(), salvo::http::Method::DELETE);
        if state.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        } else {
            res.status_code(StatusCode::NO_CONTENT);
        }
    }

    #[salvo::handler]
    async fn stable_remove_session(res: &mut Response) {
        res.status_code(StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn delete_failure_retains_link_for_retry() -> anyhow::Result<()> {
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await?;
        let addr = acceptor.local_addr()?;
        let delete_state = Arc::new(DeleteState {
            attempts: AtomicUsize::new(0),
        });
        let router = Router::with_path("api")
            .hoop(salvo::affix_state::inject(delete_state))
            .push(Router::with_path("session/ses_one").delete(remove_session))
            .push(Router::with_path("session/ses_one/interrupt").post(interrupt))
            .push(Router::with_path("experimental/session/ses_one/wait").post(wait));
        let server = tokio::spawn(Server::new(acceptor).serve(router));
        let bridge = Bridge::new(reqwest::Client::new(), format!("http://{addr}"));
        bridge
            .sessions
            .bind("ses_one".to_owned(), "/work".to_owned())?;
        let first = bridge
            .delete_session(acp::v1::schema::DeleteSessionRequest::new("ses_one"))
            .await;
        assert!(first.is_err(), "first DELETE must fail");
        assert_eq!(bridge.sessions.directory("ses_one")?, "/work");
        bridge
            .delete_session(acp::v1::schema::DeleteSessionRequest::new("ses_one"))
            .await?;
        assert!(bridge.sessions.directory("ses_one").is_err());
        server.abort();
        Ok(())
    }

    struct TaggedNotFoundState {
        interrupt_attempts: AtomicUsize,
    }

    #[salvo::handler]
    async fn tagged_not_found(req: &Request, depot: &mut Depot, res: &mut Response) {
        let state = depot
            .get_typed::<Arc<TaggedNotFoundState>>()
            .expect("tagged delete state");
        assert_eq!(req.method(), salvo::http::Method::DELETE);
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(json!({"_tag": "SessionNotFoundError"})));
        let _ = state;
    }

    #[salvo::handler]
    async fn counted_interrupt(depot: &mut Depot, res: &mut Response) {
        let state = depot
            .get_typed::<Arc<TaggedNotFoundState>>()
            .expect("tagged delete state");
        state.interrupt_attempts.fetch_add(1, Ordering::SeqCst);
        res.status_code(StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn tagged_not_found_finishes_link_before_idempotent_success() -> anyhow::Result<()> {
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await?;
        let addr = acceptor.local_addr()?;
        let state = Arc::new(TaggedNotFoundState {
            interrupt_attempts: AtomicUsize::new(0),
        });
        let router = Router::with_path("api")
            .hoop(salvo::affix_state::inject(state.clone()))
            .push(Router::with_path("session/ses_one").delete(tagged_not_found))
            .push(Router::with_path("session/ses_one/interrupt").post(counted_interrupt))
            .push(Router::with_path("experimental/session/ses_one/wait").post(wait));
        let server = tokio::spawn(Server::new(acceptor).serve(router));
        let bridge = Bridge::new(reqwest::Client::new(), format!("http://{addr}"));
        bridge
            .sessions
            .bind("ses_one".to_owned(), "/work".to_owned())?;
        bridge
            .delete_session(acp::v1::schema::DeleteSessionRequest::new("ses_one"))
            .await?;
        let attempts_after_delete = state.interrupt_attempts.load(Ordering::SeqCst);
        assert!(attempts_after_delete > 0);
        bridge
            .close_session(acp::v1::schema::CloseSessionRequest::new("ses_one"))
            .await?;
        assert_eq!(
            state.interrupt_attempts.load(Ordering::SeqCst),
            attempts_after_delete,
            "close after idempotent delete must not call remote interrupt"
        );
        server.abort();
        Ok(())
    }

    #[salvo::handler]
    async fn failed_interrupt(res: &mut Response) {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn interrupt_failure_is_returned_to_the_turn() -> anyhow::Result<()> {
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await?;
        let addr = acceptor.local_addr()?;
        let router = Router::with_path("api")
            .push(Router::with_path("session/ses_one/interrupt").post(failed_interrupt));
        let server = tokio::spawn(Server::new(acceptor).serve(router));
        let bridge = Bridge::new(reqwest::Client::new(), format!("http://{addr}"));
        let error = bridge
            .post_interrupt("ses_one", "/work")
            .await
            .expect_err("interrupt HTTP failure must reach the turn");
        assert!(error.to_string().contains("500"));
        server.abort();
        Ok(())
    }

    struct FormCleanupState {
        cancellations: AtomicUsize,
    }

    #[salvo::handler]
    async fn cancel_form(req: &Request, depot: &mut Depot, res: &mut Response) {
        assert_eq!(req.method(), salvo::http::Method::DELETE);
        let state = depot
            .get_typed::<Arc<FormCleanupState>>()
            .expect("form cleanup state");
        state.cancellations.fetch_add(1, Ordering::SeqCst);
        res.status_code(StatusCode::NO_CONTENT);
    }

    async fn assert_failed_elicitation_is_cleaned_up(error: acp::Error) -> anyhow::Result<()> {
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await?;
        let addr = acceptor.local_addr()?;
        let state = Arc::new(FormCleanupState {
            cancellations: AtomicUsize::new(0),
        });
        let router = Router::with_path("api")
            .hoop(salvo::affix_state::inject(state.clone()))
            .push(Router::with_path("session/ses_one/form/form_one").delete(cancel_form));
        let server = tokio::spawn(Server::new(acceptor).serve(router));
        let form = json!({"id": "form_one", "sessionID": "ses_one"});
        cleanup_failed_form(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            &form,
            "/work",
            &error,
            "test failed elicitation",
        )
        .await;
        assert_eq!(
            state.cancellations.load(Ordering::SeqCst),
            1,
            "failed elicitation must cancel its pending form"
        );
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn elicitation_task_error_cleans_up_form() -> anyhow::Result<()> {
        assert_failed_elicitation_is_cleaned_up(internal("block_task failed")).await
    }

    #[tokio::test]
    async fn elicitation_spawn_error_cleans_up_form() -> anyhow::Result<()> {
        assert_failed_elicitation_is_cleaned_up(internal("spawn failed")).await
    }

    #[test]
    fn initialize_does_not_advertise_unimplemented_mcp() -> anyhow::Result<()> {
        let bridge = Bridge::new(reqwest::Client::new(), "http://127.0.0.1:1".to_owned());
        let response = bridge.initialize(acp::v1::schema::InitializeRequest::new(
            acp::schema::ProtocolVersion::V1,
        ));
        let value = serde_json::to_value(response.agent_capabilities)?;
        assert_eq!(value.pointer("/mcpCapabilities/http"), Some(&json!(false)));
        assert_eq!(value.pointer("/mcpCapabilities/sse"), Some(&json!(false)));
        assert!(value.pointer("/mcpCapabilities/acp").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn per_session_mcp_is_rejected_before_session_creation() -> anyhow::Result<()> {
        let bridge = Bridge::new(reqwest::Client::new(), "http://127.0.0.1:1".to_owned());
        let request = acp::v1::schema::NewSessionRequest::new("/work").mcp_servers(vec![
            acp::v1::schema::McpServer::Http(acp::v1::schema::McpServerHttp::new(
                "remote",
                "https://example.test/mcp",
            )),
        ]);
        let error = bridge
            .new_session(request)
            .await
            .expect_err("per-session MCP must fail closed");
        assert!(error.to_string().contains("session-local tool isolation"));
        Ok(())
    }

    #[test]
    fn initialize_persists_client_capabilities() {
        let bridge = Bridge::new(reqwest::Client::new(), "http://127.0.0.1:1".to_owned());
        let request = acp::v1::schema::InitializeRequest::new(acp::schema::ProtocolVersion::V1)
            .client_capabilities(
                acp::v1::schema::ClientCapabilities::new().elicitation(
                    acp::v1::schema::ElicitationCapabilities::new()
                        .form(acp::v1::schema::ElicitationFormCapabilities::new()),
                ),
            );
        bridge.initialize(request);
        assert!(bridge.client_requests.supports_form_elicitation());
    }

    #[tokio::test]
    async fn lifecycle_requests_are_stable_and_resume_does_not_replay() -> anyhow::Result<()> {
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await?;
        let addr = acceptor.local_addr()?;
        let router = Router::with_path("api")
            .push(Router::with_path("session").get(list_sessions))
            .push(Router::with_path("session/ses_one").delete(stable_remove_session))
            .push(Router::with_path("session/ses_one/interrupt").post(interrupt))
            .push(Router::with_path("experimental/session/ses_one/wait").post(wait));
        let server = tokio::spawn(Server::new(acceptor).serve(router));
        let bridge = Bridge::new(reqwest::Client::new(), format!("http://{addr}"));
        let first = bridge
            .list_sessions(acp::v1::schema::ListSessionsRequest::new().cwd("/work"))
            .await?;
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.next_cursor.as_deref(), Some("page-two"));
        assert_eq!(
            first.sessions[0].updated_at.as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        let second = bridge
            .list_sessions(
                acp::v1::schema::ListSessionsRequest::new()
                    .cwd("/work")
                    .cursor("page-two"),
            )
            .await?;
        assert_eq!(second.sessions[0].session_id.0.as_ref(), "ses_two");
        let close = bridge
            .close_session(acp::v1::schema::CloseSessionRequest::new("ses_one"))
            .await?;
        assert_eq!(serde_json::to_value(close)?, json!({}));
        let removed = bridge
            .delete_session(acp::v1::schema::DeleteSessionRequest::new("ses_one"))
            .await?;
        assert_eq!(serde_json::to_value(removed)?, json!({}));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_session_setup_is_rejected_before_http_side_effects() -> anyhow::Result<()>
    {
        let bridge = Bridge::new(reqwest::Client::new(), "http://127.0.0.1:1".to_owned());
        let system = acp::v1::schema::SystemPrompt::new(vec![ContentBlock::Text(
            TextContent::new("do this"),
        )]);
        let request = acp::v1::schema::NewSessionRequest::new("/work").system_prompt(system);
        let error = bridge
            .new_session(request)
            .await
            .expect_err("system prompt must fail closed");
        assert!(error.to_string().contains("systemPrompt"));
        let request = acp::v1::schema::NewSessionRequest::new("/work")
            .additional_directories(vec![std::path::PathBuf::from("/shared")]);
        let error = bridge
            .new_session(request)
            .await
            .expect_err("additional roots must fail closed");
        assert!(error.to_string().contains("additionalDirectories"));
        Ok(())
    }

    #[salvo::handler]
    async fn switch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
        assert_eq!(req.headers()["x-opencode-directory"], "/work");
        let endpoint = req
            .uri()
            .path()
            .rsplit('/')
            .next()
            .expect("switch endpoint")
            .to_owned();
        let body: Value = req.parse_json().await.expect("switch body");
        let state = depot.get_typed::<Arc<State>>().expect("mock state");
        let mut selection = state.selection.lock().expect("mock selection");
        selection[&endpoint] = body
            .get(&endpoint)
            .expect("body matches switch endpoint")
            .clone();
        state.writes.lock().expect("mock writes").push(body);
        res.status_code(StatusCode::NO_CONTENT);
    }

    #[salvo::handler]
    async fn agents(req: &Request, res: &mut Response) {
        assert_eq!(req.headers()["x-opencode-directory"], "/work");
        res.render(Json(json!({"data": [
            {"id": "apex", "name": "apex", "mode": "primary", "hidden": false},
            {"id": "general", "name": "general", "mode": "primary", "hidden": false}
        ], "location": {"directory": "/work"}})));
    }

    #[salvo::handler]
    async fn models(req: &Request, res: &mut Response) {
        assert_eq!(req.headers()["x-opencode-directory"], "/work");
        res.render(Json(json!({"data": [
            {"providerID": "a", "id": "chat", "name": "Chat A", "enabled": true, "variants": []},
            {"providerID": "b", "id": "chat", "name": "Chat B", "enabled": true, "variants": [{"id": "high"}]}
        ], "location": {"directory": "/work"}})));
    }

    #[salvo::handler]
    async fn commands(res: &mut Response) {
        res.render(Json(json!({"data": []})));
    }

    #[salvo::handler]
    async fn history(res: &mut Response) {
        res.render(Json(json!({"data": [
            {"type": "user", "text": "history question"},
            {"type": "assistant", "content": [
                {"type": "text", "text": "history answer"},
                {"type": "tool", "id": "tool_one", "name": "read", "state": {
                    "status": "completed", "content": [{"type": "text", "text": "tool result"}]
                }}
            ]}
        ]})));
    }

    async fn request(
        io: &mut BufReader<tokio::io::DuplexStream>,
        method: &str,
        params: Value,
    ) -> anyhow::Result<(Value, Vec<Value>)> {
        let line = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
        );
        io.write_all(line.as_bytes()).await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut updates = Vec::new();
            loop {
                let mut line = String::new();
                anyhow::ensure!(io.read_line(&mut line).await? != 0, "ACP connection closed");
                let message: Value = serde_json::from_str(&line)?;
                if message["id"] == 1 {
                    return Ok((message, updates));
                }
                updates.push(message);
            }
        })
        .await?
    }

    #[tokio::test]
    async fn lifecycle_lists_saved_sessions() -> anyhow::Result<()> {
        let (agent, client) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(agent);
        let bridge = Bridge::new(reqwest::Client::new(), "http://127.0.0.1:1".to_owned());
        let agent = tokio::spawn(crate::serve(
            bridge,
            acp::ByteStreams::new(write.compat_write(), read.compat()),
        ));
        let mut io = BufReader::new(client);
        let (init, _) = request(&mut io, "initialize", json!({"protocolVersion": 1})).await?;
        assert!(
            init["result"]["agentCapabilities"]["sessionCapabilities"]["list"].is_object(),
            "list capability is absent: {init}"
        );
        drop(io);
        agent.await??;
        Ok(())
    }

    #[tokio::test]
    async fn config_options_use_ox_current_selection() -> anyhow::Result<()> {
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await?;
        let addr = acceptor.local_addr()?;
        let state = Arc::new(State {
            selection: Mutex::new(json!({
                "id": "ses_one", "location": {"directory": "/work"}, "agent": "general",
                "model": {"providerID": "b", "id": "chat", "variant": "high"}
            })),
            writes: Mutex::new(Vec::new()),
        });
        let router = Router::with_path("api")
            .hoop(salvo::affix_state::inject(state.clone()))
            .push(
                Router::with_path("session").post(session).push(
                    Router::with_path("ses_one")
                        .get(session)
                        .push(Router::with_path("model").post(switch))
                        .push(Router::with_path("agent").post(switch))
                        .push(Router::with_path("message").get(history)),
                ),
            )
            .push(Router::with_path("agent").get(agents))
            .push(Router::with_path("model").get(models))
            .push(Router::with_path("command").get(commands));
        let server = tokio::spawn(Server::new(acceptor).serve(router));
        let bridge = Bridge::new(reqwest::Client::new(), format!("http://{addr}"));
        let (agent, client) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(agent);
        let agent = tokio::spawn(crate::serve(
            bridge,
            acp::ByteStreams::new(write.compat_write(), read.compat()),
        ));
        let mut io = BufReader::new(client);
        let (init, _) = request(&mut io, "initialize", json!({"protocolVersion": 1})).await?;
        assert_eq!(init["result"]["protocolVersion"], 1);
        assert_invalid_requests(&mut io).await?;

        let (fresh, _) = request(
            &mut io,
            "session/new",
            json!({"cwd": "/requested", "mcpServers": []}),
        )
        .await?;
        let initial = &fresh["result"]["configOptions"];
        assert_eq!(initial[0]["currentValue"], "b/chat");
        assert_eq!(initial[0]["options"][0]["value"], "a/chat");
        assert_eq!(initial[1]["currentValue"], "high");
        assert_eq!(initial[2]["currentValue"], "general");
        assert_switches(&mut io).await?;

        let (loaded, updates) = request(
            &mut io,
            "session/load",
            json!({"cwd": "/work", "sessionId": "ses_one", "mcpServers": []}),
        )
        .await?;
        assert_eq!(
            loaded["result"]["configOptions"][0]["currentValue"], "a/chat",
            "loaded response: {loaded}"
        );
        assert_eq!(loaded["result"]["modes"]["currentModeId"], "apex");
        let updates = serde_json::to_string(&updates)?;
        for expected in [
            "history question",
            "history answer",
            "tool_call",
            "tool result",
        ] {
            assert!(
                updates.contains(expected),
                "replay missing {expected}: {updates}"
            );
        }
        assert_eq!(
            *state.writes.lock().expect("mock writes"),
            [
                json!("new"),
                json!({"model": {"providerID": "b", "id": "chat", "variant": "default"}}),
                json!({"model": {"providerID": "a", "id": "chat"}}),
                json!({"agent": "apex"}),
            ]
        );
        drop(io);
        agent.await??;
        server.abort();
        Ok(())
    }

    async fn assert_invalid_requests(
        io: &mut BufReader<tokio::io::DuplexStream>,
    ) -> anyhow::Result<()> {
        for cwd in ["relative", "/work\ninvalid"] {
            let (reply, _) =
                request(io, "session/new", json!({"cwd": cwd, "mcpServers": []})).await?;
            assert_eq!(reply["error"]["code"], -32602);
        }
        let (reply, _) = request(
            io,
            "session/load",
            json!({"sessionId": "../info", "cwd": "/work", "mcpServers": []}),
        )
        .await?;
        assert_eq!(reply["error"]["code"], -32602);

        // Per-session MCP is rejected on every session entry point, not just
        // `session/new`: an editor must never get a silent no-op on load/resume.
        let mcp_servers = json!([{
            "type": "http", "name": "remote", "url": "https://example.test/mcp", "headers": []
        }]);
        for method in ["session/new", "session/load", "session/resume"] {
            let mut params = json!({"cwd": "/work", "sessionId": "ses_one"});
            params["mcpServers"] = mcp_servers.clone();
            let (reply, _) = request(io, method, params).await?;
            assert_eq!(reply["error"]["code"], -32602, "{method}: {reply}");
            assert!(
                reply.to_string().contains("session-local tool isolation"),
                "{method} must fail closed on per-session MCP: {reply}"
            );
        }
        Ok(())
    }

    async fn assert_switches(io: &mut BufReader<tokio::io::DuplexStream>) -> anyhow::Result<()> {
        for (id, value) in [
            ("unknown", "apex"),
            ("model", "c/chat"),
            ("effort", "maximum"),
            ("mode", "hidden"),
        ] {
            let (reply, _) = request(
                io,
                "session/set_config_option",
                json!({"sessionId": "ses_one", "configId": id, "value": value}),
            )
            .await?;
            assert_eq!(reply["error"]["code"], -32602);
        }
        for (id, value, count, index) in [
            ("effort", "default", 3, 1),
            ("model", "a/chat", 2, 0),
            ("mode", "apex", 2, 1),
        ] {
            let (reply, _) = request(
                io,
                "session/set_config_option",
                json!({"sessionId": "ses_one", "configId": id, "value": value}),
            )
            .await?;
            let options = &reply["result"]["configOptions"];
            assert_eq!(options.as_array().map(Vec::len), Some(count), "{reply}");
            assert_eq!(options[index]["currentValue"], value);
        }
        Ok(())
    }

    #[test]
    fn prompt_retains_text_and_image_payload() -> anyhow::Result<()> {
        let request = PromptRequest::new(
            "ses_one",
            vec![
                ContentBlock::Text(TextContent::new("inspect this image")),
                ContentBlock::Image(ImageContent::new("AQID", "image/png")),
            ],
        );
        assert_eq!(
            prompt_body(&request)?,
            json!({
                "text": "inspect this image", "files": [{ "uri": "data:image/png;base64,AQID" }]
            })
        );
        Ok(())
    }

    #[test]
    fn prompt_rejects_resource_links_before_posting() {
        let request = PromptRequest::new(
            "ses_one",
            vec![ContentBlock::ResourceLink(
                acp::v1::schema::ResourceLink::new("readme", "file:///work/README.md"),
            )],
        );
        let error = prompt_body(&request).expect_err("resource links are not safe file inputs");
        assert!(error.to_string().contains("resource link"));
    }

    #[test]
    fn child_permission_frame_is_accepted_without_advancing_cursor() {
        let mut last_seq = Some(7);
        let child = Frame {
            seq: None,
            aggregate_id: None,
            kind: "permission.asked".to_owned(),
            data: json!({
                "id": "per_child",
                "sessionID": "ses_child",
                "metadata": {"parentID": "ses_root"}
            }),
        };
        assert!(accept_frame(&mut last_seq, &child, "ses_root"));
        assert_eq!(
            last_seq,
            Some(7),
            "an ephemeral child ask must not move the cursor"
        );

        let unrelated = Frame {
            seq: None,
            aggregate_id: None,
            kind: "permission.asked".to_owned(),
            data: json!({
                "id": "per_other",
                "sessionID": "ses_other",
                "metadata": {"parentID": "ses_unrelated"}
            }),
        };
        assert!(!accept_frame(&mut last_seq, &unrelated, "ses_root"));
    }

    struct PermissionState {
        frames: Vec<Value>,
        immediate_terminal: bool,
        writes: Mutex<Vec<Value>>,
        paths: Mutex<Vec<String>>,
        event_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
        reply_done: Mutex<Option<oneshot::Sender<()>>>,
        control_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
    }

    struct PermissionBody {
        rx: mpsc::UnboundedReceiver<String>,
    }

    impl futures_util::Stream for PermissionBody {
        type Item = Result<String, std::convert::Infallible>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.rx)
                .poll_recv(cx)
                .map(|item| item.map(Ok))
        }
    }

    struct PermissionHarness {
        base: String,
        state: Arc<PermissionState>,
        reply_done: oneshot::Receiver<()>,
        control: mpsc::UnboundedSender<Value>,
        server: tokio::task::JoinHandle<()>,
    }

    fn sse_line(frame: &Value) -> String {
        format!("data: {frame}\n\n")
    }

    fn terminal_frame(owner: &str) -> Value {
        json!({"type": "session.execution.succeeded", "data": {"sessionID": owner}})
    }

    fn permission_frame(id: &str, owner: &str, parent: Option<&str>) -> Value {
        let mut metadata = serde_json::Map::new();
        if let Some(parent) = parent {
            metadata.insert("parentID".to_owned(), json!(parent));
        }
        json!({
            "type": "permission.asked",
            "data": {
                "id": id,
                "sessionID": owner,
                "action": "shell",
                "resources": ["git status"],
                "save": ["git status"],
                "metadata": metadata,
                "source": {"type": "tool", "messageID": "msg_one", "id": "call_one"},
                "message": null
            }
        })
    }

    #[salvo::handler]
    async fn permission_stream(depot: &mut Depot, res: &mut Response) {
        let state = depot
            .get_typed::<Arc<PermissionState>>()
            .expect("permission state");
        let (tx, rx) = mpsc::unbounded_channel();
        *state.event_tx.lock().expect("event sender") = Some(tx.clone());
        tx.send(sse_line(&json!({"type": "server.connected", "data": {}})))
            .expect("send connected frame");
        for frame in &state.frames {
            tx.send(sse_line(frame)).expect("send initial frame");
        }
        if state.immediate_terminal {
            tx.send(sse_line(&terminal_frame("ses_root")))
                .expect("send immediate terminal frame");
        }
        let mut control = state
            .control_rx
            .lock()
            .expect("control receiver")
            .take()
            .expect("control receiver");
        tokio::spawn(async move {
            while let Some(frame) = control.recv().await {
                if tx.send(sse_line(&frame)).is_err() {
                    break;
                }
            }
        });
        res.stream(PermissionBody { rx });
    }

    #[salvo::handler]
    async fn permission_prompt(res: &mut Response) {
        res.status_code(StatusCode::NO_CONTENT);
    }

    #[salvo::handler]
    async fn permission_interrupt(depot: &mut Depot, res: &mut Response) {
        let state = depot
            .get_typed::<Arc<PermissionState>>()
            .expect("permission state");
        if let Some(tx) = state.event_tx.lock().expect("event sender").as_ref() {
            let _ = tx.send(sse_line(&terminal_frame("ses_root")));
        }
        res.status_code(StatusCode::NO_CONTENT);
    }

    #[salvo::handler]
    async fn permission_reply(req: &mut Request, depot: &mut Depot, res: &mut Response) {
        let state = depot
            .get_typed::<Arc<PermissionState>>()
            .expect("permission state");
        assert_eq!(req.headers()["x-opencode-directory"], "/work");
        let body: Value = req.parse_json().await.expect("permission reply body");
        state.writes.lock().expect("permission writes").push(body);
        state
            .paths
            .lock()
            .expect("permission paths")
            .push(req.uri().path().to_owned());
        if let Some(tx) = state.event_tx.lock().expect("event sender").as_ref() {
            let _ = tx.send(sse_line(&terminal_frame("ses_root")));
        }
        if let Some(done) = state.reply_done.lock().expect("reply signal").take() {
            done.send(()).expect("notify permission reply");
        }
        res.status_code(StatusCode::NO_CONTENT);
    }

    async fn start_permission_harness(
        frames: Vec<Value>,
        immediate_terminal: bool,
    ) -> anyhow::Result<PermissionHarness> {
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await?;
        let addr = acceptor.local_addr()?;
        let (control, control_rx) = mpsc::unbounded_channel();
        let (reply_done, reply_rx) = oneshot::channel();
        let state = Arc::new(PermissionState {
            frames,
            immediate_terminal,
            writes: Mutex::new(Vec::new()),
            paths: Mutex::new(Vec::new()),
            event_tx: Mutex::new(None),
            reply_done: Mutex::new(Some(reply_done)),
            control_rx: Mutex::new(Some(control_rx)),
        });
        let router = Router::with_path("api")
            .hoop(salvo::affix_state::inject(state.clone()))
            .push(Router::with_path("event").get(permission_stream))
            .push(Router::with_path("session/ses_root/prompt").post(permission_prompt))
            .push(Router::with_path("session/ses_root/interrupt").post(permission_interrupt))
            .push(
                Router::with_path("session/{sessionID}/permission/{requestID}/reply")
                    .post(permission_reply),
            );
        let server = tokio::spawn(Server::new(acceptor).serve(router));
        Ok(PermissionHarness {
            base: format!("http://{addr}"),
            state,
            reply_done: reply_rx,
            control,
            server,
        })
    }

    async fn permission_connection(
        harness: &PermissionHarness,
    ) -> anyhow::Result<(
        BufReader<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    )> {
        let bridge = Bridge::new(reqwest::Client::new(), harness.base.clone());
        bridge
            .sessions
            .bind("ses_root".to_owned(), "/work".to_owned())?;
        let (agent, client) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(agent);
        let agent = tokio::spawn(crate::serve(
            bridge,
            acp::ByteStreams::new(write.compat_write(), read.compat()),
        ));
        let mut io = BufReader::new(client);
        let (init, _) = request(&mut io, "initialize", json!({"protocolVersion": 1})).await?;
        assert_eq!(init["result"]["protocolVersion"], 1);
        Ok((io, agent))
    }

    async fn begin_permission_prompt(
        io: &mut BufReader<tokio::io::DuplexStream>,
    ) -> anyhow::Result<()> {
        io.write_all(
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                    "params": {
                        "sessionId": "ses_root",
                        "prompt": [{"type": "text", "text": "inspect"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await?;
        Ok(())
    }

    async fn next_permission_message(
        io: &mut BufReader<tokio::io::DuplexStream>,
    ) -> anyhow::Result<Value> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut line = String::new();
            anyhow::ensure!(io.read_line(&mut line).await? != 0, "ACP connection closed");
            Ok(serde_json::from_str(&line)?)
        })
        .await?
    }

    async fn wait_for_permission_request(
        io: &mut BufReader<tokio::io::DuplexStream>,
    ) -> anyhow::Result<Value> {
        loop {
            let message = next_permission_message(io).await?;
            if message["method"] == "session/request_permission" {
                return Ok(message);
            }
            anyhow::ensure!(
                message["id"] != 1,
                "prompt ended before the permission request: {message}"
            );
        }
    }

    async fn answer_permission(
        io: &mut BufReader<tokio::io::DuplexStream>,
        request: &Value,
        outcome: Value,
    ) -> anyhow::Result<()> {
        let response =
            json!({"jsonrpc": "2.0", "id": request["id"], "result": {"outcome": outcome}});
        io.write_all(format!("{response}\n").as_bytes()).await?;
        Ok(())
    }

    async fn wait_for_prompt_response(
        io: &mut BufReader<tokio::io::DuplexStream>,
    ) -> anyhow::Result<Vec<Value>> {
        let mut messages = Vec::new();
        loop {
            let message = next_permission_message(io).await?;
            if message["id"] == 1 {
                return Ok(messages);
            }
            messages.push(message);
        }
    }

    fn selected(option: &str, message: Option<&str>) -> Value {
        let mut outcome = json!({"outcome": "selected", "optionId": option});
        if let Some(message) = message {
            outcome["_meta"] = json!({"message": message});
        }
        outcome
    }

    #[tokio::test]
    async fn permission_v2_ask_forwards_all_three_replies() -> anyhow::Result<()> {
        for decision in ["once", "always", "reject"] {
            let harness = start_permission_harness(
                vec![permission_frame("per_one", "ses_root", None)],
                false,
            )
            .await?;
            let (mut io, agent) = permission_connection(&harness).await?;
            begin_permission_prompt(&mut io).await?;
            let permission = wait_for_permission_request(&mut io).await?;
            assert_eq!(permission["params"]["sessionId"], "ses_root");
            assert_eq!(permission["params"]["toolCall"]["toolCallId"], "call_one");
            assert_eq!(
                permission["params"]["options"],
                json!([
                    {"optionId": "once", "kind": "allow_once", "name": "Allow once"},
                    {"optionId": "always", "kind": "allow_always", "name": "Always allow"},
                    {"optionId": "reject", "kind": "reject_once", "name": "Reject"}
                ])
            );
            answer_permission(&mut io, &permission, selected(decision, None)).await?;
            let messages = wait_for_prompt_response(&mut io).await?;
            assert!(
                messages
                    .iter()
                    .all(|message| message["method"] != "session/request_permission")
            );
            assert_eq!(
                *harness.state.writes.lock().expect("permission writes"),
                [json!({"decision": decision})]
            );
            assert_eq!(
                *harness.state.paths.lock().expect("permission paths"),
                ["/api/session/ses_root/permission/per_one/reply"]
            );
            drop(io);
            agent.await??;
            harness.server.abort();
        }
        Ok(())
    }

    #[tokio::test]
    async fn permission_reject_forwards_feedback_as_message() -> anyhow::Result<()> {
        let harness =
            start_permission_harness(vec![permission_frame("per_one", "ses_root", None)], false)
                .await?;
        let (mut io, agent) = permission_connection(&harness).await?;
        begin_permission_prompt(&mut io).await?;
        let permission = wait_for_permission_request(&mut io).await?;
        answer_permission(
            &mut io,
            &permission,
            selected("reject", Some("try a safer command")),
        )
        .await?;
        wait_for_prompt_response(&mut io).await?;
        assert_eq!(
            *harness.state.writes.lock().expect("permission writes"),
            [json!({"decision": "reject", "message": "try a safer command"})]
        );
        drop(io);
        agent.await??;
        harness.server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn child_permission_ask_reaches_acp_and_replies_to_child_session() -> anyhow::Result<()> {
        let harness = start_permission_harness(
            vec![permission_frame("per_child", "ses_child", Some("ses_root"))],
            false,
        )
        .await?;
        let (mut io, agent) = permission_connection(&harness).await?;
        begin_permission_prompt(&mut io).await?;
        let permission = wait_for_permission_request(&mut io).await?;
        assert_eq!(permission["params"]["sessionId"], "ses_root");
        answer_permission(&mut io, &permission, selected("once", None)).await?;
        wait_for_prompt_response(&mut io).await?;
        assert_eq!(
            *harness.state.paths.lock().expect("permission paths"),
            ["/api/session/ses_child/permission/per_child/reply"]
        );
        drop(io);
        agent.await??;
        harness.server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn permission_client_disconnect_rejects_without_allowing() -> anyhow::Result<()> {
        let harness =
            start_permission_harness(vec![permission_frame("per_one", "ses_root", None)], false)
                .await?;
        let (mut io, agent) = permission_connection(&harness).await?;
        begin_permission_prompt(&mut io).await?;
        let _permission = wait_for_permission_request(&mut io).await?;
        drop(io);
        tokio::time::timeout(std::time::Duration::from_secs(5), harness.reply_done).await??;
        assert_eq!(
            *harness.state.writes.lock().expect("permission writes"),
            [json!({"decision": "reject"})]
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), agent).await???;
        harness.server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn permission_turn_cancellation_rejects_without_allowing() -> anyhow::Result<()> {
        let harness =
            start_permission_harness(vec![permission_frame("per_one", "ses_root", None)], false)
                .await?;
        let (mut io, agent) = permission_connection(&harness).await?;
        begin_permission_prompt(&mut io).await?;
        let _permission = wait_for_permission_request(&mut io).await?;
        let cancel = json!({
            "jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "ses_root"}
        });
        io.write_all(format!("{cancel}\n").as_bytes()).await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), harness.reply_done).await??;
        assert_eq!(
            *harness.state.writes.lock().expect("permission writes"),
            [json!({"decision": "reject"})]
        );
        drop(io);
        agent.await??;
        harness.server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn permission_server_timeout_settlement_cancels_acp_dialog() -> anyhow::Result<()> {
        let harness =
            start_permission_harness(vec![permission_frame("per_one", "ses_root", None)], false)
                .await?;
        let (mut io, agent) = permission_connection(&harness).await?;
        begin_permission_prompt(&mut io).await?;
        let permission = wait_for_permission_request(&mut io).await?;
        harness.control.send(json!({
            "type": "permission.replied",
            "data": {"sessionID": "ses_root", "requestID": "per_one", "reply": "reject"}
        }))?;
        let mut cancelled = false;
        for _ in 0..8 {
            let message = next_permission_message(&mut io).await?;
            if message["method"] == "$/cancel_request"
                && message["params"]["requestId"] == permission["id"]
            {
                cancelled = true;
                break;
            }
        }
        assert!(
            cancelled,
            "a server-timeout reply must cancel the pending ACP dialog"
        );
        assert!(
            harness
                .state
                .writes
                .lock()
                .expect("permission writes")
                .iter()
                .all(|body| {
                    !matches!(
                        body.get("decision").and_then(Value::as_str),
                        Some("once" | "always")
                    )
                })
        );
        harness.control.send(terminal_frame("ses_root"))?;
        wait_for_prompt_response(&mut io).await?;
        drop(io);
        agent.await??;
        harness.server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn legacy_permission_form_is_not_used_for_acp_approval() -> anyhow::Result<()> {
        let harness = start_permission_harness(
            vec![json!({
                "type": "form.created",
                "data": {"form": {
                    "id": "form_one", "sessionID": "ses_root",
                    "fields": [{"type": "string", "key": "decision", "options": [
                        {"value": "once", "label": "Allow once"},
                        {"value": "reject", "label": "Reject"}
                    ]}]
                }}
            })],
            true,
        )
        .await?;
        let (mut io, agent) = permission_connection(&harness).await?;
        begin_permission_prompt(&mut io).await?;
        let messages = wait_for_prompt_response(&mut io).await?;
        assert!(
            messages
                .iter()
                .all(|message| message["method"] != "session/request_permission"),
            "form.created must remain elicitation-only: {messages:?}"
        );
        assert!(
            harness
                .state
                .writes
                .lock()
                .expect("permission writes")
                .is_empty()
        );
        drop(io);
        agent.await??;
        harness.server.abort();
        Ok(())
    }
}
