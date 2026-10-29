//! Integration tests for `session.prompt` file inputs (`files:[{uri,…}]`).
//!
//! The request body carries upstream's `PromptInput.FileAttachment`
//! (`{uri, name?, description?, mention?}`); the server materializes each
//! `uri` into the stored `{data, mime, source, …}` shape at first admission.
//! Each test boots the real router on an ephemeral port (same pattern as
//! `tests/session.rs`). Prompts send `resume:false` so no model turn runs.

use std::net::TcpListener as StdTcpListener;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ox::config::Config;
use salvo::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

struct Harness {
    base: String,
    client: reqwest::Client,
    handle: JoinHandle<()>,
    dir: TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Harness {
    async fn spawn() -> Self {
        let port = free_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            db_path: dir.path().join("ox.db"),
            listen: format!("127.0.0.1:{port}"),
            ..Config::default()
        };
        let state = ox::build_state(config.clone()).await.expect("build state");
        let router = ox::build_router(state);
        let listen: String = config.listen.clone();
        let handle = tokio::spawn(async move {
            let acceptor = TcpListener::new(listen).bind().await;
            Server::new(acceptor).serve(router).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let mut ready = false;
        for _ in 0..50 {
            if client.get(format!("{base}/api/info")).send().await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ready, "server at {base} never became ready");
        Self {
            base,
            client,
            handle,
            dir,
        }
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .expect("POST request")
    }

    async fn session(&self) -> String {
        let resp = self
            .post(
                "/api/session",
                json!({"title": "attachments", "model": {"id": "m", "providerID": "x"}}),
            )
            .await;
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "session.create");
        let body: Value = resp.json().await.expect("create json");
        body["data"]["id"].as_str().expect("session id").to_owned()
    }

    /// Prompt with `files`, never dispatching a turn.
    async fn prompt(&self, session: &str, id: &str, files: Value) -> (u16, Value) {
        let resp = self
            .post(
                &format!("/api/session/{session}/prompt"),
                json!({"id": id, "text": "look", "files": files, "resume": false}),
            )
            .await;
        let status = resp.status().as_u16();
        (status, resp.json().await.expect("prompt json"))
    }
}

/// PNG magic followed by `extra` zero bytes.
fn png_bytes(extra: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(bytes.len() + extra, 0);
    bytes
}

fn stored_file(body: &Value) -> &Value {
    &body["data"]["payload"]["files"][0]
}

fn assert_files_error(status: u16, body: &Value) {
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["_tag"], "InvalidRequestError", "{body}");
    assert_eq!(body["field"], "files", "{body}");
}

#[tokio::test]
async fn pasted_png_data_url_is_stored_inline_without_reencoding() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let payload = B64.encode(png_bytes(40));
    let (status, body) = h
        .prompt(
            &session,
            "msg_png",
            json!([{"uri": format!("data:image/png;base64,{payload}"), "name": "a.png"}]),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let file = stored_file(&body);
    assert_eq!(file["mime"], "image/png");
    assert_eq!(file["source"], json!({"type": "inline"}));
    assert_eq!(file["name"], "a.png");
    assert_eq!(file["data"], payload.as_str());
}

#[tokio::test]
async fn data_url_mime_is_sniffed_over_the_declared_type() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let payload = B64.encode(png_bytes(8));
    let (status, body) = h
        .prompt(
            &session,
            "msg_sniff",
            json!([{"uri": format!("data:application/octet-stream;base64,{payload}")}]),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored_file(&body)["mime"], "image/png");
}

#[tokio::test]
async fn file_uri_png_is_read_and_sniffed() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let bytes = png_bytes(16);
    let path = h.dir.path().join("pic.png");
    std::fs::write(&path, &bytes).expect("write png");
    let uri = format!("file://{}", path.display());
    let (status, body) = h
        .prompt(&session, "msg_file", json!([{ "uri": uri }]))
        .await;
    assert_eq!(status, 200, "{body}");
    let file = stored_file(&body);
    assert_eq!(file["mime"], "image/png");
    assert_eq!(file["source"], json!({"type": "uri", "uri": uri}));
    assert_eq!(file["name"], "pic.png");
    assert_eq!(file["data"], B64.encode(&bytes).as_str());
}

#[tokio::test]
async fn file_uri_text_honours_the_line_range() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let path = h.dir.path().join("notes.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").expect("write text");
    let uri = format!("file://{}?start=2&end=3", path.display());
    let (status, body) = h
        .prompt(&session, "msg_text", json!([{ "uri": uri }]))
        .await;
    assert_eq!(status, 200, "{body}");
    let file = stored_file(&body);
    assert_eq!(file["mime"], "text/plain");
    let decoded = B64
        .decode(file["data"].as_str().expect("data"))
        .expect("base64");
    assert_eq!(String::from_utf8(decoded).expect("utf8"), "two\nthree");
}

