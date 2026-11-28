//! Responses SSE decoding: typed Responses events become the engine's
//! [`StreamEvent`] vocabulary.
//!
//! Function and custom tool calls are announced by `response.output_item.added`
//! with an item id, while input deltas refer to that item id rather than a call
//! ordinal. The decoder keeps only a small item-id → (ordinal, call id, name)
//! map for the life of one stream. Raw output items are kept once per item
//! (`added` / `done`, or the terminal response's `output` array) and moved into
//! a single [`StreamEvent::ProviderState`] at the terminal event; no transcript
//! is rebuilt from deltas.
//!
//! Text, refusal and reasoning deltas use the existing stream vocabulary.
//! Content-part lifecycle events, citations/annotations and server-side tool
//! events carry no engine-side event; a new upstream event type is ignored so
//! it cannot break a turn. Refusal is surfaced as text, as in Chat Completions.
//!
//! Terminal contract: `response.completed` emits usage (when reported),
//! `Finish { reason: "stop" }` and `Done`; `response.incomplete` emits the same
//! shape with `Finish { reason: "length" }`. These reasons are the engine's
//! `map_finish` vocabulary — the upstream event names are not recognized
//! there. `response.failed`, `response.cancelled` and `error` fail the stream
//! without emitting text, tool, usage or terminal events.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::chat_completions::stream::StreamEvent;
use crate::chat_completions::wire::Usage;
use crate::error::ChatError;
use crate::responses::replay::{ResponsesOrigin, ResponsesReplayInput, ResponsesReplayState};
use crate::sse::SseDecoder;

/// Wire marker recorded with replayed items; Chat Completions and Messages use
/// different markers and must never consume a Responses state.
const PROTOCOL_MARKER: &str = "responses/v1";

/// Endpoint identity needed to build a replayable state.
#[derive(Debug)]
struct ReplayIdentity {
    provider: String,
    endpoint: String,
    model: String,
}

/// Decodes one Responses stream.
#[derive(Debug, Default)]
pub struct ResponsesDecoder {
    /// Announced tool items: item id → the engine's view of the call.
    tools: HashMap<String, ToolIdentity>,
    /// How many tool calls this stream has seen.
    next_tool: u32,
    /// Raw output items in wire order, keyed by `output_index`.
    output_items: BTreeMap<u64, Value>,
    /// Item id → `output_index`, for `done` events without an explicit index.
    item_slots: HashMap<String, u64>,
    /// Endpoint identity for the terminal replay record, when requested.
    replay: Option<ReplayIdentity>,
    /// Fingerprint of the request input that produced this stream.
    fingerprint: Option<(String, usize)>,
    /// Highest `sequence_number` seen, used for background resume.
    sequence_number: Option<u64>,
    /// Response id seen on any event, used for chained state modes.
    response_id: Option<String>,
    /// Whether the terminal replay record has already been emitted.
    state_emitted: bool,
}

/// What the engine needs to know about one tool-call item.
#[derive(Debug)]
struct ToolIdentity {
    /// Dense ordinal in this response.
    index: u32,
    /// Call id echoed by the matching result.
    call_id: Option<String>,
    /// Tool name.
    name: Option<String>,
}

