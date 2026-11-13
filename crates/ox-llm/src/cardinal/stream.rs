//! SSE framing, chunk decoding and delta assembly for cardinal's streaming
//! Chat responses.
//!
//! Three layers, each independently testable:
//!
//! 1. [`SseFramer`] — bytes to `data:` payloads. A port of cardinal's own
//!    framer (`cardinal/src/protocol/sse.rs`): UTF-8 BOM stripped, `\n` /
//!    `\r\n` / bare `\r` line endings, `:`-prefixed comment (keep-alive)
//!    lines ignored, multi-line `data` values joined with `\n`, and the same
//!    line/event size limits.
//! 2. [`ChunkDecoder`] — one payload to zero or more [`StreamEvent`]s, the
//!    similar vocabulary to cardinal's `StreamDecoder::observe_raw` (both the
//!    DeepSeek `reasoning_content` and OpenRouter `reasoning` field names,
//!    the running tool-call index fallback, and usage-only chunks with an
//!    empty `choices` array that do not terminate the stream). Deliberate
//!    deviations: refusal deltas are surfaced as text; `finish_reason: error`
//!    is an upstream error rather than a successful finish; and Cardinal's
//!    in-band error object is checked before any event is emitted.
//! 3. [`Assembler`] — the deltas folded back into a final assistant message:
//!    text, reasoning, and tool calls keyed by index with concatenated
//!    argument fragments.

use std::collections::BTreeMap;

use serde::Serialize;

use super::ChatError;
use crate::cardinal::wire::{CardinalChatUsageProof, Usage};
use crate::sse::SseDecoder;

pub use crate::sse::SseFramer;

/// One increment of a streaming chat response.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A text fragment (`choices[0].delta.content`).
    TextDelta(String),
    /// A reasoning fragment (`delta.reasoning_content` or `delta.reasoning`).
    ReasoningDelta(String),
    /// A tool-call fragment; `arguments` is a JSON *fragment*.
    ToolCallDelta {
        /// Position of the call in this response (parallel calls differ).
        index: u32,
        /// Call id — usually only the first fragment carries it.
        id: Option<String>,
        /// Function name — usually only the first fragment carries it.
        name: Option<String>,
        /// Argument JSON fragment to concatenate.
        arguments: String,
    },
    /// `choices[0].finish_reason`, preserved raw (`stop`, `tool_calls`,
    /// `length`, `content_filter`, ...).
    Finish {
        /// The reason string exactly as cardinal sent it.
        reason: String,
    },
    /// Token usage, usually in the trailing chunk with empty `choices`.
    Usage(Usage),
    /// The `data: [DONE]` terminator.
    Done,
}

/// Decodes cardinal's Chat SSE payloads into [`StreamEvent`]s.
///
/// Mirrors `cardinal/src/protocol/openai_chat.rs`'s `StreamDecoder`: text and
/// reasoning deltas, `tool_calls` fragments (with a running index for vendors
/// that omit `index`), `finish_reason`, usage (including usage-only chunks
/// with an empty `choices` array), and in-band `error` objects.
#[derive(Debug, Default)]
pub struct ChunkDecoder {
    tool_index: u32,
}

impl SseDecoder for ChunkDecoder {
    fn decode(&mut self, payload: &str, out: &mut Vec<StreamEvent>) -> Result<(), ChatError> {
        ChunkDecoder::decode(self, payload, out)
    }
}

