//! Stable session lifecycle orchestration over ox's public HTTP API.

use std::path::Path;

use agent_client_protocol::Error;
use agent_client_protocol::v1::schema::{
    CloseSessionRequest, CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse,
    ListSessionsRequest, ListSessionsResponse, ResumeSessionRequest, ResumeSessionResponse,
    SessionCapabilities, SessionCloseCapabilities, SessionConfigOption, SessionDeleteCapabilities,
    SessionId, SessionInfo, SessionListCapabilities, SessionModeState, SessionResumeCapabilities,
};
use serde::Deserialize;
use serde_json::Value;

use super::bridge::Bridge;
use super::session::{setup_directory, validate_directory, validate_id};

pub(super) fn capabilities() -> SessionCapabilities {
    SessionCapabilities::new()
        .list(SessionListCapabilities::new())
        .delete(SessionDeleteCapabilities::new())
        .resume(SessionResumeCapabilities::new())
        .close(SessionCloseCapabilities::new())
}

#[derive(Deserialize)]
pub(super) struct Restored {
    pub(super) modes: SessionModeState,
    pub(super) options: Vec<SessionConfigOption>,
}

#[derive(Deserialize)]
struct SessionPage {
    data: Vec<Value>,
    cursor: Option<Cursor>,
}

#[derive(Deserialize)]
struct Cursor {
    next: Option<String>,
}

impl Bridge {
    pub(super) async fn restore_session(
        &self,
        id: &SessionId,
        directory: &str,
        replay: Option<&agent_client_protocol::v1::ConnectionTo<agent_client_protocol::Client>>,
    ) -> Result<Restored, Error> {
        validate_id(&id.0)?;
        let restore = self.sessions.begin_restore(&id.0, directory.to_owned())?;
        let info = self.session_info(&id.0, directory).await?;
        let directory = info.directory_header()?;
        let (catalog, model) = self.configuration(&info, &directory).await?;
        if let Some(connection) = replay {
            self.replay(id, &directory, connection).await?;
        }
        let response = Restored {
            modes: catalog.modes(info.agent.as_deref()),
            options: catalog.options(model.as_ref(), info.agent.as_deref()),
        };
        restore.commit(directory)?;
        if let Some(connection) = replay {
            self.send_available_commands(id, connection).await?;
        }
        Ok(response)
    }

    pub async fn resume_session(
        &self,
        request: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        let directory = setup_directory(
            &request.cwd,
            &request.additional_directories,
            &request.mcp_servers,
            request.system_prompt.as_ref(),
        )?;
        let restored = self
            .restore_session(&request.session_id, &directory, None)
            .await?;
        Ok(ResumeSessionResponse::new()
            .modes(restored.modes)
            .config_options(restored.options))
    }

    pub async fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, Error> {
        let (directory, filter) = match request.cwd {
            Some(path) => {
                let header = setup_directory(&path, &[], &[], None)?;
                (header, Some(path))
            }
            None => (String::new(), None),
        };
        let mut url =
            url::Url::parse(&format!("{}/api/session", self.base)).map_err(super::internal)?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("order", "desc")
                .append_pair("limit", "50");
            if let Some(filter) = &filter {
                query.append_pair(
                    "directory",
                    validate_directory(filter)
                        .map_err(|_| Error::invalid_params().data("invalid session list cwd"))?,
                );
            }
            if let Some(cursor) = request.cursor.filter(|cursor| !cursor.is_empty()) {
                query.append_pair("cursor", &cursor);
            }
        }
        let page = {
            let response = self
                .http
                .get(url)
                .header("x-opencode-directory", &directory)
                .send()
                .await
                .map_err(super::internal)?;
            let status = response.status();
            let body = response.text().await.map_err(super::internal)?;
            if !status.is_success() {
                return Err(super::wire_error(status.as_u16(), &body));
            }
            serde_json::from_str::<SessionPage>(&body).map_err(super::internal)?
        };
        let sessions = page
            .data
            .iter()
            .map(listed_session)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = page
            .cursor
            .and_then(|cursor| cursor.next)
            .filter(|cursor| !cursor.is_empty());
        Ok(ListSessionsResponse::new(sessions).next_cursor(next_cursor))
    }

    pub async fn close_session(
        &self,
        request: CloseSessionRequest,
    ) -> Result<CloseSessionResponse, Error> {
        let id = request.session_id.0;
        validate_id(&id)?;
        if let Some(link) = self.sessions.detach(&id)? {
            close_link(&self.http, &self.base, &link).await?;
            link.finish().await?;
        }
        Ok(CloseSessionResponse::new())
    }

    pub async fn delete_session(
        &self,
        request: DeleteSessionRequest,
    ) -> Result<DeleteSessionResponse, Error> {
        let id = request.session_id.0;
        validate_id(&id)?;
        let link = self.sessions.detach(&id)?;
        if let Some(link) = &link {
            close_link(&self.http, &self.base, link).await?;
        }
        let url = format!("{}/api/session/{id}", self.base);
        let response = self
            .http
            .delete(url)
            .send()
            .await
            .map_err(super::internal)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            let body = response.text().await.map_err(super::internal)?;
            let body: Value = serde_json::from_str(&body).map_err(super::internal)?;
            if body.get("_tag").and_then(Value::as_str) == Some("SessionNotFoundError") {
                if let Some(link) = link {
                    link.finish().await?;
                }
                return Ok(DeleteSessionResponse::new());
            }
            return Err(super::wire_error(404, &body.to_string()));
        }
        Self::checked(response).await?;
        if let Some(link) = link {
            link.finish().await?;
        }
        Ok(DeleteSessionResponse::new())
    }
}

async fn close_link(
    http: &reqwest::Client,
    base: &str,
    link: &super::session::Detach<'_>,
) -> Result<(), Error> {
    let id = link.id.to_owned();
    let turn = link.turn.clone();
    if let Some(turn) = &turn {
        turn.cancel.notify_one();
    }
    let request = http.post(format!("{base}/api/session/{id}/interrupt"));
    Bridge::send(request, &link.directory).await?;
    if let Some(turn) = &turn {
        // A detach concurrent with admission interrupts after the prompt is on the server.
        turn.admission_done.cancelled().await;
        Bridge::send(
            http.post(format!("{base}/api/session/{id}/interrupt")),
            &link.directory,
        )
        .await?;
    }
    Bridge::send(
        http.post(format!("{base}/api/experimental/session/{id}/wait")),
        &link.directory,
    )
    .await?;
    if turn.is_some() {
        link.turn
            .as_ref()
            .expect("turn checked above")
            .detached
            .cancel();
    }
    Ok(())
}

fn listed_session(value: &Value) -> Result<SessionInfo, Error> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| super::internal("session list item has no id"))?;
    validate_id(id)?;
    let cwd = value
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .ok_or_else(|| super::internal("session list item has no directory"))?;
    validate_directory(Path::new(cwd))
        .map_err(|_| super::internal("session list item has an invalid directory"))?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let updated_at = value
        .pointer("/time/updated")
        .and_then(Value::as_i64)
        .and_then(|millis| jiff::Timestamp::from_millisecond(millis).ok())
        .map(|timestamp| timestamp.to_string());
    Ok(SessionInfo::new(id.to_owned(), cwd.to_owned())
        .title(title)
        .updated_at(updated_at))
}
