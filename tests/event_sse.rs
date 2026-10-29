//! Integration tests for `event.subscribe` (GET /api/event, SSE).
//!
//! Drives the real router over HTTP with a minimal SSE framing reader:
//! connected frame → durable/ephemeral frames in order, cursor replay after
//! a disconnect, no-dup/no-loss under concurrent publish during replay, and
//! the idle heartbeat comment.

use std::future::poll_fn;
use std::net::TcpListener as StdTcpListener;
use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;
use ox::api::AppState;
use ox::config::Config;
use ox::event_bus::EventBus;
use ox::schema::event::SessionEventData;
use ox::schema::support::{LocationRef, ModelRef};
use salvo::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct TestServer {
    base: String,
    bus: EventBus,
    handle: JoinHandle<()>,
    _dir: TempDir,
}

async fn spawn_server(heartbeat_ms: u64) -> TestServer {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        db_path: dir.path().join("ox.db"),
        listen: format!("127.0.0.1:{port}"),
        event_heartbeat_ms: heartbeat_ms,
        ..Config::default()
    };
    let db = ox::db::Db::open(&config.db_path).await.unwrap();
    let bus = EventBus::new(db.clone());
    let cardinal = ox::cardinal::client::CardinalClient::new(
        config.llm.cardinal_url.clone(),
        config.llm.cardinal_key.clone(),
    )
    .unwrap();
    let model_catalog = ox::model_catalog::ModelCatalog::new(&config.llm);
    let config = std::sync::Arc::new(config);
    let runner = ox::runner::Runner::new(
        std::sync::Arc::clone(&config),
        std::env::current_dir().unwrap(),
        db.clone(),
        bus.clone(),
        std::sync::Arc::new(cardinal),
        std::sync::Arc::new(ox::runner::ToolRegistry::new()),
        std::sync::Arc::clone(&model_catalog),
    );
    let state = AppState {
        bus: bus.clone(),
        db,
        directory: std::env::current_dir().unwrap(),
        live: runner.live(),
        runner,
        config_path: dir.path().join("ox.toml"),
    };
    let router = ox::build_router(state);
    let listen = format!("127.0.0.1:{port}");
    let handle = tokio::spawn(async move {
        let acceptor = TcpListener::new(listen).bind().await;
        Server::new(acceptor).serve(router).await;
    });

    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..50 {
        if client.get(format!("{base}/api/info")).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "server at {base} never became ready");
    TestServer {
        base,
        bus,
        handle,
        _dir: dir,
    }
}

// ---------------------------------------------------------------------------
// Event factories
// ---------------------------------------------------------------------------

fn location() -> LocationRef {
    LocationRef {
        directory: "/tmp/ox".to_string(),
        workspace_id: None,
    }
}

fn created_event(sid: &str) -> SessionEventData {
    SessionEventData::SessionCreated {
        session_id: sid.to_string(),
        project_id: "proj".to_string(),
        location: location(),
        subpath: None,
        parent_id: None,
        slug: sid.to_string(),
        title: Some("Test".to_string()),
        agent: Some("build".to_string()),
        model: Some(ModelRef {
            id: "claude-test".to_string(),
            provider_id: "anthropic".to_string(),
            variant: None,
        }),
        metadata: None,
        permissions: None,
        version: "1.0.0".to_string(),
    }
}

fn renamed_event(sid: &str, title: &str) -> SessionEventData {
    SessionEventData::SessionRenamed {
        session_id: sid.to_string(),
        title: title.to_string(),
    }
}

fn agent_selected_event(sid: &str) -> SessionEventData {
    SessionEventData::SessionAgentSelected {
        session_id: sid.to_string(),
        agent: "build".to_string(),
        previous: None,
    }
}

