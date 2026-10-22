//! Wire-exact OpenAI Chat Completions request types.
//!
//! Cardinal's chat kernel (`cardinal/src/protocol/openai_chat.rs`) validates
//! only `model` (string) and a non-empty `messages` array, then forwards every
//! other field through verbatim — so the structs here serialize the canonical
//! OpenAI shapes and nothing provider-specific:
//!
//! - [`ChatRequest`] always carries `"stream": true` and
//!   `"stream_options": {"include_usage": true}` (cardinal pins the same flag
//!   on its side; sending it explicitly keeps ox compatible with any
//!   OpenAI-compatible relay).
//! - [`ChatMessage::Assistant`] serializes `content` as an explicit `null`
//!   when the message carries only tool calls (the canonical OpenAI history
//!   form providers expect for follow-up `role: "tool"` messages).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A streaming chat completion request.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatRequest {
    /// Model identifier as cardinal exposes it (`model.list`).
    pub model: String,
    /// Conversation history; must be non-empty (cardinal rejects empty).
    pub messages: Vec<ChatMessage>,
    /// Tools the model may call; omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    /// Tool selection policy; defaults to [`ToolChoice::Auto`]. Serialized
    /// only alongside a non-empty `tools` array — `tool_choice` without
    /// tools is meaningless on the wire.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Sampling temperature; omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Completion token cap; omitted when unset (cardinal requires > 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Reasoning effort for the turn; cardinal parses it from the body and
    /// re-emits it per provider dialect. Derived from the session's model
    /// variant at [`reasoning_effort_of`] — omitted when there is no
    /// selection, so unselected turns stay byte-identical to before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Always `true` — this client speaks streaming Chat only.
    #[serde(default)]
    pub stream: StreamEnabled,
    /// Always `{"include_usage": true}` so the final chunk carries usage.
    #[serde(default)]
    pub stream_options: IncludeUsage,
}

impl ChatRequest {
    /// A request with the mandatory fields and canonical streaming settings.
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::default(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            stream: StreamEnabled,
            stream_options: IncludeUsage,
        }
    }
}

impl Serialize for ChatRequest {
    /// Canonical OpenAI field order; `tools`/`tool_choice` appear only when
    /// tools are offered, and the optional sampling fields only when set.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ChatRequest", 9)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("messages", &self.messages)?;
        if !self.tools.is_empty() {
            state.serialize_field("tools", &self.tools)?;
            state.serialize_field("tool_choice", &self.tool_choice)?;
        }
        if let Some(temperature) = &self.temperature {
            state.serialize_field("temperature", temperature)?;
        }
        if let Some(max_tokens) = &self.max_tokens {
            state.serialize_field("max_tokens", max_tokens)?;
        }
        if let Some(reasoning_effort) = &self.reasoning_effort {
            state.serialize_field("reasoning_effort", reasoning_effort)?;
        }
        state.serialize_field("stream", &self.stream)?;
        state.serialize_field("stream_options", &self.stream_options)?;
        state.end()
    }
}

/// The chat-wire `reasoning_effort` for a session's model variant.
/// `None`/`""`/`"default"` mean "no selection" — the field is omitted and the
/// gateway applies its own default. The accepted wire names are
/// `off|low|medium|high|xhigh|max` (measured against the live gateway); the
/// OpenAI-style `"none"` maps to `"off"` (an explicit client off, distinct
/// from omitting the field). Anything else is dropped (with a debug log)
/// rather than forwarded: the gateway relays invalid names to the provider,
/// which answers HTTP 400 and would kill the turn.
pub(crate) fn reasoning_effort_of(variant: Option<&str>) -> Option<String> {
    /// The names the live gateway accepts on the chat wire (2026-10-21
    /// probe: anything else is `upstream HTTP 400`).
    const ACCEPTED: [&str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];
    let variant = variant
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "default")?;
    if variant == "none" {
        return Some("off".to_owned());
    }
    if ACCEPTED.contains(&variant) {
        return Some(variant.to_owned());
    }
    tracing::debug!(
        variant,
        "unknown model variant; not forwarding as reasoning_effort \
         (the gateway would answer 400)"
    );
    None
}

