//! Webhook ingress for the Responses protocol.
//!
//! Verification is fail-closed: a missing secret, missing header, skewed
//! timestamp or bad signature yields 401 and produces no event. Verified
//! events are de-duplicated by `webhook-id` and broadcast as ephemeral bus
//! events; this handler never mutates a session or runs provider work on the
//! HTTP thread.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use ox_llm::responses::webhook::{WebhookDedup, WebhookError, WebhookHeaders, verify_webhook_at};
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde_json::{Map, Value, json};
use sha2::Sha256;

/// Ephemeral bus event emitted for an accepted webhook.
pub(crate) const EVENT_KIND: &str = "response.webhook.received";
/// Upper bound on an inbound webhook body.
const MAX_WEBHOOK_BODY: usize = 256 * 1024;

type HmacSha256 = Hmac<Sha256>;

static DEDUP: OnceLock<WebhookDedup> = OnceLock::new();

fn client_error(res: &mut Response, status: StatusCode, tag: &str, message: &str) {
    res.status_code(status);
    res.render(Json(json!({ "_tag": tag, "message": message })));
}

/// Decode the offered signatures. An unparseable entry is ignored: another
/// candidate may still be valid, and an all-invalid set fails verification.
fn decode_signatures(header: &str) -> Vec<Vec<u8>> {
    header
        .split_whitespace()
        .filter_map(|entry| {
            let encoded = entry
                .strip_prefix("v1,")
                .or_else(|| entry.strip_prefix("s="))
                .unwrap_or(entry);
            BASE64_STANDARD.decode(encoded).ok()
        })
        .collect()
}

fn sign(secret: &str, payload: &[u8]) -> Option<[u8; 32]> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload);
    Some(mac.finalize().into_bytes().into())
}

