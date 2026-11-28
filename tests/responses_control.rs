//! Lifecycle coverage for the native Responses transport: every control path
//! is typed, built relative to the configured base URL, and preserves upstream
//! 4xx as errors instead of turning them into empty successes.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ox_llm::chat_completions::wire::{ChatMessage, ChatRequest, UserContent};
use ox_llm::error::ChatError;
use ox_llm::http_client;
use ox_llm::responses::client::ResponsesClient;
use ox_llm::responses::control::{ListParams, ResponsesControl};
use ox_llm::responses::options::ResponsesOptions;
use ox_llm::responses::wire::serde_json;

#[derive(Debug)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

/// One canned response per connection; each request is a fresh connection so
/// the test can assert the whole method/path/body triple without a proxy.
fn mock(responses: Vec<(u16, &'static str)>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock listener binds");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("mock has an address")
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&requests);
    // Detached: an early test panic must never hang the suite in `join()`.
    let _ = thread::spawn(move || {
        for (status, body) in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut raw = Vec::new();
            let mut byte = [0u8; 1];
            let head_end = loop {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    break raw.len();
                }
                raw.push(byte[0]);
                if raw.ends_with(b"\r\n\r\n") {
                    break raw.len();
                }
            };
            let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or_default().to_owned();
            let mut method = String::new();
            let mut path = String::new();
            let mut parts = request_line.split_whitespace();
            if let Some(value) = parts.next() {
                method = value.to_owned();
            }
            if let Some(value) = parts.next() {
                path = value.to_owned();
            }
            let length = lines
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let mut payload = vec![0u8; length];
            let _ = stream.read_exact(&mut payload);
            sink.lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(Recorded {
                    method,
                    path,
                    body: String::from_utf8_lossy(&payload).into_owned(),
                });
            let reply = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
        }
    });
    Mock { base, requests }
}

