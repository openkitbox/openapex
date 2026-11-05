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
    Content, ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::Value;

/// Translate one ox event into zero or more ACP session updates.
///
/// `tool_names` carries the tool-name map the bridge learns from
/// `session.tool.input.started` frames (`session.tool.called` itself does not
/// repeat the name, only the id and input).
pub fn translate(kind: &str, data: &Value, tool_names: &ToolNames) -> Vec<SessionUpdate> {
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
            .map(|id| {
                vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id.to_owned(),
                    ToolCallUpdateFields::new(),
                ))]
            })
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
            // ACP's UsageUpdate requires the context-window size, which ox's
            // usage events do not carry (tokens: input/output/reasoning/cache
            // only) — no honest mapping exists yet, so usage stays unmapped.
            Vec::new()
        }
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

/// Tool id → name, per turn (see [`learn_tool_name`]).
pub type ToolNames = std::collections::HashMap<String, String>;

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
        call = call.raw_input(input.clone());
    }
    call
}

/// Terminal tool events → a settled card update with ox's content verbatim.
fn tool_settled(id: String, data: &Value, status: ToolCallStatus) -> ToolCallUpdate {
    let mut fields = ToolCallUpdateFields::new().status(status);
    // ox `Content` objects carry {type: text, text} / {type: file...}; ACP
    // wants ToolCallContent wrappers. Text content maps directly; anything
    // else rides in rawOutput (set below), which editors show as raw JSON.
    let text = data
        .array("content")
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let text = item.get("text")?.as_str()?;
                    Some(ToolCallContent::Content(Content::new(ContentBlock::Text(
                        TextContent::new(text.to_owned()),
                    ))))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !text.is_empty() {
        fields = fields.content(text);
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
        Some("user") => message
            .string("text")
            .map(|text| {
                vec![SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(text.to_owned())),
                ))]
            })
            .unwrap_or_default(),
        Some("assistant") => message
            .array("content")
            .map(|parts| parts.iter().filter_map(replay_part).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
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
    Some(SessionUpdate::ToolCall(call))
}

/// ox `Content[]` (text parts) → ACP tool-call content.
fn content_blocks(content: Option<&Value>) -> Option<Vec<ToolCallContent>> {
    let content = content?.as_array()?;
    let blocks = content
        .iter()
        .filter_map(|item| {
            let text = item.get("text")?.as_str()?;
            Some(ToolCallContent::Content(Content::new(ContentBlock::Text(
                TextContent::new(text.to_owned()),
            ))))
        })
        .collect::<Vec<_>>();
    (!blocks.is_empty()).then_some(blocks)
}
