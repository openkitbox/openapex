//! Integration tests for the session lifecycle endpoints (Task 4).
//!
//! Each test boots the real router on an ephemeral port (same pattern as
//! `tests/skeleton.rs`) and drives it over HTTP. Tests that need to seed
//! history publish durable events directly on the same in-process event bus
//! the server uses, so the projections and the API stay consistent.

use std::net::TcpListener as StdTcpListener;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ox::config::Config;
use ox::db::Db;
use ox::event_bus::EventBus;
use ox::schema::event::SessionEventData;
use ox::schema::inbox::{Delivery, Item, UserPayload};
use ox::schema::support::{FinishReason, Finite, ModelRef, Prompt, TokenCache, TokenUsage};
use salvo::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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
    db: Db,
    bus: EventBus,
    handle: JoinHandle<()>,
    _dir: TempDir,
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
        let db_path = dir.path().join("ox.db");
        let config = Config {
            db_path,
            listen: format!("127.0.0.1:{port}"),
            ..Config::default()
        };
        let state = ox::build_state(config.clone()).await.expect("build state");
        let db = state.db.clone();
        let bus = state.bus.clone();
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
            db,
            bus,
            handle,
            _dir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .send()
            .await
            .expect("GET request")
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .json(&body)
            .send()
            .await
            .expect("POST request")
    }

    async fn patch(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .patch(self.url(path))
            .json(&body)
            .send()
            .await
            .expect("PATCH request")
    }

    async fn delete(&self, path: &str) -> reqwest::Response {
        self.client
            .delete(self.url(path))
            .send()
            .await
            .expect("DELETE request")
    }

    /// Create a session over HTTP and return its id.
    async fn create(&self, body: Value) -> String {
        let resp = self.post("/api/session", body).await;
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "session.create");
        let body: Value = resp.json().await.expect("create json");
        body["data"]["id"]
            .as_str()
            .expect("created session id")
            .to_owned()
    }

    /// `session.get` and unwrap the `data` envelope.
    async fn session_info(&self, session_id: &str) -> Value {
        let resp = self.get(&format!("/api/session/{session_id}")).await;
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "session.get");
        let body: Value = resp.json().await.expect("get json");
        body["data"].clone()
    }

    /// Publish a durable event on the server's bus.
    async fn publish(&self, event: SessionEventData) {
        self.bus.publish(event).await.expect("publish event");
    }
}

// ---------------------------------------------------------------------------
// Direct database helpers
// ---------------------------------------------------------------------------

async fn query_one<T, F>(db: &Db, sql: &str, params: Vec<ox_db::turso::Value>, f: F) -> Option<T>
where
    F: FnOnce(&ox_db::turso::Row) -> ox_db::turso::Result<T>,
{
    let conn = db.acquire().await.expect("query_one");
    let mut stmt = conn.prepare(sql).await.expect("query_one");
    let row = ox::db::optional(&mut stmt, params)
        .await
        .expect("query_one");
    row.map(|row| f(&row).expect("query_one row"))
}

async fn query_all<T, F>(db: &Db, sql: &str, params: Vec<ox_db::turso::Value>, f: F) -> Vec<T>
where
    F: Fn(&ox_db::turso::Row) -> ox_db::turso::Result<T>,
{
    let conn = db.acquire().await.expect("query_all");
    let mut stmt = conn.prepare(sql).await.expect("query_all");
    let mut rows = stmt.query(params).await.expect("query_all");
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.expect("query_all") {
        out.push(f(&row).expect("query_all row"));
    }
    out
}

async fn count_rows(db: &Db, sql: &str, params: Vec<ox_db::turso::Value>) -> i64 {
    query_one(db, sql, params, |row| row.get(0))
        .await
        .expect("count")
}

/// `SELECT <column> FROM session_v2 WHERE id = ?` as text.
async fn session_text(db: &Db, session_id: &str, column: &str) -> Option<String> {
    let sql = format!("SELECT {column} FROM session_v2 WHERE id = ?1");
    query_one(
        db,
        &sql,
        vec![ox_db::turso::Value::Text(session_id.to_owned())],
        |row| row.get::<Option<String>>(0),
    )
    .await
    .flatten()
}

// ---------------------------------------------------------------------------
// History seeding (bus-level, mirrors what the prompt/step endpoints will do)
// ---------------------------------------------------------------------------

/// Admit and deliver a user message through the inbox, exactly as
/// `session.prompt` will. Returns the message id.
async fn push_user_message(bus: &EventBus, session_id: &str, text: &str) -> String {
    let message_id = ox::id::id("msg");
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: session_id.to_owned(),
        inbox_id: message_id.clone(),
        item: Item::User {
            payload: UserPayload {
                prompt: Prompt {
                    text: text.to_owned(),
                    files: None,
                    agents: None,
                    skills: None,
                },
                metadata: None,
            },
            delivery: Delivery::Queue,
        },
    })
    .await
    .expect("enqueue user message");
    bus.publish(SessionEventData::InboxDelivered {
        session_id: session_id.to_owned(),
        inbox_id: message_id.clone(),
    })
    .await
    .expect("deliver user message");
    message_id
}