#[tokio::test]
async fn lifecycle_paths_are_relative_and_results_are_typed() {
    let mock = mock(vec![
        (
            200,
            r#"{"id":"resp-1","status":"completed","output":[{"type":"message"}]}"#,
        ),
        (200, r#"{"id":"resp-1","status":"cancelled","output":[]}"#),
        (200, r#"{"id":"resp-1","deleted":true}"#),
        (
            200,
            r#"{"data":[{"id":"item-1","type":"message"}],"has_more":false}"#,
        ),
        (
            200,
            r#"{"input_tokens":42,"input_tokens_details":{"cached_tokens":7}}"#,
        ),
        (200, r#"{"output":[{"type":"message"}],"id":"resp-2"}"#),
        (200, r#"{"id":"conv-1","metadata":{"a":"b"}}"#),
        (200, r#"{"id":"conv-1","metadata":{"a":"c"}}"#),
        (200, r#"{"data":[{"id":"conv-1"}],"has_more":false}"#),
        (200, r#"{"id":"conv-1","deleted":true}"#),
        (200, r#"{"id":"item-9","type":"message"}"#),
        (200, r#"{"data":[{"id":"item-9"}],"has_more":false}"#),
        (200, r#"{"id":"item-9","deleted":true}"#),
    ]);
    let client = http_client().build().expect("client builds");
    let control = ResponsesControl::new(&client, &mock.base, Some("secret"));

    let response = control.retrieve("resp-1").await.expect("retrieve succeeds");
    assert_eq!(response.id, "resp-1");
    assert_eq!(response.status, "completed");
    assert_eq!(response.output.len(), 1);
    let cancelled = control.cancel("resp-1").await.expect("cancel succeeds");
    assert_eq!(cancelled.status, "cancelled");
    let deleted = control.delete("resp-1").await.expect("delete succeeds");
    assert!(deleted.deleted);
    let items = control
        .list_input_items("resp-1", &ListParams::default().with_limit(3))
        .await
        .expect("items succeed");
    assert_eq!(items.data.len(), 1);
    assert!(!items.has_more);
    let tokens = control
        .count_tokens(&serde_json::json!({"model":"m"}))
        .await
        .expect("count succeeds");
    assert_eq!(tokens.input_tokens, 42);
    let compact = control
        .compact(&serde_json::json!({"model":"m"}))
        .await
        .expect("compact succeeds");
    assert_eq!(compact.output.len(), 1);
    let conversation = control
        .create_conversation(&serde_json::json!({}))
        .await
        .expect("create succeeds");
    assert_eq!(conversation.id, "conv-1");
    let updated = control
        .update_conversation("conv-1", &serde_json::json!({"metadata":{"a":"c"}}))
        .await
        .expect("update succeeds");
    assert_eq!(updated.metadata, Some(serde_json::json!({"a":"c"})));
    let conversations = control
        .list_conversations(&ListParams::default())
        .await
        .expect("list succeeds");
    assert_eq!(conversations.data.len(), 1);
    let conversation_deleted = control
        .delete_conversation("conv-1")
        .await
        .expect("delete succeeds");
    assert!(conversation_deleted.deleted);
    let item = control
        .create_conversation_item("conv-1", &serde_json::json!({"type":"message"}))
        .await
        .expect("create item succeeds");
    assert_eq!(item["id"], "item-9");
    let conversation_items = control
        .list_conversation_items("conv-1", &ListParams::default())
        .await
        .expect("list items succeeds");
    assert_eq!(conversation_items.data.len(), 1);
    let deleted_item = control
        .delete_conversation_item("conv-1", "item-9")
        .await
        .expect("delete item succeeds");
    assert!(deleted_item.deleted);

    let recorded = mock
        .requests
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let paths: Vec<_> = recorded
        .iter()
        .map(|request| request.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/v1/responses/resp-1",
            "/v1/responses/resp-1/cancel",
            "/v1/responses/resp-1",
            "/v1/responses/resp-1/input_items?limit=3",
            "/v1/responses/count_tokens",
            "/v1/responses/compact",
            "/v1/conversations",
            "/v1/conversations/conv-1",
            "/v1/conversations",
            "/v1/conversations/conv-1",
            "/v1/conversations/conv-1/items",
            "/v1/conversations/conv-1/items",
            "/v1/conversations/conv-1/items/item-9",
        ]
    );
    assert!(
        recorded
            .iter()
            .all(|request| request.method != "GET" || request.body.is_empty())
    );
    assert_eq!(recorded[3].method, "GET");
    assert_eq!(recorded[9].method, "DELETE");
    assert_eq!(recorded[11].method, "GET");
}

#[tokio::test]
async fn base_url_that_already_contains_v1_is_not_duplicated() {
    let mock = mock(vec![(
        200,
        r#"{"id":"resp-1","status":"queued","output":[]}"#,
    )]);
    let client = http_client().build().expect("client builds");
    let base = format!("{}/v1", mock.base);
    let control = ResponsesControl::new(&client, &base, None);
    let response = control.retrieve("resp-1").await.expect("retrieve succeeds");
    assert_eq!(response.status, "queued");
    let recorded = mock
        .requests
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(recorded[0].path, "/v1/responses/resp-1");
}

#[tokio::test]
async fn disconnected_background_stream_resumes_once_without_replaying_text() {
    let first = "data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{\"id\":\"resp-1\"}}\n\n\
                 data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"first\"}\n\n";
    let resumed = "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"second\"}\n\n\
                   data: {\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{\"id\":\"resp-1\",\"status\":\"completed\",\"output\":[]}}\n\n\
                   data: [DONE]\n\n";
    let mock = mock(vec![(200, first), (200, resumed)]);
    let client = ResponsesClient::with_options(
        &mock.base,
        None,
        None,
        "vendor",
        ResponsesOptions {
            background: Some(true),
            ..ResponsesOptions::default()
        },
    )
    .expect("client builds");
    let mut request = ChatRequest::new(
        "gpt-5",
        vec![ChatMessage::User {
            content: UserContent::Text("hello".to_owned()),
        }],
    );
    request
        .set_responses_options(&ResponsesOptions {
            background: Some(true),
            ..ResponsesOptions::default()
        })
        .expect("options serialize");
    let mut stream = client.stream_chat(&request).await.expect("stream opens");
    let mut texts = Vec::new();
    let mut finished = false;
    while let Some(event) = stream.next_event().await {
        match event.expect("no protocol error") {
            ox_llm::chat_completions::stream::StreamEvent::TextDelta(text) => texts.push(text),
            ox_llm::chat_completions::stream::StreamEvent::Done => {
                finished = true;
                break;
            }
            _ => {}
        }
    }
    assert!(finished, "the resumed stream reaches Done");
    assert_eq!(texts, vec!["first", "second"]);

    let recorded = mock
        .requests
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(recorded.len(), 2, "exactly one reconnect");
    assert_eq!(recorded[0].method, "POST");
    assert!(
        recorded[1].path.contains("starting_after=2"),
        "{}",
        recorded[1].path
    );
    assert!(
        recorded[1].path.contains("stream=true"),
        "{}",
        recorded[1].path
    );
}

#[tokio::test]
async fn long_background_tier_polls_retrieve_and_emits_one_stream() {
    let mock = mock(vec![
        (200, r#"{"id":"resp-2","status":"queued","output":[]}"#),
        (200, r#"{"id":"resp-2","status":"in_progress","output":[]}"#),
        (
            200,
            r#"{"id":"resp-2","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":1,"output_tokens":2}}"#,
        ),
    ]);
    let options = ResponsesOptions {
        background: Some(true),
        service_tier: Some("2h".to_owned()),
        ..ResponsesOptions::default()
    };
    let client = ResponsesClient::with_options(&mock.base, None, None, "vendor", options.clone())
        .expect("client builds")
        .with_polling(Duration::ZERO, 4);
    let mut request = ChatRequest::new(
        "gpt-5",
        vec![ChatMessage::User {
            content: UserContent::Text("hello".to_owned()),
        }],
    );
    request
        .set_responses_options(&options)
        .expect("options serialize");
    let mut stream = client
        .stream_chat(&request)
        .await
        .expect("poll stream opens");
    let mut texts = Vec::new();
    let mut finished = false;
    while let Some(event) = stream.next_event().await {
        match event.expect("no polling error") {
            ox_llm::chat_completions::stream::StreamEvent::TextDelta(text) => texts.push(text),
            ox_llm::chat_completions::stream::StreamEvent::Done => {
                finished = true;
                break;
            }
            _ => {}
        }
    }
    assert!(finished, "polling converges into Done");
    assert_eq!(texts, vec!["done"]);
    let recorded = mock
        .requests
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[0].method, "POST");
    assert_eq!(recorded[1].path, "/v1/responses/resp-2");
    assert_eq!(recorded[2].path, "/v1/responses/resp-2");
    let body: serde_json::Value =
        serde_json::from_str(&recorded[0].body).expect("create body is JSON");
    assert_eq!(body["stream"], false);
    assert_eq!(body["background"], true);
    assert_eq!(body["service_tier"], "2h");
}

#[tokio::test]
async fn not_found_and_conflict_are_errors_not_empty_successes() {
    let mock = mock(vec![
        (404, r#"{"error":{"message":"no such response"}}"#),
        (409, r#"{"error":{"message":"already cancelled"}}"#),
    ]);
    let client = http_client().build().expect("client builds");
    let control = ResponsesControl::new(&client, &mock.base, None);
    let missing = control
        .retrieve("resp-missing")
        .await
        .expect_err("404 is an error");
    assert!(
        matches!(missing, ChatError::Http { status: 404, .. }),
        "expected HTTP 404, got {missing:?}"
    );
    let conflict = control
        .cancel("resp-done")
        .await
        .expect_err("409 is an error");
    assert!(
        matches!(conflict, ChatError::Http { status: 409, .. }),
        "expected HTTP 409, got {conflict:?}"
    );
}
