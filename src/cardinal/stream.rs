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
//!    exact vocabulary of cardinal's `StreamDecoder::observe_raw` (both the
//!    DeepSeek `reasoning_content` and OpenRouter `reasoning` field names,
//!    the running tool-call index fallback, and usage-only chunks with an
//!    empty `choices` array that do not terminate the stream).
//! 3. [`Assembler`] — the deltas folded back into a final assistant message:
//!    text, reasoning, and tool calls keyed by index with concatenated
//!    argument fragments.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{CardinalError, MAX_EVENT_BYTES, MAX_LINE_BYTES};
use crate::cardinal::wire::Usage;

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

/// Incremental SSE framer: feed bytes, receive complete `data` payloads.
///
/// Only the `data` field is collected; `event:`/`id:` and any unknown field
/// are ignored, and `:`-prefixed comment lines (cardinal's keep-alives) are
/// dropped — the same tolerance cardinal's framer shows.
#[derive(Debug, Default)]
pub struct SseFramer {
    line: Vec<u8>,
    data: Vec<u8>,
    event_bytes: usize,
    skip_lf: bool,
    started: bool,
}

impl SseFramer {
    /// A fresh framer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte chunk, appending every completed payload to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<String>) -> Result<(), CardinalError> {
        let mut bytes = chunk;
        if !self.started {
            self.started = true;
            if bytes.len() >= 3 && bytes[..3] == [0xEF, 0xBB, 0xBF] {
                bytes = &bytes[3..];
            }
        }
        for byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(out)?,
                b'\r' => {
                    self.end_line(out)?;
                    self.skip_lf = true;
                }
                _ => {
                    self.line.push(*byte);
                    if self.line.len() > MAX_LINE_BYTES {
                        return Err(CardinalError::Sse(format!(
                            "SSE line exceeds {MAX_LINE_BYTES} bytes"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Flush a trailing partial line/event (a connection that closed without a
    /// final blank line). Returns any final payload via `out`.
    pub fn finish(&mut self, out: &mut Vec<String>) {
        if !self.line.is_empty() {
            // Ignore framing errors on the flush path: the event boundary is
            // best-effort, and an oversized trailing line is reported on the
            // next `push` anyway.
            let _ = self.end_line(out);
        }
        if !self.data.is_empty() {
            out.push(self.take_event());
        }
    }

    fn end_line(&mut self, out: &mut Vec<String>) -> Result<(), CardinalError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if !self.data.is_empty() {
                out.push(self.take_event());
            }
            return Ok(());
        }
        // Comment (keep-alive) lines: everything up to the first `:` is the
        // field; a leading `:` means the whole line is a comment.
        if line[0] == b':' {
            return Ok(());
        }
        let value = match line.iter().position(|byte| *byte == b':') {
            Some(position) => {
                let mut value = &line[position + 1..];
                if value.first() == Some(&b' ') {
                    value = &value[1..];
                }
                value
            }
            // A line without a colon is a field with an empty value; for a
            // `data`-only consumer it carries nothing.
            None => return Ok(()),
        };
        if line[0] != b'd' || !line.starts_with(b"data:") {
            // `event:`, `id:`, and unknown fields are ignored.
            return Ok(());
        }
        let value = std::str::from_utf8(value)
            .map_err(|_| CardinalError::Sse("SSE payload is not valid UTF-8".to_owned()))?;
        self.event_bytes += value.len() + 1;
        if self.event_bytes > MAX_EVENT_BYTES {
            return Err(CardinalError::Sse(format!(
                "SSE event exceeds {MAX_EVENT_BYTES} bytes"
            )));
        }
        self.data.extend_from_slice(value.as_bytes());
        self.data.push(b'\n');
        Ok(())
    }

    fn take_event(&mut self) -> String {
        if self.data.last() == Some(&b'\n') {
            self.data.pop();
        }
        self.event_bytes = 0;
        let payload = String::from_utf8_lossy(&self.data).into_owned();
        self.data.clear();
        payload
    }
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

impl ChunkDecoder {
    /// A fresh decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one `data:` payload, appending events to `out`.
    ///
    /// `[DONE]` yields [`StreamEvent::Done`]; an empty payload yields nothing.
    /// A payload that is not valid JSON is fatal (cardinal returns a failed
    /// turn for it too); an in-band `error` object becomes
    /// [`CardinalError::Upstream`].
    pub fn decode(
        &mut self,
        payload: &str,
        out: &mut Vec<StreamEvent>,
    ) -> Result<(), CardinalError> {
        let data = payload.trim();
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            out.push(StreamEvent::Done);
            return Ok(());
        }
        let value: serde_json::Value = serde_json::from_str(data)
            .map_err(|error| CardinalError::Sse(format!("invalid JSON payload: {error}")))?;
        if let Some(error) = value.get("error")
            && !error.is_null()
        {
            return Err(CardinalError::Upstream(describe_error(error)));
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
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        cached_tokens: usage
            .get("prompt_cache_hit_tokens")
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
            })
            .and_then(serde_json::Value::as_u64),
        reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .or_else(|| usage.get("reasoning_tokens"))
            .and_then(serde_json::Value::as_u64),
    })
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
    pub fn finish(mut self) -> AssembledMessage {
        self.message.tool_calls = self.tool_calls.into_values().collect();
        self.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cardinal::wire::Usage;

    /// Run bytes through the framer + decoder, splitting the input into small
    /// chunks (3 bytes) so incremental framing (split lines, split CRLF) is
    /// exercised on every fixture.
    fn decode_bytes(input: &[u8], split: usize) -> Result<Vec<StreamEvent>, CardinalError> {
        let mut framer = SseFramer::new();
        let mut decoder = ChunkDecoder::new();
        let mut events = Vec::new();
        let mut payloads = Vec::new();
        for chunk in input.chunks(split) {
            framer.push(chunk, &mut payloads)?;
            for payload in payloads.drain(..) {
                decoder.decode(&payload, &mut events)?;
            }
        }
        framer.finish(&mut payloads);
        for payload in payloads.drain(..) {
            decoder.decode(&payload, &mut events)?;
        }
        Ok(events)
    }

    /// Convenience wrapper asserting success.
    fn events(input: &str) -> Vec<StreamEvent> {
        decode_bytes(input.as_bytes(), 3).unwrap()
    }

    #[test]
    fn text_deltas_accumulate_in_order() {
        let input = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                     data: [DONE]\n\n";
        let mut assembler = Assembler::new();
        let parsed = events(input);
        assert_eq!(
            parsed,
            vec![
                StreamEvent::TextDelta("Hel".to_owned()),
                StreamEvent::TextDelta("lo".to_owned()),
                StreamEvent::Finish {
                    reason: "stop".to_owned()
                },
                StreamEvent::Done,
            ]
        );
        assembler.extend(&parsed);
        let message = assembler.finish();
        assert_eq!(message.text, "Hello");
        assert_eq!(message.finish_reason.as_deref(), Some("stop"));
        assert!(message.done);
        assert!(message.tool_calls.is_empty());
    }

    #[test]
    fn empty_content_delta_is_skipped() {
        let input = "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\n\n\
                     data: [DONE]\n\n";
        assert_eq!(events(input), vec![StreamEvent::Done]);
    }

    #[test]
    fn reasoning_deltas_support_both_vendor_field_names() {
        let input = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"reasoning\":\"more\"},\"finish_reason\":null}]}\n\n\
                     data: [DONE]\n\n";
        let parsed = events(input);
        assert_eq!(
            parsed,
            vec![
                StreamEvent::ReasoningDelta("think ".to_owned()),
                StreamEvent::ReasoningDelta("more".to_owned()),
                StreamEvent::Done,
            ]
        );
        let mut assembler = Assembler::new();
        assembler.extend(&parsed);
        assert_eq!(assembler.finish().reasoning, "think more");
    }

    #[test]
    fn tool_call_fragments_assemble_across_chunks() {
        // Fragments modeled on cardinal's emission: id+name on the first
        // chunk, argument JSON split across two more.
        let input = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                     data: [DONE]\n\n";
        let parsed = events(input);
        assert_eq!(
            parsed,
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_a".to_owned()),
                    name: Some("read".to_owned()),
                    arguments: String::new(),
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: "{\"path\":".to_owned(),
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: "\"/tmp\"}".to_owned(),
                },
                StreamEvent::Finish {
                    reason: "tool_calls".to_owned()
                },
                StreamEvent::Done,
            ]
        );
        let mut assembler = Assembler::new();
        assembler.extend(&parsed);
        let message = assembler.finish();
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].id, "call_a");
        assert_eq!(message.tool_calls[0].name, "read");
        assert_eq!(message.tool_calls[0].arguments, "{\"path\":\"/tmp\"}");
        assert_eq!(message.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn parallel_tool_calls_are_keyed_by_index() {
        let input = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"glob\",\"arguments\":\"{\\\"p\\\":\"}}]},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":null}]}\n\n\
                     data: [DONE]\n\n";
        let mut assembler = Assembler::new();
        assembler.extend(&events(input));
        let message = assembler.finish();
        assert_eq!(message.tool_calls.len(), 2);
        assert_eq!(message.tool_calls[0].name, "read");
        assert_eq!(message.tool_calls[0].arguments, "{}");
        assert_eq!(message.tool_calls[1].id, "b");
        assert_eq!(message.tool_calls[1].name, "glob");
        assert_eq!(message.tool_calls[1].arguments, "{\"p\":1}");
    }

    #[test]
    fn missing_tool_index_falls_back_to_running_counter() {
        // Cardinal's decoder: a fragment without `index` continues the
        // current call (`unwrap_or(tool_index)`, then `max`). Later fragments
        // of one call are the real-world case.
        let input = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n\
                     data: [DONE]\n\n";
        let parsed = events(input);
        assert_eq!(
            parsed[0],
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("a".to_owned()),
                name: Some("read".to_owned()),
                arguments: String::new(),
            }
        );
        assert_eq!(
            parsed[1],
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: "{}".to_owned(),
            }
        );
    }

    #[test]
    fn usage_only_chunk_with_empty_choices_does_not_terminate() {
        let input = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":3},\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n\
                     data: [DONE]\n\n";
        let mut assembler = Assembler::new();
        assembler.extend(&events(input));
        let message = assembler.finish();
        assert_eq!(message.text, "ok");
        assert_eq!(
            message.usage,
            Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 7,
                cached_tokens: Some(3),
                reasoning_tokens: Some(2),
            })
        );
        assert!(message.done);
    }

    #[test]
    fn usage_extracts_both_vendor_detail_shapes() {
        // DeepSeek style: flat `prompt_cache_hit_tokens` + details object.
        let input = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"prompt_cache_hit_tokens\":4,\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\ndata: [DONE]\n\n";
        let parsed = events(input);
        assert_eq!(
            parsed[0],
            StreamEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                cached_tokens: Some(4),
                reasoning_tokens: Some(2),
            })
        );
        // OpenAI style: prompt_tokens_details.cached_tokens + bare
        // `reasoning_tokens`.
        let input = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":6},\"reasoning_tokens\":1}}\n\ndata: [DONE]\n\n";
        let parsed = events(input);
        assert_eq!(
            parsed[0],
            StreamEvent::Usage(Usage {
                prompt_tokens: 7,
                completion_tokens: 3,
                cached_tokens: Some(6),
                reasoning_tokens: Some(1),
            })
        );
    }

    #[test]
    fn usage_without_both_mandatory_counts_is_ignored() {
        let input = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12}}\n\n\
                     data: [DONE]\n\n";
        assert_eq!(events(input), vec![StreamEvent::Done]);
    }

    #[test]
    fn finish_reasons_are_preserved_raw() {
        for reason in ["stop", "tool_calls", "length", "content_filter"] {
            let input = format!(
                "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}\n\ndata: [DONE]\n\n"
            );
            assert_eq!(
                events(&input),
                vec![
                    StreamEvent::Finish {
                        reason: reason.to_owned()
                    },
                    StreamEvent::Done,
                ],
                "finish_reason {reason} must survive verbatim"
            );
        }
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        // SSE allows a payload split over several `data:` lines.
        let input = "data: {\"choices\":[{\"delta\":{\"content\":\ndata: \"joined\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
        assert_eq!(
            events(input),
            vec![
                StreamEvent::TextDelta("joined".to_owned()),
                StreamEvent::Done
            ]
        );
    }

    #[test]
    fn crlf_line_endings_and_comments_are_tolerated() {
        let input = ": ping\r\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\r\n\
                     \r\n\
                     : keep-alive\r\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"b\"},\"finish_reason\":null}]}\r\n\
                     \r\n\
                     event: message\r\n\
                     data: [DONE]\r\n\
                     \r\n";
        assert_eq!(
            events(input),
            vec![
                StreamEvent::TextDelta("a".to_owned()),
                StreamEvent::TextDelta("b".to_owned()),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn leading_bom_is_stripped() {
        let input = "\u{feff}data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
        assert_eq!(
            events(input),
            vec![StreamEvent::TextDelta("x".to_owned()), StreamEvent::Done]
        );
    }

    #[test]
    fn malformed_framing_lines_are_skipped_not_fatal() {
        // Garbage line without a colon, comment, unknown field, and a
        // `data ` line (space before the colon is a different field name per
        // the SSE spec) — all tolerated; the real payload still decodes.
        let input = "garbage\n\
                     :comment without space\n\
                     retry: 3000\n\
                     data : {\"choices\":[]}\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                     data: [DONE]\n\n";
        assert_eq!(
            events(input),
            vec![StreamEvent::TextDelta("ok".to_owned()), StreamEvent::Done]
        );
    }

    #[test]
    fn invalid_json_payload_is_fatal() {
        let error = decode_bytes(b"data: {not json}\n\n", 3).unwrap_err();
        assert!(
            matches!(error, CardinalError::Sse(ref message) if message.contains("invalid JSON")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn in_band_error_object_is_reported() {
        let error = decode_bytes(
            b"data: {\"error\":{\"message\":\"upstream exploded\",\"type\":\"server_error\"}}\n\n",
            3,
        )
        .unwrap_err();
        assert!(
            matches!(error, CardinalError::Upstream(ref message) if message == "upstream exploded"),
            "unexpected error: {error}"
        );
        // A literal `"error": null` is not an error.
        assert_eq!(
            events(
                "data: {\"error\":null,\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
            ),
            vec![StreamEvent::TextDelta("a".to_owned()), StreamEvent::Done]
        );
    }

    #[test]
    fn trailing_partial_event_is_flushed() {
        // No final blank line before EOF: the last payload still decodes.
        let input = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\ndata: [DONE]";
        assert_eq!(
            events(input),
            vec![StreamEvent::TextDelta("x".to_owned()), StreamEvent::Done]
        );
    }

    #[test]
    fn oversized_line_is_fatal() {
        let mut framer = SseFramer::new();
        let mut out = Vec::new();
        let big = vec![b'x'; MAX_LINE_BYTES + 2];
        assert!(matches!(
            framer.push(&big, &mut out),
            Err(CardinalError::Sse(_))
        ));
    }
}