/// Verify, de-duplicate and hand one event to `sink`. The sink is only called
/// after every check passes, so a rejection cannot leak an event downstream.
fn deliver(
    secret: &str,
    headers: WebhookHeaders<'_>,
    body: &[u8],
    dedup: &WebhookDedup,
    now_secs: i64,
    sink: &mut dyn FnMut(Value),
) -> Result<(), WebhookError> {
    let signatures = decode_signatures(headers.signature);
    let event = verify_webhook_at(headers, body, &signatures, now_secs, |payload| {
        sign(secret, payload).unwrap_or([0u8; 32])
    })?;
    if !dedup.accept(headers.id) {
        return Ok(());
    }
    // Provider data is untrusted by construction: no event can promote its
    // own text into a trusted local instruction.
    let mut untrusted = Map::with_capacity(2);
    untrusted.insert("untrusted".to_owned(), Value::Bool(true));
    untrusted.insert("data".to_owned(), event.data);
    let mut payload = Map::with_capacity(5);
    payload.insert(
        "_tag".to_owned(),
        Value::String("ResponsesWebhookEvent".to_owned()),
    );
    payload.insert("id".to_owned(), Value::String(event.id));
    payload.insert("type".to_owned(), Value::String(event.kind));
    if let Some(created_at) = event.created_at {
        payload.insert("created_at".to_owned(), Value::from(created_at));
    }
    if let Some(response_id) = event.response_id {
        payload.insert("response_id".to_owned(), Value::String(response_id));
    }
    if let Some(conversation_id) = event.conversation_id {
        payload.insert("conversation_id".to_owned(), Value::String(conversation_id));
    }
    payload.insert("data".to_owned(), Value::Object(untrusted));
    sink(Value::Object(payload));
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn secret_for(state: &super::AppState) -> Option<String> {
    std::env::var("OX_RESPONSES_WEBHOOK_SECRET")
        .ok()
        .filter(|secret| !secret.is_empty())
        .or_else(|| {
            state
                .services()
                .config
                .mcp
                .events
                .webhook
                .secret
                .clone()
                .filter(|secret| !secret.is_empty())
        })
}

/// `POST /ox/responses/webhook` — authenticate, de-duplicate and broadcast.
#[handler]
pub(crate) async fn webhook(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = super::state(depot, res) else {
        return;
    };
    let Some(secret) = secret_for(state) else {
        client_error(
            res,
            StatusCode::UNAUTHORIZED,
            "UnauthorizedError",
            "no Responses webhook secret configured",
        );
        return;
    };
    let webhook_id = req
        .header::<&str>("webhook-id")
        .unwrap_or_default()
        .to_owned();
    let webhook_timestamp = req
        .header::<&str>("webhook-timestamp")
        .unwrap_or_default()
        .to_owned();
    let webhook_signature = req
        .header::<&str>("webhook-signature")
        .unwrap_or_default()
        .to_owned();
    let bytes = match req.payload_with_max_size(MAX_WEBHOOK_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => {
            client_error(
                res,
                StatusCode::BAD_REQUEST,
                "InvalidRequestError",
                "invalid webhook body",
            );
            return;
        }
    };
    let headers = WebhookHeaders {
        id: &webhook_id,
        timestamp: &webhook_timestamp,
        signature: &webhook_signature,
    };
    let mut delivered = None;
    let result = deliver(
        &secret,
        headers,
        bytes,
        DEDUP.get_or_init(WebhookDedup::default),
        now_secs(),
        &mut |payload| delivered = Some(payload),
    );
    if let Err(error) = result {
        client_error(
            res,
            StatusCode::UNAUTHORIZED,
            "UnauthorizedError",
            &error.to_string(),
        );
        return;
    }
    if let Some(payload) = delivered {
        let directory = state.directory.to_string_lossy();
        state
            .bus
            .broadcast_ephemeral(EVENT_KIND, payload, Some(directory.as_ref()));
    }
    res.status_code(StatusCode::OK);
    res.render(Json(json!({ "received": true })));
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::{EVENT_KIND, decode_signatures, deliver, sign};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use ox_llm::responses::webhook::{WebhookDedup, WebhookError, WebhookHeaders};
    use serde_json::Value;

    fn signed_body(id: &str, timestamp: &str, body: &str, secret: &str) -> String {
        let signed = format!("{id}.{timestamp}.{body}");
        let tag = sign(secret, signed.as_bytes()).expect("HMAC accepts any key");
        format!("v1,{}", BASE64_STANDARD.encode(tag))
    }

    #[test]
    fn good_signature_delivers_exactly_one_untrusted_event() {
        let body = r#"{"id":"evt-1","type":"response.completed","data":{"id":"resp-1","output":[{"text":"untrusted"}]}}"#;
        let signature = signed_body("evt-1", "1000", body, "secret");
        let headers = WebhookHeaders {
            id: "evt-1",
            timestamp: "1000",
            signature: &signature,
        };
        let dedup = WebhookDedup::default();
        let mut delivered = Vec::new();
        deliver(
            "secret",
            headers,
            body.as_bytes(),
            &dedup,
            1010,
            &mut |value| delivered.push(value),
        )
        .expect("well-signed event is delivered");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["_tag"], "ResponsesWebhookEvent");
        assert_eq!(delivered[0]["response_id"], "resp-1");
        assert_eq!(delivered[0]["data"]["untrusted"], true);
        assert_eq!(
            delivered[0]["data"]["data"]["output"][0]["text"],
            "untrusted"
        );

        deliver(
            "secret",
            headers,
            body.as_bytes(),
            &dedup,
            1010,
            &mut |value| delivered.push(value),
        )
        .expect("duplicate is acknowledged without delivery");
        assert_eq!(delivered.len(), 1, "webhook-id must deduplicate");
        assert_eq!(EVENT_KIND, "response.webhook.received");
    }

    #[test]
    fn dedup_uses_webhook_id_not_body_id() {
        let first = r#"{"id":"body-a","type":"response.completed","data":{}}"#;
        let second = r#"{"id":"body-b","type":"response.completed","data":{}}"#;
        let first_signature = signed_body("evt-dedup", "1000", first, "secret");
        let second_signature = signed_body("evt-dedup", "1000", second, "secret");
        let dedup = WebhookDedup::default();
        let mut delivered = Vec::new();

        for (body, signature) in [(first, first_signature), (second, second_signature)] {
            deliver(
                "secret",
                WebhookHeaders {
                    id: "evt-dedup",
                    timestamp: "1000",
                    signature: &signature,
                },
                body.as_bytes(),
                &dedup,
                1010,
                &mut |value| delivered.push(value),
            )
            .expect("signed delivery is acknowledged");
        }
        assert_eq!(delivered.len(), 1, "dedup key is the webhook-id header");
    }

    #[test]
    fn bad_signature_and_wrong_secret_produce_no_event() {
        let body = r#"{"id":"evt-2","type":"response.failed","data":{}}"#;
        let signature = signed_body("evt-2", "1000", body, "secret");
        let headers = WebhookHeaders {
            id: "evt-2",
            timestamp: "1000",
            signature: &signature,
        };
        let dedup = WebhookDedup::default();
        let delivered: Mutex<Vec<Value>> = Mutex::new(Vec::new());
        let error = deliver(
            "other-secret",
            headers,
            body.as_bytes(),
            &dedup,
            1010,
            &mut |value| delivered.lock().expect("test mutex").push(value),
        )
        .expect_err("a wrong secret fails verification");
        assert_eq!(error, WebhookError::SignatureMismatch);
        assert!(delivered.lock().expect("test mutex").is_empty());
    }

    #[test]
    fn signature_entries_are_decoded_with_a_rotating_key() {
        let signature = signed_body("evt-3", "1000", "{}", "secret");
        let entries = decode_signatures(&format!("v1,AAAA {signature}"));
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|tag| tag.len() == 32));
    }

    #[test]
    fn stale_timestamp_produces_no_event() {
        let body = r#"{"id":"evt-4","type":"response.completed","data":{}}"#;
        let signature = signed_body("evt-4", "1000", body, "secret");
        let headers = WebhookHeaders {
            id: "evt-4",
            timestamp: "1000",
            signature: &signature,
        };
        let mut delivered = Vec::new();
        let error = deliver(
            "secret",
            headers,
            body.as_bytes(),
            &WebhookDedup::default(),
            2000,
            &mut |value| delivered.push(value),
        )
        .expect_err("stale delivery is rejected");
        assert_eq!(error, WebhookError::StaleTimestamp);
        assert!(delivered.is_empty());
    }
}
