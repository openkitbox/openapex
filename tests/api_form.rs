//! `/api/session/{id}/form` end to end: a `question` tool call opens a form,
//! an HTTP client answers it, and the tool call settles with the answers.

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ox::config::Config;
use ox::runner::{Tool, ToolContext, ToolRegistry};
use ox::tool::question::QuestionTool;
use salvo::Listener;

/// Boot the server with the built-in toolset on a free port.
///
/// The form registry is process-wide and its event bus is (re)wired by every
/// boot, so tests in this binary must boot one at a time; the returned guard
/// keeps the lock until the test ends.
async fn server() -> (String, tempfile::TempDir, ServerGuard) {
    static BOOT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = ServerGuard {
        _lock: BOOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral port")
        .local_addr()
        .expect("addr")
        .port();
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        db_path: dir.path().join("ox.db"),
        listen: format!("127.0.0.1:{port}"),
        ..Config::default()
    };
    let state = ox::build_state_with(config.clone(), unused_chat(), ToolRegistry::with_builtins())
        .await
        .expect("build state");
    let router = ox::build_router(state);
    let listen = config.listen.clone();
    tokio::spawn(async move {
        let acceptor = salvo::conn::TcpListener::new(listen).bind().await;
        salvo::Server::new(acceptor).serve(router).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client.get(format!("{base}/api/info")).send().await.is_ok() {
            return (base, dir, guard);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

/// Holds the boot lock for the lifetime of one test.
struct ServerGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

/// The form endpoints never call the model; an empty transport is enough.
fn unused_chat() -> std::sync::Arc<dyn ox::runner::Chat> {
    struct UnusedChat;
    impl ox::runner::Chat for UnusedChat {
        fn stream_chat<'a>(
            &'a self,
            _request: &'a ox::cardinal::wire::ChatRequest,
        ) -> ox::cardinal::chat::ChatFuture<'a> {
            Box::pin(async { Err(ox::cardinal::CardinalError::EarlyClose) })
        }
    }
    std::sync::Arc::new(UnusedChat)
}

#[tokio::test]
async fn a_question_tool_call_is_answered_over_http() {
    let (base, _dir, _guard) = server().await;
    let client = reqwest::Client::new();
    let session_id = "ses_form_http".to_owned();

    // The tool call, exactly as the runner would issue it.
    let ctx = ToolContext {
        session_id: session_id.clone(),
        directory: std::env::temp_dir(),
        call_id: "call_form".to_owned(),
        cancellation: CancellationToken::new(),
    };
    let task = tokio::spawn(async move {
        QuestionTool
            .execute(
                ctx,
                json!({
                    "questions": [{
                        "question": "Which layer should change?",
                        "header": "Layer",
                        "options": [{"label": "API"}, {"label": "Storage"}]
                    }]
                }),
            )
            .await
    });

    // The client lists the session's forms, then reads one.
    let form_id = loop {
        let body: Value = client
            .get(format!("{base}/api/session/{session_id}/form"))
            .send()
            .await
            .expect("list")
            .json()
            .await
            .expect("json");
        let data = body["data"].as_array().expect("array");
        if let Some(info) = data.first() {
            break info["id"].as_str().expect("id").to_owned();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let detail: Value = client
        .get(format!("{base}/api/session/{session_id}/form/{form_id}"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(detail["data"]["state"]["status"], "pending");
    assert_eq!(detail["data"]["fields"][0]["title"], "Layer");

    // Unknown sessions answer an empty list (the TUI's "global" bootstrap).
    let global: Value = client
        .get(format!("{base}/api/session/global/form"))
        .send()
        .await
        .expect("global")
        .json()
        .await
        .expect("json");
    assert_eq!(global["data"].as_array().unwrap().len(), 0);

    // Answer it. Upstream's `session.form.reply` is `HttpApiSchema.NoContent`,
    // and the TUI client hard-codes `successStatus: 204`.
    let replied = client
        .post(format!(
            "{base}/api/session/{session_id}/form/{form_id}/reply"
        ))
        .json(&json!({ "answer": { "q0": "Storage" } }))
        .send()
        .await
        .expect("reply");
    assert_eq!(replied.status(), 204);

    let output = task.await.expect("join");
    assert_eq!(output.error, None);
    assert_eq!(
        output.content,
        json!(
            "User has answered your questions: \"Which layer should change?\"=\"Storage\". You can now continue with the user's answers in mind."
        )
    );

    // The form is consumed; a second reply conflicts with the settled tag
    // the TUI client treats as benign, and the list is empty.
    let again = client
        .post(format!(
            "{base}/api/session/{session_id}/form/{form_id}/reply"
        ))
        .json(&json!({ "answer": { "q0": "API" } }))
        .send()
        .await
        .expect("reply again");
    assert_eq!(again.status(), 409);
    let again_body: Value = again.json().await.expect("json");
    assert_eq!(again_body["_tag"], "FormAlreadySettledError");
    let after: Value = client
        .get(format!("{base}/api/session/{session_id}/form"))
        .send()
        .await
        .expect("list after")
        .json()
        .await
        .expect("json");
    assert_eq!(after["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_cancelled_form_fails_the_tool_with_its_message() {
    let (base, _dir, _guard) = server().await;
    let client = reqwest::Client::new();
    let session_id = "ses_form_cancel".to_owned();

    let ctx = ToolContext {
        session_id: session_id.clone(),
        directory: std::env::temp_dir(),
        call_id: "call_form2".to_owned(),
        cancellation: CancellationToken::new(),
    };
    let task = tokio::spawn(async move {
        QuestionTool
            .execute(
                ctx,
                json!({
                    "questions": [{
                        "question": "Proceed?",
                        "options": [{"label": "Yes"}, {"label": "No"}]
                    }]
                }),
            )
            .await
    });
    let form_id = loop {
        let body: Value = client
            .get(format!("{base}/api/session/{session_id}/form"))
            .send()
            .await
            .expect("list")
            .json()
            .await
            .expect("json");
        if let Some(info) = body["data"].as_array().and_then(|data| data.first()) {
            break info["id"].as_str().expect("id").to_owned();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let cancelled = client
        .delete(format!(
            "{base}/api/session/{session_id}/form/{form_id}?message=pick%20for%20me"
        ))
        .send()
        .await
        .expect("cancel");
    assert_eq!(cancelled.status(), 204);

    let output = task.await.expect("join");
    assert_eq!(output.error.as_deref(), Some("pick for me"));

    // An unknown form answers 404 on every route.
    for (method, path) in [
        (
            "GET",
            format!("{base}/api/session/{session_id}/form/frm_missing"),
        ),
        (
            "DELETE",
            format!("{base}/api/session/{session_id}/form/frm_missing"),
        ),
    ] {
        let response = match method {
            "GET" => client.get(&path).send().await.expect("get"),
            _ => client.delete(&path).send().await.expect("delete"),
        };
        assert_eq!(response.status(), 404, "{method} {path}");
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["_tag"], "FormNotFoundError");
    }
}

#[tokio::test]
async fn a_malformed_reply_body_is_a_400() {
    let (base, _dir, _guard) = server().await;
    let client = reqwest::Client::new();
    let session_id = "ses_form_bad".to_owned();
    let ctx = ToolContext {
        session_id: session_id.clone(),
        directory: std::env::temp_dir(),
        call_id: "call_form3".to_owned(),
        cancellation: CancellationToken::new(),
    };
    let task = tokio::spawn(async move {
        QuestionTool
            .execute(
                ctx,
                json!({
                    "questions": [{
                        "question": "Proceed?",
                        "options": [{"label": "Yes"}, {"label": "No"}]
                    }]
                }),
            )
            .await
    });
    let form_id = loop {
        let body: Value = client
            .get(format!("{base}/api/session/{session_id}/form"))
            .send()
            .await
            .expect("list")
            .json()
            .await
            .expect("json");
        if let Some(info) = body["data"].as_array().and_then(|data| data.first()) {
            break info["id"].as_str().expect("id").to_owned();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let bad = client
        .post(format!(
            "{base}/api/session/{session_id}/form/{form_id}/reply"
        ))
        .body("not json")
        .send()
        .await
        .expect("bad reply");
    assert_eq!(bad.status(), 400);

    // Clean up so the tool call does not park for its whole timeout.
    let cancelled = client
        .delete(format!("{base}/api/session/{session_id}/form/{form_id}"))
        .send()
        .await
        .expect("cancel");
    assert_eq!(cancelled.status(), 204);
    let _ = task.await;
}

/// The TUI builds its question dialog purely from the ephemeral `form.*`
/// events on `GET /api/event`; a form opened by the tool must arrive there,
/// and an HTTP answer must be followed by `form.replied`.
#[tokio::test]
async fn a_form_reaches_the_sse_event_stream() {
    let (base, _dir, _guard) = server().await;
    let client = reqwest::Client::new();
    let session_id = "ses_form_sse".to_owned();

    // Subscribe before the tool call, exactly like the TUI does.
    let response = client
        .get(format!("{base}/api/event"))
        .send()
        .await
        .expect("sse");
    assert_eq!(response.status(), 200);

    let ctx = ToolContext {
        session_id: session_id.clone(),
        directory: std::env::temp_dir(),
        call_id: "call_form_sse".to_owned(),
        cancellation: CancellationToken::new(),
    };
    let task = tokio::spawn(async move {
        QuestionTool
            .execute(
                ctx,
                json!({
                    "questions": [{
                        "question": "Which layer should change?",
                        "header": "Layer",
                        "options": [{"label": "API"}, {"label": "Storage"}]
                    }]
                }),
            )
            .await
    });

    // Read frames until the created event; the tool call answers nothing, so
    // cancel the form once it is seen.
    let mut buffer = String::new();
    let mut saw_created = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut response = response;
    while saw_created.is_none() && tokio::time::Instant::now() < deadline {
        let Some(chunk) = response.chunk().await.expect("chunk") else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(boundary) = buffer.find("\n\n") {
            let frame = buffer.drain(..boundary + 2).collect::<String>();
            let Ok(event) = serde_json::from_str::<Value>(frame.trim_start_matches("data: "))
            else {
                continue;
            };
            if event["type"] == "form.created" && event["data"]["form"]["sessionID"] == session_id {
                saw_created = Some(event);
                break;
            }
        }
    }
    let created = saw_created.expect("form.created on the SSE stream");
    let form_id = created["data"]["form"]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_eq!(created["data"]["form"]["title"], "Questions");
    assert_eq!(created["data"]["form"]["fields"][0]["type"], "string");
    assert_eq!(
        created["data"]["form"]["fields"][0]["options"][0]["label"],
        "API"
    );

    // Answer it and expect the replied event through the same stream.
    let replied_http = client
        .post(format!(
            "{base}/api/session/{session_id}/form/{form_id}/reply"
        ))
        .json(&json!({ "answer": { "q0": "Storage" } }))
        .send()
        .await
        .expect("reply");
    assert_eq!(replied_http.status(), 204);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut saw_replied = false;
    while !saw_replied && tokio::time::Instant::now() < deadline {
        let Some(chunk) = response.chunk().await.expect("chunk") else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(boundary) = buffer.find("\n\n") {
            let frame = buffer.drain(..boundary + 2).collect::<String>();
            let Ok(event) = serde_json::from_str::<Value>(frame.trim_start_matches("data: "))
            else {
                continue;
            };
            if event["type"] == "form.replied" && event["data"]["id"] == form_id {
                assert_eq!(event["data"]["sessionID"], session_id);
                assert_eq!(event["data"]["answer"]["q0"], "Storage");
                saw_replied = true;
                break;
            }
        }
    }
    assert!(saw_replied, "form.replied on the SSE stream");

    let output = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("tool settles")
        .expect("join");
    assert_eq!(output.error, None);
}
