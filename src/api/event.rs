//! `event.subscribe` — GET /api/event, SSE stream of the global event bus.
//!
//! Port of `packages/server/src/handlers/event.ts` + `event-feed.ts`:
//! - an immediate `server.connected` frame (`{id, type, data}` — upstream
//!   sends no `created` field on this one);
//! - every committed bus event (durable + ephemeral) as `data: {json}\n\n`;
//! - a `: heartbeat` comment every 15s (upstream `Stream.tick("15 seconds")`,
//!   merged into the event stream; configurable via
//!   `Config::event_heartbeat_ms`, default identical);
//! - response headers `Content-Type: text/event-stream`,
//!   `Cache-Control: no-cache, no-transform`, `X-Accel-Buffering: no`,
//!   `X-Content-Type-Options: nosniff`.
//!
//! The wire envelope is the flattened upstream `V2Event`:
//! `{id, created, type, data, durable?: {aggregateID, seq, version}}`.
//! Durable events carry `durable`; ephemeral events never do.
//!
//! ## Cursor replay (ox extension)
//!
//! Upstream `/api/event` has **no** cursor: the endpoint is volatile by
//! contract ("a slow consumer overflows and fails the stream, and events
//! during disconnection are missed") and the TUI subscribes with no
//! parameters. ox adds an opt-in replay, reusing the cursor vocabulary of
//! the upstream `session.log` endpoint (`after` = exclusive per-aggregate
//! `Event.Seq`):
//!
//! ```text
//! GET /api/event?session=<sessionID>&after=<seq>
//! ```
//!
//! replays that session's durable log (`seq > after`, ordered by seq) after
//! the `server.connected` frame, then continues live. Both parameters must
//! be given together; without them the stream is pure live (upstream
//! behavior).
//!
//! ## No-dup discipline
//!
//! The broadcast receiver is subscribed **before** the replay query runs.
//! Events published while the replay reads are buffered there; any buffered
//! durable event of the replayed session with `seq <=` the last replayed
//! seq is dropped, so the replay/live boundary neither duplicates nor loses
//! events. Ephemeral events have no seq and are always passed through
//! (missing one during a disconnect matches upstream's volatile contract).
//!
//! ## Backpressure / lag
//!
//! Publishers never block on slow clients (tokio broadcast). If a consumer
//! falls more than `BROADCAST_CAPACITY` events behind, the receiver gets
//! `Lagged` — the stream is then no longer complete, so it is closed and
//! the client reconnects with its cursor (upstream fails the stream on
//! subscriber overflow; reconnect-with-cursor is ox's repair path).

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use salvo::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use salvo::http::{HeaderName, HeaderValue, StatusCode};
use salvo::prelude::*;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use super::state;
use crate::event_bus::EventBus;
use crate::schema::event::EventEnvelope;

/// Capacity of the per-connection channel feeding the HTTP body. When a
/// client stalls, the producer parks here (backpressure) without blocking
/// publishers.
const BODY_CHANNEL_CAPACITY: usize = 64;

/// Aggregate position a replay covered: `(aggregate_id, last_replayed_seq)`.
/// Live events from this same aggregate at or below this seq were already
/// delivered by the replay and must be dropped.
type ReplayCursor = (String, i64);

