use std::collections::HashMap;
use std::time::Duration;

use ox_schema::support::FileInput;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::form::{Answer, FormInfo};
use crate::{Client, Error, Result};

/// `POST …/prompt` body, borrowing the caller's text and attachments (a
/// base64 image can be tens of MB, so nothing here is cloned).
#[derive(Serialize)]
struct PromptBody<'a> {
    id: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "<[FileInput]>::is_empty")]
    files: &'a [FileInput],
}

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
    #[must_use]
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
    /// Set on child (subagent) sessions; absent on roots.
    #[serde(default, rename = "parentID")]
    pub parent_id: Option<String>,
}

/// One session of a family tree (`GET /ox/session/{id}/family`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FamilyMember {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    /// Absent on the root.
    #[serde(default, rename = "parentID")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub time: SessionTime,
    /// The server's runner has work scheduled or in flight for the session.
    #[serde(default)]
    pub running: bool,
}

/// Epoch milliseconds, as stored by the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct SessionTime {
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub updated: i64,
    /// Start of the latest turn; absent until the session first runs.
    #[serde(default)]
    pub busy: Option<i64>,
    /// End of the latest turn; absent until one finishes.
    #[serde(default)]
    pub idle: Option<i64>,
}

impl SessionTime {
    /// A turn is in flight: it started (`busy`) and has not ended since
    /// (`idle` absent or older). The server stamps `busy` on
    /// `execution.started` and `idle` (strictly after `busy`) on every end
    /// (`projector.rs`). A crashed server can leave a stale `true` behind.
    #[must_use]
    pub fn is_running(&self) -> bool {
        match (self.busy, self.idle) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(busy), Some(idle)) => idle < busy,
        }
    }
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
struct MemoryModeEnvelope {
    data: MemoryModeData,
}

#[derive(Deserialize)]
struct MemoryModeData {
    mode: String,
}

#[derive(Deserialize)]
struct MemoryClearedEnvelope {
    data: MemoryClearedData,
}

#[derive(Deserialize)]
struct MemoryClearedData {
    archived: u64,
}

#[derive(Deserialize)]
struct SessionEnvelope {
    data: StoredSession,
}

#[derive(Deserialize)]
struct StoredSession {
    #[serde(default)]
    time: SessionTime,
    #[serde(default)]
    location: Option<StoredLocation>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<StoredModel>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
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
    /// Agent id (`apex`).
    pub agent: Option<String>,
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    /// Raw variant; `default` is the server's "none" and is left to the caller.
    pub variant: Option<String>,
    pub title: Option<String>,
    pub time: SessionTime,
    /// Set on child (subagent) sessions; absent on roots.
    pub parent_id: Option<String>,
}

#[derive(Deserialize)]
struct StoredLocation {
    directory: String,
}

#[derive(Deserialize)]
struct CreatedSession {
    id: String,
}

/// One entry of `GET /api/shell`: a background shell session.
#[derive(Debug, Clone, Deserialize)]
pub struct ShellInfo {
    pub id: String,
    /// `running`, `exited`, `timeout` or `killed`.
    pub status: String,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub file: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub exit: Option<i32>,
    #[serde(default)]
    pub signal: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
    pub time: ShellTime,
}

impl ShellInfo {
    /// The session that started this shell, when the server recorded one.
    pub fn session_id(&self) -> Option<&str> {
        self.metadata.get("sessionID").and_then(Value::as_str)
    }
}

/// Epoch milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ShellTime {
    pub started: i64,
    #[serde(default)]
    pub completed: Option<i64>,
}

/// `GET /api/shell/{id}/output`: one page of captured output.
#[derive(Debug, Clone, Deserialize)]
pub struct ShellOutputPage {
    pub output: String,
    /// Where the next read continues.
    pub cursor: u64,
    pub size: u64,
    pub truncated: bool,
}

/// The newest part of a shell's capture, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellTail {
    pub text: String,
    /// Output before `text` exists but was not returned.
    pub omitted: bool,
}

/// How far [`Client::shell_read`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellReadMode {
    /// One page: a poll of a shell that is still writing.
    Page,
    /// Up to the capture's size, jumping to the newest `max_bytes` when more
    /// is pending: the final read of a shell that exited.
    Drain,
}

