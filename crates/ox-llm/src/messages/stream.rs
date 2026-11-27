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

use super::replay::{MessagesReplayState, ReplayBlock};
use crate::chat_completions::stream::StreamEvent;
use crate::chat_completions::wire::Usage;
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
    /// Identity of the endpoint producing this response, when it can be
    /// trusted for a native thinking replay.
    replay_identity: Option<(String, String, String)>,
    /// Thinking blocks by Messages content-block index, in wire order.
    replay_blocks: BTreeMap<u64, PendingBlock>,
    /// Where text and tool calls sat relative to those blocks.
    replay_text_index: Option<u64>,
    replay_tool_index: Option<u64>,
    /// Whether a replay state was already emitted for this stream.
    replay_emitted: bool,
}

/// One thinking block under assembly. Only a stopped block with the opaque
/// bytes the endpoint needs is ever emitted for replay.
#[derive(Debug)]
enum PendingBlock {
    Thinking {
        text: String,
        signature: String,
        done: bool,
    },
    RedactedThinking {
        data: String,
        done: bool,
    },
}

impl PendingBlock {
    fn thinking() -> Self {
        Self::Thinking {
            text: String::new(),
            signature: String::new(),
            done: false,
        }
    }

    fn append_thinking(&mut self, text: &str) {
        if let Self::Thinking { text: into, .. } = self {
            into.push_str(text);
        }
    }

    fn append_signature(&mut self, signature: &str) {
        if let Self::Thinking {
            signature: into, ..
        } = self
        {
            into.push_str(signature);
        }
    }

    fn append_redacted(&mut self, data: &str) {
        if let Self::RedactedThinking { data: into, .. } = self {
            into.push_str(data);
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Thinking { done, .. } | Self::RedactedThinking { done, .. } => *done = true,
        }
    }

    fn finish(&self, index: u64) -> Option<ReplayBlock> {
        match self {
            Self::Thinking {
                text,
                signature,
                done: true,
            } if !text.is_empty() && !signature.is_empty() => Some(ReplayBlock::Thinking {
                index,
                thinking: text.clone(),
                signature: signature.clone(),
            }),
            Self::RedactedThinking { data, done: true } if !data.is_empty() => {
                Some(ReplayBlock::RedactedThinking {
                    index,
                    data: data.clone(),
                })
            }
            _ => None,
        }
    }
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
    /// counts. Cache-write tokens are preserved separately as well.
    fn merged(&self) -> Option<Usage> {
        if !self.seen {
            return None;
        }
        let prompt = self.input.unwrap_or(0)
            + self.cache_read.unwrap_or(0)
            + self.cache_creation.unwrap_or(0);
        Some(Usage::new(
            prompt,
            self.output.unwrap_or(0),
            self.cache_read,
            self.cache_creation,
            None,
        ))
    }
}

impl MessagesDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind decoding to the endpoint that produced the response. Without a
    /// binding no signatures or opaque blocks are retained.
    #[must_use]
    pub fn with_replay(
        mut self,
        provider: impl Into<String>,
        endpoint: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.replay_identity = Some((provider.into(), endpoint.into(), model.into()));
        self
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
                match block_type {
                    "tool_use" => {
                        if self.replay_identity.is_some() && self.replay_tool_index.is_none() {
                            self.replay_tool_index = Some(index);
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
                            // The initial `input` is `{}` for every real call;
                            // arguments arrive as `input_json_delta` fragments.
                            arguments: String::new(),
                        });
                    }
                    "thinking" if self.replay_identity.is_some() => {
                        self.replay_blocks.insert(
                            index,
                            PendingBlock::Thinking {
                                text: block
                                    .and_then(|block| block.get("thinking"))
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                signature: String::new(),
                                done: false,
                            },
                        );
                    }
                    "redacted_thinking" if self.replay_identity.is_some() => {
                        self.replay_blocks.insert(
                            index,
                            PendingBlock::RedactedThinking {
                                data: block
                                    .and_then(|block| block.get("data"))
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                done: false,
                            },
                        );
                    }
                    "text"
                        if self.replay_identity.is_some() && self.replay_text_index.is_none() =>
                    {
                        self.replay_text_index = Some(index);
                    }
                    _ => {}
                }
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
                            if self.replay_identity.is_some() {
                                self.replay_blocks
                                    .entry(index)
                                    .or_insert_with(PendingBlock::thinking)
                                    .append_thinking(thinking);
                            }
                            out.push(StreamEvent::ReasoningDelta(thinking.to_owned()));
                        }
                    }
                    "signature_delta" => {
                        if self.replay_identity.is_some()
                            && let Some(signature) = delta.get("signature").and_then(Value::as_str)
                            && !signature.is_empty()
                        {
                            self.replay_blocks
                                .entry(index)
                                .or_insert_with(PendingBlock::thinking)
                                .append_signature(signature);
                        }
                    }
                    "redacted_thinking" => {
                        if self.replay_identity.is_some()
                            && let Some(data) = delta.get("data").and_then(Value::as_str)
                            && !data.is_empty()
                        {
                            self.replay_blocks
                                .entry(index)
                                .or_insert_with(|| PendingBlock::RedactedThinking {
                                    data: String::new(),
                                    done: false,
                                })
                                .append_redacted(data);
                        }
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
            "content_block_stop" => {
                if let Some(index) = value.get("index").and_then(Value::as_u64)
                    && let Some(pending) = self.replay_blocks.get_mut(&index)
                {
                    pending.stop();
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
                let terminal = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    .is_some();
                if terminal
                    && !self.usage_emitted
                    && let Some(usage) = self.usage.merged()
                {
                    self.usage_emitted = true;
                    out.push(StreamEvent::Usage(usage));
                }
            }
            "message_stop" => {
                if !self.usage_emitted
                    && let Some(usage) = self.usage.merged()
                {
                    self.usage_emitted = true;
                    out.push(StreamEvent::Usage(usage));
                }
                if !self.replay_emitted
                    && let Some((provider, endpoint, model)) = self.replay_identity.clone()
                {
                    let blocks: Vec<ReplayBlock> = self
                        .replay_blocks
                        .iter()
                        .filter_map(|(index, block)| block.finish(*index))
                        .collect();
                    if !blocks.is_empty() {
                        let state = MessagesReplayState::new(
                            provider,
                            endpoint,
                            model,
                            blocks,
                            self.replay_text_index,
                            self.replay_tool_index,
                        );
                        let record = state
                            .into_provider_state()
                            .map_err(|error| ChatError::Sse(format!("replay state: {error}")))?;
                        self.replay_emitted = true;
                        out.push(StreamEvent::ProviderState(record));
                    }
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
