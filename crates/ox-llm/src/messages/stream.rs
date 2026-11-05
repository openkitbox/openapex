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