/// Output read from a cursor by [`Client::shell_read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRead {
    pub text: String,
    /// Where the next read continues.
    pub end: u64,
    /// Bytes between the requested cursor and `text` were never returned
    /// (the server dropped them, or the drain jumped over them).
    pub skipped: bool,
}

/// One attachment's payload, exactly as the server stored it: raw bytes, never
/// base64. Produced once per fetch and returned by value; not `Clone`, so the
/// payload cannot be duplicated by accident.
#[derive(Debug, PartialEq, Eq)]
pub struct MediaBytes {
    /// The response's `content-type`. The server sniffs it from the bytes
    /// (`ox-server/src/api/media.rs`), so it can differ from the declaration the
    /// row carries and is the one to trust when both are known.
    pub mime: String,
    /// The row's file name, absent when the wire carried none (display only).
    pub name: Option<String>,
    pub bytes: Vec<u8>,
}

/// Bound for every REST call. Set per request, not on the shared client:
/// the SSE stream uses the same client and must live indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for an MCP connect/disconnect. The server holds the request until
/// the server's startup settles, so this must exceed any reasonable
/// `[mcp.timeout] startup` budget (30 s by default); with
/// [`REQUEST_TIMEOUT`] a slow but successful start would read as a failure.
const MCP_TOGGLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// `Number.MAX_SAFE_INTEGER`: a cursor no capture reaches, so the answer only
/// carries the capture's `size` (upstream's probe).
const MAX_SAFE_CURSOR: u64 = 9_007_199_254_740_991;
/// The attachment's file name, when the row carries one
/// (`ox-server/src/api/media.rs`): a header because it is display-only, so a
/// multipart body would cost the response a second copy of the payload.
const NAME_HEADER: &str = "x-ox-name";
/// What the media endpoint answers when it recognises none of the bytes it
/// sniffed, and what this client falls back to when the header is missing or
/// unreadable on the wire.
const DEFAULT_MEDIA_MIME: &str = "application/octet-stream";

/// Percent-encode `value` as one URL path segment: every byte outside the
/// unreserved set is escaped, so a `/` or `?` in an MCP server name cannot
/// reach another route. URL parsing still resolves a `.` / `..` segment
/// (escaped or not) as a relative step; such a name lands on an unknown path
/// and fails as a 404, never on another real route.
fn encode_path_segment(value: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            other => {
                write!(out, "%{other:02X}").expect("writing to a String cannot fail");
            }
        }
    }
    out
}

