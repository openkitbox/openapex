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
