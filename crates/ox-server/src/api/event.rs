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
use std::sync::Arc;
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
use ox_core::event_bus::EventBus;
use ox_schema::event::EventEnvelope;

/// Capacity of the per-connection channel feeding the HTTP body. When a
/// client stalls, the producer parks here (backpressure) without blocking
/// publishers.
const BODY_CHANNEL_CAPACITY: usize = 64;

/// Aggregate position a replay covered: `(aggregate_id, last_replayed_seq)`.
/// Live events from this same aggregate at or below this seq were already
/// delivered by the replay and must be dropped.
type ReplayCursor = (String, i64);

struct StreamOptions {
    period: Duration,
    cursor: Option<ReplayCursor>,
    only: Option<String>,
    types: Option<Vec<String>>,
    connected: bool,
    follow: bool,
    runtime: Arc<crate::runtime::Runtime>,
}

/// How many durable envelopes one replay query materializes at most. Frames
/// are rendered page by page, so a reconnect into a long session never holds
/// the whole log in memory.
const REPLAY_PAGE: i64 = 500;

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
    let types = req.query::<Vec<String>>("types");
    // Guard against a zero interval spinning the select loop.
    let period = Duration::from_millis(state.services().config.event_heartbeat_ms.max(1));
    stream(
        res,
        bus,
        StreamOptions {
            period,
            cursor,
            only: None,
            types,
            connected: true,
            follow: true,
            runtime: state.runtime.clone(),
        },
    )
    .await;
}

/// `GET /api/experimental/session/{sessionID}/log` — the session-scoped durable
/// log the TUI's devtools bar reads (`session.log`, `follow=false` by default).
///
/// Upstream streams `SessionEvent.Durable | EventLog.Synced` items; ox reuses
/// the `/api/event` frame vocabulary (envelopes carrying `type`, `created`,
/// `data` and `durable`), which is what the devtools consumer reads
/// (`component/devtools-bar.tsx:165`). `after` is the exclusive per-aggregate
/// sequence and defaults to 0, so the first call replays the whole log.
///
/// Documented deviations: no `log.synced` marker (the only consumer skips it),
/// and no heartbeats when streaming a finite log (`follow=false` ends after the
/// replay, so there is nothing to keep alive).
#[handler] // operation_id: session.log
pub(crate) async fn log(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        return invalid_cursor(res, "Missing sessionID");
    };
    if !session_exists(&state.db, &session_id).await {
        res.status_code(StatusCode::NOT_FOUND);
        return res.render(Json(json!({
            "_tag": "SessionNotFoundError",
            "sessionID": session_id,
            "message": format!("Session not found: {session_id}"),
        })));
    }
    // Absent `after` means "the whole log", and aggregate sequences start at
    // 0 (`session.created`), so the inclusive start is -1; a supplied `after`
    // stays exclusive.
    let after = match req.query::<String>("after") {
        None => -1,
        Some(raw) => match raw.parse::<i64>() {
            Ok(n) if n >= 0 => n,
            _ => return invalid_cursor(res, "`after` must be a non-negative integer sequence"),
        },
    };
    let follow = req
        .query::<String>("follow")
        .map(|raw| raw == "true" || raw == "1")
        .unwrap_or(false);
    let bus: EventBus = state.bus.clone();
    let period = Duration::from_millis(state.services().config.event_heartbeat_ms.max(1));
    stream(
        res,
        bus,
        StreamOptions {
            period,
            cursor: Some((session_id.clone(), after)),
            only: Some(session_id),
            types: None,
            connected: false,
            follow,
            runtime: state.runtime.clone(),
        },
    )
    .await;
}

async fn session_exists(db: &ox_db::Db, session_id: &str) -> bool {
    let Ok(conn) = db.acquire().await else {
        return false;
    };
    let Ok(mut stmt) = conn.prepare("SELECT 1 FROM session_v2 WHERE id = ?1").await else {
        return false;
    };
    match stmt.query((session_id,)).await {
        Ok(mut rows) => matches!(rows.next().await, Ok(Some(_))),
        Err(_) => false,
    }
}