#[handler] // operation_id: event.subscribe
pub(crate) async fn subscribe(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };

    // Cursor: `session` + `after`, both or neither (upstream clients send
    // neither). `after` mirrors upstream `session.log`: NumberFromString,
    // exclusive, per-aggregate seq.
    let session = req.query::<String>("session");
    let after = match req.query::<String>("after") {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Ok(n) if n >= 0 => Some(n),
            _ => {
                invalid_cursor(res, "`after` must be a non-negative integer sequence");
                return;
            }
        },
    };
    let cursor = match (session, after) {
        (Some(session), Some(after)) => Some((session, after)),
        (None, None) => None,
        (None, Some(_)) => {
            invalid_cursor(
                res,
                "`after` requires `session` naming the aggregate to replay",
            );
            return;
        }
        (Some(_), None) => {
            invalid_cursor(
                res,
                "`session` without `after` replays from the beginning; pass `after=0`",
            );
            return;
        }
    };

    let bus: EventBus = state.bus.clone();
    // Guard against a zero interval spinning the select loop.
    let period = Duration::from_millis(state.config.event_heartbeat_ms.max(1));

    // Subscribe to the broadcast BEFORE the replay query runs (in the task
    // below): anything committed while the replay reads is buffered here and
    // deduplicated against the replayed range.
    let mut live = bus.subscribe();
    let (tx, rx) = mpsc::channel::<String>(BODY_CHANNEL_CAPACITY);

    tokio::spawn(async move {
        // Upstream sends `server.connected` immediately on connect (no
        // `created` field on this frame).
        let connected =
            json!({ "id": crate::id::id("evt"), "type": "server.connected", "data": {} });
        if tx.send(format!("data: {connected}\n\n")).await.is_err() {
            return; // client already gone
        }

        // Replay the requested durable range, remembering where it ended.
        let mut replayed: Option<ReplayCursor> = None;
        if let Some((session_id, after)) = cursor {
            match bus.event_log(&session_id, after).await {
                Ok(events) => {
                    for env in &events {
                        if render_frame(&tx, env).await.is_err() {
                            return; // client disconnected mid-replay
                        }
                    }
                    replayed = events.last().and_then(|last| {
                        last.durable
                            .as_ref()
                            .map(|d| (d.aggregate_id.clone(), d.seq))
                    });
                }
                Err(error) => {
                    tracing::error!(error = %error, "event replay failed; closing stream");
                    return;
                }
            }
        }

        // Live loop: forward broadcast events, emit heartbeats when idle.
        // interval_at (not interval) so the first heartbeat waits a full
        // period instead of firing immediately.
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if tx.send(": heartbeat\n\n".to_owned()).await.is_err() {
                        return; // client disconnected
                    }
                }
                event = live.recv() => match event {
                    Ok(env) => {
                        // Drop durable events the replay already delivered.
                        if let Some((aggregate_id, seq)) = &replayed
                            && let Some(durable) = &env.durable
                            && durable.aggregate_id == *aggregate_id
                            && durable.seq <= *seq
                        {
                            continue;
                        }
                        if render_frame(&tx, &env).await.is_err() {
                            return; // client disconnected
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // The stream can no longer be complete: close it so
                        // the client reconnects with its cursor.
                        tracing::warn!(skipped, "event subscriber lagged; closing stream for resync");
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    });

    res.status_code(StatusCode::OK);
    let headers = res.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    res.stream(EventBody { rx });
}

/// Serialize one bus envelope into an SSE frame and push it to the body.
async fn render_frame(
    tx: &mpsc::Sender<String>,
    env: &EventEnvelope,
) -> Result<(), mpsc::error::SendError<String>> {
    // Serialization of `EventEnvelope<Value>` is infallible in practice;
    // a failure drops the frame rather than tearing down the stream.
    match serde_json::to_string(env) {
        Ok(line) => tx.send(format!("data: {line}\n\n")).await,
        Err(error) => {
            tracing::warn!(error = %error, event_type = %env.kind, "failed to encode event frame");
            Ok(())
        }
    }
}

/// 400 `InvalidCursorError` (cursor parameters only; mirrors the upstream
/// error tag used for malformed log cursors).
fn invalid_cursor(res: &mut Response, message: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidCursorError",
        "message": message,
    })));
}

/// Bridges the producer task to the salvo response body. When the response
/// is dropped (client disconnect), `rx` goes away and the producer's next
/// send fails, ending the task.
struct EventBody {
    rx: mpsc::Receiver<String>,
}

impl Stream for EventBody {
    type Item = Result<String, std::convert::Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx)
            .poll_recv(cx)
            .map(|item| item.map(Ok))
    }
}