impl ResponsesDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the endpoint that produced this stream so the terminal event can
    /// carry its raw items in a replayable [`StreamEvent::ProviderState`].
    #[must_use]
    pub fn with_replay(
        mut self,
        provider: impl Into<String>,
        endpoint: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.replay = Some(ReplayIdentity {
            provider: provider.into(),
            endpoint: endpoint.into(),
            model: model.into(),
        });
        self
    }

    /// Bind the input fingerprint so the next chained turn can verify that the
    /// prefix it drops is byte-identical to the one this response saw.
    #[must_use]
    pub fn with_fingerprint(mut self, hash: &str, len: usize) -> Self {
        self.fingerprint = Some((hash.to_owned(), len));
        self
    }

    /// Highest `sequence_number` seen so far, for background resume.
    #[must_use]
    pub fn last_sequence_number(&self) -> Option<u64> {
        self.sequence_number
    }

    /// Response id seen so far, for chained state modes.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// The identity for `item_id`, allocating a dense ordinal if the item was
    /// never announced (a forward-compatible fallback for new event orders).
    fn identity(&mut self, item_id: &str) -> &mut ToolIdentity {
        if !self.tools.contains_key(item_id) {
            let index = self.next_tool;
            self.next_tool += 1;
            self.tools.insert(
                item_id.to_owned(),
                ToolIdentity {
                    index,
                    call_id: None,
                    name: None,
                },
            );
        }
        self.tools
            .get_mut(item_id)
            .expect("identity was inserted above")
    }

    /// Slot for one raw item: the wire's `output_index`, a previously announced
    /// item's slot, or the next position when an endpoint supplies neither.
    fn slot(&self, value: &Value, item_id: Option<&str>) -> u64 {
        if let Some(index) = value.get("output_index").and_then(Value::as_u64) {
            return index;
        }
        if let Some(slot) = item_id.and_then(|id| self.item_slots.get(id)) {
            return *slot;
        }
        self.output_items
            .keys()
            .next_back()
            .map_or(0, |last| last + 1)
    }

    /// Keep the announced item once; its `done` event replaces it with the
    /// complete item.
    fn on_output_item_added(&mut self, value: &mut Value) {
        let Some(item) = value
            .get_mut("item")
            .map(std::mem::take)
            .filter(Value::is_object)
        else {
            return;
        };
        let id = item.get("id").and_then(Value::as_str);
        let slot = self.slot(value, id);
        if let Some(id) = id {
            self.item_slots.insert(id.to_owned(), slot);
            if matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call" | "custom_tool_call")
            ) {
                let identity = self.identity(id);
                identity.call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                identity.name = item.get("name").and_then(Value::as_str).map(str::to_owned);
            }
        }
        self.output_items.insert(slot, item);
    }

    fn on_output_item_done(&mut self, value: &mut Value) {
        let Some(item) = value
            .get_mut("item")
            .map(std::mem::take)
            .filter(Value::is_object)
        else {
            return;
        };
        let id = item.get("id").and_then(Value::as_str);
        let slot = self.slot(value, id);
        if let Some(id) = id
            && !self.item_slots.contains_key(id)
        {
            self.item_slots.insert(id.to_owned(), slot);
        }
        self.output_items.insert(slot, item);
    }

    /// Emit the replay record before usage/finish/done, exactly once per
    /// stream. The terminal `output` array wins over the accumulated items;
    /// endpoints that omit it still replay the items seen in the stream.
    fn record_state(&mut self, value: &mut Value, out: &mut Vec<StreamEvent>) {
        let Some(identity) = &self.replay else {
            return;
        };
        let items =
            terminal_output(value).unwrap_or_else(|| self.output_items.values().cloned().collect());
        let state = ResponsesReplayState::new(
            ResponsesOrigin::new(&identity.provider, &identity.endpoint, PROTOCOL_MARKER),
            ResponsesReplayInput {
                model: identity.model.clone(),
                response_id: self.response_id.clone(),
                conversation_id: response_conversation_id(value),
                sequence_number: self.sequence_number,
                items,
                input_fingerprint: self
                    .fingerprint
                    .as_ref()
                    .map_or_else(String::new, |(hash, _)| hash.clone()),
                input_len: self.fingerprint.as_ref().map_or(0, |(_, len)| *len),
            },
        );
        match state.into_provider_state() {
            Ok(record) => out.push(StreamEvent::ProviderState(record)),
            Err(error) => tracing::warn!(%error, "could not serialize Responses replay state"),
        }
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
        let mut value: Value = serde_json::from_str(payload)
            .map_err(|error| ChatError::Sse(format!("invalid JSON payload: {error}")))?;
        let Some(kind) = value.get("type").and_then(Value::as_str).map(str::to_owned) else {
            return Ok(());
        };
        if let Some(sequence) = value.get("sequence_number").and_then(Value::as_u64) {
            self.sequence_number = Some(sequence);
        }
        if let Some(id) = response_id_of(&value) {
            self.response_id = Some(id.to_owned());
        }
        match kind.as_str() {
            "response.output_text.delta" | "response.refusal.delta" => text_delta(&value, out),
            "response.reasoning_summary_text.delta" | "response.reasoning.delta" => {
                reasoning_delta(&value, out);
            }
            "response.output_item.added" => self.on_output_item_added(&mut value),
            "response.output_item.done" => self.on_output_item_done(&mut value),
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                self.tool_delta(&value, out);
            }
            "response.completed" | "response.incomplete" => {
                if !self.state_emitted {
                    self.record_state(&mut value, out);
                    self.state_emitted = true;
                }
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
            "response.failed" => return Err(upstream_error(&value, "response failed")),
            "response.cancelled" => return Err(upstream_error(&value, "response cancelled")),
            "error" => return Err(upstream_error(&value, "upstream error")),
            // Forward compatibility: lifecycle, annotation, server-side tool
            // and unknown events must not fail the turn or duplicate deltas.
            _ => {}
        }
        Ok(())
    }
}