/// Shared SSE plumbing: optional replay from `cursor`, then live forwarding
/// (filtered to `only`'s aggregate when set) until the client goes away.
async fn stream(res: &mut Response, bus: EventBus, options: StreamOptions) {
    let StreamOptions {
        period,
        cursor,
        only,
        types,
        connected,
        follow,
        runtime,
    } = options;
    // Subscribe to the broadcast BEFORE the replay query runs (in the task
    // below): anything committed while the replay reads is buffered here and
    // deduplicated against the replayed range.
    let mut live = bus.subscribe();
    let (tx, rx) = mpsc::channel::<String>(BODY_CHANNEL_CAPACITY);
    let stop = runtime.stop().clone();

    let task = tokio::spawn(async move {
        // Upstream sends `server.connected` immediately on connect (no
        // `created` field on this frame).
        if connected {
            let frame =
                json!({ "id": ox_schema::id::id("evt"), "type": "server.connected", "data": {} });
            if tx.send(format!("data: {frame}\n\n")).await.is_err() {
                return; // client already gone
            }
        }

        // Replay the requested durable range, remembering where it ended.
        // Paged: a reconnect into a long session reads one bounded page at a
        // time instead of the whole log, and the emitted frames are identical
        // (same seq order, one page boundary is invisible on the wire).
        let mut replayed: Option<ReplayCursor> = None;
        if let Some((session_id, after)) = cursor {
            let mut after = after;
            let mut last_seq: Option<i64> = None;
            loop {
                match tokio::select! {
                    result = bus.event_log(&session_id, after, REPLAY_PAGE) => result,
                    _ = stop.cancelled() => return,
                } {
                    Ok(events) => {
                        for env in &events {
                            if matches_event_type(env, types.as_deref())
                                && render_frame(&tx, env).await.is_err()
                            {
                                return; // client disconnected mid-replay
                            }
                        }
                        let complete = (events.len() as i64) < REPLAY_PAGE;
                        match events
                            .last()
                            .and_then(|last| last.durable.as_ref().map(|d| d.seq))
                        {
                            Some(seq) => {
                                after = seq;
                                last_seq = Some(seq);
                            }
                            None => break,
                        }
                        if complete {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "event replay failed; closing stream");
                        return;
                    }
                }
            }
            replayed = last_seq.map(|seq| (session_id, seq));
        }
        if !follow {
            return; // finite log: dropping `tx` ends the body
        }

        // Live loop: forward broadcast events, emit heartbeats when idle.
        // interval_at (not interval) so the first heartbeat waits a full
        // period instead of firing immediately.
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = heartbeat.tick() => {
                    if tx.send(": heartbeat\n\n".to_owned()).await.is_err() {
                        return; // client disconnected
                    }
                }
                event = live.recv() => match event {
                    Ok(env) => {
                        if let Some(aggregate) = &only
                            && env
                                .durable
                                .as_ref()
                                .is_none_or(|durable| durable.aggregate_id != *aggregate)
                        {
                            continue;
                        }
                        // Drop durable events the replay already delivered.
                        if let Some((aggregate_id, seq)) = &replayed
                            && let Some(durable) = &env.durable
                            && durable.aggregate_id == *aggregate_id
                            && durable.seq <= *seq
                        {
                            continue;
                        }
                        if !matches_event_type(&env, types.as_deref()) {
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
    if let Err(task) = runtime.adopt(task)
        && let Err(error) = task.await
    {
        tracing::debug!(%error, "event stream task ended after shutdown sealed ownership");
    }

    sse_stream(res, rx);
}

/// 200 with the SSE header set, streaming `rx` as the body.
pub(super) fn sse_stream(res: &mut Response, rx: mpsc::Receiver<String>) {
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

fn matches_event_type(env: &EventEnvelope, types: Option<&[String]>) -> bool {
    types.is_none_or(|types| {
        types.is_empty()
            || env.kind == "server.connected"
            || types.iter().any(|kind| kind == &env.kind)
    })
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

#[cfg(test)]
mod tests {
    use salvo::http::Response;
    use serde_json::json;

    use ox_schema::event::EventEnvelope;

    fn envelope(kind: &str) -> EventEnvelope {
        EventEnvelope {
            id: format!("id-{kind}"),
            created: 0,
            kind: kind.to_owned(),
            data: json!({}),
            durable: None,
            location: None,
            metadata: None,
        }
    }

    #[test]
    fn event_types_filter_is_unfiltered_without_types() {
        assert!(super::matches_event_type(
            &envelope("message.updated"),
            None
        ));
    }

    #[test]
    fn event_types_filter_treats_empty_types_as_unfiltered() {
        let types = Vec::new();
        assert!(super::matches_event_type(
            &envelope("message.updated"),
            Some(&types)
        ));
    }

    #[test]
    fn event_types_filter_matches_one_or_many_types_exactly() {
        let types = vec!["message.updated".to_owned()];
        assert!(super::matches_event_type(
            &envelope("message.updated"),
            Some(&types)
        ));
        assert!(!super::matches_event_type(
            &envelope("message.created"),
            Some(&types)
        ));

        let types = vec!["message.created".to_owned(), "message.updated".to_owned()];
        assert!(super::matches_event_type(
            &envelope("message.updated"),
            Some(&types)
        ));
    }

    #[test]
    fn event_types_filter_applies_to_replay_and_live_frames() {
        let types = vec!["message.updated".to_owned()];
        let replay = envelope("message.updated");
        let live = envelope("message.created");
        assert!(super::matches_event_type(&replay, Some(&types)));
        assert!(!super::matches_event_type(&live, Some(&types)));
    }

    #[test]
    fn server_connected_is_retained_even_with_types_filter() {
        let types = vec!["message.updated".to_owned()];
        assert!(super::matches_event_type(
            &envelope("server.connected"),
            Some(&types)
        ));
    }

    #[test]
    fn event_types_filter_does_not_change_session_or_after_values() {
        let session = "session-1";
        let after = 7_i64;
        let types = vec!["message.updated".to_owned()];
        assert_eq!((session, after), ("session-1", 7));
        assert!(super::matches_event_type(
            &envelope("message.updated"),
            Some(&types)
        ));
    }

    #[tokio::test]
    async fn sse_stream_filters_replay_and_live_frames() {
        use salvo::test::ResponseExt;

        let db = ox_db::Db::open_in_memory().await.expect("test database");
        let bus = ox_core::event_bus::EventBus::new(db.clone());
        let conn = db.acquire().await.expect("test database connection");
        conn.execute(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, 1)",
            ["session-1"],
        )
        .await
        .expect("event sequence");
        conn.execute(
            "INSERT INTO event (id, aggregate_id, seq, created, type, data)
             VALUES (?1, ?2, 0, 1, 'message.updated@v1', '{}')",
            ("event-1", "session-1"),
        )
        .await
        .expect("matching replay event");
        conn.execute(
            "INSERT INTO event (id, aggregate_id, seq, created, type, data)
             VALUES (?1, ?2, 1, 2, 'message.created@v1', '{}')",
            ("event-2", "session-1"),
        )
        .await
        .expect("filtered replay event");
        let mut response = Response::new();
        let runtime = crate::runtime::Runtime::new();
        super::stream(
            &mut response,
            bus,
            super::StreamOptions {
                period: std::time::Duration::from_secs(60),
                cursor: Some(("session-1".to_owned(), -1)),
                only: None,
                types: Some(vec!["message.updated".to_owned()]),
                connected: true,
                follow: false,
                runtime: runtime.clone(),
            },
        )
        .await;
        let body = response.take_string().await.expect("SSE replay body");
        assert!(body.contains("server.connected"));
        assert!(body.contains("message.updated"));
        assert!(!body.contains("message.created"));

        runtime.stop().cancel();
    }

    #[tokio::test]
    async fn sse_stream_filters_live_frames_before_rendering() {
        use std::future::poll_fn;
        use std::pin::Pin;

        use salvo::http::Body;

        let db = ox_db::Db::open_in_memory().await.expect("test database");
        let bus = ox_core::event_bus::EventBus::new(db);
        let live_bus = bus.clone();
        let mut response = Response::new();
        let runtime = crate::runtime::Runtime::new();
        super::stream(
            &mut response,
            bus,
            super::StreamOptions {
                period: std::time::Duration::from_secs(60),
                cursor: None,
                only: None,
                types: Some(vec!["message.updated".to_owned()]),
                connected: true,
                follow: true,
                runtime: runtime.clone(),
            },
        )
        .await;
        let mut body = response.take_body();
        let connected = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .expect("connected frame should arrive")
        .expect("connected frame should exist")
        .expect("connected frame should be valid")
        .into_data()
        .expect("connected frame should contain data");
        assert!(String::from_utf8_lossy(&connected).contains("server.connected"));

        live_bus.broadcast_ephemeral("message.created", json!({}), None);
        live_bus.broadcast_ephemeral("message.updated", json!({}), None);
        let updated = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .expect("matching live frame should arrive")
        .expect("matching live frame should exist")
        .expect("matching live frame should be valid")
        .into_data()
        .expect("matching live frame should contain data");
        let updated = String::from_utf8(updated.to_vec()).expect("SSE is UTF-8");
        assert!(updated.contains("message.updated"));
        assert!(!updated.contains("message.created"));
        drop(body);
        runtime.stop().cancel();
    }
}
