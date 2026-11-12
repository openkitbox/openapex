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

/// Tool schema types live in the contract crate; the chat request re-exports
/// them so `cardinal::wire` stays the one place the wire shapes are read from.
pub use ox_schema::wire::{FunctionDef, FunctionType, ToolDef};

/// A streaming chat completion request.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatRequest {
    /// Model identifier as cardinal exposes it (`model.list`).
    pub model: String,
    /// Conversation history; must be non-empty (cardinal rejects empty).
    pub messages: Vec<ChatMessage>,
    /// Tools the model may call; omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: std::sync::Arc<Vec<ToolDef>>,
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
            messages: pair_tool_results(messages),
            tools: std::sync::Arc::new(Vec::new()),
            tool_choice: ToolChoice::default(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            stream: StreamEnabled,
            stream_options: IncludeUsage,
        }
    }

    /// Apply the selected model variant to this request's wire policy.
    ///
    /// Construction deliberately leaves reasoning unset; callers opt into the
    /// shared compatibility mapping at the request boundary.
    pub fn apply_reasoning_effort(&mut self, variant: Option<&str>) {
        self.reasoning_effort = reasoning_effort_of(variant);
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
            state.serialize_field("tools", self.tools.as_slice())?;
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

/// The byte length of the request's serialized body, without building it.
///
/// The calibration observation (`super::super::runner::step`) divides the
/// provider's prompt tokens by the local estimate of the body it just sent, and
/// all it needs from that body is its size. Serializing into a counting writer
/// produces the same bytes as `serde_json::to_string` — same serializer, same
/// field order — while skipping a copy the size of the whole transcript.
pub fn serialized_len(request: &ChatRequest) -> Result<usize, serde_json::Error> {
    /// `std::io::Write` that keeps only the count.
    struct Counter(usize);

    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, request)?;
    Ok(counter.0)
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
    #[must_use]
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
    #[must_use]
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

/// The function invocation of a [`ToolCall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// The arguments as a JSON *string* (not a JSON object).
    pub arguments: String,
}

/// Placeholder content for a synthesized tool result: tells the model the
/// tool produced nothing usable, so it can retry instead of hallucinating
/// an outcome.
pub(crate) const NO_RESULT_CONTENT: &str = "[no result recorded: interrupted or folded]";

/// Make the history wire-safe for strict providers (Moonshot/OpenAI/DeepSeek
/// all 400 the exact shape this guards): every assistant `tool_calls` entry
/// must be answered by a `role:"tool"` message **after** it. ox can emit a
/// call without a result when a crash or kill lands mid-tool-execution — the
/// part stays `Streaming`/`Running`, which renders no result message. Each
/// unanswered id gets a placeholder result spliced in directly after its
/// call-bearing message, the adjacency those providers require. An answer
/// that arrives later (a split turn) already counts as answered — joining it
/// closer is the relay's dialect fixup's business, not ours. A fully paired
/// history passes through untouched: the scan is borrow-only and allocates
/// nothing beyond the (normally empty) missing list.
pub(crate) fn pair_tool_results(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    // First pass, borrowed: which call ids lack a later answer.
    let mut missing: Vec<(usize, String)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let ChatMessage::Assistant {
            tool_calls: Some(calls),
            ..
        } = message
        else {
            continue;
        };
        for call in calls {
            let answered = messages[index + 1..].iter().any(|later| {
                matches!(later, ChatMessage::Tool { tool_call_id, .. } if tool_call_id == &call.id)
            });
            if !answered {
                missing.push((index, call.id.clone()));
            }
        }
    }
    if missing.is_empty() {
        return messages;
    }
    // Second pass, owned: splice the placeholders in right after their
    // assistant message. `missing` is already ordered by (index, call order)
    // because both loops walk forward.
    let mut spliced = Vec::with_capacity(messages.len() + missing.len());
    let mut missing = missing.into_iter().peekable();
    for (index, message) in messages.into_iter().enumerate() {
        spliced.push(message);
        while missing.peek().is_some_and(|(at, _)| *at == index) {
            let (_, id) = missing.next().expect("peeked");
            spliced.push(ChatMessage::Tool {
                tool_call_id: id,
                content: NO_RESULT_CONTENT.to_owned(),
            });
        }
    }
    spliced
}
