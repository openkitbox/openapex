//! Pure translation from ox event envelopes to ACP session updates.
//!
//! Every function here takes `&serde_json::Value` event data and produces
//! [`SessionUpdate`]s (or `None` for events that carry no ACP meaning). No
//! I/O, no state — the bridge owns sequencing and delivery.
//!
//! Event-type ground truth is `src/schema/event.rs` (serde renames) and the
//! emitting sites in `src/runner/step.rs`:
//! - `session.text.delta` / `session.reasoning.delta` are the chunk sources
//!   (`session.message.content.updated` exists in the schema but no runner
//!   site publishes it);
//! - `session.tool.called/.progress/.success/.failed` carry the tool
//!   lifecycle (`session.tool.input.*` deltas are noise: ox aggregates input
//!   server-side, and ACP cards want the settled `rawInput`);
//! - `session.execution.succeeded/.failed/.interrupted` are the turn-level
//!   terminators (`session.step.ended` fires once per step, many per turn).

use agent_client_protocol::v1::schema::{
    CompactionStatus, CompactionSummaryChunk, CompactionUpdate, Content, ContentBlock,
    ContentChunk, Notice, NoticeSeverity, SessionInfoUpdate, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use serde_json::Value;

pub type ToolNames = std::collections::HashMap<String, String>;
#[derive(Default)]
pub struct CompactionIds {
    active: std::collections::HashMap<String, String>,
    next: u64,
}

impl CompactionIds {
    pub fn new() -> Self {
        Self::default()
    }
}

pub fn translate_with_compactions(
    kind: &str,
    data: &Value,
    tool_names: &ToolNames,
    compactions: &mut CompactionIds,
) -> Vec<SessionUpdate> {
    match kind {
        "session.text.delta" => data
            .string("delta")
            .map(|delta| {
                chunk(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(delta.to_owned())),
                )))
            })
            .unwrap_or_default(),
        "session.reasoning.delta" => data
            .string("delta")
            .map(|delta| {
                chunk(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(delta.to_owned())),
                )))
            })
            .unwrap_or_default(),
        "session.tool.called" => data
            .string("id")
            .map(|id| {
                let name = tool_names.get(id).map(String::as_str).unwrap_or("tool");
                vec![SessionUpdate::ToolCall(tool_called(
                    id.to_owned(),
                    name,
                    data,
                ))]
            })
            .unwrap_or_default(),
        "session.tool.progress" => data
            .string("id")
            .map(|id| vec![SessionUpdate::ToolCallUpdate(tool_progress(id, data))])
            .unwrap_or_default(),
        "session.tool.success" => data
            .string("id")
            .map(|id| {
                vec![SessionUpdate::ToolCallUpdate(tool_settled(
                    id.to_owned(),
                    data,
                    ToolCallStatus::Completed,
                ))]
            })
            .unwrap_or_default(),
        "session.tool.failed" => data
            .string("id")
            .map(|id| {
                vec![SessionUpdate::ToolCallUpdate(tool_settled(
                    id.to_owned(),
                    data,
                    ToolCallStatus::Failed,
                ))]
            })
            .unwrap_or_default(),
        "session.usage.updated" | "session.usage.recorded" => {
            super::updates::usage(data).into_iter().collect()
        }
        "session.compaction.started" => compaction_started(data, compactions).into_iter().collect(),
        "session.compaction.delta" => compaction_delta(data, compactions).into_iter().collect(),
        "session.compaction.ended" => compaction_ended(data, compactions).into_iter().collect(),
        "session.compaction.failed" => compaction_failed(data, compactions).into_iter().collect(),
        "session.renamed" => data
            .string("title")
            .map(|title| {
                vec![SessionUpdate::SessionInfoUpdate(
                    SessionInfoUpdate::new().title(title.to_owned()),
                )]
            })
            .unwrap_or_default(),
        "session.synthetic" => data
            .string("text")
            .filter(|text| !text.is_empty())
            .map(|text| {
                let description = data
                    .string("description")
                    .filter(|description| !description.is_empty())
                    .map(str::to_owned);
                vec![SessionUpdate::Notice(
                    Notice::new(NoticeSeverity::Info, text).description(description),
                )]
            })
            .unwrap_or_default(),
        // Everything else (session lifecycle, step bookkeeping, inbox,
        // provider-echo frames) has no ACP counterpart.
        _ => Vec::new(),
    }
}

/// Learn a tool name from `session.tool.input.started` data (`{id, name}`).
pub fn learn_tool_name(tool_names: &mut ToolNames, data: &Value) {
    if let (Some(id), Some(name)) = (data.string("id"), data.string("name")) {
        tool_names.insert(id.to_owned(), name.to_owned());
    }
}

fn chunk(update: SessionUpdate) -> Vec<SessionUpdate> {
    vec![update]
}