/// Literal `true` on the wire: this client only streams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamEnabled;

impl Serialize for StreamEnabled {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for StreamEnabled {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match bool::deserialize(deserializer)? {
            true => Ok(Self),
            false => Err(serde::de::Error::custom(
                "ox only issues streaming chat requests",
            )),
        }
    }
}

/// Literal `{"include_usage": true}` on the wire: ask for usage in the final
/// chunk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncludeUsage;

impl Serialize for IncludeUsage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("IncludeUsage", 1)?;
        state.serialize_field("include_usage", &true)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for IncludeUsage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer)?;
        Ok(Self)
    }
}

/// OpenAI `tool_choice` string forms; ox defaults to `"auto"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call tools.
    #[default]
    Auto,
    /// The model must not call tools.
    None,
    /// The model must call at least one tool.
    Required,
}

/// One part of a user message that is not plain text. Cardinal forwards
/// `content` verbatim, so an image reaches the provider as the standard
/// `image_url` part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserPart {
    /// `{"type":"text","text":"..."}`
    Text { text: String },
    /// `{"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}`
    #[serde(rename = "image_url")]
    Image { image_url: ImageUrl },
}

/// The `image_url` object of a [`UserPart::Image`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

/// The content of a user turn: plain text, or a parts list carrying media
/// beside its text (a tool that attaches an image appends such a turn after
/// its `tool` result — providers accept an image only as user content). The
/// wire form is the OpenAI one: a JSON string, or an array of typed parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<UserPart>),
}

impl From<String> for UserContent {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for UserContent {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<Vec<UserPart>> for UserContent {
    fn from(parts: Vec<UserPart>) -> Self {
        Self::Parts(parts)
    }
}

impl UserContent {
    /// The plain-text view: the text itself; a parts list has none, and reads
    /// as empty.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Parts(_) => "",
        }
    }
}

/// One message of the conversation history, tagged by `role` exactly like the
/// OpenAI wire format: one variant per role. The split once cost a production
/// incident — a dedicated media variant serialized as `role: "usermedia"`
/// (the derived tag of its name), and GLM 400'd every image send with
/// 1214「角色信息不正确」. Media now rides [`ChatMessage::User`] as
/// [`UserContent::Parts`], so no variant name can ever leak onto the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    /// System / developer instructions.
    System { content: String },
    /// User input: plain text or a parts list with media (see [`UserContent`]).
    User { content: UserContent },
    /// Assistant turn: text and/or tool calls. `content` serializes as
    /// explicit `null` when absent.
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
        /// Reasoning text replayed from the row's reasoning parts. Thinking
        /// vendors (GLM/Z.ai `reasoningField: "reasoning_content"`, Kimi)
        /// reject follow-up turns that omit it; absent when the turn carries
        /// none — plain-chat providers must not see the key at all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    /// A tool result for a prior [`ToolCall::id`].
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl ChatMessage {
    /// `{"role": "system", "content": ...}`
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    /// `{"role": "user", "content": ...}`
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: UserContent::from(content.into()),
        }
    }

    /// Assistant turn with text and no tool calls.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: Some(content.into()),
            tool_calls: None,
            reasoning_content: None,
        }
    }

    /// Assistant turn carrying only tool calls (`content: null` on the wire).
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self::Assistant {
            content: None,
            tool_calls: Some(tool_calls),
            reasoning_content: None,
        }
    }

    /// `{"role": "tool", "tool_call_id": ..., "content": ...}`
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        }
    }
}

