//! `GET /ox/lease` — an ox-only long-lived connection that holds one client
//! lease for as long as it stays open.
//!
//! The lease is the [`LeaseGuard`](crate::lifecycle::LeaseGuard) owned by the
//! producer task. It is released when the task ends, which happens as soon as
//! the response body is dropped (`tx.closed()`), or at the next heartbeat
//! whose send fails. Once the server has committed to exit the endpoint
//! answers 503 `{"error":"closing"}`.

use std::time::Duration;

use salvo::http::StatusCode;
use salvo::prelude::*;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use super::event::sse_stream;
use super::state;

#[handler]
pub(crate) async fn lease(depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Ok(guard) = app.lifecycle.acquire(std::time::Instant::now()) else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        res.render(Json(json!({ "error": "closing" })));
        return;
    };
    // Same heartbeat knob as `/api/event`; a zero period would spin.
    let period = Duration::from_millis(app.services().config.event_heartbeat_ms.max(1));
    let (tx, rx) = mpsc::channel::<String>(1);

    let task = tokio::spawn(async move {
        // Held for the connection's lifetime; dropped on every exit path.
        let _guard = guard;
        if tx.send(": lease\n\n".to_owned()).await.is_err() {
            return;
        }
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = tx.closed() => return,
                _ = heartbeat.tick() => {
                    if tx.send(": heartbeat\n\n".to_owned()).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    if let Err(task) = app.runtime.adopt(task)
        && let Err(error) = task.await
    {
        tracing::debug!(%error, "lease task ended after shutdown sealed ownership");
    }

    sse_stream(res, rx);
}