#[tokio::test]
async fn file_uri_directory_is_listed_dirs_first() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let root = h.dir.path().join("listing");
    std::fs::create_dir_all(root.join("zdir")).expect("mkdir");
    std::fs::create_dir_all(root.join("adir")).expect("mkdir");
    std::fs::write(root.join("b.txt"), "b").expect("write");
    std::fs::write(root.join("a.txt"), "a").expect("write");
    let uri = format!("file://{}", root.display());
    let (status, body) = h.prompt(&session, "msg_dir", json!([{ "uri": uri }])).await;
    assert_eq!(status, 200, "{body}");
    let file = stored_file(&body);
    assert_eq!(file["mime"], "application/x-directory");
    assert_eq!(file["name"], "listing");
    let decoded = B64
        .decode(file["data"].as_str().expect("data"))
        .expect("base64");
    assert_eq!(
        String::from_utf8(decoded).expect("utf8"),
        "adir/\nzdir/\na.txt\nb.txt"
    );
}

#[tokio::test]
async fn invalid_inputs_are_rejected_with_files_field() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let missing = format!("file://{}", h.dir.path().join("nope.png").display());
    let cases: Vec<(&str, String)> = vec![
        ("non_base64", "data:text/plain,hello".to_owned()),
        ("bad_base64", "data:image/png;base64,@@@@".to_owned()),
        ("bad_padding", "data:image/png;base64,QUJD=".to_owned()),
        ("missing", missing),
        ("https", "https://example.com/x.png".to_owned()),
    ];
    for (id, uri) in cases {
        let (status, body) = h
            .prompt(&session, &format!("msg_{id}"), json!([{ "uri": uri }]))
            .await;
        assert_files_error(status, &body);
    }
}

#[tokio::test]
async fn oversized_data_url_is_rejected_by_length_alone() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    // 28_000_000 base64 chars decode to 21_000_000 bytes > 20 MiB.
    let uri = format!("data:image/png;base64,{}", "A".repeat(28_000_000));
    let (status, body) = h
        .prompt(
            &session,
            "msg_big",
            json!([{"uri": uri, "name": "big.png"}]),
        )
        .await;
    assert_files_error(status, &body);
    let message = body["message"].as_str().expect("message");
    assert!(message.contains("20971520"), "{message}");
    assert!(message.contains("big.png"), "{message}");
}

/// Data URLs alone cannot reach the total (the 32 MiB JSON body cap binds
/// first), so the budget is exercised with `file:` inputs, whose request
/// bytes are tiny.
#[tokio::test]
async fn inputs_over_the_total_budget_are_rejected_though_each_fits() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    // Two 17 MiB files: each under the 20 MiB per-file limit, 34 MiB together
    // over the 32 MiB total.
    let bytes = png_bytes(17 * 1024 * 1024);
    let one = h.dir.path().join("one.png");
    let two = h.dir.path().join("two.png");
    std::fs::write(&one, &bytes).expect("write one");
    std::fs::write(&two, &bytes).expect("write two");
    let (status, body) = h
        .prompt(
            &session,
            "msg_total",
            json!([
                {"uri": format!("file://{}", one.display())},
                {"uri": format!("file://{}", two.display()), "name": "second.png"},
            ]),
        )
        .await;
    assert_files_error(status, &body);
    let message = body["message"].as_str().expect("message");
    assert!(message.contains("33554432 byte total limit"), "{message}");
    assert!(message.contains("second.png"), "{message}");
}

#[tokio::test]
async fn body_above_the_old_8_mib_limit_is_accepted() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let payload = B64.encode(png_bytes(7 * 1024 * 1024));
    assert!(payload.len() > 8 * 1024 * 1024);
    let (status, body) = h
        .prompt(
            &session,
            "msg_9mib",
            json!([{"uri": format!("data:image/png;base64,{payload}")}]),
        )
        .await;
    assert_eq!(
        status,
        200,
        "{}",
        &body.to_string()[..body.to_string().len().min(300)]
    );
    assert_eq!(stored_file(&body)["mime"], "image/png");
}

#[tokio::test]
async fn prompt_without_files_is_unchanged() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let resp = h
        .post(
            &format!("/api/session/{session}/prompt"),
            json!({"id": "msg_plain", "text": "hi", "resume": false}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["data"]["payload"]["text"], "hi");
    assert!(body["data"]["payload"].get("files").is_none());
}

#[tokio::test]
async fn replay_returns_the_stored_payload_without_rereading_files() {
    let h = Harness::spawn().await;
    let session = h.session().await;
    let path = h.dir.path().join("once.png");
    std::fs::write(&path, png_bytes(4)).expect("write");
    let files = json!([{ "uri": format!("file://{}", path.display()) }]);
    let (status, first) = h.prompt(&session, "msg_replay", files.clone()).await;
    assert_eq!(status, 200, "{first}");

    std::fs::remove_file(&path).expect("remove");
    let (status, second) = h.prompt(&session, "msg_replay", files).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(stored_file(&second), stored_file(&first));
}