/// Token usage as reported by cardinal in the final chunk. The two counts
/// are what cardinal's own decoder treats as mandatory; the optional details
/// mirror its vendor-shape fallbacks (DeepSeek's flat `prompt_cache_hit_tokens`
/// vs OpenAI's `prompt_tokens_details.cached_tokens`, and
/// `completion_tokens_details.reasoning_tokens` vs bare `reasoning_tokens`).
/// Deserialization goes through the stream decoder, not a derived impl — the
/// vendor fallback keys cannot be expressed as plain fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// Prompt (input) tokens.
    pub prompt_tokens: u64,
    /// Completion (output) tokens.
    pub completion_tokens: u64,
    /// Prompt tokens served from cache, when the vendor reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Reasoning tokens, when the vendor reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// A tool call issued by the assistant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Call identifier echoed back in the matching [`ChatMessage::Tool`].
    pub id: String,
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: FunctionType,
    pub function: FunctionCall,
}

/// Literal `"function"` discriminator used by [`ToolCall`] and [`ToolDef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FunctionType {
    #[serde(rename = "function")]
    Function,
}

/// The function invocation of a [`ToolCall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// The arguments as a JSON *string* (not a JSON object).
    pub arguments: String,
}

/// A tool offered to the model (OpenAI tools array entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: FunctionType,
    pub function: FunctionDef,
}

