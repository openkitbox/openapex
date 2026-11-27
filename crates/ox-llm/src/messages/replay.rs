//! Provider state for replaying native Messages thinking blocks verbatim.
//!
//! Thinking signatures and redacted payloads are opaque: only the endpoint
//! that produced them can accept them again. The state carries the producing
//! provider, the exact Messages URL and the final wire model, so the writer
//! can refuse a replay whose identity has changed. It lives in the existing
//! `AssistantMessage.provider_state` record; no new table or wire field is
//! needed, and serde skips it on the internal Chat wire.

use std::sync::Arc;

use ox_schema::support::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The key under which the replay state is stored in `ProviderState`.
pub const REPLAY_KEY: &str = "ox.messages.replay.v1";

/// The identity a Messages request must match before native thinking replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessagesOrigin<'a> {
    /// The internal provider id chosen for the request.
    pub provider: &'a str,
    /// The exact Messages URL of that provider's endpoint.
    pub endpoint: &'a str,
}

impl<'a> MessagesOrigin<'a> {
    /// Identity of one concrete Messages endpoint.
    #[must_use]
    pub fn new(provider: &'a str, endpoint: &'a str) -> Self {
        Self { provider, endpoint }
    }
}

/// A completed thinking block, in the order it appeared on the response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplayBlock {
    /// Signed thinking text. An unsigned block is never stored here.
    Thinking {
        /// Position of this content block in the original response.
        index: u64,
        /// The original thinking text.
        thinking: String,
        /// The original signature, authenticating that exact text.
        signature: String,
    },
    /// An opaque redacted thinking block.
    RedactedThinking {
        /// Position of this content block in the original response.
        index: u64,
        /// The provider's opaque payload, byte for byte.
        data: String,
    },
}

/// Native state gathered from one completed Messages response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesReplayState {
    /// Internal provider id that produced the blocks.
    pub provider: String,
    /// Messages URL that produced the blocks.
    pub endpoint: String,
    /// Wire model of that request.
    pub model: String,
    /// Position of the first text block in the original response.
    #[serde(default)]
    pub text_index: Option<u64>,
    /// Position of the first tool-use block in the original response.
    #[serde(default)]
    pub tool_index: Option<u64>,
    /// Completed signed/redacted blocks in their original order.
    pub blocks: Vec<ReplayBlock>,
}

impl MessagesReplayState {
    /// State for one response from a known endpoint.
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        endpoint: impl Into<String>,
        model: impl Into<String>,
        blocks: Vec<ReplayBlock>,
        text_index: Option<u64>,
        tool_index: Option<u64>,
    ) -> Self {
        Self {
            provider: provider.into(),
            endpoint: endpoint.into(),
            model: model.into(),
            text_index,
            tool_index,
            blocks,
        }
    }

    /// Whether `origin` and `model` are the endpoint that produced this state.
    #[must_use]
    pub fn matches(&self, origin: &MessagesOrigin<'_>, model: &str) -> bool {
        origin.provider == self.provider && origin.endpoint == self.endpoint && model == self.model
    }

    /// Bytes the replayed blocks add to a request that serde skips them on.
    #[must_use]
    pub fn extra_wire_bytes(&self) -> usize {
        serde_json::to_string(&self.blocks).map_or(0, |json| json.len())
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
/// Old rows and rows written by another protocol have no such key and simply
/// keep the pre-existing reasoning-as-text fallback.
#[must_use]
pub fn from_provider_state(state: Option<&Record>) -> Option<Arc<MessagesReplayState>> {
    let value = state?.get(REPLAY_KEY)?;
    let decoded = MessagesReplayState::deserialize(value).ok()?;
    (!decoded.blocks.is_empty()).then(|| Arc::new(decoded))
}
