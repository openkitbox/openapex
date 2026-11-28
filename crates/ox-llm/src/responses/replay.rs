//! Opaque Responses replay state carried in `AssistantMessage.provider_state`.
//!
//! Response items, reasoning and encrypted content are only meaningful to the
//! endpoint that produced them: replaying them through another provider, model
//! or protocol endpoint would corrupt the turn. The state records the exact
//! origin and the caller compares it with [`ResponsesOrigin`] before using any
//! of it. Serde skips it on the internal Chat wire, so no other protocol sees
//! these bytes.

use std::sync::Arc;

use ox_schema::support::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The key under which the replay state is stored in `ProviderState`.
pub const REPLAY_KEY: &str = "ox.responses.replay.v1";

/// Identity of the endpoint that produced a Responses item list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponsesOrigin<'a> {
    /// Internal provider id chosen for the request.
    pub provider: &'a str,
    /// Exact Responses URL of that provider's endpoint.
    pub endpoint: &'a str,
    /// Marker for the wire dialect that produced the items.
    pub protocol_marker: &'a str,
}

impl<'a> ResponsesOrigin<'a> {
    /// Identity of one concrete Responses endpoint.
    #[must_use]
    pub fn new(provider: &'a str, endpoint: &'a str, protocol_marker: &'a str) -> Self {
        Self {
            provider,
            endpoint,
            protocol_marker,
        }
    }
}

/// The response-specific parts of one replay state.
///
/// Grouped with [`ResponsesOrigin`] so constructing a state cannot grow an
/// unbounded argument list as new protocol fields are added.
#[derive(Debug, Default)]
pub struct ResponsesReplayInput {
    /// Wire model of that request.
    pub model: String,
    /// Id of the completed response, when the endpoint supplied one.
    pub response_id: Option<String>,
    /// Conversation the response was part of, when any.
    pub conversation_id: Option<String>,
    /// Raw response items, in their original order.
    pub items: Vec<Value>,
    /// Fingerprint of the input the items were generated from.
    pub input_fingerprint: String,
    /// Byte length of that input (cheap second check before hashing again).
    pub input_len: usize,
    /// Sequence number assigned by the Responses stream, when present.
    pub sequence_number: Option<u64>,
}

/// Native state gathered from one completed Responses response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesReplayState {
    /// Internal provider id that produced the items.
    pub provider: String,
    /// Responses URL that produced the items.
    pub endpoint: String,
    /// Wire model of that request.
    pub model: String,
    /// Wire protocol marker, so a Chat/Messages item can never be replayed here.
    pub protocol_marker: String,
    /// Id of the completed response, when the endpoint supplied one.
    #[serde(default)]
    pub response_id: Option<String>,
    /// Conversation the response was part of, when any.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Raw response items, in their original order.
    pub items: Vec<Value>,
    /// Fingerprint of the input the items were generated from.
    pub input_fingerprint: String,
    /// Byte length of that input (cheap second check before hashing again).
    pub input_len: usize,
    /// Sequence number assigned by the Responses stream, when present.
    #[serde(default)]
    pub sequence_number: Option<u64>,
}

impl ResponsesReplayState {
    /// State for one response from a known endpoint.
    #[must_use]
    pub fn new(origin: ResponsesOrigin<'_>, input: ResponsesReplayInput) -> Self {
        Self {
            provider: origin.provider.to_owned(),
            endpoint: origin.endpoint.to_owned(),
            protocol_marker: origin.protocol_marker.to_owned(),
            model: input.model,
            response_id: input.response_id,
            conversation_id: input.conversation_id,
            items: input.items,
            input_fingerprint: input.input_fingerprint,
            input_len: input.input_len,
            sequence_number: input.sequence_number,
        }
    }

    /// Whether `origin` and `model` identify the endpoint that produced this
    /// state. A mismatch rejects the replay; callers must use the text fallback.
    #[must_use]
    pub fn matches(&self, origin: &ResponsesOrigin<'_>, model: &str) -> bool {
        origin.provider == self.provider
            && origin.endpoint == self.endpoint
            && origin.protocol_marker == self.protocol_marker
            && model == self.model
    }

