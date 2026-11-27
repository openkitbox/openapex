//! Responses SSE decoding: typed Responses events become the engine's
//! [`StreamEvent`] vocabulary.
//!
//! Function calls are announced by `response.output_item.added` with an item
//! id, while argument deltas refer to that item id rather than a call ordinal.
//! The decoder keeps only a small item-id → (ordinal, call id, name) map for
//! the life of one stream; it never rebuilds or copies the transcript.
//!
//! Terminal contract: `response.completed` emits usage (when reported),
//! `Finish { reason: "stop" }` and `Done`; `response.incomplete` emits the same
//! shape with `Finish { reason: "length" }`. These reasons are the engine's
//! `map_finish` vocabulary — the upstream event names are not recognized
//! there. `response.failed`, `response.cancelled` and `error` fail the stream.
//! Unknown event types are ignored so a new upstream event cannot break a turn.

use std::collections::HashMap;

use serde_json::Value;

use crate::chat_completions::stream::StreamEvent;
use crate::chat_completions::wire::Usage;
use crate::error::ChatError;
use crate::sse::SseDecoder;

/// Decodes one Responses stream.
#[derive(Debug, Default)]
pub struct ResponsesDecoder {
    /// Announced function-call items: item id → the engine's view of the call.
    tools: HashMap<String, ToolIdentity>,
    /// How many tool calls this stream has seen.
    next_tool: u32,
}

/// What the engine needs to know about one function-call item.
#[derive(Debug)]
struct ToolIdentity {
    /// Dense ordinal in this response.
    index: u32,
    /// Call id echoed by the matching result.
    call_id: Option<String>,
    /// Function name.
    name: Option<String>,
}

impl ResponsesDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The identity for `item_id`, allocating a dense ordinal if the item was
    /// never announced (a forward-compatible fallback for new event orders).
    fn identity(&mut self, item_id: &str) -> &mut ToolIdentity {
        let next = &mut self.next_tool;
        self.tools.entry(item_id.to_owned()).or_insert_with(|| {
            let index = *next;
            *next += 1;
            ToolIdentity {
                index,
                call_id: None,
                name: None,
            }
        })
    }
}

impl SseDecoder for ResponsesDecoder {
    fn decode(&mut self, payload: &str, out: &mut Vec<StreamEvent>) -> Result<(), ChatError> {
        let payload = payload.trim();
        if payload.is_empty() {
            return Ok(());
        }
        if payload == "[DONE]" {
            out.push(StreamEvent::Done);
            return Ok(());
        }
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| ChatError::Sse(format!("invalid JSON payload: {error}")))?;
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return Ok(());
        };
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(StreamEvent::TextDelta(delta.to_owned()));
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(StreamEvent::ReasoningDelta(delta.to_owned()));
                }
            }
            "response.output_item.added" => {
                let Some(item) = value.get("item") else {
                    return Ok(());
                };
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return Ok(());
                }
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let identity = self.identity(id);
                identity.call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                identity.name = item.get("name").and_then(Value::as_str).map(str::to_owned);
            }
            "response.function_call_arguments.delta" => {
                let (Some(item_id), Some(delta)) = (
                    value.get("item_id").and_then(Value::as_str),
                    value.get("delta").and_then(Value::as_str),
                ) else {
                    return Ok(());
                };
                if delta.is_empty() {
                    return Ok(());
                }
                let identity = self.identity(item_id);
                out.push(StreamEvent::ToolCallDelta {
                    index: identity.index,
                    id: identity.call_id.clone(),
                    name: identity.name.clone(),
                    arguments: delta.to_owned(),
                });
            }
            "response.completed" | "response.incomplete" => {
                if let Some(usage) = usage_from(&value) {
                    out.push(StreamEvent::Usage(usage));
                }
                let reason = if kind == "response.completed" {
                    "stop"
                } else {
                    "length"
                };
                out.push(StreamEvent::Finish {
                    reason: reason.to_owned(),
                });
                out.push(StreamEvent::Done);
            }
            "response.failed" => {
                return Err(upstream_error(&value, "response failed"));
            }
            "response.cancelled" => {
                return Err(upstream_error(&value, "response cancelled"));
            }
            "error" => {
                return Err(upstream_error(&value, "upstream error"));
            }
            // Forward compatibility: a new event type must not fail the turn.
            _ => {}
        }
        Ok(())
    }
}

/// The usage object of a terminal response event, when present.
fn usage_from(value: &Value) -> Option<Usage> {
    let usage = value
        .get("response")
        .and_then(|response| response.get("usage"))
        .or_else(|| value.get("usage"))?;
    let count = |parent: Option<&Value>, field: &str| {
        parent
            .and_then(|parent| parent.get(field))
            .and_then(Value::as_u64)
    };
    let input_details = usage.get("input_tokens_details");
    let output_details = usage.get("output_tokens_details");
    Some(Usage::new(
        count(Some(usage), "input_tokens").unwrap_or(0),
        count(Some(usage), "output_tokens").unwrap_or(0),
        count(input_details, "cached_tokens"),
        count(input_details, "cache_write_tokens"),
        count(output_details, "reasoning_tokens"),
    ))
}

