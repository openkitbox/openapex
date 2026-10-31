//! Shared fixtures for tests that need a real `AppState` over an in-memory db.

use std::sync::Arc;

use crate::api::AppState;
use ox_core::config::Config;
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_engine::runner::{Runner, ToolRegistry};
use ox_llm::cardinal::chat::{Chat, ChatFuture};
use ox_llm::cardinal::{CardinalError, ChatRequest};
use ox_schema::event::SessionEventData;
use ox_schema::support::LocationRef;

/// A transport that is never dialled: deletion must not need the network.
struct NoChat;

impl Chat for NoChat {
    fn stream_chat<'a>(&'a self, _request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(async { Err(CardinalError::EarlyClose) })
    }
}

pub(crate) async fn test_state() -> AppState {
    test_state_with(Config::default()).await
}

pub(crate) async fn test_state_with(config: Config) -> AppState {
    let config = Arc::new(config);
    let db = Db::open_in_memory().await.expect("open db");
    let bus = EventBus::new(db.clone());
    let catalog = ox_llm::model_catalog::ModelCatalog::new(&config.llm);
    let directory = std::env::temp_dir();
    let runner = Runner::new(
        config,
        directory.clone(),
        db.clone(),
        bus.clone(),
        Arc::new(NoChat),
        Arc::new(ToolRegistry::new()),
        catalog,
    );
    AppState {
        db,
        bus,
        directory,
        live: runner.live(),
        runner,
        config_path: std::path::PathBuf::new(),
        lifecycle: crate::lifecycle::Lifecycle::new(std::time::Instant::now()),
    }
}

/// Create `id` through the real event path, then give it one message, so
/// `session_v2`, `session_message` and `event` all hold rows for it.
pub(crate) async fn seed_session(app: &AppState, id: &str, parent: Option<&str>) {
    app.bus
        .publish(SessionEventData::SessionCreated {
            session_id: id.to_owned(),
            project_id: "default".to_owned(),
            location: LocationRef {
                directory: "/tmp".to_owned(),
                workspace_id: None,
            },
            subpath: None,
            parent_id: parent.map(str::to_owned),
            slug: format!("slug-{id}"),
            title: None,
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            version: "test".to_owned(),
        })
        .await
        .expect("create session");
    app.bus
        .publish(SessionEventData::SessionSynthetic {
            session_id: id.to_owned(),
            text: format!("message of {id}"),
            description: None,
            metadata: None,
        })
        .await
        .expect("synthetic message");

    // The six id-less-FK magic-context tables the projector must clear.
    let conn = app.db.acquire().await.expect("conn");
    let statements = [
        "INSERT INTO mc_frozen_state (session_id, state, version) VALUES (?1, '{}', 1)",
        "INSERT INTO mc_compartments (id, session_id, coverage_end, importance, tiers, created_at) \
         VALUES (?1 || '-c', ?1, 'm', 0.5, '{}', 1)",
        "INSERT INTO mc_note (id, session_id, title, body, status, created_at, updated_at) \
         VALUES (?1 || '-n', ?1, 't', 'b', 'active', 1, 1)",
        "INSERT INTO mc_historian_run (id, session_id, state, attempt, created_at) \
         VALUES (?1 || '-h', ?1, 'done', 1, 1)",
        "INSERT INTO mc_tag (session_id, tag, message_id, part_path, digest) VALUES (?1, 1, 'm', 'p', 'd')",
        "INSERT INTO mc_drop (session_id, tag, reason, created_at) VALUES (?1, 1, 'r', 1)",
    ];
    for sql in statements {
        conn.execute(sql, [id]).await.expect(sql);
    }
}

/// Rows of `session_id` across every table a session leaves state in.
pub(crate) async fn rows_of(db: &Db, id: &str) -> Vec<(&'static str, i64)> {
    let tables = [
        ("session_v2", "id"),
        ("session_message", "session_id"),
        ("event", "aggregate_id"),
        ("mc_frozen_state", "session_id"),
        ("mc_compartments", "session_id"),
        ("mc_note", "session_id"),
        ("mc_historian_run", "session_id"),
        ("mc_tag", "session_id"),
        ("mc_drop", "session_id"),
    ];
    let conn = db.acquire().await.expect("conn");
    let mut counts = Vec::new();
    for (table, column) in tables {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1");
        let mut stmt = conn.prepare(&sql).await.expect("prepare");
        let mut rows = stmt.query([id]).await.expect("query");
        let row = rows.next().await.expect("next").expect("count row");
        counts.push((table, row.get::<i64>(0).expect("count")));
    }
    counts
}
