//! The bridge: state + behavior of the `ox acp` translator.
//!
//! One [`Bridge`] per stdio connection maps ACP sessions to ox sessions
//! (identity mapping: the ox id IS the ACP id) and turns each `session/prompt`
//! into a subscribed-then-POSTed turn whose SSE frames stream out as ACP
//! updates until a turn-level terminator arrives.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, EmbeddedResourceResource, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, SessionId, SessionMode, SessionModeId,
    SessionModeState, SessionNotification, SetSessionModeRequest, SetSessionModeResponse,
    StopReason,
};
use serde_json::{Value, json};

use super::internal;
use super::stream::{self, Frame};
use super::translate;
use super::wire_error;

/// ACP session id ↔ ox session link state.
#[derive(Debug, Clone)]
struct SessionLink {
    /// The directory the session was created under; carried on every
    /// subsequent ox call (`x-opencode-directory` must match the session's
    /// `session_v2.directory` for the handler to find it).
    directory: String,
    /// The agent ox currently runs for this session (`session/set_mode`).
    current_mode: SessionModeId,
}

/// Shared bridge state: cheap to clone into every ACP handler.
#[derive(Clone)]
pub struct Bridge {
    http: reqwest::Client,
    base: Arc<str>,
    sessions: Arc<Mutex<HashMap<String, SessionLink>>>,
    /// One notify per session with an in-flight prompt: `session/cancel`
    /// flips it, the turn task settles with `StopReason::Cancelled`.
    cancels: Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
}

impl Bridge {
    pub fn new(base: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            cancels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `initialize` — echo the negotiated protocol version and advertise the
    /// v1 surface (load + image prompts; no embedded context, no MCP).
    pub fn initialize(&self, request: InitializeRequest) -> InitializeResponse {
        InitializeResponse::new(request.protocol_version)
            .agent_info(Implementation::new("ox", env!("CARGO_PKG_VERSION")))
            .agent_capabilities(
                AgentCapabilities::new()
                    .load_session(true)
                    .prompt_capabilities(PromptCapabilities::new().image(true)),
            )
    }

    /// `session/new` — create the ox session scoped to the editor's `cwd`.
    pub async fn new_session(
        &self,
        request: NewSessionRequest,
    ) -> Result<NewSessionResponse, acp::Error> {
        let directory = request.cwd.to_string_lossy().to_string();
        // ox resolves the create directory from the request itself the same
        // way the opencode client carries it (`location[directory]` query,
        // then the `x-opencode-directory` header — `request_directory` in
        // src/api/session.rs). The query form is sent here; the header form
        // rides along for every later call on the session.
        let response = self
            .http
            .post(format!(
                "{}/api/session?location%5Bdirectory%5D={}",
                self.base,
                percent_encode(&directory)
            ))
            .header("x-opencode-directory", &directory)
            .json(&json!({}))
            .send()
            .await
            .map_err(internal)?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(wire_error(status.as_u16(), &body));
        }
        let id = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value.pointer("/data/id")?.as_str().map(str::to_owned))
            .ok_or_else(|| internal("ox session create response carried no data.id"))?;