impl Client {
    /// Validate and canonicalize a Home directory, without creating a session.
    pub async fn validate_directory(&self, directory: &str) -> Result<String> {
        let resp = self
            .request(reqwest::Method::POST, "/ox/location")
            .query(&[("directory", directory)])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<SessionLocation>().await?.directory)
    }

    pub async fn move_session(&self, session: &str, directory: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/move"),
            )
            .json(&serde_json::json!({"directory": directory}))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn directories(&self, query: &str) -> Result<Vec<String>> {
        let resp = self
            .request(reqwest::Method::GET, "/ox/directories")
            .query(&[("query", query)])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json().await?)
    }

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
            time: stored.time,
            parent_id: stored.parent_id,
        })
    }

    /// Root sessions in `directory`, filtered on the server before its limit.
    pub async fn sessions(&self, directory: &str) -> Result<Vec<SessionSummary>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/session")
            .query(&[
                ("directory", directory),
                ("limit", "50"),
                ("order", "desc"),
                ("parentID", "null"),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Page<SessionSummary>>().await?.data)
    }

    /// Ids of every session with work scheduled or in flight, children
    /// included (`GET /api/session/active`).
    pub async fn active_sessions(&self) -> Result<Vec<String>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/session/active")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        let active = resp.json::<Envelope<HashMap<String, Value>>>().await?.data;
        Ok(active.into_keys().collect())
    }

    /// The whole session tree `current` belongs to, root first and then its
    /// descendants breadth-first (`GET /ox/session/{id}/family`, an ox-only
    /// endpoint: upstream's `parentID` filter reaches direct children only).
    /// The server bounds the tree and reports each session's agent and
    /// whether it is running.
    pub async fn session_family(&self, current: &str) -> Result<Vec<FamilyMember>> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!("/ox/session/{current}/family"),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<Vec<FamilyMember>>>().await?.data)
    }

    /// Background shell sessions of the server (`GET /api/shell`).
    pub async fn shells(&self) -> Result<Vec<ShellInfo>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/shell")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<Vec<ShellInfo>>>().await?.data)
    }

    /// One page of a shell's captured output from byte `cursor`; the page
    /// size is the server default.
    pub async fn shell_output(&self, id: &str, cursor: u64) -> Result<ShellOutputPage> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/api/shell/{id}/output"))
            .query(&[("cursor", cursor)])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<ShellOutputPage>>().await?.data)
    }

    /// Read a shell's output from byte `cursor`, `max_bytes` per page
    /// (upstream `ShellDisplay.loadBackgroundOutput`). `Drain` keeps paging
    /// until the size seen in the first answer; when more than `max_bytes` is
    /// still pending it jumps to `size - max_bytes`, drops what it read before
    /// the jump and reports the skip. A page that does not move the cursor
    /// ends the read.
    pub async fn shell_read(
        &self,
        id: &str,
        cursor: u64,
        mode: ShellReadMode,
        max_bytes: usize,
    ) -> Result<ShellRead> {
        let mut read = ShellRead {
            text: String::new(),
            end: cursor,
            skipped: false,
        };
        // The size the first page reports: output written while draining is
        // left for the next read, so a shell that writes as fast as it is
        // read cannot keep the drain going.
        let mut first_size = None;
        loop {
            let page = self.shell_output_page(id, read.end, max_bytes).await?;
            let target = *first_size.get_or_insert(page.size);
            read.skipped |= page.truncated;
            read.text.push_str(&page.output);
            if page.cursor <= read.end {
                break;
            }
            read.end = page.cursor;
            if mode == ShellReadMode::Page || read.end >= target {
                break;
            }
            let tail = target.saturating_sub(max_bytes as u64);
            if tail > read.end {
                read.end = tail;
                read.skipped = true;
                read.text.clear();
            }
        }
        if read.skipped {
            // The server decodes lossily: a character cut by the cursor shows
            // up as replacement characters at the very start.
            let cut = read.text.len() - read.text.trim_start_matches('\u{fffd}').len();
            read.text.drain(..cut);
        }
        Ok(read)
    }

    /// `GET /api/shell/{id}/output?cursor&limit`.
    async fn shell_output_page(
        &self,
        id: &str,
        cursor: u64,
        limit: usize,
    ) -> Result<ShellOutputPage> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/api/shell/{id}/output"))
            .query(&[("cursor", cursor), ("limit", limit as u64)])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<Envelope<ShellOutputPage>>().await?.data)
    }

    /// The newest `max_bytes` of a shell's capture. A probe at an unreachable
    /// cursor learns the capture's size, then pages are read from
    /// `size - max_bytes` up to that size; output appended meanwhile is left
    /// for the next call. A character cut by the start cursor is dropped.
    pub async fn shell_tail(&self, id: &str, max_bytes: usize) -> Result<ShellTail> {
        let size = self.shell_output(id, MAX_SAFE_CURSOR).await?.size;
        let start = size.saturating_sub(max_bytes as u64);
        let mut cursor = start;
        let mut text = String::new();
        let mut omitted = start > 0;
        loop {
            let page = self.shell_output(id, cursor).await?;
            if cursor == start {
                omitted |= page.truncated;
            }
            let progressed = page.cursor > cursor;
            cursor = page.cursor;
            text.push_str(&page.output);
            if !progressed || cursor >= size {
                break;
            }
        }
        if start > 0 {
            // The server decodes lossily: a character cut by the cursor shows
            // up as replacement characters at the very start.
            let cut = text.len() - text.trim_start_matches('\u{fffd}').len();
            text.drain(..cut);
        }
        if text.len() > max_bytes {
            let mut from = text.len() - max_bytes;
            while !text.is_char_boundary(from) {
                from += 1;
            }
            text.drain(..from);
            omitted = true;
        }
        Ok(ShellTail { text, omitted })
    }

    /// `DELETE /api/shell/{id}` (204): kill the shell session.
    pub async fn kill_shell(&self, id: &str) -> Result<()> {
        let resp = self
            .request(reqwest::Method::DELETE, &format!("/api/shell/{id}"))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
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

    /// `GET /api/session/{id}/memory`: the session's memory switch, `"on"`
    /// or `"off"`.
    pub async fn memory_mode(&self, session: &str) -> Result<String> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!("/api/session/{session}/memory"),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<MemoryModeEnvelope>().await?.data.mode)
    }

    /// `POST /api/session/{id}/memory` with `{mode}` (204).
    pub async fn set_memory_mode(&self, session: &str, mode: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/memory"),
            )
            .json(&serde_json::json!({ "mode": mode }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// `POST /api/session/{id}/memory/clear` for the session's project:
    /// archives its active memories and returns how many.
    pub async fn clear_memory(&self, session: &str) -> Result<u64> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/memory/clear"),
            )
            .json(&serde_json::json!({ "scope": "project" }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<MemoryClearedEnvelope>().await?.data.archived)
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

    /// One stored attachment's raw bytes, an ox-only endpoint
    /// (`GET /ox/session/{id}/media/{message}?index=N[&part=…]`).
    /// A message row carries its payloads inline as
    /// base64, so [`crate::store::MediaMeta`] keeps only identity and the viewer
    /// asks for the one image it is about to draw.
    ///
    /// `index` addresses a user prompt's `files[index]`; with `part` it
    /// addresses `content[index]` of that assistant tool call instead. A bad
    /// index or a non-`file` content answers 400, an unknown message or part
    /// 404, both as [`Error::Status`].
    pub async fn media(
        &self,
        session: &str,
        message: &str,
        part: Option<&str>,
        index: u32,
    ) -> Result<MediaBytes> {
        let mut req = self
            .request(
                reqwest::Method::GET,
                &format!("/ox/session/{session}/media/{message}"),
            )
            .query(&[("index", index)]);
        if let Some(part) = part {
            req = req.query(&[("part", part)]);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or(DEFAULT_MEDIA_MIME)
            .to_owned();
        let name = resp
            .headers()
            .get(NAME_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(MediaBytes {
            mime,
            name,
            bytes: resp.bytes().await?.into(),
        })
    }

    /// `files` are serialized by reference and omitted when empty.
    ///
    /// `id` becomes the inbox and message id, so the caller's optimistic echo
    /// can be matched to the `session.inbox.delivered` that names it.
    pub async fn prompt(
        &self,
        session: &str,
        id: &str,
        text: &str,
        files: &[FileInput],
    ) -> Result<Value> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/prompt"),
            )
            .json(&PromptBody { id, text, files })
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

    /// `POST /api/session/{session}/background`: move the session's blocking
    /// `shell`/`subagent` calls to the background (upstream
    /// `session.background`).
    pub async fn background(&self, session: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/background"),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// `PATCH /api/session/{session}/inbox/{inbox}` with `{"delivery":"steer"}`
    /// (204): deliver a queued prompt at the next step boundary instead of
    /// after the turn.
    pub async fn steer_queued(&self, session: &str, inbox: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::PATCH,
                &format!("/api/session/{session}/inbox/{inbox}"),
            )
            .json(&serde_json::json!({ "delivery": "steer" }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }

    /// `DELETE /api/session/{session}/inbox/{inbox}` (204): drop a queued
    /// prompt before it is delivered.
    pub async fn cancel_queued(&self, session: &str, inbox: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::DELETE,
                &format!("/api/session/{session}/inbox/{inbox}"),
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

    /// `POST /api/experimental/mcp/{server}/connect` (204, upstream
    /// `mcp.connect`): start `server`, even one disabled in the config. The
    /// server's status has settled when this returns; a failed start is not
    /// an error here, it shows up as `failed` in [`Client::mcp`].
    pub async fn mcp_connect(&self, server: &str) -> Result<()> {
        let path = format!(
            "/api/experimental/mcp/{}/connect",
            encode_path_segment(server)
        );
        self.mcp_toggle(&path).await
    }

    /// `POST /api/experimental/mcp/{server}/disconnect` (204, upstream
    /// `mcp.disconnect`): stop `server` and mark it `disabled`.
    pub async fn mcp_disconnect(&self, server: &str) -> Result<()> {
        let path = format!(
            "/api/experimental/mcp/{}/disconnect",
            encode_path_segment(server)
        );
        self.mcp_toggle(&path).await
    }

    /// POST a connect/disconnect `path`, waiting out the server's startup.
    async fn mcp_toggle(&self, path: &str) -> Result<()> {
        let resp = self
            .request(reqwest::Method::POST, path)
            .timeout(MCP_TOGGLE_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
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
