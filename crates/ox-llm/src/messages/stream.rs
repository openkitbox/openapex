//! Messages SSE decoding: `message_start` / `content_block_*` /
//! `message_delta` / `message_stop` into the engine's [`StreamEvent`]
//! vocabulary.
//!
//! The engine consumes one event language (`TextDelta`, `ReasoningDelta`,
//! `ToolCallDelta`, `Finish`, `Usage`, `Done`), so the protocol difference ends
//! here: text and thinking deltas pass through, `input_json_delta` fragments
//! become tool-call argument fragments keyed by a dense tool ordinal (Messages
//! indexes *content blocks*, of which only some are tool calls), and usage is
//! merged from the two places Messages reports it.
//!
//! Terminal contract: `message_delta` carries the stop reason and is the
//! stream's terminal signal for the shared driver; `message_stop` is the
//! explicit terminator. An `error` payload fails the stream.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::cardinal::stream::StreamEvent;
use crate::cardinal::wire::Usage;
use crate::error::ChatError;
use crate::sse::SseDecoder;

/// The `anthropic-version` every Messages request declares.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Decodes one Messages stream.
#[derive(Debug, Default)]
pub struct MessagesDecoder {
    /// Messages content-block index → the dense tool ordinal the engine sees.
    tools: BTreeMap<u64, u32>,
    /// How many tool calls this response has started.
    next_tool: u32,
    /// Input-side usage from `message_start` (cumulative output arrives later).
    usage: UsageAccum,
    /// Whether a usage event was already emitted for this stream.
    usage_emitted: bool,
}

/// The running usage total. Messages splits it: `message_start` reports the
/// input side, `message_delta` reports (cumulative) output.
#[derive(Debug, Default)]
struct UsageAccum {
    input: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    output: Option<u64>,
    seen: bool,
}

impl UsageAccum {
    fn absorb(&mut self, usage: &Value) {
        let field = |name: &str| usage.get(name).and_then(Value::as_u64);
        if let Some(value) = field("input_tokens") {
            self.input = Some(value);
        }
        if let Some(value) = field("cache_read_input_tokens") {
            self.cache_read = Some(value);
        }
        if let Some(value) = field("cache_creation_input_tokens") {
            self.cache_creation = Some(value);
        }
        if let Some(value) = field("output_tokens") {
            // `message_delta` reports a cumulative total, never an increment.
            self.output = Some(value);
        }
        self.seen = true;
    }

    /// The ox usage view: prompt tokens include every input class (fresh,
    /// cache-read and cache-write), because that is what the context budget
    /// counts. `None` until anything was reported.
    fn merged(&self) -> Option<Usage> {
        if !self.seen {
            return None;
        }
        let prompt = self.input.unwrap_or(0)
            + self.cache_read.unwrap_or(0)
            + self.cache_creation.unwrap_or(0);
        Some(Usage {
            prompt_tokens: prompt,
            completion_tokens: self.output.unwrap_or(0),
            cached_tokens: self.cache_read,
            reasoning_tokens: None,
        })
    }
}

impl MessagesDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The tool ordinal for a content-block index, allocating one if the block
    /// was never announced.
    fn tool_ordinal(&mut self, block: u64) -> u32 {
        *self.tools.entry(block).or_insert_with(|| {
            let ordinal = self.next_tool;
            self.next_tool += 1;
            ordinal
        })
    }
}