    /// Whether the input a follow-up would replay still matches the recorded one.
    #[must_use]
    pub fn fingerprint_matches(&self, fingerprint: &str, len: usize) -> bool {
        self.input_fingerprint == fingerprint && self.input_len == len
    }

    /// Bytes the replayed items add to a request that serde skips them on.
    #[must_use]
    pub fn extra_wire_bytes(&self) -> usize {
        /// Counts serialized bytes without allocating the serialized body.
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
        serde_json::to_writer(&mut counter, &self.items).map_or(0, |()| counter.0)
    }

    /// Serialize into the existing opaque provider-state record.
    pub fn into_provider_state(self) -> Result<Record, serde_json::Error> {
        let value: Value = serde_json::to_value(self)?;
        let mut record = Record::new();
        record.insert(REPLAY_KEY.to_owned(), value);
        Ok(record)
    }
}

/// Recover native state from a durable provider-state record.
///
/// Rows written by another protocol or by an older build have no such key and
/// simply use the text fallback. A state with no items carries nothing to
/// replay and is likewise ignored.
#[must_use]
pub fn from_provider_state(state: Option<&Record>) -> Option<Arc<ResponsesReplayState>> {
    let value = state?.get(REPLAY_KEY)?;
    let decoded = ResponsesReplayState::deserialize(value).ok()?;
    (!decoded.items.is_empty()).then(|| Arc::new(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> ResponsesReplayState {
        ResponsesReplayState::new(
            ResponsesOrigin::new(
                "vendor",
                "https://vendor.example/v1/responses",
                "responses/v1",
            ),
            ResponsesReplayInput {
                model: "gpt-5".to_owned(),
                response_id: Some("resp_1".to_owned()),
                items: vec![
                    json!({"type": "message", "content": [{"type": "output_text", "text": "hi"}]}),
                ],
                input_fingerprint: "fnv:1234".to_owned(),
                input_len: 64,
                sequence_number: Some(1),
                ..Default::default()
            },
        )
    }

    #[test]
    fn matches_rejects_changed_origin_and_model() {
        let state = state();
        let origin = ResponsesOrigin::new(
            "vendor",
            "https://vendor.example/v1/responses",
            "responses/v1",
        );
        assert!(state.matches(&origin, "gpt-5"));
        assert!(
            !state.matches(&origin, "gpt-4"),
            "a different model cannot replay"
        );
        assert!(!state.matches(
            &ResponsesOrigin::new(
                "other",
                "https://vendor.example/v1/responses",
                "responses/v1"
            ),
            "gpt-5"
        ));
        assert!(!state.matches(
            &ResponsesOrigin::new(
                "vendor",
                "https://other.example/v1/responses",
                "responses/v1"
            ),
            "gpt-5"
        ));
        assert!(!state.matches(
            &ResponsesOrigin::new(
                "vendor",
                "https://vendor.example/v1/responses",
                "messages/v1"
            ),
            "gpt-5"
        ));
    }

    #[test]
    fn provider_state_round_trip_preserves_items() {
        let state = state();
        let expected_items = state.items.clone();
        let record = state.into_provider_state().expect("state serializes");
        assert_eq!(record.len(), 1, "only the replay key is written");
        let restored = from_provider_state(Some(&record)).expect("state recovers");
        assert_eq!(restored.items, expected_items);
        assert_eq!(restored.response_id.as_deref(), Some("resp_1"));
        assert!(restored.fingerprint_matches("fnv:1234", 64));
        assert!(!restored.fingerprint_matches("fnv:1234", 65));
        assert!(restored.extra_wire_bytes() > 0);
    }

    #[test]
    fn foreign_or_empty_state_is_ignored() {
        let mut record = Record::new();
        record.insert("ox.messages.replay.v1".to_owned(), json!({"blocks": []}));
        assert!(from_provider_state(Some(&record)).is_none());
        assert!(from_provider_state(None).is_none());

        let mut empty = state();
        empty.items.clear();
        let record = empty.into_provider_state().expect("empty state serializes");
        assert!(from_provider_state(Some(&record)).is_none());
    }
}
