use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::form::{Answer, FormInfo};
use crate::{Client, Error, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub directory: String,
    pub canonical: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Envelope<T> {
    data: T,
}

/// One entry of `/api/agent`: only the fields the TUI chrome needs.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentRef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

/// `/api/model/default`: the model a new session would start with.
#[derive(Debug, Clone, Deserialize)]
pub struct DefaultModel {
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
}

/// One entry of `GET /api/model`: the fields the model picker needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelInfo {
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default)]
    pub name: String,
    /// Disabled models stay in the catalogue but are not selectable.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Token limits; `context == 0` means the server does not know the window.
    #[serde(default)]
    pub limit: ModelLimit,
}

/// `limit` of a catalogue entry; only the context window is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct ModelLimit {
    #[serde(default)]
    pub context: i64,
}

fn enabled_by_default() -> bool {
    true
}

/// One match of `GET /api/fs/find`: a path relative to the session directory.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FileEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
}

impl FileEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == "directory"
    }
}

/// One entry of `/api/provider`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderRef {
    pub id: String,
    pub name: String,
}

/// One entry of `/api/mcp` (upstream `mcp.list`): an MCP server the engine
/// knows about, with its live status.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpRef {
    pub name: String,
    pub status: McpStatus,
}

/// Live status of one MCP server. Upstream serializes it as a tagged object
/// (`{"status":"connected"}` / `{"status":"failed","error":"…"}`); the error
/// text rides along for parity, the TUI itself only keys on the tag.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpStatus {
    Connected,
    Pending,
    Disabled,
    Failed {
        #[serde(default)]
        error: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocationInfo {
    pub directory: String,
    pub project: ProjectRef,
}

/// One row of `GET /api/session`: what the session list dialog shows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    /// Absent until the server (or the user) names the session.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub time: SessionTime,
    #[serde(default)]
    pub location: Option<SessionLocation>,
}

/// Epoch milliseconds, as stored by the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct SessionTime {
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub updated: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionLocation {
    pub directory: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Cursor {
    pub previous: Option<String>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    #[serde(default)]
    pub cursor: Cursor,
}

#[derive(Deserialize)]
struct CreateEnvelope {
    data: CreatedSession,
}

#[derive(Deserialize)]
struct SessionEnvelope {
    data: StoredSession,
}

#[derive(Deserialize)]
struct StoredSession {
    #[serde(default)]
    location: Option<StoredLocation>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<StoredModel>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
struct StoredModel {
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(default)]
    variant: Option<String>,
}

/// What a stored session remembers about itself (`GET /api/session/{id}`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    /// `location.directory`.
    pub directory: Option<String>,
    /// Agent id (`build`).
    pub agent: Option<String>,
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    /// Raw variant; `default` is the server's "none" and is left to the caller.
    pub variant: Option<String>,
    pub title: Option<String>,
}

#[derive(Deserialize)]
struct StoredLocation {
    directory: String,
}

#[derive(Deserialize)]
struct CreatedSession {
    id: String,
}

/// Bound for every REST call. Set per request, not on the shared client:
/// the SSE stream uses the same client and must live indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