impl SseDecoder for MessagesDecoder {
    fn decode(&mut self, payload: &str, out: &mut Vec<StreamEvent>) -> Result<(), ChatError> {
        let payload = payload.trim();
        if payload.is_empty() {
            return Ok(());
        }
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| ChatError::Sse(format!("invalid JSON payload: {error}")))?;
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return Ok(());
        };
        match kind {
            "message_start" => {
                if let Some(usage) = value
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    self.usage.absorb(usage);
                }
            }
            "content_block_start" => {
                let Some(index) = value.get("index").and_then(Value::as_u64) else {
                    return Ok(());
                };
                let block = value.get("content_block");
                let block_type = block
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if block_type != "tool_use" {
                    return Ok(());
                }
                let ordinal = self.tool_ordinal(index);
                out.push(StreamEvent::ToolCallDelta {
                    index: ordinal,
                    id: block
                        .and_then(|block| block.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    name: block
                        .and_then(|block| block.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    // The initial `input` is `{}` for every real call; the
                    // arguments arrive as `input_json_delta` fragments.
                    arguments: String::new(),
                });
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(delta) = value.get("delta") else {
                    return Ok(());
                };
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(StreamEvent::TextDelta(text.to_owned()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(thinking) = delta.get("thinking").and_then(Value::as_str)
                            && !thinking.is_empty()
                        {
                            out.push(StreamEvent::ReasoningDelta(thinking.to_owned()));
                        }
                    }
                    "signature_delta" => {
                        // A signature authenticates this exact thinking block
                        // for replay. ox does not store one yet (rows carry no
                        // opaque provider metadata), so it is dropped rather
                        // than kept for the life of one process.
                        let signature_bytes = delta
                            .get("signature")
                            .and_then(|signature| signature.as_str())
                            .map_or(0, str::len);
                        tracing::debug!(
                            signature_bytes,
                            "messages thinking signature is not persisted; the block replays unsigned"
                        );
                    }
                    "input_json_delta" => {
                        let ordinal = self.tool_ordinal(index);
                        out.push(StreamEvent::ToolCallDelta {
                            index: ordinal,
                            id: None,
                            name: None,
                            arguments: delta
                                .get("partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        });
                    }
                    // `citations_delta` and anything newer: not modelled.
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(usage) = value.get("usage") {
                    self.usage.absorb(usage);
                }
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    out.push(StreamEvent::Finish {
                        reason: reason.to_owned(),
                    });
                }
                if let Some(usage) = self.usage.merged() {
                    self.usage_emitted = true;
                    out.push(StreamEvent::Usage(usage));
                }
            }
            "message_stop" => {
                if !self.usage_emitted
                    && let Some(usage) = self.usage.merged()
                {
                    out.push(StreamEvent::Usage(usage));
                }
                out.push(StreamEvent::Done);
            }
            "ping" => {}
            "error" => {
                let error = value.get("error");
                let message = error
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| payload.to_owned());
                let kind = error
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str);
                return Err(match kind {
                    Some(kind) => ChatError::Upstream(format!("{kind}: {message}")),
                    None => ChatError::Upstream(message),
                });
            }
            // Forward compatibility: a new event type must not fail the turn.
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(decoder: &mut MessagesDecoder, payloads: &[&str]) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        for payload in payloads {
            decoder.decode(payload, &mut out).expect("decode");
        }
        out
    }

    #[test]
    fn text_thinking_and_signature_deltas_map_to_the_engine_vocabulary() {
        let mut decoder = MessagesDecoder::new();
        let events = decode_all(
            &mut decoder,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningDelta("hmm".to_owned()),
                StreamEvent::TextDelta("hello".to_owned()),
                StreamEvent::Finish {
                    reason: "end_turn".to_owned()
                },
                StreamEvent::Usage(Usage {
                    prompt_tokens: 15,
                    completion_tokens: 7,
                    cached_tokens: Some(5),
                    reasoning_tokens: None,
                }),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn tool_calls_get_dense_ordinals_independent_of_block_index() {
        let mut decoder = MessagesDecoder::new();
        let events = decode_all(
            &mut decoder,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t2","name":"glob"}}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("t1".to_owned()),
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
                    arguments: "\"x\"}".to_owned(),
                },
                StreamEvent::ToolCallDelta {
                    index: 1,
                    id: Some("t2".to_owned()),
                    name: Some("glob".to_owned()),
                    arguments: String::new(),
                },
            ]
        );
    }

    #[test]
    fn an_error_event_fails_the_stream_with_its_type_and_message() {
        let mut decoder = MessagesDecoder::new();
        let mut out = Vec::new();
        let error = decoder
            .decode(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
                &mut out,
            )
            .expect_err("error payload");
        assert!(error.to_string().contains("overloaded_error"));
        assert!(error.to_string().contains("busy"));
    }

    #[test]
    fn an_unknown_event_type_is_ignored() {
        let mut decoder = MessagesDecoder::new();
        let events = decode_all(&mut decoder, &[r#"{"type":"tool_projection_warning"}"#]);
        assert!(events.is_empty());
    }

    #[test]
    fn usage_survives_when_only_message_start_reported_it() {
        let mut decoder = MessagesDecoder::new();
        let events = decode_all(
            &mut decoder,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":1}}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert!(matches!(
            events.as_slice(),
            [
                StreamEvent::Usage(Usage {
                    prompt_tokens: 3,
                    completion_tokens: 1,
                    ..
                }),
                StreamEvent::Done
            ]
        ));
    }
}