/// `session.tool.called` → an ACP tool card in `in_progress`.
///
/// The title is the tool name plus the first distinguishing input bit (e.g.
/// `read src/main.rs`), the kind is mapped from ox's tool vocabulary, and the
/// ox input object moves into `rawInput` verbatim.
fn tool_called(id: String, name: &str, data: &Value) -> ToolCall {
    let input = data.get("input");
    let mut call = ToolCall::new(id, title(name, input))
        .name(name.to_owned())
        .kind(tool_kind(name))
        .status(ToolCallStatus::InProgress);
    if let Some(input) = input {
        // ACP owns rawInput after translation; this is the ownership boundary.
        call = call.raw_input(input.clone());
    }
    call
}

/// Terminal tool events → a settled card update with ox's content verbatim.
fn tool_settled(id: String, data: &Value, status: ToolCallStatus) -> ToolCallUpdate {
    let mut fields = ToolCallUpdateFields::new().status(status);
    if let Some(content) = tool_content(data.get("content")) {
        fields = fields.content(content);
    }
    if let Some(output) = data.get("content") {
        // ACP owns rawOutput after translation; this is the ownership boundary.
        fields = fields.raw_output(output.clone());
    }
    if let Some(locations) = tool_locations(data.get("metadata")) {
        fields = fields.locations(locations);
    }
    if status == ToolCallStatus::Failed && fields.content.is_none() {
        let message = data
            .obj("error")
            .and_then(|error| error.string("message"))
            .unwrap_or("tool failed");
        fields = fields.content(vec![ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new(message.to_owned())),
        ))]);
    }
    ToolCallUpdate::new(id, fields)
}

fn compaction_id(data: &Value, compactions: &mut CompactionIds) -> Option<String> {
    let session_id = data.string("sessionID")?;
    if let Some(id) = data
        .string("inputID")
        .or_else(|| data.string("compactionID"))
    {
        compactions
            .active
            .insert(session_id.to_owned(), id.to_owned());
        return Some(id.to_owned());
    }
    if let Some(id) = compactions.active.get(session_id) {
        return Some(id.clone());
    }
    compactions.next = compactions.next.wrapping_add(1);
    let id = format!("{session_id}:compaction-{}", compactions.next);
    compactions.active.insert(session_id.to_owned(), id.clone());
    Some(id)
}

fn compaction_started(data: &Value, compactions: &mut CompactionIds) -> Option<SessionUpdate> {
    Some(SessionUpdate::CompactionUpdate(CompactionUpdate::new(
        compaction_id(data, compactions)?,
        CompactionStatus::InProgress,
    )))
}

fn compaction_delta(data: &Value, compactions: &mut CompactionIds) -> Option<SessionUpdate> {
    Some(SessionUpdate::CompactionSummaryChunk(
        CompactionSummaryChunk::new(
            compaction_id(data, compactions)?,
            ContentBlock::Text(TextContent::new(data.string("text")?)),
        ),
    ))
}

fn compaction_ended(data: &Value, compactions: &mut CompactionIds) -> Option<SessionUpdate> {
    let id = compaction_id(data, compactions)?;
    compactions.active.remove(data.string("sessionID")?);
    Some(SessionUpdate::CompactionUpdate(
        CompactionUpdate::new(id, CompactionStatus::Completed).summary(vec![ContentBlock::Text(
            TextContent::new(data.string("text")?),
        )]),
    ))
}

fn compaction_failed(data: &Value, compactions: &mut CompactionIds) -> Option<SessionUpdate> {
    let message = data
        .obj("error")
        .and_then(|error| error.string("message"))
        .or_else(|| data.string("message"))?;
    let id = compaction_id(data, compactions)?;
    compactions.active.remove(data.string("sessionID")?);
    Some(SessionUpdate::CompactionUpdate(
        CompactionUpdate::new(id, CompactionStatus::Failed).error(message),
    ))
}

fn tool_progress(id: &str, _data: &Value) -> ToolCallUpdate {
    // The initial tool_called update owns rawInput, and the terminal update
    // owns rawOutput. Progress events can arrive for every streamed chunk, so
    // repeating those owned JSON subtrees here would be an avoidable deep copy.
    ToolCallUpdate::new(
        id.to_owned(),
        ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
    )
}

fn tool_content(value: Option<&Value>) -> Option<Vec<ToolCallContent>> {
    let items = value?.as_array()?;
    let mut content = Vec::new();
    for item in items {
        if let Some(item) = tool_content_item(item) {
            content.push(item);
        }
    }
    (!content.is_empty()).then_some(content)
}

fn tool_content_item(item: &Value) -> Option<ToolCallContent> {
    match item.string("type") {
        Some("text") => Some(ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(item.string("text")?),
        )))),
        Some("file") => {
            let uri = item.string("uri")?;
            Some(ToolCallContent::Content(Content::new(
                super::content::resource_link(uri, item.string("name"), None),
            )))
        }
        _ => None,
    }
}

fn tool_locations(value: Option<&Value>) -> Option<Vec<ToolCallLocation>> {
    let items = value?.get("files")?.as_array()?;
    let locations = items
        .iter()
        .filter_map(|item| {
            let path = item.string("file")?;
            Some(ToolCallLocation::new(path))
        })
        .collect::<Vec<_>>();
    (!locations.is_empty()).then_some(locations)
}
/// One-line title: `name` plus the most identifying input field.
fn title(name: &str, input: Option<&Value>) -> String {
    let detail = input
        .and_then(|input| {
            [
                "filePath",
                "path",
                "pattern",
                "command",
                "query",
                "url",
                "description",
            ]
            .iter()
            .find_map(|key| input.string(key))
        })
        .unwrap_or("");
    if detail.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {detail}")
    }
}