        let (modes, current) = self.modes().await;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| internal("session table poisoned"))?;
        sessions.insert(
            id.clone(),
            SessionLink {
                directory,
                current_mode: current.clone(),
            },
        );
        Ok(NewSessionResponse::new(id).modes(SessionModeState::new(current, modes)))
    }

    /// `session/load` — the editor reopened a thread: verify the session
    /// exists in ox and advertise the same modes as `session/new`.
    /// (Transcript replay lands with the M4 milestone.)
    pub async fn load_session(
        &self,
        request: LoadSessionRequest,
        _connection: &acp::ConnectionTo<acp::Client>,
    ) -> Result<LoadSessionResponse, acp::Error> {
        let session = request.session_id.to_string();
        let directory = request.cwd.to_string_lossy().to_string();
        let response = self
            .http
            .get(format!("{}/api/session/{}", self.base, session))
            .header("x-opencode-directory", &directory)
            .send()
            .await
            .map_err(internal)?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(wire_error(status.as_u16(), &body));
        }
        let (modes, current) = self.modes().await;
        self.sessions
            .lock()
            .map_err(|_| internal("session table poisoned"))?
            .insert(
                session,
                SessionLink {
                    directory,
                    current_mode: current.clone(),
                },
            );
        Ok(LoadSessionResponse::new().modes(SessionModeState::new(current, modes)))
    }

    /// `session/set_mode` — switch ox agent, remember the new mode.
    pub async fn set_session_mode(
        &self,
        request: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse, acp::Error> {
        let session = request.session_id.to_string();
        let directory = self.directory(&session)?;
        let response = self
            .http
            .post(format!("{}/api/session/{}/agent", self.base, session))
            .header("x-opencode-directory", &directory)
            .json(&json!({ "agent": request.mode_id.to_string() }))
            .send()
            .await
            .map_err(internal)?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(wire_error(status.as_u16(), &body));
        }
        if let Ok(mut sessions) = self.sessions.lock()
            && let Some(link) = sessions.get_mut(session.as_str())
        {
            link.current_mode = request.mode_id;
        }
        Ok(SetSessionModeResponse::new())
    }

    /// `session/cancel` — wake the in-flight turn and interrupt ox. The turn
    /// loop sees the interrupt frame and answers `StopReason::Cancelled`.
    pub async fn cancel(&self, session_id: &SessionId) {
        let session = session_id.to_string();
        if let Ok(cancels) = self.cancels.lock()
            && let Some(notify) = cancels.get(session.as_str())
        {
            notify.notify_waiters();
        }
        // Fire-and-forget: the interrupt frame on the stream is the truth;
        // an error here only means the turn already settled.
        let url = format!("{}/api/session/{}/interrupt", self.base, session);
        if let Err(error) = self.http.post(url).send().await {
            tracing::debug!(error = %error, "interrupt POST failed");
        }
    }

    /// Run one prompt turn: subscribe → POST → translate → settle.
    ///
    /// `Err` carries ox's HTTP error verbatim for the JSON-RPC error reply.
    pub(crate) async fn run_prompt(
        &self,
        request: &PromptRequest,
        connection: &acp::ConnectionTo<acp::Client>,
    ) -> Result<StopReason, acp::Error> {
        let session = request.session_id.to_string();
        let directory = self.directory(&session)?;
        let cancel = Arc::new(tokio::sync::Notify::new());
        if let Ok(mut cancels) = self.cancels.lock() {
            cancels.insert(session.clone(), cancel.clone());
        }
        let outcome = self
            .drive_turn(&session, &directory, request, connection, &cancel)
            .await;
        if let Ok(mut cancels) = self.cancels.lock() {
            cancels.remove(session.as_str());
        }
        outcome
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
        connection: &acp::ConnectionTo<acp::Client>,
        cancel: &Arc<tokio::sync::Notify>,
    ) -> Result<StopReason, acp::Error> {
        let mut stream = stream::open(&self.http, &self.base, Some(session), None).await?;
        self.post_prompt(session, directory, request).await?;

        let mut tool_names = translate::ToolNames::new();
        let mut last_seq: Option<i64> = None;
        let mut reconnects = 0usize;

        loop {
            tokio::select! {
                _ = cancel.notified() => {
                    // The cancel handler also POSTs interrupt; doing it here
                    // too keeps the ordering deterministic for this turn.
                    self.post_interrupt(session).await;
                    return Ok(StopReason::Cancelled);
                }
                frame = stream.next() => match frame {
                    None => {
                        reconnects += 1;
                        if reconnects > 2 {
                            tracing::warn!(session, "event stream ended twice mid-turn");
                            return Ok(StopReason::EndTurn);
                        }
                        tracing::info!(session, reconnects, "event stream ended; reconnecting");
                        // Reconnect through the documented replay path when a
                        // durable cursor is known; otherwise fall back to the
                        // live-only global stream.
                        stream = stream::open(&self.http, &self.base, Some(session), last_seq).await?;
                    }
                    Some(frame) => {
                        if frame.seq.is_some() {
                            last_seq = frame.seq;
                        }
                        // The live-only subscription is global: other
                        // sessions' traffic passes through and is dropped.
                        if frame.data.get("sessionID").and_then(Value::as_str)
                            != Some(session)
                        {
                            continue;
                        }
                        if let Some(stop) =
                            self.consume(&frame, &mut tool_names, request, connection)
                        {
                            return Ok(stop);
                        }
                    }
                }
            }
        }
    }

    /// Translate one frame; returns `Some(stop)` on a turn terminator.
    fn consume(
        &self,
        frame: &Frame,
        tool_names: &mut translate::ToolNames,
        request: &PromptRequest,
        connection: &acp::ConnectionTo<acp::Client>,
    ) -> Option<StopReason> {
        if frame.kind == "session.tool.input.started" {
            translate::learn_tool_name(tool_names, &frame.data);
        }
        for update in translate::translate(&frame.kind, &frame.data, tool_names) {
            let notification = SessionNotification::new(request.session_id.clone(), update);
            if let Err(error) = connection.send_notification(notification) {
                tracing::warn!(error = %error.message, "session/update send failed");
            }
        }
        match frame.kind.as_str() {
            "session.execution.succeeded" => Some(StopReason::EndTurn),
            // ox's turn-level failure: the error detail already streamed; the
            // editor sees a non-success stop (refusal is ACP's "agent gave up").
            "session.execution.failed" => Some(StopReason::Refusal),
            "session.execution.interrupted" => Some(StopReason::Cancelled),
            _ => None,
        }
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
            let text = response.text().await.unwrap_or_default();
            return Err(wire_error(status.as_u16(), &text));
        }
        Ok(())
    }

    async fn post_interrupt(&self, session: &str) {
        let url = format!("{}/api/session/{}/interrupt", self.base, session);
        if let Err(error) = self.http.post(url).send().await {
            tracing::debug!(error = %error, "interrupt POST failed");
        }
    }

    /// The directory a session was created under (unknown session → error).
    fn directory(&self, session: &str) -> Result<String, acp::Error> {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(session).map(|link| link.directory.clone()))
            .ok_or_else(|| internal(format!("unknown session `{session}`")))
    }

    /// ox agents → ACP modes (from `GET /api/agent`); ox's default agent is
    /// `build` when present, else the first reported.
    async fn modes(&self) -> (Vec<SessionMode>, SessionModeId) {
        let names: Vec<String> = match self
            .http
            .get(format!("{}/api/agent", self.base))
            .send()
            .await
        {
            Ok(response) => match response.json::<Value>().await {
                Ok(value) => value
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|agent| agent.get("name")?.as_str().map(str::to_owned))
                    .collect(),
                Err(error) => {
                    tracing::debug!(error = %error, "agent list decode failed");
                    Vec::new()
                }
            },
            Err(error) => {
                tracing::debug!(error = %error, "agent list request failed");
                Vec::new()
            }
        };
        let modes: Vec<SessionMode> = names
            .iter()
            .map(|name| SessionMode::new(name.clone(), name.clone()))
            .collect();
        let current = modes
            .iter()
            .find(|mode| mode.id.to_string() == "build")
            .or_else(|| modes.first())
            .map(|mode| mode.id.clone())
            .unwrap_or_else(|| SessionModeId::new("build"));
        (modes, current)
    }
}