fn text_delta_event(sid: &str) -> SessionEventData {
    SessionEventData::TextDelta {
        session_id: sid.to_string(),
        assistant_message_id: "msg_delta".to_string(),
        ordinal: 0,
        delta: "hello".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Minimal SSE reader
// ---------------------------------------------------------------------------

/// Await the next item of a body stream with a timeout.
async fn next_chunk<S>(body: &mut S) -> Option<reqwest::Result<bytes::Bytes>>
where
    S: Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    tokio::time::timeout(
        Duration::from_secs(10),
        poll_fn(|cx| Pin::new(&mut *body).poll_next(cx)),
    )
    .await
    .ok()?
}

/// Read the next raw SSE frame (the text between `\n\n` boundaries).
async fn next_frame(
    buffer: &mut String,
    body: &mut (impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
) -> Option<String> {
    loop {
        if let Some(pos) = buffer.find("\n\n") {
            return Some(buffer.drain(..pos + 2).collect());
        }
        let chunk = next_chunk(body).await?.unwrap();
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// Read the next `data:` payload, skipping comment/heartbeat frames.
async fn next_data_frame(
    buffer: &mut String,
    body: &mut (impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
) -> Option<String> {
    loop {
        let frame = next_frame(buffer, body).await?;
        for line in frame.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                return Some(data.to_string());
            }
        }
    }
}

/// Parse a data payload as JSON, failing with the raw text on mismatch.
fn json_frame(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|error| panic!("bad frame JSON {error}: {raw}"))
}

/// Connect to `/api/event` (with an optional query string).
async fn connect(base: &str, query: &str) -> reqwest::Response {
    reqwest::get(format!("{base}/api/event{query}"))
        .await
        .unwrap()
}

/// Assert the next data frame is `server.connected` and drain it.
async fn expect_connected(
    buffer: &mut String,
    body: &mut (impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
) {
    let frame = json_frame(&next_data_frame(buffer, body).await.unwrap());
    assert_eq!(frame["type"], "server.connected");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_delivers_connected_durable_and_ephemeral_in_order() {
    let server = spawn_server(15_000).await;

    let resp = connect(&server.base, "").await;
    // Upstream event.subscribe response headers.
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.headers()["cache-control"], "no-cache, no-transform");
    assert_eq!(resp.headers()["x-accel-buffering"], "no");
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");

    let mut body = resp.bytes_stream();
    let mut buf = String::new();

    // server.connected comes first: upstream sends only id/type/data.
    let connected = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
    assert_eq!(connected["type"], "server.connected");
    assert_eq!(connected["data"], json!({}));
    assert!(connected["id"].as_str().unwrap().starts_with("evt_"));
    assert!(connected.get("created").is_none());

    let sid = "ses_stream1";
    server.bus.publish(created_event(sid)).await.unwrap();
    server
        .bus
        .publish(renamed_event(sid, "renamed"))
        .await
        .unwrap();
    server.bus.publish(text_delta_event(sid)).await.unwrap();

    let f1 = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
    let f2 = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
    let f3 = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());

    // Durable envelope: flattened V2Event with `durable: {aggregateID, seq, version}`.
    assert!(f1["id"].as_str().unwrap().starts_with("evt_"));
    assert_eq!(f1["type"], "session.created");
    assert_eq!(f1["data"]["sessionID"], sid);
    assert!(f1["created"].is_i64());
    assert_eq!(
        f1["durable"],
        json!({"aggregateID": sid, "seq": 0, "version": 1})
    );
    assert!(f2["id"].as_str().unwrap().starts_with("evt_"));
    assert_eq!(f2["type"], "session.renamed");
    assert_eq!(f2["durable"]["seq"], 1);

    // Ephemeral envelope: no `durable` key at all.
    assert!(f3["id"].as_str().unwrap().starts_with("evt_"));
    assert_eq!(f3["type"], "session.text.delta");
    assert!(f3.get("durable").is_none());
    assert_eq!(f3["data"]["delta"], "hello");

    server.handle.abort();
}

#[tokio::test]
async fn replay_from_cursor_then_live() {
    let server = spawn_server(15_000).await;
    let sid = "ses_replay1";
    let base = server.base.clone();

    // A client connects, sees the first durable event, disconnects.
    let resp = connect(&base, "").await;
    let mut body = resp.bytes_stream();
    let mut buf = String::new();
    expect_connected(&mut buf, &mut body).await;
    server.bus.publish(created_event(sid)).await.unwrap();
    let seen = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
    assert_eq!(seen["durable"]["seq"], 0);
    assert!(seen["id"].as_str().unwrap().starts_with("evt_"));
    // Dropping the body closes the connection (bytes_stream consumed resp).
    drop(body);

    // Two more durable events are published while disconnected — they exist
    // only in the durable log, so the frames below provably come from the
    // replay query, not a live buffer.
    server.bus.publish(renamed_event(sid, "a")).await.unwrap(); // seq 1
    server.bus.publish(agent_selected_event(sid)).await.unwrap(); // seq 2

    // Reconnect with the cursor after seq 0: replay seq > 0, then live.
    let resp = connect(&base, &format!("?session={sid}&after=0")).await;
    let mut body = resp.bytes_stream();
    let mut buf = String::new();
    expect_connected(&mut buf, &mut body).await;

    let r1 = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
    let r2 = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
    assert_eq!(r1["type"], "session.renamed");
    assert_eq!(r1["durable"]["seq"], 1);
    assert_eq!(r2["type"], "session.agent.selected");
    assert_eq!(r2["durable"]["seq"], 2);

    // Live events flow after the replayed range.
    server.bus.publish(renamed_event(sid, "b")).await.unwrap(); // seq 3
    let r3 = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
    assert!(r3["id"].as_str().unwrap().starts_with("evt_"));
    assert_eq!(r3["durable"]["seq"], 3);

    // Cursor validation: `after` without `session`, and a malformed `after`.
    let bad = connect(&base, "?after=0").await;
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    let bad: Value = bad.json().await.unwrap();
    assert_eq!(bad["_tag"], "InvalidCursorError");
    let bad = connect(&base, &format!("?session={sid}&after=nope")).await;
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    server.handle.abort();
}

#[tokio::test]
async fn concurrent_publish_during_replay_is_not_duplicated_or_lost() {
    let server = spawn_server(15_000).await;
    let sid = "ses_nodup1";
    let base = server.base.clone();

    // Client sees seq 0 live, disconnects.
    let resp = connect(&base, "").await;
    let mut body = resp.bytes_stream();
    let mut buf = String::new();
    expect_connected(&mut buf, &mut body).await;
    server.bus.publish(created_event(sid)).await.unwrap(); // seq 0
    next_data_frame(&mut buf, &mut body).await.unwrap(); // seq 0
    drop(body);

    // Reconnect with after=0 while a burst of publishes races the replay.
    let resp = connect(&base, &format!("?session={sid}&after=0")).await;
    let mut body = resp.bytes_stream();
    let mut buf = String::new();

    let publisher = {
        let bus = server.bus.clone();
        let sid = sid.to_string();
        tokio::spawn(async move {
            for i in 0..10 {
                bus.publish(renamed_event(&sid, &format!("r{i}")))
                    .await
                    .unwrap(); // seq 1..=10
            }
        })
    };
    publisher.await.unwrap();

    expect_connected(&mut buf, &mut body).await;
    let mut seen = Vec::new();
    while seen.len() < 10 {
        let frame = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
        assert_eq!(frame["durable"]["aggregateID"], sid);
        seen.push(frame["durable"]["seq"].as_i64().unwrap());
    }
    seen.sort_unstable();
    // Every seq exactly once — whether it arrived via replay or the buffer.
    assert_eq!(seen, (1..=10).collect::<Vec<i64>>());

    server.handle.abort();
}

#[tokio::test]
async fn heartbeat_comment_arrives_when_idle() {
    let server = spawn_server(50).await; // default is 15000; shrink for the test

    let resp = connect(&server.base, "").await;
    let mut body = resp.bytes_stream();
    let mut buf = String::new();
    next_data_frame(&mut buf, &mut body).await.unwrap(); // connected

    let started = std::time::Instant::now();
    let frame = next_frame(&mut buf, &mut body).await.unwrap();
    assert_eq!(frame.trim(), ": heartbeat");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "heartbeat took {:?}",
        started.elapsed()
    );

    server.handle.abort();
}

#[tokio::test]
async fn empty_replay_then_live() {
    let server = spawn_server(15_000).await;
    let base = server.base.clone();

    // Unknown session: the replay succeeds with an empty log, then the
    // stream continues with live events.
    let resp = connect(&base, "?session=ses_missing&after=0").await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let mut body = resp.bytes_stream();
    let mut buf = String::new();
    expect_connected(&mut buf, &mut body).await;

    server.bus.publish(created_event("ses_late")).await.unwrap();
    let frame = json_frame(&next_data_frame(&mut buf, &mut body).await.unwrap());
    assert!(frame["id"].as_str().unwrap().starts_with("evt_"));

    server.handle.abort();
}