impl ToolDef {
    /// A function tool with a JSON-Schema `parameters` object.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: FunctionType::Function,
            function: FunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// The schema part of a [`ToolDef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the `arguments` object.
    pub parameters: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_request_serializes_exactly() {
        let request = ChatRequest::new("glm-4.7", vec![ChatMessage::user("hi")]);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({
                "model": "glm-4.7",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true,
                "stream_options": {"include_usage": true},
            }),
            "no tools → no tools/tool_choice on the wire"
        );
    }

    /// A media user turn rides `role: "user"` with a parts-list content —
    /// what the OpenAI format means by a multimodal user message. The removed
    /// `UserMedia` variant serialized as `role: "usermedia"`, which thinking
    /// vendors reject (GLM: 400 1214「角色信息不正确」— every image send
    /// through cardinal 400'd).
    #[test]
    fn media_user_turn_carries_user_role() {
        let request = ChatRequest::new(
            "k3-256k",
            vec![ChatMessage::User {
                content: UserContent::Parts(vec![UserPart::Image {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,iVBOR".to_owned(),
                    },
                }]),
            }],
        );
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value["messages"][0]["role"],
            json!("user"),
            "a media user turn is role \"user\" with array content"
        );
        assert_eq!(
            value["messages"][0]["content"][0]["type"],
            json!("image_url")
        );
        // Untagged `UserContent` round-trips: the array parses back as parts.
        let parsed: ChatRequest = serde_json::from_value(value).unwrap();
        assert!(matches!(
            &parsed.messages[0],
            ChatMessage::User {
                content: UserContent::Parts(_)
            }
        ));
    }

    #[test]
    fn full_request_serializes_exactly() {
        let request = ChatRequest {
            temperature: Some(0.7),
            max_tokens: Some(4096),
            tools: vec![ToolDef::function(
                "read",
                "Read a file",
                json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            )],
            ..ChatRequest::new(
                "deepseek-v3.2",
                vec![
                    ChatMessage::system("You are ox."),
                    ChatMessage::user("list files"),
                    ChatMessage::assistant_tool_calls(vec![ToolCall {
                        id: "call_1".to_owned(),
                        kind: FunctionType::Function,
                        function: FunctionCall {
                            name: "read".to_owned(),
                            arguments: r#"{"path": "/tmp"}"#.to_owned(),
                        },
                    }]),
                    ChatMessage::tool("call_1", "file contents"),
                ],
            )
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({
                "model": "deepseek-v3.2",
                "messages": [
                    {"role": "system", "content": "You are ox."},
                    {"role": "user", "content": "list files"},
                    {"role": "assistant", "content": null, "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\": \"/tmp\"}"},
                    }]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "file contents"},
                ],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "read",
                        "description": "Read a file",
                        "parameters": {
                            "type": "object",
                            "properties": {"path": {"type": "string"}},
                        },
                    },
                }],
                "tool_choice": "auto",
                "temperature": 0.7,
                "max_tokens": 4096,
                "stream": true,
                "stream_options": {"include_usage": true},
            })
        );
    }

    #[test]
    fn assistant_text_message_omits_tool_calls_key() {
        let message = ChatMessage::assistant("done");
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(
            value,
            json!({"role": "assistant", "content": "done"}),
            "tool_calls must be absent, not null, for plain assistant turns"
        );
    }

    #[test]
    fn assistant_reasoning_content_replays_only_when_present() {
        let message = ChatMessage::Assistant {
            content: None,
            tool_calls: None,
            reasoning_content: Some("thought hard".to_owned()),
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(
            value,
            json!({"role": "assistant", "content": null, "reasoning_content": "thought hard"}),
            "thinking vendors 400 without reasoning_content; tool_calls stays absent"
        );
    }

    #[test]
    fn request_round_trips_through_json() {
        let request = ChatRequest::new("glm-4.7", vec![ChatMessage::user("hi")]);
        let bytes = serde_json::to_vec(&request).unwrap();
        let parsed: ChatRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn reasoning_effort_of_maps_variant_names() {
        // No selection in every flavor: the field stays off the wire.
        assert_eq!(reasoning_effort_of(None), None);
        assert_eq!(reasoning_effort_of(Some("")), None);
        assert_eq!(reasoning_effort_of(Some("  ")), None);
        assert_eq!(reasoning_effort_of(Some("default")), None);
        // The OpenAI-style explicit off: distinct from omitting the field.
        assert_eq!(reasoning_effort_of(Some("none")), Some("off".to_owned()));
        // Every advertised level passes through (trimmed).
        for name in ["off", "low", "medium", "high", "xhigh", "max"] {
            assert_eq!(
                reasoning_effort_of(Some(name)),
                Some(name.to_owned()),
                "{name} must pass through"
            );
        }
        assert_eq!(reasoning_effort_of(Some(" high ")), Some("high".to_owned()));
        // Unknown names are dropped, never forwarded (gateway answers 400).
        assert_eq!(reasoning_effort_of(Some("ultra")), None);
        assert_eq!(reasoning_effort_of(Some("minimal")), None);
        assert_eq!(reasoning_effort_of(Some("bogus")), None);
    }

    #[test]
    fn reasoning_effort_is_absent_without_a_selection() {
        let request = ChatRequest::new("k3-256k", vec![ChatMessage::user("hi")]);
        let value = serde_json::to_value(&request).unwrap();
        assert!(
            value.get("reasoning_effort").is_none(),
            "no variant selected → no reasoning_effort key: {value}"
        );
    }

    #[test]
    fn reasoning_effort_serializes_between_max_tokens_and_stream() {
        let mut request = ChatRequest::new("k3-256k", vec![ChatMessage::user("hi")]);
        request.max_tokens = Some(64);
        request.reasoning_effort = Some("high".to_owned());
        let text = serde_json::to_string(&request).unwrap();
        let max_tokens = text.find("\"max_tokens\"").expect("max_tokens present");
        let effort = text.find("\"reasoning_effort\"").expect("effort present");
        let stream = text.find("\"stream\"").expect("stream present");
        assert!(
            max_tokens < effort && effort < stream,
            "field order: {text}"
        );
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["reasoning_effort"], json!("high"));
    }

    #[test]
    fn usage_serializes_as_plain_counts() {
        let value = serde_json::to_value(Usage {
            prompt_tokens: 12,
            completion_tokens: 7,
            cached_tokens: Some(3),
            reasoning_tokens: Some(2),
        })
        .unwrap();
        assert_eq!(
            value,
            json!({
                "prompt_tokens": 12,
                "completion_tokens": 7,
                "cached_tokens": 3,
                "reasoning_tokens": 2,
            })
        );
        let value = serde_json::to_value(Usage::default()).unwrap();
        assert_eq!(
            value,
            json!({"prompt_tokens": 0, "completion_tokens": 0}),
            "optional details are absent when unset"
        );
    }
}