/// Start (and optionally settle) an assistant message. Returns its id.
async fn push_assistant_message(bus: &EventBus, session_id: &str, settle: bool) -> String {
    let message_id = ox::id::id("msg");
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    bus.publish(SessionEventData::StepStarted {
        session_id: session_id.to_owned(),
        assistant_message_id: message_id.clone(),
        agent: "build".to_owned(),
        model: ModelRef {
            id: "model".to_owned(),
            provider_id: "provider".to_owned(),
            variant: None,
        },
        snapshot: None,
        started,
    })
    .await
    .expect("step started");
    if settle {
        bus.publish(SessionEventData::StepEnded {
            session_id: session_id.to_owned(),
            assistant_message_id: message_id.clone(),
            finish: FinishReason::Stop,
            raw_finish: None,
            provider_state: None,
            cost: Finite::Int(0),
            tokens: TokenUsage {
                input: 3,
                output: 4,
                reasoning: 0,
                cache: TokenCache { read: 0, write: 0 },
            },
            snapshot: None,
            files: None,
        })
        .await
        .expect("step ended");
    }
    message_id
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_get_list_update_delete_round_trip() {
    let h = Harness::spawn().await;

    let session_id = h.create(json!({ "title": "hello" })).await;
    assert!(session_id.starts_with("ses_"), "id {session_id}");

    // session.create returns the full Session.Info (`additionalProperties:
    // false`: exactly these keys for a titled session).
    let info = h.session_info(&session_id).await;
    let mut keys: Vec<&str> = info
        .as_object()
        .expect("info object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "cost",
            "id",
            "location",
            "projectID",
            "time",
            "title",
            "tokens"
        ],
        // The root session's subpath is the empty string in the DB, and the
        // wire shape omits it (upstream `row.path ? … : undefined`).
        "Session.Info key set"
    );
    assert_eq!(info["id"], json!(session_id));
    assert_eq!(info["title"], json!("hello"));
    assert_eq!(info["projectID"], json!("default"));
    assert_eq!(info["cost"], json!(0));
    assert_eq!(
        info["tokens"],
        json!({
            "input": 0, "output": 0, "reasoning": 0,
            "cache": {"read": 0, "write": 0},
        })
    );
    assert!(info["time"]["created"].as_i64().is_some());
    assert!(info["time"]["updated"].as_i64().is_some());
    assert!(info["time"].get("idle").is_none());
    assert_eq!(info["location"].as_object().expect("location").len(), 1);

    // DB row exists with the generated id.
    let n = count_rows(
        &h.db,
        "SELECT count(*) FROM session_v2 WHERE id = ?1",
        vec![ox_db::turso::Value::Text(session_id.clone())],
    )
    .await;
    assert_eq!(n, 1);

    // session.list → SessionsResponse envelope with our session + cursors.
    let listed: Value = h.get("/api/session").await.json().await.expect("list json");
    let data = listed["data"].as_array().expect("data array");
    assert!(
        data.iter().any(|s| s["id"] == json!(session_id)),
        "list must contain the created session"
    );
    assert!(listed["cursor"].is_object());
    assert!(listed["cursor"]["previous"].is_string());
    assert!(listed["cursor"]["next"].is_string());

    // session.update → 204, and the title sticks.
    let resp = h
        .patch(
            &format!("/api/session/{session_id}"),
            json!({"title": "renamed"}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(h.session_info(&session_id).await["title"], json!("renamed"));

    // Metadata and permissions round-trip through their events.
    let resp = h
        .patch(
            &format!("/api/session/{session_id}"),
            json!({"metadata": {"origin": "test"}}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        h.session_info(&session_id).await["metadata"],
        json!({"origin": "test"})
    );

    let resp = h
        .patch(
            &format!("/api/session/{session_id}"),
            json!({"permissions": [{"action": "read", "resource": "/tmp", "effect": "allow"}]}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        h.session_info(&session_id).await["permissions"],
        json!([{"action": "read", "resource": "/tmp", "effect": "allow"}])
    );

    // session.remove → 204, then 404 and a purged event log.
    let events_before = count_rows(
        &h.db,
        "SELECT count(*) FROM event WHERE aggregate_id = ?1",
        vec![ox_db::turso::Value::Text(session_id.clone())],
    )
    .await;
    assert!(events_before > 0);

    let resp = h.delete(&format!("/api/session/{session_id}")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    let resp = h.get(&format!("/api/session/{session_id}")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let err: Value = resp.json().await.expect("error json");
    assert_eq!(err["_tag"], json!("SessionNotFoundError"));
    assert_eq!(err["sessionID"], json!(session_id));

    assert_eq!(
        count_rows(
            &h.db,
            "SELECT count(*) FROM session_v2 WHERE id = ?1",
            vec![ox_db::turso::Value::Text(session_id.clone())],
        )
        .await,
        0
    );
    assert_eq!(
        count_rows(
            &h.db,
            "SELECT count(*) FROM event WHERE aggregate_id = ?1",
            vec![ox_db::turso::Value::Text(session_id.clone())],
        )
        .await,
        0
    );
    assert_eq!(
        count_rows(
            &h.db,
            "SELECT count(*) FROM event_sequence WHERE aggregate_id = ?1",
            vec![ox_db::turso::Value::Text(session_id.clone())],
        )
        .await,
        0
    );

    // The session is gone from the list too.
    let listed: Value = h.get("/api/session").await.json().await.expect("list json");
    assert!(
        !listed["data"]
            .as_array()
            .expect("data")
            .iter()
            .any(|s| s["id"] == json!(session_id))
    );
}

#[tokio::test]
async fn list_orders_newest_first_and_filters_by_parent() {
    let h = Harness::spawn().await;

    let first = h.create(json!({"title": "first"})).await;
    let second = h.create(json!({"title": "second"})).await;

    let listed: Value = h
        .get("/api/session?order=asc")
        .await
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    let first_pos = ids
        .iter()
        .position(|id| *id == first)
        .expect("first listed");
    let second_pos = ids
        .iter()
        .position(|id| *id == second)
        .expect("second listed");
    assert!(first_pos < second_pos, "asc order: {ids:?}");

    let listed: Value = h
        .get("/api/session?order=desc")
        .await
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    let first_pos = ids
        .iter()
        .position(|id| *id == first)
        .expect("first listed");
    let second_pos = ids
        .iter()
        .position(|id| *id == second)
        .expect("second listed");
    assert!(second_pos < first_pos, "desc order: {ids:?}");

    // `parentID=null` selects the roots. Forks carry `parent_id = NULL`
    // upstream, so the forked child is a root too.
    let child = {
        push_user_message(&h.bus, &second, "fork me").await;
        let resp = h
            .post(&format!("/api/session/{second}/fork"), json!({}))
            .await;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: Value = resp.json().await.expect("fork json");
        body["data"]["id"].as_str().expect("child id").to_owned()
    };

    let roots: Value = h
        .get("/api/session?parentID=null")
        .await
        .json()
        .await
        .expect("json");
    let root_ids: Vec<&str> = roots["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(root_ids.contains(&second.as_str()));
    assert!(root_ids.contains(&child.as_str()));

    // Nothing is actually a `parent_id` child of `second` (forks are siblings).
    let children: Value = h
        .get(&format!("/api/session?parentID={second}"))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(children["data"].as_array().expect("data").len(), 0);

    // Search filters on the title.
    let searched: Value = h
        .get("/api/session?search=first")
        .await
        .json()
        .await
        .expect("json");
    let search_ids: Vec<&str> = searched["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(search_ids, vec![first.as_str()]);
}

// The TUI's session list (`dialog-session-list.tsx`) filters by
// `project` + `subpath`, where `subpath` is `path.relative(project.directory,
// directory)` — the empty string when the two are equal. Upstream stores a
// root session's path as `''` (never NULL), so that page lists the roots; a
// NULL root path (ox's old behavior) makes the list permanently empty.
//
// ox models one synthetic project per directory, so a nested directory is its
// own project root with subpath `''` too — the filter below still matches it
// only because both sessions share the synthetic `default` project id. The
// assertion pinned here is the wire/DB shape: root paths are `''`, never NULL.
#[tokio::test]
async fn list_by_project_and_empty_subpath_returns_root_sessions() {
    let h = Harness::spawn().await;

    let root = h.create(json!({"title": "root"})).await;
    let other = h
        .create(json!({
            "title": "other",
            "location": {"directory": format!("{}/sub/dir", h.base)}
        }))
        .await;

    // What the TUI actually sends: project + subpath= (empty string).
    let listed: Value = h
        .get("/api/session?parentID=null&order=desc&project=default&subpath=")
        .await
        .json()
        .await
        .expect("json");
    let ids: Vec<String> = listed["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect();
    assert!(
        ids.iter().any(|id| id == &root),
        "root session must match subpath=\"\": {ids:?}"
    );

    // The wire shape omits `subpath` for roots (upstream `row.path ? … :
    // undefined`); the DB row carries `''`, not NULL.
    let info = h.session_info(&root).await;
    assert!(info.get("subpath").is_none(), "info: {info}");
    for id in [&root, &other] {
        let n = count_rows(
            &h.db,
            "SELECT count(*) FROM session_v2 WHERE id = ?1 AND path = ''",
            vec![ox_db::turso::Value::Text(id.clone())],
        )
        .await;
        assert_eq!(n, 1, "session `{id}` path must be the empty string");
    }
}

// ox models one synthetic `default` project for every directory, so the TUI's
// `/sessions` filter (`project` + `subpath=`) alone matches sessions from all
// directories. The list must additionally be scoped to the directory the
// client is working in (`x-opencode-directory`).
#[tokio::test]
async fn list_by_project_is_scoped_to_the_request_directory() {
    let h = Harness::spawn().await;

    let here = h
        .create(json!({"title": "here", "location": {"directory": "/work/here"}}))
        .await;
    let there = h
        .create(json!({"title": "there", "location": {"directory": "/work/there"}}))
        .await;

    let listed: Value = h
        .client
        .get(h.url("/api/session?parentID=null&order=desc&project=default&subpath="))
        .header("x-opencode-directory", "/work/here")
        .send()
        .await
        .expect("GET request")
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(ids, vec![here.as_str()], "other dir leaked: {there}");
}

// Upstream's `session.form.list` filters its form cache by the raw `sessionID`
// string without checking the session exists (`form.ts:168-174`), so the TUI's
// bootstrap call with the literal id `"global"` gets `200 []`. It must not 404.
#[tokio::test]
async fn session_form_list_answers_empty_for_unknown_sessions() {
    let h = Harness::spawn().await;

    let resp = h.get("/api/session/global/form").await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["data"].as_array().expect("data").len(), 0);

    let resp = h.get("/api/session/ses_unknown/form").await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn child_session_model_inherits_once_and_explicit_model_wins() {
    let h = Harness::spawn().await;
    let parent = h
        .create(json!({
            "title": "parent",
            "model": {"id": "parent-model", "providerID": "provider", "variant": "high"}
        }))
        .await;

    let inherited = h
        .create(json!({"parentID": parent, "title": "inherited"}))
        .await;
    assert_eq!(
        h.session_info(&inherited).await["model"],
        json!({"id": "parent-model", "providerID": "provider", "variant": "high"})
    );

    let explicit = h
        .create(json!({
            "parentID": parent,
            "title": "explicit",
            "model": {"id": "child-model", "providerID": "provider", "variant": "low"}
        }))
        .await;
    assert_eq!(
        h.session_info(&explicit).await["model"],
        json!({"id": "child-model", "providerID": "provider", "variant": "low"})
    );

    // A later parent switch does not rewrite either already-created child.
    let response = h
        .post(
            &format!("/api/session/{parent}/model"),
            json!({"model": {"id": "parent-model", "providerID": "provider", "variant": "medium"}}),
        )
        .await;
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        h.session_info(&inherited).await["model"]["variant"],
        json!("high")
    );
    assert_eq!(
        h.session_info(&explicit).await["model"]["variant"],
        json!("low")
    );

    // Idempotent recreation of an existing child never overwrites its model.
    let response = h
        .post(
            "/api/session",
            json!({
                "id": inherited,
                "parentID": parent,
                "model": {"id": "replacement", "providerID": "provider", "variant": "low"}
            }),
        )
        .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        h.session_info(&inherited).await["model"],
        json!({"id": "parent-model", "providerID": "provider", "variant": "high"})
    );
}

#[tokio::test]
async fn switch_agent_and_model_mutate_session_and_append_messages() {
    let h = Harness::spawn().await;
    let session_id = h.create(json!({"title": "switch"})).await;

    // switchAgent → 204, session_v2.agent updated, agent-switched message.
    let resp = h
        .post(
            &format!("/api/session/{session_id}/agent"),
            json!({"agent": "plan"}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        session_text(&h.db, &session_id, "agent").await.as_deref(),
        Some("plan")
    );

    let switched: Vec<(String, String)> = query_all(
        &h.db,
        "SELECT type, data FROM session_message WHERE session_id = ?1 AND type = 'agent-switched'",
        vec![ox_db::turso::Value::Text(session_id.clone())],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .await;
    assert_eq!(switched.len(), 1);
    let data: Value = serde_json::from_str(&switched[0].1).expect("agent data");
    assert_eq!(data["agent"], json!("plan"));
    assert_eq!(data["previous"], Value::Null);

    // A second switch records the previous agent from the session row.
    let resp = h
        .post(
            &format!("/api/session/{session_id}/agent"),
            json!({"agent": "build"}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        session_text(&h.db, &session_id, "agent").await.as_deref(),
        Some("build")
    );
    let switched: Vec<String> = query_all(
        &h.db,
        "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'agent-switched' ORDER BY seq",
        vec![ox_db::turso::Value::Text(session_id.clone())],
        |row| row.get(0),
    )
    .await;
    assert_eq!(switched.len(), 2);
    let latest: Value = serde_json::from_str(&switched[1]).expect("agent data");
    assert_eq!(latest["previous"], json!("plan"));

    // switchModel → 204, session_v2.model updated, model-switched message.
    let resp = h
        .post(
            &format!("/api/session/{session_id}/model"),
            json!({"model": {"id": "gpt", "providerID": "openai"}}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let stored = session_text(&h.db, &session_id, "model")
        .await
        .expect("model");
    // The published event carries the normalized variant, so the projected
    // row stores it concretely (upstream `session.get` renders the same).
    assert_eq!(
        serde_json::from_str::<Value>(&stored).expect("model json"),
        json!({"id": "gpt", "providerID": "openai", "variant": "default"})
    );

    // Session.Info normalises the missing variant to "default".
    let info = h.session_info(&session_id).await;
    assert_eq!(
        info["model"],
        json!({"id": "gpt", "providerID": "openai", "variant": "default"})
    );

    // Switching to the same model is a no-op (no extra message).
    let resp = h
        .post(
            &format!("/api/session/{session_id}/model"),
            json!({"model": {"id": "gpt", "providerID": "openai"}}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let model_switched = count_rows(
        &h.db,
        "SELECT count(*) FROM session_message WHERE session_id = ?1 AND type = 'model-switched'",
        vec![ox_db::turso::Value::Text(session_id.clone())],
    )
    .await;
    assert_eq!(model_switched, 1);

    // Missing body fields are a 400.
    let resp = h
        .post(&format!("/api/session/{session_id}/agent"), json!({}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let resp = h
        .post(&format!("/api/session/{session_id}/model"), json!({}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // Unknown session → 404.
    let resp = h
        .post("/api/session/ses_missing/agent", json!({"agent": "plan"}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn view_advances_the_viewed_watermark_once() {
    let h = Harness::spawn().await;
    let session_id = h.create(json!({"title": "view"})).await;

    // Before any execution terminates there is no idle watermark: view is a
    // no-op, exactly like upstream.
    let resp = h
        .post(
            &format!("/api/session/{session_id}/view"),
            json!({"idle": 1}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(
        session_text(&h.db, &session_id, "time_viewed")
            .await
            .is_none()
    );

    // Terminating an execution stamps `time_idle`.
    h.publish(SessionEventData::ExecutionSucceeded {
        session_id: session_id.clone(),
    })
    .await;
    let idle: i64 = query_one(
        &h.db,
        "SELECT time_idle FROM session_v2 WHERE id = ?1",
        vec![ox_db::turso::Value::Text(session_id.clone())],
        |row| row.get(0),
    )
    .await
    .expect("time_idle");

    let resp = h
        .post(
            &format!("/api/session/{session_id}/view"),
            json!({"idle": idle}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let viewed: i64 = query_one(
        &h.db,
        "SELECT time_viewed FROM session_v2 WHERE id = ?1",
        vec![ox_db::turso::Value::Text(session_id.clone())],
        |row| row.get(0),
    )
    .await
    .expect("time_viewed");
    assert_eq!(viewed, idle);

    // Session.Info exposes the watermark.
    let info = h.session_info(&session_id).await;
    assert_eq!(info["time"]["idle"], json!(idle));
    assert_eq!(info["time"]["viewed"], json!(idle));

    // Re-viewing the same watermark (or a newer idle) publishes nothing.
    for candidate in [idle, idle + 1_000] {
        let resp = h
            .post(
                &format!("/api/session/{session_id}/view"),
                json!({"idle": candidate}),
            )
            .await;
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    }
    let viewed_after: i64 = query_one(
        &h.db,
        "SELECT time_viewed FROM session_v2 WHERE id = ?1",
        vec![ox_db::turso::Value::Text(session_id.clone())],
        |row| row.get(0),
    )
    .await
    .expect("time_viewed");
    assert_eq!(viewed_after, idle);

    // `idle` is required.
    let resp = h
        .post(&format!("/api/session/{session_id}/view"), json!({}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    let resp = h
        .post("/api/session/ses_missing/view", json!({"idle": 1}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // session.active reports the (always empty) in-process execution set.
    let active: Value = h
        .get("/api/session/active")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(active, json!({"data": {}}));
}

#[tokio::test]
async fn fork_copies_settled_history_and_reserves_sequence() {
    let h = Harness::spawn().await;
    let parent = h.create(json!({"title": "parent"})).await;

    let user_one = push_user_message(&h.bus, &parent, "first question").await;
    let assistant = push_assistant_message(&h.bus, &parent, true).await;
    let user_two = push_user_message(&h.bus, &parent, "second question").await;

    let parent_messages: Vec<(String, i64)> = query_all(
        &h.db,
        "SELECT id, seq FROM session_message WHERE session_id = ?1 ORDER BY seq",
        vec![ox_db::turso::Value::Text(parent.clone())],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .await;
    assert_eq!(
        parent_messages.len(),
        3,
        "seeded history: {parent_messages:?}"
    );
    assert_eq!(parent_messages[0].0, user_one);
    assert_eq!(parent_messages[1].0, assistant);
    assert_eq!(parent_messages[2].0, user_two);

    // Fork `before` the last user message: everything up to (not including) it.
    let resp = h
        .post(
            &format!("/api/session/{parent}/fork"),
            json!({"before": user_two}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "session.fork");
    let body: Value = resp.json().await.expect("fork json");
    let child = body["data"]["id"].as_str().expect("child id").to_owned();
    assert!(child.starts_with("ses_"));
    assert_ne!(child, parent);

    // Child Session.Info: fork link, boundary, forked title, zeroed usage.
    // Upstream projectFork sets `parent_id = null` (forks are siblings, linked
    // only through `fork_session_id`), so `parentID` is absent from the Info.
    let info = &body["data"];
    assert!(info.get("parentID").is_none(), "parentID: {info}");
    assert_eq!(info["fork"]["sessionID"], json!(parent));
    assert_eq!(
        info["fork"]["boundary"],
        json!({"type": "before", "messageID": user_two})
    );
    assert_eq!(info["title"], json!("parent (fork #1)"));
    assert_eq!(info["cost"], json!(0));
    assert_eq!(info["tokens"]["input"], json!(0));

    let parent_row: Vec<(String, String, Option<String>)> = query_all(
        &h.db,
        "SELECT project_id, directory, path FROM session_v2 WHERE id = ?1",
        vec![ox_db::turso::Value::Text(parent.clone())],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .await;
    let (project_id, directory, path) = parent_row.into_iter().next().expect("parent row");
    type ChildRow = (
        String,
        String,
        Option<String>,
        String,
        String,
        Option<String>,
    );
    let child_row: Vec<ChildRow> = query_all(
        &h.db,
        "SELECT project_id, directory, path, fork_session_id, fork_boundary, parent_id
         FROM session_v2 WHERE id = ?1",
        vec![ox_db::turso::Value::Text(child.clone())],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )
    .await;
    let (child_project, child_directory, child_path, fork_session_id, fork_boundary, parent_id) =
        child_row.into_iter().next().expect("child row");
    assert_eq!(child_project, project_id);
    assert_eq!(child_directory, directory);
    assert_eq!(child_path, path);
    assert_eq!(fork_session_id, parent);
    assert_eq!(
        serde_json::from_str::<Value>(&fork_boundary).expect("boundary json"),
        json!({"type": "before", "messageID": user_two})
    );
    assert!(parent_id.is_none(), "fork rows have no parent_id");

    // Copied rows: same seqs as the parent's first two, ids derived from the
    // forking event (`${SessionMessage.ID.fromEvent(event.id)}_${seq}` —
    // upstream only requires the `msg_` prefix, and ox event ids carry a
    // `_{counter}` suffix).
    let copied: Vec<(String, i64, String)> = query_all(
        &h.db,
        "SELECT id, seq, type FROM session_message WHERE session_id = ?1 ORDER BY seq",
        vec![ox_db::turso::Value::Text(child.clone())],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .await;
    assert_eq!(
        copied.iter().map(|(_, seq, _)| *seq).collect::<Vec<_>>(),
        vec![parent_messages[0].1, parent_messages[1].1]
    );
    assert_eq!(copied[0].2, "user");
    assert_eq!(copied[1].2, "assistant");
    let prefix = copied[0]
        .0
        .strip_suffix(&format!("_{}", copied[0].1))
        .expect("derived id suffix");
    assert!(prefix.starts_with("msg_"), "{prefix}");
    for (id, seq, _) in &copied {
        assert_eq!(id, &format!("{prefix}_{seq}"));
    }
    let distinct_prefixes: std::collections::HashSet<&str> = copied
        .iter()
        .filter_map(|(id, seq, _)| id.strip_suffix(&format!("_{seq}")))
        .collect();
    assert_eq!(
        distinct_prefixes.len(),
        1,
        "all copies derive from one event: {copied:?}"
    );

    // The child's durable sequence is reserved past the copied history.
    let reserved: i64 = query_one(
        &h.db,
        "SELECT seq FROM event_sequence WHERE aggregate_id = ?1",
        vec![ox_db::turso::Value::Text(child.clone())],
        |row| row.get(0),
    )
    .await
    .expect("reserved sequence");
    assert_eq!(
        reserved,
        copied
            .iter()
            .map(|(_, seq, _)| *seq)
            .max()
            .expect("max seq")
    );
    assert!(reserved >= 1);

    // Fork `through` the newest message copies the whole history.
    let resp = h
        .post(&format!("/api/session/{parent}/fork"), json!({}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("fork json");
    let whole = body["data"]["id"].as_str().expect("child id").to_owned();
    assert_eq!(
        body["data"]["fork"]["boundary"],
        json!({"type": "through", "messageID": user_two})
    );
    // Upstream forkTitle matches `^(.+) \(fork #(\d+)\)$` on the *parent's*
    // title — the parent is untitled-as-fork, so both forks read "(fork #1)".
    assert_eq!(body["data"]["title"], json!("parent (fork #1)"));
    let copied_count = count_rows(
        &h.db,
        "SELECT count(*) FROM session_message WHERE session_id = ?1",
        vec![ox_db::turso::Value::Text(whole.clone())],
    )
    .await;
    assert_eq!(copied_count, 3);

    // Forking a fork increments the title counter.
    let resp = h
        .post(&format!("/api/session/{child}/fork"), json!({}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("fork json");
    assert_eq!(body["data"]["title"], json!("parent (fork #2)"));

    // Unknown boundaries and empty sessions are client errors.
    let resp = h
        .post(
            &format!("/api/session/{parent}/fork"),
            json!({"before": "msg_nope"}),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let err: Value = resp.json().await.expect("error json");
    assert_eq!(err["_tag"], json!("MessageNotFoundError"));

    let empty = h.create(json!({"title": "empty"})).await;
    let resp = h
        .post(&format!("/api/session/{empty}/fork"), json!({}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let err: Value = resp.json().await.expect("error json");
    assert_eq!(err["_tag"], json!("InvalidRequestError"));
    assert_eq!(err["kind"], json!("empty_session"));

    let resp = h.post("/api/session/ses_missing/fork", json!({})).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // Forks are siblings of the parent (parent_id is NULL upstream), so they
    // survive `session.remove` cascades, which walk `parent_id` links only.
    let resp = h.delete(&format!("/api/session/{parent}")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        count_rows(
            &h.db,
            "SELECT count(*) FROM session_v2 WHERE id = ?1",
            vec![ox_db::turso::Value::Text(parent.clone())],
        )
        .await,
        0
    );
    for kept in [&child, &whole] {
        assert_eq!(
            count_rows(
                &h.db,
                "SELECT count(*) FROM session_v2 WHERE id = ?1",
                vec![ox_db::turso::Value::Text(kept.clone())],
            )
            .await,
            1,
            "fork {kept} must survive the parent's removal"
        );
    }
}

#[tokio::test]
async fn message_list_and_get_return_protocol_shapes_in_seq_order() {
    let h = Harness::spawn().await;
    let session_id = h.create(json!({"title": "messages"})).await;

    let user_one = push_user_message(&h.bus, &session_id, "hello").await;
    let assistant = push_assistant_message(&h.bus, &session_id, true).await;
    let user_two = push_user_message(&h.bus, &session_id, "again").await;

    // Default order is newest first.
    let page: Value = h
        .get(&format!("/api/session/{session_id}/message"))
        .await
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = page["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec![user_two.as_str(), assistant.as_str(), user_one.as_str()]
    );
    assert!(page["cursor"]["previous"].is_string());
    assert!(page["cursor"]["next"].is_string());

    // Ascending order matches the durable seq order.
    let page: Value = h
        .get(&format!("/api/session/{session_id}/message?order=asc"))
        .await
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = page["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec![user_one.as_str(), assistant.as_str(), user_two.as_str()]
    );

    // The user message carries the flattened prompt fields.
    let first = &page["data"][0];
    assert_eq!(first["type"], json!("user"));
    assert_eq!(first["id"], json!(user_one));
    assert_eq!(first["text"], json!("hello"));
    assert!(first["time"]["created"].as_i64().is_some());
    assert!(first.get("metadata").is_none());

    // The settled assistant message carries finish + usage.
    let second = &page["data"][1];
    assert_eq!(second["type"], json!("assistant"));
    assert_eq!(second["id"], json!(assistant));
    assert_eq!(second["finish"], json!("stop"));
    assert_eq!(second["tokens"]["input"], json!(3));
    assert_eq!(second["content"], json!([]));

    // Type filter.
    let page: Value = h
        .get(&format!("/api/session/{session_id}/message?type=user"))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(
        page["data"].as_array().expect("data").len(),
        2,
        "only the two user messages"
    );

    // Pagination: limit 2, then follow the `next` cursor.
    let page: Value = h
        .get(&format!("/api/session/{session_id}/message?limit=2"))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(page["data"].as_array().expect("data").len(), 2);
    let next = page["cursor"]["next"]
        .as_str()
        .expect("next cursor")
        .to_owned();
    let page: Value = h
        .get(&format!("/api/session/{session_id}/message?cursor={next}"))
        .await
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = page["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, vec![user_one.as_str()]);

    // session.message.get
    let resp = h
        .get(&format!("/api/session/{session_id}/message/{assistant}"))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["data"]["type"], json!("assistant"));
    assert_eq!(body["data"]["id"], json!(assistant));

    let resp = h
        .get(&format!("/api/session/{session_id}/message/msg_nope"))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let err: Value = resp.json().await.expect("json");
    assert_eq!(err["_tag"], json!("MessageNotFoundError"));

    let resp = h.get("/api/session/ses_missing/message").await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // An invalid cursor is a 400, not a 500.
    let resp = h
        .get(&format!(
            "/api/session/{session_id}/message?cursor=not-a-cursor"
        ))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // `create` is idempotent for a supplied id.
    let supplied = ox::id::id("ses");
    let resp = h
        .post("/api/session", json!({"id": supplied, "title": "supplied"}))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["data"]["id"], json!(supplied));
    assert_eq!(body["data"]["title"], json!("supplied"));
    assert_eq!(
        h.create(json!({"id": supplied, "title": "ignored"})).await,
        supplied
    );
    assert_eq!(h.session_info(&supplied).await["title"], json!("supplied"));
}

#[tokio::test]
async fn list_previous_page_comes_out_in_requested_order() {
    let h = Harness::spawn().await;

    // Four sessions created in order, so time_updated ascends with creation.
    let mut created: Vec<String> = Vec::new();
    for title in ["one", "two", "three", "four"] {
        created.push(h.create(json!({ "title": title })).await);
    }
    let [one, two, three, four]: [String; 4] = created.try_into().expect("four sessions");

    fn page_ids(body: &Value) -> Vec<&str> {
        body["data"]
            .as_array()
            .expect("data")
            .iter()
            .filter_map(|s| s["id"].as_str())
            .collect()
    }

    // Page 1 (newest first): [four, three].
    let page1: Value = h
        .get("/api/session?limit=2")
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(page_ids(&page1), vec![four.as_str(), three.as_str()]);
    let next = page1["cursor"]["next"].as_str().expect("next").to_owned();

    // Page 2: [two, one], still in the requested descending order.
    let page2: Value = h
        .get(&format!("/api/session?cursor={next}"))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(page_ids(&page2), vec![two.as_str(), one.as_str()]);
    let previous = page2["cursor"]["previous"]
        .as_str()
        .expect("previous")
        .to_owned();

    // The previous page must come out newest-first too (the store fetches it
    // ascending and reverses; upstream store.ts `rows.toReversed()`).
    let back: Value = h
        .get(&format!("/api/session?cursor={previous}"))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(
        page_ids(&back),
        vec![four.as_str(), three.as_str()],
        "previous page in requested (descending) order"
    );

    // A request-level `order` alongside a cursor is ignored, not rejected.
    let resp = h
        .get(&format!("/api/session?cursor={next}&order=asc"))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // An invalid cursor renders the upstream `InvalidCursorError` tag.
    let resp = h.get("/api/session?cursor=not-a-cursor").await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let err: Value = resp.json().await.expect("json");
    assert_eq!(err["_tag"], json!("InvalidCursorError"));
    assert!(err["message"].is_string());
}

#[tokio::test]
async fn message_cursors_embed_concrete_order() {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;

    let h = Harness::spawn().await;
    let session_id = h.create(json!({"title": "cursor order"})).await;
    let one = push_user_message(&h.bus, &session_id, "one").await;
    let two = push_user_message(&h.bus, &session_id, "two").await;

    let decoded_cursor = |raw: &str| -> Value {
        let bytes = B64.decode(raw).expect("base64url cursor");
        serde_json::from_slice(&bytes).expect("cursor json")
    };

    // Default page embeds the effective `order: "desc"` in both cursors.
    let page: Value = h
        .get(&format!("/api/session/{session_id}/message?limit=1"))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(page["data"].as_array().expect("data").len(), 1);
    for key in ["previous", "next"] {
        let cursor = decoded_cursor(page["cursor"][key].as_str().expect(key));
        assert_eq!(cursor["order"], json!("desc"), "{key} cursor: {cursor}");
        assert_eq!(cursor["direction"], json!(key));
    }

    // An explicit `order=asc` is embedded concretely and survives following
    // the cursor without repeating the query param.
    let page: Value = h
        .get(&format!(
            "/api/session/{session_id}/message?order=asc&limit=1"
        ))
        .await
        .json()
        .await
        .expect("json");
    let next = page["cursor"]["next"].as_str().expect("next").to_owned();
    assert_eq!(decoded_cursor(&next)["order"], json!("asc"));
    let page: Value = h
        .get(&format!("/api/session/{session_id}/message?cursor={next}"))
        .await
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = page["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, vec![two.as_str()], "ascending continuation");

    // cursor + order stays a 400 here, tagged `InvalidCursorError` upstream.
    let resp = h
        .get(&format!(
            "/api/session/{session_id}/message?cursor={next}&order=asc"
        ))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let err: Value = resp.json().await.expect("json");
    assert_eq!(err["_tag"], json!("InvalidCursorError"));

    // A legacy cursor without an embedded order still walks, defaulting desc:
    // next from the newest message yields the remaining older one.
    let legacy = B64.encode(
        json!({"id": two, "direction": "next"})
            .to_string()
            .into_bytes(),
    );
    let page: Value = h
        .get(&format!(
            "/api/session/{session_id}/message?cursor={legacy}"
        ))
        .await
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = page["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, vec![one.as_str()], "orderless cursor defaults to desc");
    let next = page["cursor"]["next"].as_str().expect("next").to_owned();
    assert_eq!(decoded_cursor(&next)["order"], json!("desc"));
}

/// The stored transcript as the tagger reads it: `data` decoded with the row's
/// id and type injected, in seq order.
async fn stored_values(db: &Db, session_id: &str) -> Vec<Value> {
    let rows = query_all(
        db,
        "SELECT id, type, data FROM session_message WHERE session_id = ?1 ORDER BY seq",
        vec![ox_db::turso::Value::Text(session_id.to_owned())],
        |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<String>(2)?,
            ))
        },
    )
    .await;
    rows.into_iter()
        .map(|(id, kind, data)| {
            let mut value: Value = serde_json::from_str(&data).expect("message json");
            value["id"] = json!(id);
            value["type"] = json!(kind);
            value
        })
        .collect()
}

/// A drop is a view, not a delete: once applied, the transcript the client
/// reads shows the same `[dropped §N§]` placeholder the model was given, and
/// the content is still in the row for `ctx_expand` to recover.
#[tokio::test]
async fn an_applied_drop_reads_back_as_the_placeholder_the_model_saw() {
    let h = Harness::spawn().await;
    let session_id = h.create(json!({ "title": "drops" })).await;
    let message_id = push_user_message(&h.bus, &session_id, "a detail nobody needs").await;

    // Mint tags exactly as a pass does, then queue one drop and apply it.
    let values = stored_values(&h.db, &session_id).await;
    let tags = ox::mctx::tagger::Tagger
        .assign(&h.db, &session_id, &values)
        .await
        .expect("assign tags");
    let tag = tags
        .iter()
        .find(|item| item.message_id == message_id)
        .expect("the user text is taggable")
        .tag;
    assert!(
        ox::mctx::tagger::queue_drop(&h.db, &session_id, tag, "stale")
            .await
            .expect("queue the drop")
    );
    ox::mctx::tagger::mark_drops_applied(&h.db, &session_id, &[tag])
        .await
        .expect("apply the drop");

    let placeholder = format!("[dropped §{tag}§]");
    let page: Value = h
        .get(&format!("/api/session/{session_id}/message"))
        .await
        .json()
        .await
        .expect("messages json");
    let listed = page["data"].to_string();
    assert!(
        listed.contains(&placeholder),
        "the read path shows the placeholder: {listed}"
    );
    assert!(
        !listed.contains("a detail nobody needs"),
        "and not the dropped text: {listed}"
    );

    // The single-message endpoint (what a TUI row fetch hits) agrees.
    let one: Value = h
        .get(&format!("/api/session/{session_id}/message/{message_id}"))
        .await
        .json()
        .await
        .expect("message json");
    assert!(
        one["data"].to_string().contains(&placeholder),
        "one message reads the same: {}",
        one["data"]
    );

    // Nothing was deleted: the row still holds what `ctx_expand` returns.
    let stored = stored_values(&h.db, &session_id).await;
    assert!(
        stored
            .iter()
            .any(|value| value.to_string().contains("a detail nobody needs")),
        "the content is still stored"
    );
}

/// Every pooled connection must come back with the small page cache: turso's
/// 2000-page default is 8 MiB resident per connection, and ox keeps up to
/// eight (`MAX_CONNECTIONS`, two idle). A regression here is invisible in
/// behaviour and only shows up as megabytes of footprint, so it is pinned.
#[tokio::test]
async fn pooled_connections_open_with_the_capped_page_cache() {
    let dir = TempDir::new().expect("tempdir");
    let db = Db::open(&dir.path().join("ox.db")).await.expect("open db");

    // Three acquisitions with the guard dropped in between: the first creates a
    // connection, the rest may reuse it or open another — all must be capped.
    for _ in 0..3 {
        let conn = db.acquire().await.expect("acquire");
        let mut stmt = conn.prepare("PRAGMA cache_size").await.expect("pragma");
        let mut rows = stmt.query(()).await.expect("pragma query");
        let row = rows.next().await.expect("pragma row").expect("a row");
        let pages: i64 = row.get(0).expect("cache_size");
        assert_eq!(
            pages,
            ox::db::PAGE_CACHE_PAGES,
            "a pooled connection opened with turso's default page cache"
        );
    }
}

// ---------------------------------------------------------------------------
// session.command / rpc.call (upstream endpoints with nothing registered)
// ---------------------------------------------------------------------------

/// ox registers no slash commands, so a known session answers upstream's
/// `CommandNotFoundError` (404) with the command name echoed.
#[tokio::test]
async fn command_endpoint_reports_unregistered_commands() {
    let h = Harness::spawn().await;
    let id = h.create(json!({})).await;

    let resp = h
        .post(
            &format!("/api/session/{id}/command"),
            json!({ "name": "goal", "text": "ship it" }),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["_tag"], "CommandNotFoundError");
    assert_eq!(body["command"], "goal");
    assert_eq!(body["message"], "Command not found: goal");
}

/// The session lookup runs before the command lookup (upstream handler order).
#[tokio::test]
async fn command_endpoint_checks_the_session_first() {
    let h = Harness::spawn().await;
    let resp = h
        .post(
            "/api/session/ses_missing/command",
            json!({ "name": "goal", "text": "" }),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["_tag"], "SessionNotFoundError");
}

/// `name` and `text` are required by the upstream payload schema.
#[tokio::test]
async fn command_endpoint_rejects_a_malformed_body() {
    let h = Harness::spawn().await;
    let id = h.create(json!({})).await;
    let resp = h
        .post(
            &format!("/api/session/{id}/command"),
            json!({ "text": "x" }),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["_tag"], "InvalidRequestError");
}

/// No RPC is registered, so every call answers upstream's unregistered-RPC
/// error: HTTP 400 `RpcError` with `type: "rpc.unavailable"`. The 2.2.1 TUI
/// issues `opencode.goal/get` on every session open and ignores this reply.
#[tokio::test]
async fn rpc_endpoint_reports_unavailable_rpcs() {
    let h = Harness::spawn().await;
    let resp = h
        .post(
            "/api/rpc/opencode.goal/get",
            json!({ "input": { "sessionID": "ses_x" } }),
        )
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["_tag"], "RpcError");
    assert_eq!(body["type"], "rpc.unavailable");
    assert_eq!(body["message"], "RPC is unavailable: opencode.goal");
}

/// `input` is optional in the upstream payload, so an empty body still reaches
/// the same answer instead of a parse error.
#[tokio::test]
async fn rpc_endpoint_accepts_an_empty_body() {
    let h = Harness::spawn().await;
    let resp = h
        .client
        .post(h.url("/api/rpc/x/y"))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["type"], "rpc.unavailable");
}