/// An upstream failure message, with the provider's code when it sent one.
fn upstream_error(value: &Value, fallback: &str) -> ChatError {
    let error = value
        .get("error")
        .filter(|error| error.is_object())
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("error"))
                .filter(|error| error.is_object())
        });
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| error.and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or(fallback);
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .or_else(|| value.get("code").and_then(Value::as_str));
    let message = match code {
        Some(code) => format!("{code}: {message}"),
        None => message.to_owned(),
    };
    ChatError::Upstream(message)
}

#[cfg(test)]
mod tests {
    use super::ResponsesDecoder;
    use crate::chat_completions::stream::StreamEvent;
    use crate::error::ChatError;
    use crate::sse::SseDecoder;

    fn decode_all(payloads: &[&str]) -> Vec<StreamEvent> {
        let mut decoder = ResponsesDecoder::new();
        let mut events = Vec::new();
        for payload in payloads {
            decoder
                .decode(payload, &mut events)
                .expect("payload decodes");
        }
        events
    }

    #[test]
    fn text_and_reasoning_deltas_become_generic_events() {
        let events = decode_all(&[
            r#"{"type":"response.output_text.delta","delta":"hel"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"think"}"#,
            r#"{"type":"response.output_text.delta","delta":"lo"}"#,
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("hel".to_owned()),
                StreamEvent::ReasoningDelta("think".to_owned()),
                StreamEvent::TextDelta("lo".to_owned()),
            ]
        );
    }

    #[test]
    fn function_call_deltas_carry_the_announced_identity() {
        let events = decode_all(&[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item-9","call_id":"call-9","name":"read"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"item-9","delta":"{\"path\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"item-9","delta":"\"x\"}"}"#,
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-9".to_owned()),
                    name: Some("read".to_owned()),
                    arguments: r#"{"path":"#.to_owned(),
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-9".to_owned()),
                    name: Some("read".to_owned()),
                    arguments: r#""x"}"#.to_owned(),
                },
            ]
        );
    }

    #[test]
    fn completed_and_incomplete_emit_usage_finish_and_done() {
        let completed = decode_all(&[
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":30,"output_tokens":12,"input_tokens_details":{"cached_tokens":10,"cache_write_tokens":4},"output_tokens_details":{"reasoning_tokens":3}}}}"#,
        ]);
        assert_eq!(completed.len(), 3);
        let StreamEvent::Usage(usage) = &completed[0] else {
            panic!("first event must be usage: {completed:?}");
        };
        assert_eq!(usage.prompt_tokens, 30);
        assert_eq!(usage.completion_tokens, 12);
        assert_eq!(usage.cached_tokens, Some(10));
        assert_eq!(usage.cache_write_tokens, Some(4));
        assert_eq!(usage.reasoning_tokens, Some(3));
        assert_eq!(
            completed[1],
            StreamEvent::Finish {
                reason: "stop".to_owned()
            }
        );
        assert_eq!(completed[2], StreamEvent::Done);

        let incomplete = decode_all(&[
            r#"{"type":"response.incomplete","response":{"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        ]);
        assert_eq!(incomplete.len(), 3);
        assert!(matches!(incomplete[0], StreamEvent::Usage(_)));
        assert_eq!(
            incomplete[1],
            StreamEvent::Finish {
                reason: "length".to_owned()
            }
        );
        assert_eq!(incomplete[2], StreamEvent::Done);

        let bare = decode_all(&[r#"{"type":"response.incomplete"}"#]);
        assert_eq!(
            bare,
            vec![
                StreamEvent::Finish {
                    reason: "length".to_owned()
                },
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn failures_are_upstream_errors_and_unknown_events_are_ignored() {
        let mut decoder = ResponsesDecoder::new();
        let mut events = Vec::new();
        decoder
            .decode(r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"boom"}}}"#, &mut events)
            .expect_err("failed response is an error");
        decoder
            .decode(
                r#"{"type":"error","code":"rate_limit","message":"slow down"}"#,
                &mut events,
            )
            .expect_err("error event is an error");
        decoder
            .decode(r#"{"type":"response.cancelled"}"#, &mut events)
            .expect_err("cancelled response is an error");
        assert!(events.is_empty(), "failures must not emit events");

        let ignored = decode_all(&[
            r#"{"type":"response.output_item.added","item":{"type":"message","id":"msg-1"}}"#,
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"future.event","delta":"whatever"}"#,
        ]);
        assert!(ignored.is_empty());

        let mut decoder = ResponsesDecoder::new();
        let error = decoder
            .decode(r#"{"type":"error","message":"plain"}"#, &mut events)
            .expect_err("plain error still fails");
        assert!(matches!(error, ChatError::Upstream(message) if message.contains("plain")));
    }
}
