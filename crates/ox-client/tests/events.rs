mod support;

use std::time::Duration;

use ox_client::{Client, sse::StreamMsg};
use support::MockOx;

#[tokio::test]
async fn first_connect_uses_global_stream_and_emits_frames() {
    let mock = MockOx::default();
    let frame = "data: {\"id\":\"evt_1\",\"created\":1,\"type\":\"session.text.delta\",\"data\":{\"sessionID\":\"ses_1\",\"assistantMessageID\":\"msg_1\",\"ordinal\":0,\"delta\":\"hi\"}}\n\n";
    mock.reply_raw(
        "/api/event",
        &format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{frame}"
        ),
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let (_seq_tx, seq_rx) = tokio::sync::watch::channel(None);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = ox_client::sse::spawn_events(client, "ses_1".into(), seq_rx, tx);

    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        first,
        StreamMsg::Reconnected | StreamMsg::Event(_)
    ));

    // The delta frame is sent before the stream closes (and thus before the
    // first `Reconnected`), so it may already be the message consumed above.
    let mut saw_delta = matches!(&first, StreamMsg::Event(env) if env.kind == "session.text.delta");
    for _ in 0..20 {
        if saw_delta {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(StreamMsg::Event(env))) => {
                if env.kind == "session.text.delta" {
                    saw_delta = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(saw_delta);
    handle.abort();

    let requests = mock.requests();
    assert!(requests[0].starts_with("GET /api/event "));
}

#[tokio::test]
async fn reconnect_uses_session_and_after() {
    let mock = MockOx::default();
    mock.reply_raw(
        "/api/event",
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let (_seq_tx, seq_rx) = tokio::sync::watch::channel(Some(41));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = ox_client::sse::spawn_events(client, "ses_1".into(), seq_rx, tx);

    for _ in 0..40 {
        if mock.request_count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.abort();

    let requests = mock.requests();
    assert!(requests.len() >= 2, "expected a reconnect attempt");
    assert!(requests[1].contains("/api/event?session=ses_1&after=41"));
}