impl ChunkDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one `data:` payload, appending events to `out`.
    ///
    /// `[DONE]` yields [`StreamEvent::Done`]; an empty payload yields nothing.
    /// A payload that is not valid JSON is fatal (cardinal returns a failed
    /// turn for it too); an in-band `error` object becomes
    /// [`ChatError::Upstream`].
    pub fn decode(&mut self, payload: &str, out: &mut Vec<StreamEvent>) -> Result<(), ChatError> {
        let data = payload.trim();
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            out.push(StreamEvent::Done);
            return Ok(());
        }
        let value: serde_json::Value = serde_json::from_str(data)
            .map_err(|error| ChatError::Sse(format!("invalid JSON payload: {error}")))?;
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            return Err(ChatError::Upstream(describe_error(error)));
        }
        if let Some(usage) = usage_from(&value) {
            out.push(StreamEvent::Usage(usage));
        }
        let Some(choice) = value
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .and_then(|choices| choices.first())
        else {
            // A usage-only chunk with an empty `choices` array is valid and
            // does not terminate the stream.
            return Ok(());
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(serde_json::Value::as_str)
                && !text.is_empty()
            {
                out.push(StreamEvent::TextDelta(text.to_owned()));
            }
            if let Some(text) = delta.get("refusal").and_then(serde_json::Value::as_str)
                && !text.is_empty()
            {
                out.push(StreamEvent::TextDelta(text.to_owned()));
            }
            // Vendors disagree on the thinking-delta field name:
            // DeepSeek-style `reasoning_content` vs OpenRouter-style
            // `reasoning`. Both carry the same concept.
            let reasoning = delta
                .get("reasoning_content")
                .and_then(serde_json::Value::as_str)
                .or_else(|| delta.get("reasoning").and_then(serde_json::Value::as_str));
            if let Some(reasoning) = reasoning
                && !reasoning.is_empty()
            {
                out.push(StreamEvent::ReasoningDelta(reasoning.to_owned()));
            }
            if let Some(tool_calls) = delta
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
            {
                for call in tool_calls {
                    let index = call
                        .get("index")
                        .and_then(serde_json::Value::as_u64)
                        .map_or(self.tool_index, |value| value as u32);
                    self.tool_index = self.tool_index.max(index);
                    let function = call.get("function");
                    out.push(StreamEvent::ToolCallDelta {
                        index,
                        id: call
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        name: function
                            .and_then(|function| function.get("name"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        arguments: function
                            .and_then(|function| function.get("arguments"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    });
                }
            }
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
        {
            if reason == "error" {
                return Err(ChatError::Upstream("upstream stream failed".to_owned()));
            }
            out.push(StreamEvent::Finish {
                reason: reason.to_owned(),
            });
        }
        Ok(())
    }
}

/// Best-effort human-readable text for an in-band error object.
fn describe_error(error: &serde_json::Value) -> String {
    if let Some(message) = error.get("message").and_then(serde_json::Value::as_str) {
        return message.to_owned();
    }
    if let Some(text) = error.as_str() {
        return text.to_owned();
    }
    error.to_string()
}

/// Extract usage exactly like cardinal's chat kernel: both `prompt_tokens`
/// and `completion_tokens` must be present, then the optional cache-hit and
/// reasoning details are picked up from either vendor shape.
fn usage_from(value: &serde_json::Value) -> Option<Usage> {
    let usage = value.get("usage")?;
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_u64)?;
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(serde_json::Value::as_u64)?;
    Some(Usage::from_cardinal_chat(
        prompt_tokens,
        completion_tokens,
        usage
            .get("prompt_cache_hit_tokens")
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
            })
            .and_then(serde_json::Value::as_u64),
        usage
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .or_else(|| usage.get("reasoning_tokens"))
            .and_then(serde_json::Value::as_u64),
        usage
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens_included"))
            .and_then(serde_json::Value::as_bool),
        CardinalChatUsageProof::new(),
    ))
}

/// A tool call reconstructed from its streaming fragments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AssembledToolCall {
    /// Call id (from the first fragment that carried one).
    pub id: String,
    /// Function name (from the first fragment that carried one).
    pub name: String,
    /// Concatenated argument JSON.
    pub arguments: String,
}

/// The assistant message a stream assembled into.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AssembledMessage {
    /// The full text content.
    pub text: String,
    /// The full reasoning content (empty when the vendor sent none).
    pub reasoning: String,
    /// Tool calls in `index` order.
    pub tool_calls: Vec<AssembledToolCall>,
    /// The last `finish_reason` seen, raw.
    pub finish_reason: Option<String>,
    /// The last usage seen.
    pub usage: Option<Usage>,
    /// Whether `[DONE]` was observed.
    pub done: bool,
}

/// Folds [`StreamEvent`]s into an [`AssembledMessage`].
#[derive(Debug, Default)]
pub struct Assembler {
    message: AssembledMessage,
    tool_calls: BTreeMap<u32, AssembledToolCall>,
}

impl Assembler {
    /// A fresh assembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event in.
    pub fn push(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta(text) => self.message.text.push_str(text),
            StreamEvent::ReasoningDelta(text) => self.message.reasoning.push_str(text),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                let call = self.tool_calls.entry(*index).or_default();
                if let Some(id) = id
                    && call.id.is_empty()
                {
                    call.id.clone_from(id);
                }
                if let Some(name) = name
                    && call.name.is_empty()
                {
                    call.name.clone_from(name);
                }
                call.arguments.push_str(arguments);
            }
            StreamEvent::Finish { reason } => self.message.finish_reason = Some(reason.clone()),
            StreamEvent::Usage(usage) => self.message.usage = Some(*usage),
            StreamEvent::Done => self.message.done = true,
        }
    }

    /// Fold many events in.
    pub fn extend<'a>(&mut self, events: impl IntoIterator<Item = &'a StreamEvent>) {
        for event in events {
            self.push(event);
        }
    }

    /// Consume the assembler and return the assembled message with tool calls
    /// in `index` order.
    #[must_use]
    pub fn finish(mut self) -> AssembledMessage {
        self.message.tool_calls = self.tool_calls.into_values().collect();
        self.message
    }
}
