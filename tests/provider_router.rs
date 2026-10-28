//! Integration test: [`EndpointRouter`] against mock endpoints speaking the
//! exact HTTP/SSE dialect (`POST /v1/chat/completions`, non-2xx status lines,
//! `data:` chunks, `data: [DONE]`). Asserts both the failover decision and
//! the request bytes each endpoint received.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ox::cardinal::CardinalError;
use ox::cardinal::chat::Chat;
use ox::cardinal::client::CardinalClient;
use ox::cardinal::router::EndpointRouter;
use ox::cardinal::wire::ChatRequest;
use ox::config::{CandidateConfig, Config, LlmConfig, ModelRouteConfig, ProviderConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const OK_SSE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";

const STATUS_429: &str = "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n";
const STATUS_400: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";

/// One mock endpoint: captures the first request it receives, replays the
/// scripted response, then closes. `hits` counts accepted connections.
async fn spawn_mock(
    response: &'static str,
    hits: Arc<AtomicUsize>,
) -> (SocketAddr, Arc<Mutex<Option<String>>>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let capture = captured.clone();
    let handle = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        hits.fetch_add(1, Ordering::SeqCst);
        let request = read_request(&mut socket).await;
        *capture.lock().await = Some(request);
        socket.write_all(response.as_bytes()).await.ok();
    });
    (addr, captured, handle)
}

