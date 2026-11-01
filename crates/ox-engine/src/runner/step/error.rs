use ox_llm::cardinal::ChatError;
use ox_llm::cardinal::wire::{ChatMessage, ChatRequest};
use ox_schema::support::{SessionError, SessionErrorResponse};
/// Upstream `TOOLS_INTERRUPTED` as an `aborted` session error.
pub(super) fn aborted_error(message: &str) -> SessionError {
    SessionError {
        kind: "aborted".to_owned(),
        message: message.to_owned(),
        status: None,
        response: None,
    }
}

/// Upstream `unknownFinish` (step.ts): the session error for a stream that
/// ended without a recognized finish reason (`InvalidProviderOutput`
/// classified `incomplete-stream`).
pub(super) fn invalid_output_error() -> SessionError {
    SessionError {
        kind: "provider.invalid-output".to_owned(),
        message: "The provider response ended with an unknown finish reason.".to_owned(),
        status: None,
        response: None,
    }
}

/// The ids an upstream named as unanswered in a tool-call pairing rejection,
/// when the body lists any (Moonshot names them after the marker; DeepSeek's
/// phrasing names none). None for any other body — the digest is what the
/// caller logs then.
pub(super) fn pairing_rejection_ids(body: &str) -> Option<Vec<String>> {
    const MARKER: &str = "did not have response messages:";
    let at = body.find(MARKER)?;
    let rest = body[at + MARKER.len()..].lines().next().unwrap_or("");
    // The listed ids end where the enclosing JSON string does — a backslash
    // begins the escaped closing quote (verbatim upstream body, or wrapped
    // by cardinal into `\",\"`); provider ids never carry one, so cutting at
    // the first is safe. The plain `","` shape covers a non-escaped tail.
    let end = rest.find('\\').unwrap_or(rest.len());
    let rest = rest[..end].split("\",\"").next().unwrap_or(rest);
    let ids: Vec<String> = rest
        .split([',', ' '])
        .map(str::trim)
        .map(|id| id.trim_matches(|c: char| c == '"' || c == '\\' || c == '\''))
        .filter(|id| !id.is_empty() && *id != "...")
        .map(str::to_owned)
        .take(20)
        .collect();
    Some(ids)
}

/// The request's tool-call ids in wire order, capped for one log line. The
/// request is post pairing-guard: an id the upstream names that is absent
/// here was never sent by ox.
pub(super) fn tool_call_digest(request: &ChatRequest) -> String {
    const CAP: usize = 60;
    let mut ids = Vec::new();
    let mut total = 0usize;
    for message in &request.messages {
        if let ChatMessage::Assistant {
            tool_calls: Some(calls),
            ..
        } = message
        {
            for call in calls {
                total += 1;
                if ids.len() < CAP {
                    ids.push(call.id.clone());
                }
            }
        }
    }
    if total > CAP {
        ids.push(format!("…(+{} more)", total - CAP));
    }
    ids.join(",")
}

/// Map a cardinal failure to a session error (upstream `toSessionError`):
/// the provider taxonomy for HTTP/stream failures, upstream's transport
/// family for connection-level faults.
pub(super) fn to_session_error(error: &ChatError) -> SessionError {
    let (kind, status, response) = match error {
        ChatError::Http { status, body } => (
            // Any remaining 4xx is a deterministic rejection of this request;
            // server-side statuses report a provider failure.
            if (400..500).contains(status) {
                "provider.invalid-request"
            } else {
                "provider.unknown"
            },
            Some(*status),
            Some(SessionErrorResponse { body: body.clone() }),
        ),
        ChatError::Upstream(body) => (
            // An in-band provider failure forwarded by cardinal.
            "provider.unknown",
            None,
            Some(SessionErrorResponse { body: body.clone() }),
        ),
        ChatError::NoRoute { .. } => (
            // A routing configuration error: deterministic, request-shaped,
            // and independent of any endpoint, so it recurs identically on
            // retry — the same bucket as a 4xx rejection.
            "provider.invalid-request",
            None,
            None,
        ),
        ChatError::EarlyClose | ChatError::Sse(_) | ChatError::Transport(_) => {
            ("provider.transport", None, None)
        }
    };
    SessionError {
        kind: kind.to_owned(),
        message: error.to_string(),
        status: status.map(i64::from),
        response,
    }
}