impl Client {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .header("x-opencode-directory", &self.directory)
            .timeout(REQUEST_TIMEOUT)
    }

    pub async fn location(&self) -> Result<LocationInfo> {
        let resp = self
            .request(reqwest::Method::GET, "/api/location")
            .query(&[("location[directory]", self.directory.as_str())])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json().await?)
    }

    /// The directory stored on the session (`session.location.directory`).
    /// Upstream's session footer shows this, not the server-resolved location.
    pub async fn session_directory(&self, session: &str) -> Result<Option<String>> {
        Ok(self.session_info(session).await?.directory)
    }

    /// The stored session's directory, agent and model.
    pub async fn session_info(&self, session: &str) -> Result<SessionInfo> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/api/session/{session}"))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        let stored = resp.json::<SessionEnvelope>().await?.data;
        let (model_id, provider_id, variant) = match stored.model {
            Some(model) => (Some(model.id), Some(model.provider_id), model.variant),
            None => (None, None, None),
        };
        Ok(SessionInfo {
            directory: stored.location.map(|location| location.directory),
            agent: stored.agent,
            model_id,
            provider_id,
            variant,
            title: stored.title,
        })
    }

    /// Root sessions, most recently updated first (`GET /api/session`, the
    /// upstream list dialog's query: `limit 50, order desc, parentID null`).
    pub async fn sessions(&self) -> Result<Vec<SessionSummary>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/session")
            .query(&[("limit", "50"), ("order", "desc"), ("parentID", "null")])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Page<SessionSummary>>().await?.data)
    }

    /// `PATCH /api/session/{id}` with `{title}` (204). An empty title makes the
    /// server regenerate one, so callers filter that out first.
    pub async fn rename_session(&self, session: &str, title: &str) -> Result<()> {
        let resp = self
            .request(reqwest::Method::PATCH, &format!("/api/session/{session}"))
            .json(&serde_json::json!({ "title": title }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// `DELETE /api/session/{id}` (204); the server cascades to descendants.
    pub async fn delete_session(&self, session: &str) -> Result<()> {
        let resp = self
            .request(reqwest::Method::DELETE, &format!("/api/session/{session}"))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// `POST /api/session/{id}/fork` with no body: forks through the newest
    /// message. Returns the new session id.
    pub async fn fork_session(&self, session: &str) -> Result<String> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/fork"),
            )
            .json(&serde_json::json!({}))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<CreateEnvelope>().await?.data.id)
    }

    pub async fn create_session(&self) -> Result<String> {
        let resp = self
            .request(reqwest::Method::POST, "/api/session")
            .json(&serde_json::json!({
                "location": { "directory": self.directory }
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<CreateEnvelope>().await?.data.id)
    }

    /// First page: `order=desc&limit=100`. Later pages MUST NOT send `order`
    /// (the server rejects cursor+order; the order lives inside the cursor).
    pub async fn messages(
        &self,
        session: &str,
        cursor: Option<&str>,
    ) -> Result<Page<ox_schema::message::Message>> {
        let mut req = self.request(
            reqwest::Method::GET,
            &format!("/api/session/{session}/message"),
        );
        req = match cursor {
            Some(cursor) => req.query(&[("cursor", cursor)]),
            None => req.query(&[("order", "desc"), ("limit", "100")]),
        };
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json().await?)
    }

    pub async fn prompt(&self, session: &str, text: &str) -> Result<Value> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/prompt"),
            )
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json().await?)
    }

    pub async fn interrupt(&self, session: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/interrupt"),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// Visible primary agents, in server order (the first is the default).
    pub async fn agents(&self) -> Result<Vec<AgentRef>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/agent")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<Vec<AgentRef>>>().await?.data)
    }

    /// The model a fresh session starts on.
    pub async fn default_model(&self) -> Result<DefaultModel> {
        let resp = self
            .request(reqwest::Method::GET, "/api/model/default")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<DefaultModel>>().await?.data)
    }

    /// Provider catalogue (display names live here, not on the model).
    pub async fn providers(&self) -> Result<Vec<ProviderRef>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/provider")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<Vec<ProviderRef>>>().await?.data)
    }

    /// Model catalogue in server order (`GET /api/model`).
    pub async fn models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/model")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<Vec<ModelInfo>>>().await?.data)
    }

    /// Switch the session's model (`POST /api/session/{id}/model`, 204).
    pub async fn switch_model(
        &self,
        session: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/model"),
            )
            .json(&serde_json::json!({
                "model": { "id": model_id, "providerID": provider_id }
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// Switch the session's agent (`POST /api/session/{id}/agent`, 204).
    pub async fn switch_agent(&self, session: &str, agent: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/agent"),
            )
            .json(&serde_json::json!({ "agent": agent }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// Configured MCP servers with their live status (`GET /api/mcp`).
    pub async fn mcp(&self) -> Result<Vec<McpRef>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/mcp")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<Vec<McpRef>>>().await?.data)
    }

    /// Paths under the working directory matching `query` (`GET /api/fs/find`,
    /// the `@` file picker). The server caps and orders the result.
    pub async fn find_files(&self, query: &str) -> Result<Vec<FileEntry>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/fs/find")
            .query(&[("query", query), ("limit", "20")])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<Vec<FileEntry>>>().await?.data)
    }

    /// Pending forms of one session (`GET /api/session/{id}/form`), oldest id
    /// first. Unknown sessions answer an empty list.
    pub async fn forms(&self, session: &str) -> Result<Vec<FormInfo>> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!("/api/session/{session}/form"),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<Vec<FormInfo>>>().await?.data)
    }

    /// Answer a form (`POST …/form/{id}/reply`, 204). A form that already
    /// settled answers 409 and a vanished one 404; both surface as
    /// [`Error::Status`] for the caller to treat as benign.
    pub async fn reply_form(&self, session: &str, form: &str, answer: &Answer) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/form/{form}/reply"),
            )
            .json(&serde_json::json!({ "answer": answer }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// Dismiss a form (`DELETE …/form/{id}`, 204); errors as [`Client::reply_form`].
    pub async fn cancel_form(&self, session: &str, form: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::DELETE,
                &format!("/api/session/{session}/form/{form}"),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rest_requests_time_out_but_the_event_stream_does_not() {
        let client = Client::new("http://127.0.0.1:1", "/p");
        let rest = client
            .request(reqwest::Method::GET, "/api/session")
            .build()
            .expect("request");
        assert_eq!(rest.timeout(), Some(&REQUEST_TIMEOUT));
        let stream = client
            .http
            .get("http://127.0.0.1:1/api/event")
            .build()
            .expect("request");
        assert_eq!(stream.timeout(), None);
    }

    #[test]
    fn model_entries_deserialize_the_live_shape_and_default_to_enabled() {
        // Live shape from `GET /api/model`: many fields we do not read.
        let body = json!({
            "data": [
                { "id": "fake-1", "modelID": "fake-1", "providerID": "fake", "name": "Fake One",
                  "type": "chat", "variants": [], "status": "active", "enabled": true },
                { "id": "m2", "modelID": "m2", "providerID": "fake", "name": "Off", "enabled": false },
                { "modelID": "m3", "providerID": "other" }
            ]
        });
        let parsed: Envelope<Vec<ModelInfo>> = serde_json::from_value(body).unwrap();
        let rows: Vec<(&str, &str, &str, bool)> = parsed
            .data
            .iter()
            .map(|m| {
                (
                    m.provider_id.as_str(),
                    m.model_id.as_str(),
                    m.name.as_str(),
                    m.enabled,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("fake", "fake-1", "Fake One", true),
                ("fake", "m2", "Off", false),
                ("other", "m3", "", true),
            ]
        );
    }

    #[test]
    fn mcp_entries_deserialize_the_tagged_status_object() {
        // Live shape from `GET /api/mcp`: `status` is a tagged object, and a
        // failure carries an `error` payload next to the tag.
        let body = json!({
            "data": [
                { "name": "context7", "status": { "status": "connected" } },
                { "name": "broken", "status": { "status": "failed", "error": "boom" } },
                { "name": "warming", "status": { "status": "pending" } },
                { "name": "off", "status": { "status": "disabled" } }
            ]
        });
        let parsed: Envelope<Vec<McpRef>> = serde_json::from_value(body).unwrap();
        let statuses: Vec<McpStatus> = parsed
            .data
            .into_iter()
            .map(|server| server.status)
            .collect();
        assert_eq!(
            statuses,
            vec![
                McpStatus::Connected,
                McpStatus::Failed {
                    error: "boom".into()
                },
                McpStatus::Pending,
                McpStatus::Disabled,
            ]
        );
    }
}