/// ox tool vocabulary → ACP tool kinds.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" | "glob" => ToolKind::Read,
        "edit" | "write" | "patch" | "diff" => ToolKind::Edit,
        "grep" | "capability_search" | "search_graph" | "codegraph_search_graph" | "ctx_search" => {
            ToolKind::Search
        }
        "shell" | "subagent" => ToolKind::Execute,
        "question" | "ctx_note" | "ctx_memory" => ToolKind::Think,
        "web_search" | "webfetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Tiny accessors keeping the translation readable.
trait ValueExt {
    fn string(&self, key: &str) -> Option<&str>;
    fn obj(&self, key: &str) -> Option<&Value>;
    fn array(&self, key: &str) -> Option<&Vec<Value>>;
}

impl ValueExt for Value {
    fn string(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }
    fn obj(&self, key: &str) -> Option<&Value> {
        self.get(key).filter(|value| value.is_object())
    }
    fn array(&self, key: &str) -> Option<&Vec<Value>> {
        self.get(key).and_then(Value::as_array)
    }
}

/// Replay one history message (`GET /api/session/{id}/message`) as ACP
/// updates, mirroring upstream `replay.ts`:
/// - user text → `UserMessageChunk`;
/// - assistant `text` / `reasoning` parts → `AgentMessageChunk` /
///   `AgentThoughtChunk`;
/// - assistant `tool` parts → one settled `ToolCall` in its final state
///   (`state.status` completed/error/running/streaming);
/// - everything else (idle markers, compaction, synthetic records) is
///   display-only and skipped.
pub fn replay_message(message: &Value) -> Vec<SessionUpdate> {
    match message.string("type") {
        Some("user") => replay_user(message),
        Some("assistant") => message
            .array("content")
            .map(|parts| parts.iter().filter_map(replay_part).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn replay_user(message: &Value) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    if let Some(text) = message.string("text") {
        updates.push(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_owned())),
        )));
    }
    if let Some(content) = message.get("content").and_then(Value::as_array) {
        for item in content {
            if item.string("type") == Some("resource_link") {
                let Some(uri) = item.string("uri") else {
                    continue;
                };
                updates.push(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    super::content::resource_link(
                        uri,
                        item.string("name"),
                        item.string("description"),
                    ),
                )));
            }
        }
    }
    if let Some(files) = message.get("files").and_then(Value::as_array) {
        for file in files {
            let (Some(data), Some(mime)) = (file.string("data"), file.string("mime")) else {
                continue;
            };
            updates.push(SessionUpdate::UserMessageChunk(ContentChunk::new(
                ContentBlock::Image(agent_client_protocol::v1::schema::ImageContent::new(
                    data, mime,
                )),
            )));
        }
    }
    updates
}

fn replay_part(part: &Value) -> Option<SessionUpdate> {
    match part.string("type")? {
        "text" => Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(part.string("text")?.to_owned())),
        ))),
        "reasoning" => Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(part.string("text")?.to_owned())),
        ))),
        "tool" => replay_tool(part),
        _ => None,
    }
}

/// A settled tool card from a stored assistant `tool` part.
fn replay_tool(part: &Value) -> Option<SessionUpdate> {
    let id = part.string("id")?.to_owned();
    let name = part.string("name")?;
    let state = part.obj("state")?;
    let status = match state.string("status") {
        Some("completed") => ToolCallStatus::Completed,
        Some("error") => ToolCallStatus::Failed,
        Some("running") => ToolCallStatus::InProgress,
        Some("streaming") => ToolCallStatus::InProgress,
        _ => ToolCallStatus::Pending,
    };
    let input = state.get("input").filter(|input| input.is_object());
    let mut call = ToolCall::new(id, title(name, input))
        .name(name.to_owned())
        .kind(tool_kind(name))
        .status(status);
    if let Some(input) = input {
        call = call.raw_input(input.clone());
    }
    if let Some(content) = content_blocks(state.get("content")) {
        call = call.content(content);
    } else if status == ToolCallStatus::Failed
        && let Some(message) = state.obj("error").and_then(|error| error.string("message"))
    {
        call = call.content(vec![ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new(message.to_owned())),
        ))]);
    }
    if let Some(output) = state.get("content") {
        call = call.raw_output(output.clone());
    }
    if let Some(locations) = tool_locations(state.get("metadata")) {
        call = call.locations(locations);
    }
    Some(SessionUpdate::ToolCall(call))
}

/// ox `Content[]` (text parts) → ACP tool-call content.
fn content_blocks(content: Option<&Value>) -> Option<Vec<ToolCallContent>> {
    let content = content?.as_array()?;
    let blocks = content
        .iter()
        .filter_map(tool_content_item)
        .collect::<Vec<_>>();
    (!blocks.is_empty()).then_some(blocks)
}
