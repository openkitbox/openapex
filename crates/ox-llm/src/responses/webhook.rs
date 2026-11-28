//! Verification rules for inbound Responses webhooks.
//!
//! The HTTP boundary owns the crypto primitive (HMAC-SHA256 lives with the
//! server crate); this module owns the protocol rules: signed payload layout,
//! timestamp freshness, constant-time signature comparison, typed event
//! parsing and bounded id de-duplication. Nothing here touches session state;
//! a verified event is handed back to the caller for delivery.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

/// Maximum accepted clock skew for `webhook-timestamp` (seconds).
pub const WEBHOOK_TOLERANCE_SECS: i64 = 300;
/// How long an event id stays in the de-duplication window.
const DEDUP_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Upper bound on retained event ids; older entries are dropped first.
const DEDUP_CAPACITY: usize = 4096;

/// The three headers that make a webhook verifiable.
#[derive(Debug, Clone, Copy)]
pub struct WebhookHeaders<'a> {
    /// Unique event id, also the de-duplication key.
    pub id: &'a str,
    /// Unix timestamp of the delivery attempt.
    pub timestamp: &'a str,
    /// Whitespace-separated `v1,<base64>` signatures (several allow key rotation).
    pub signature: &'a str,
}

/// A verified, typed webhook event. Provider payload fields ox does not
/// interpret stay in `data` instead of being silently discarded.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookEvent {
    /// Provider event id.
    pub id: String,
    /// Event kind (`response.completed`, `response.failed`, …).
    pub kind: String,
    /// Provider creation timestamp, when present.
    pub created_at: Option<i64>,
    /// The response id, when the provider included one.
    pub response_id: Option<String>,
    /// The conversation id, when the provider included one.
    pub conversation_id: Option<String>,
    /// Verbatim event data.
    pub data: Value,
}

/// Why a webhook was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WebhookError {
    /// A required header is absent.
    #[error("missing webhook header `{0}`")]
    MissingHeader(&'static str),
    /// The timestamp is not an integer Unix timestamp.
    #[error("invalid webhook timestamp")]
    InvalidTimestamp,
    /// The timestamp is outside the freshness window.
    #[error("stale webhook timestamp")]
    StaleTimestamp,
    /// No offered signature matched the signed payload.
    #[error("invalid webhook signature")]
    SignatureMismatch,
    /// The body is not a JSON event object.
    #[error("invalid webhook body: {0}")]
    InvalidBody(String),
}

#[derive(Debug, Deserialize)]
struct EventWire {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    data: Value,
}