/// Percent-encode everything outside the unreserved set (query value).
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Translate an ACP prompt into ox's `PromptBody` JSON (`src/api/session.rs`):
/// text blocks concatenate into `text`; images become ox `FileAttachment`s in
/// the base64 data-URL shape the opencode client sends; resource links and
/// embedded text resources ride in the text.
fn prompt_body(request: &PromptRequest) -> Result<Value, acp::Error> {
    let mut text = String::new();
    let mut files = Vec::new();
    for block in &request.prompt {
        match block {
            ContentBlock::Text(text_content) => {
                text.push_str(&text_content.text);
            }
            ContentBlock::Image(image) => {
                files.push(json!({
                    "data": format!("data:{};base64,{}", image.mime_type, image.data),
                    "mime": image.mime_type,
                    "source": { "type": "file", "path": "" },
                }));
            }
            ContentBlock::ResourceLink(link) => {
                text.push_str(&link.uri);
                text.push('\n');
            }
            ContentBlock::Resource(resource) => {
                if let EmbeddedResourceResource::TextResourceContents(text_resource) =
                    &resource.resource
                {
                    text.push_str(&text_resource.text);
                    text.push('\n');
                }
            }
            ContentBlock::Audio(_) => {
                return Err(internal("audio prompts are not supported"));
            }
            other => {
                return Err(internal(format!(
                    "unsupported prompt content block: {other:?}"
                )));
            }
        }
    }
    let mut body = json!({ "text": text });
    if !files.is_empty() {
        body["files"] = json!(files);
    }
    Ok(body)
}