impl ResponsesDecoder {
    /// One tool-input delta, keyed by the announced item id.
    fn tool_delta(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        let (Some(item_id), Some(delta)) = (
            value.get("item_id").and_then(Value::as_str),
            value.get("delta").and_then(Value::as_str),
        ) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        let identity = self.identity(item_id);
        out.push(StreamEvent::ToolCallDelta {
            index: identity.index,
            id: identity.call_id.clone(),
            name: identity.name.clone(),
            arguments: delta.to_owned(),
        });
    }
}

/// Text and refusal deltas share the engine's text event.
fn text_delta(value: &Value, out: &mut Vec<StreamEvent>) {
    if let Some(delta) = value.get("delta").and_then(Value::as_str)
        && !delta.is_empty()
    {
        out.push(StreamEvent::TextDelta(delta.to_owned()));
    }
}

fn reasoning_delta(value: &Value, out: &mut Vec<StreamEvent>) {
    if let Some(delta) = value.get("delta").and_then(Value::as_str)
        && !delta.is_empty()
    {
        out.push(StreamEvent::ReasoningDelta(delta.to_owned()));
    }
}

/// Response id on any event (`response.id` or `response_id`).
fn response_id_of(value: &Value) -> Option<&str> {
    value
        .get("response")
        .and_then(|response| response.get("id"))
        .and_then(Value::as_str)
        .or_else(|| value.get("response_id").and_then(Value::as_str))
}

/// Conversation id carried by a terminal response, when present.
fn response_conversation_id(value: &Value) -> Option<String> {
    value
        .get("response")
        .and_then(|response| response.get("conversation"))
        .and_then(|conversation| conversation.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Take the complete `output` array out of a terminal event without cloning it.
fn terminal_output(value: &mut Value) -> Option<Vec<Value>> {
    let output = value
        .get_mut("response")?
        .get_mut("output")?
        .as_array_mut()?;
    Some(std::mem::take(output))
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

    #[test]
    fn failed_cancelled_and_error_never_emit_text_events() {
        let payloads = [
            r#"{"type":"response.failed","response":{"id":"resp-1","output":[{"type":"message","content":[{"type":"output_text","text":"partial"}]}],"error":{"code":"server_error","message":"boom"}}}"#,
            r#"{"type":"response.cancelled","response":{"id":"resp-2","output":[{"type":"message","content":[{"type":"output_text","text":"cancelled text"}]}]}}"#,
            r#"{"type":"error","code":"rate_limit","message":"slow down"}"#,
        ];

        for payload in payloads {
            let mut decoder = ResponsesDecoder::new();
            let mut events = Vec::new();
            let error = decoder
                .decode(payload, &mut events)
                .expect_err("terminal failure is an upstream error");
            assert!(
                matches!(error, ChatError::Upstream(_)),
                "failure must be an upstream error, got {error:?}"
            );
            assert!(
                events.is_empty(),
                "failed streams must not emit text, tool or terminal events: {events:?}"
            );
        }
    }

    #[test]
    fn sequence_and_response_id_are_tracked_from_any_event() {
        let mut decoder = ResponsesDecoder::new();
        let mut events = Vec::new();
        decoder
            .decode(
                r#"{"type":"response.created","sequence_number":3,"response":{"id":"resp-3"}}"#,
                &mut events,
            )
            .expect("created event decodes");
        decoder
            .decode(
                r#"{"type":"response.output_text.delta","sequence_number":5,"delta":"x"}"#,
                &mut events,
            )
            .expect("delta decodes");

        assert_eq!(decoder.last_sequence_number(), Some(5));
        assert_eq!(decoder.response_id(), Some("resp-3"));
    }
}