/// Verify and parse one webhook at an explicit wall-clock instant.
///
/// `signatures` contains the decoded `webhook-signature` candidates, and
/// `mac` computes HMAC-SHA256 over an arbitrary payload with the configured
/// secret. Injecting both keeps this module dependency-light and makes the
/// protocol rules testable without a live HTTP server.
pub fn verify_webhook_at(
    headers: WebhookHeaders<'_>,
    body: &[u8],
    signatures: &[Vec<u8>],
    now_secs: i64,
    mac: impl Fn(&[u8]) -> [u8; 32],
) -> Result<WebhookEvent, WebhookError> {
    if headers.id.is_empty() {
        return Err(WebhookError::MissingHeader("webhook-id"));
    }
    if headers.timestamp.is_empty() {
        return Err(WebhookError::MissingHeader("webhook-timestamp"));
    }
    if headers.signature.is_empty() {
        return Err(WebhookError::MissingHeader("webhook-signature"));
    }
    let timestamp = headers
        .timestamp
        .parse::<i64>()
        .map_err(|_| WebhookError::InvalidTimestamp)?;
    let age = now_secs.saturating_sub(timestamp).unsigned_abs();
    if age > WEBHOOK_TOLERANCE_SECS as u64 {
        return Err(WebhookError::StaleTimestamp);
    }
    let body =
        std::str::from_utf8(body).map_err(|error| WebhookError::InvalidBody(error.to_string()))?;
    let signed = format!("{}.{}.{body}", headers.id, headers.timestamp);
    let expected = mac(signed.as_bytes());
    if !signatures
        .iter()
        .any(|candidate| constant_time_eq(&expected, candidate))
    {
        return Err(WebhookError::SignatureMismatch);
    }
    let wire: EventWire =
        serde_json::from_str(body).map_err(|error| WebhookError::InvalidBody(error.to_string()))?;
    let ids = |name: &str| {
        wire.data
            .get(name)
            .and_then(Value::as_str)
            .or_else(|| {
                wire.data
                    .get("response")
                    .and_then(|r| r.get(name))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned)
    };
    Ok(WebhookEvent {
        id: wire.id,
        kind: wire.kind,
        created_at: wire.created_at,
        response_id: ids("id").or_else(|| ids("response_id")),
        conversation_id: ids("conversation_id"),
        data: wire.data,
    })
}

/// Compare two signatures without an early-exit branch on secret material.
fn constant_time_eq(expected: &[u8; 32], candidate: &[u8]) -> bool {
    let mut difference = if candidate.len() == 32 { 0u8 } else { 1u8 };
    for (left, right) in expected.iter().zip(candidate.iter().take(32)) {
        difference |= left ^ right;
    }
    difference == 0
}

/// Bounded de-duplication window keyed by `webhook-id`.
#[derive(Debug, Default)]
pub struct WebhookDedup {
    seen: Mutex<HashMap<String, Instant>>,
}

impl WebhookDedup {
    /// Accept an id once, then reject repeats until its TTL expires.
    /// The window is bounded; growth cannot be driven by an untrusted peer.
    pub fn accept(&self, id: &str) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        seen.retain(|_, touched| now.duration_since(*touched) < DEDUP_TTL);
        if seen.contains_key(id) {
            return false;
        }
        if seen.len() >= DEDUP_CAPACITY {
            let oldest = seen
                .iter()
                .min_by_key(|entry| *entry.1)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                seen.remove(&oldest);
            }
        }
        seen.insert(id.to_owned(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{WebhookDedup, WebhookError, WebhookHeaders, constant_time_eq, verify_webhook_at};

    fn mac(payload: &[u8]) -> [u8; 32] {
        let mut tag = [0u8; 32];
        for (index, byte) in payload.iter().enumerate() {
            tag[index % 32] ^= byte;
        }
        tag
    }

    fn headers<'a>(id: &'a str, timestamp: &'a str) -> WebhookHeaders<'a> {
        WebhookHeaders {
            id,
            timestamp,
            signature: "v1,c2ln",
        }
    }

    #[test]
    fn verification_checks_timestamp_and_signature() {
        let body =
            br#"{"id":"evt-1","type":"response.completed","created_at":10,"data":{"id":"resp-1"}}"#;
        let event = verify_webhook_at(headers("evt-1", "1000"), body, &[mac(b"evt-1.1000.{\"id\":\"evt-1\",\"type\":\"response.completed\",\"created_at\":10,\"data\":{\"id\":\"resp-1\"}}").to_vec()], 1010, mac)
            .expect("valid signature");
        assert_eq!(event.id, "evt-1");
        assert_eq!(event.kind, "response.completed");
        assert_eq!(event.response_id.as_deref(), Some("resp-1"));

        assert_eq!(
            verify_webhook_at(headers("evt-1", "1000"), body, &[], 1010, mac),
            Err(WebhookError::SignatureMismatch)
        );
        assert_eq!(
            verify_webhook_at(
                headers("evt-1", "1000"),
                body,
                &[mac(b"evt-1.1000.{}").to_vec()],
                1010,
                mac
            ),
            Err(WebhookError::SignatureMismatch)
        );
        assert_eq!(
            verify_webhook_at(headers("evt-1", "1000"), body, &[], 2000, mac),
            Err(WebhookError::StaleTimestamp)
        );
        assert_eq!(
            verify_webhook_at(headers("evt-1", "2000"), body, &[], 1000, mac),
            Err(WebhookError::StaleTimestamp),
            "a far-future delivery must fail closed as well"
        );
        assert_eq!(
            verify_webhook_at(headers("evt-1", "nope"), body, &[], 1010, mac),
            Err(WebhookError::InvalidTimestamp)
        );
    }

    #[test]
    fn comparison_is_length_aware_and_not_prefix_based() {
        let expected = mac(b"payload");
        let mut wrong = expected.to_vec();
        wrong.push(0);
        assert!(constant_time_eq(&expected, &expected));
        assert!(!constant_time_eq(&expected, &wrong));
        assert!(!constant_time_eq(&expected, &expected[..31]));
    }

    #[test]
    fn dedup_accepts_each_id_once() {
        let dedup = WebhookDedup::default();
        assert!(dedup.accept("evt-1"));
        assert!(!dedup.accept("evt-1"));
        assert!(dedup.accept("evt-2"));
    }
}