/// Read one HTTP request (head + content-length body) from the socket.
async fn read_request(socket: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if socket.read_exact(&mut byte).await.is_err() {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("content-length") {
                return None;
            }
            value.trim().parse().ok()
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if socket.read_exact(&mut body).await.is_ok() {
        buf.extend_from_slice(&body);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// The JSON body of a captured request (everything after the blank line).
fn request_body(captured: &str) -> &str {
    captured
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

fn config_with(providers: &[(&str, SocketAddr)], routes: &[(&str, &[(&str, &str)])]) -> Config {
    let mut config = Config::default();
    for (name, addr) in providers {
        config.llm.providers.insert(
            (*name).to_owned(),
            ProviderConfig {
                base_url: format!("http://{addr}"),
                api_key: None,
            },
        );
    }
    for (id, candidates) in routes {
        config.llm.models.insert(
            (*id).to_owned(),
            ModelRouteConfig {
                candidates: candidates
                    .iter()
                    .map(|(provider, model)| CandidateConfig {
                        provider: (*provider).to_owned(),
                        model: (*model).to_owned(),
                    })
                    .collect(),
            },
        );
    }
    config
}

fn router_with(config: &Config) -> EndpointRouter {
    // The implicit endpoint is never contacted in these tests (explicit
    // routes + configured providers disable the passthrough).
    let cardinal = CardinalClient::new("http://127.0.0.1:9", None).unwrap();
    EndpointRouter::from_config(&config.llm, cardinal).unwrap()
}

fn request(model: &str) -> ChatRequest {
    // The mocks do not validate message content; the assertions target the
    // model field and the attempt sequence.
    ChatRequest::new(model, Vec::new())
}

#[tokio::test]
async fn failover_moves_to_the_next_candidate_and_rewrites_the_wire_model() {
    let (addr_a, captured_a, handle_a) =
        spawn_mock(STATUS_429, Arc::new(AtomicUsize::new(0))).await;
    let hits_b = Arc::new(AtomicUsize::new(0));
    let (addr_b, captured_b, handle_b) = spawn_mock(OK_SSE, hits_b.clone()).await;

    let config = config_with(
        &[("a", addr_a), ("b", addr_b)],
        &[("demo", &[("a", "wire-a"), ("b", "wire-b")])],
    );
    let router = router_with(&config);

    // The returned stream is dropped unconsumed: the assertions target the
    // request bytes each mock captured, and stream parsing is already
    // covered by tests/cardinal_stream.rs.
    let _ = router
        .stream_chat(&request("demo"))
        .await
        .expect("second candidate serves");

    let a = captured_a.lock().await.clone().expect("a was contacted");
    assert!(a.starts_with("POST /v1/chat/completions"));
    assert!(request_body(&a).contains("\"model\":\"wire-a\""));
    let b = captured_b.lock().await.clone().expect("b was contacted");
    assert!(
        request_body(&b).starts_with("{\"model\":\"wire-b\",\"messages\":"),
        "canonical field order, got: {b}"
    );
    assert_eq!(hits_b.load(Ordering::SeqCst), 1);

    handle_a.abort();
    handle_b.abort();
}

#[tokio::test]
async fn non_failover_status_stops_the_chain() {
    let (addr_a, captured_a, handle_a) =
        spawn_mock(STATUS_400, Arc::new(AtomicUsize::new(0))).await;
    let hits_b = Arc::new(AtomicUsize::new(0));
    let (addr_b, _captured_b, handle_b) = spawn_mock(OK_SSE, hits_b.clone()).await;

    let config = config_with(
        &[("a", addr_a), ("b", addr_b)],
        &[("demo", &[("a", "wire-a"), ("b", "wire-b")])],
    );
    let router = router_with(&config);

    // `expect_err` is unusable here: the Ok type (the boxed chat stream) has
    // no `Debug` impl, so the error is extracted with let-else instead.
    let Err(error) = router.stream_chat(&request("demo")).await else {
        panic!("400 must surface without failover");
    };
    assert!(matches!(error, CardinalError::Http { status: 400, .. }));
    assert!(captured_a.lock().await.is_some());
    assert_eq!(
        hits_b.load(Ordering::SeqCst),
        0,
        "second endpoint must not be contacted"
    );

    handle_a.abort();
    handle_b.abort();
}

#[tokio::test]
async fn exhausted_candidates_surface_the_last_error() {
    let (addr_a, captured_a, handle_a) =
        spawn_mock(STATUS_429, Arc::new(AtomicUsize::new(0))).await;
    let (addr_b, captured_b, handle_b) =
        spawn_mock(STATUS_429, Arc::new(AtomicUsize::new(0))).await;

    let config = config_with(
        &[("a", addr_a), ("b", addr_b)],
        &[("demo", &[("a", "wire-a"), ("b", "wire-b")])],
    );
    let router = router_with(&config);

    let Err(error) = router.stream_chat(&request("demo")).await else {
        panic!("all candidates failed");
    };
    assert!(matches!(error, CardinalError::Http { status: 429, .. }));
    assert!(captured_a.lock().await.is_some());
    assert!(captured_b.lock().await.is_some());

    handle_a.abort();
    handle_b.abort();
}

#[tokio::test]
async fn unrouted_model_is_an_error_when_providers_are_configured() {
    let (addr_a, _captured, handle_a) = spawn_mock(OK_SSE, Arc::new(AtomicUsize::new(0))).await;
    let config = config_with(&[("a", addr_a)], &[]);
    let router = router_with(&config);

    let Err(error) = router.stream_chat(&request("ghost")).await else {
        panic!("explicit routing must not fall back to cardinal");
    };
    assert!(matches!(error, CardinalError::NoRoute { model } if model == "ghost"));

    handle_a.abort();
}

#[tokio::test]
async fn transport_failure_fails_over_to_the_next_candidate() {
    // A provably closed port: bind and immediately drop (no dependence on
    // well-known ports).
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = closed.local_addr().unwrap();
    drop(closed);

    let (addr_b, captured_b, handle_b) = spawn_mock(OK_SSE, Arc::new(AtomicUsize::new(0))).await;
    let config = config_with(
        &[("dead", dead_addr), ("b", addr_b)],
        &[("demo", &[("dead", "wire-x"), ("b", "wire-b")])],
    );
    let router = router_with(&config);

    let stream = router
        .stream_chat(&request("demo"))
        .await
        .expect("second candidate serves after connect refusal");
    drop(stream);
    let b = captured_b.lock().await.clone().expect("b was contacted");
    assert!(request_body(&b).contains("\"model\":\"wire-b\""));

    handle_b.abort();
}

#[tokio::test]
async fn passthrough_sends_the_logical_model_unchanged() {
    let (addr, captured, handle) = spawn_mock(OK_SSE, Arc::new(AtomicUsize::new(0))).await;

    // No [providers]: the implicit endpoint is the passthrough target.
    let config = Config {
        llm: LlmConfig {
            cardinal_url: format!("http://{addr}"),
            ..Default::default()
        },
        ..Default::default()
    };
    let cardinal = CardinalClient::new(config.llm.cardinal_url.clone(), None).unwrap();
    let router = EndpointRouter::from_config(&config.llm, cardinal).unwrap();

    let _ = router
        .stream_chat(&request("k3-256k"))
        .await
        .expect("passthrough serves");

    let request_bytes = captured.lock().await.clone().expect("mock was contacted");
    assert!(
        request_body(&request_bytes).starts_with("{\"model\":\"k3-256k\",\"messages\":"),
        "wire-exact passthrough required, got: {request_bytes}"
    );

    handle.abort();
}
