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

    tokio::spawn(async move {
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

    sse_stream(res, rx);
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use salvo::conn::{Listener, TcpListener};
    use salvo::prelude::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use crate::api::{AppState, build_router};
    use crate::test_support::{test_state, test_state_with};
    use ox_core::config::Config;

    const WAIT: Duration = Duration::from_secs(5);

    async fn serve(app: &AppState) -> SocketAddr {
        let acceptor = TcpListener::new("127.0.0.1:0").bind().await;
        let addr = acceptor
            .local_addr()
            .expect("local addr")
            .to_string()
            .parse()
            .expect("socket addr");
        tokio::spawn(Server::new(acceptor).serve(build_router(app.clone())));
        addr
    }

    async fn send_get(addr: SocketAddr, path: &str) -> TcpStream {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        stream
    }

    /// Read until `needle` appears; returns everything read so far.
    async fn read_until(stream: &mut TcpStream, needle: &str) -> String {
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        let deadline = Instant::now() + WAIT;
        while !String::from_utf8_lossy(&seen).contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            let n = tokio::time::timeout(left, stream.read(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"))
                .expect("read");
            assert!(n > 0, "eof before {needle:?}");
            seen.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    async fn wait_leases(app: &AppState, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.lifecycle.lease_count() != want {
            assert!(
                Instant::now() < deadline,
                "lease_count stuck at {} (want {want})",
                app.lifecycle.lease_count()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn lease_is_held_for_the_life_of_the_connection() {
        let app = test_state().await;
        let addr = serve(&app).await;

        let mut conn = send_get(addr, "/ox/lease").await;
        let head = read_until(&mut conn, ": lease").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: text/event-stream"),
            "{head}"
        );
        assert_eq!(app.lifecycle.lease_count(), 1);

        drop(conn);
        wait_leases(&app, 0).await;
    }

    #[tokio::test]
    async fn concurrent_connections_each_hold_a_lease() {
        let app = test_state().await;
        let addr = serve(&app).await;

        let mut a = send_get(addr, "/ox/lease").await;
        let mut b = send_get(addr, "/ox/lease").await;
        read_until(&mut a, ": lease").await;
        read_until(&mut b, ": lease").await;
        assert_eq!(app.lifecycle.lease_count(), 2);

        drop(a);
        wait_leases(&app, 1).await;
        drop(b);
        wait_leases(&app, 0).await;
    }

    #[tokio::test]
    async fn closing_server_refuses_new_leases() {
        let app = test_state().await;
        let addr = serve(&app).await;

        let grace = Duration::from_secs(10);
        assert!(
            app.lifecycle
                .try_close(Instant::now() + grace, false, grace)
        );

        let mut conn = send_get(addr, "/ox/lease").await;
        let mut body = String::new();
        conn.read_to_string(&mut body).await.expect("read response");
        assert!(body.starts_with("HTTP/1.1 503"), "{body}");
        assert!(body.contains(r#"{"error":"closing"}"#), "{body}");
        assert_eq!(app.lifecycle.lease_count(), 0);
    }

    #[tokio::test]
    async fn lease_is_not_served_under_api() {
        let app = test_state().await;
        let addr = serve(&app).await;

        let mut conn = send_get(addr, "/api/lease").await;
        let mut body = String::new();
        conn.read_to_string(&mut body).await.expect("read response");
        assert!(body.starts_with("HTTP/1.1 404"), "{body}");
    }

    #[tokio::test]
    async fn heartbeat_follows_the_event_heartbeat_config() {
        let config = Config {
            event_heartbeat_ms: 30,
            ..Config::default()
        };
        let app = test_state_with(config).await;
        let addr = serve(&app).await;

        let mut conn = send_get(addr, "/ox/lease").await;
        let seen = read_until(&mut conn, ": heartbeat").await;
        assert!(seen.contains(": lease"), "{seen}");
    }
}
