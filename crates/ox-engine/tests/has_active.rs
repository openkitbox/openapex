//! `Runner::has_active` through the real runner: false on an empty runner,
//! true while any session's turn is in flight, false once it settles.
//!
//! The `Chat` seam is a gate that parks the first request until the test
//! releases it, so "turn in flight" is held open deterministically instead of
//! being raced against a sleep.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream;
use ox_core::config::Config;
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_engine::runner::{Chat, Runner, ToolRegistry};
use ox_llm::cardinal::chat::{BoxChatStream, ChatFuture};
use ox_llm::cardinal::{CardinalError, ChatRequest, StreamEvent};
use ox_schema::event::SessionEventData;
use ox_schema::inbox::{Delivery, Item, UserPayload};
use ox_schema::support::{LocationRef, Prompt};
use tokio::sync::Notify;

const SESSION: &str = "ses_has_active";

struct GatedChat {
    gate: Arc<Notify>,
}

impl Chat for GatedChat {
    fn stream_chat<'a>(&'a self, _request: &'a ChatRequest) -> ChatFuture<'a> {
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            gate.notified().await;
            let events = vec![
                StreamEvent::TextDelta("done".to_owned()),
                StreamEvent::Finish {
                    reason: "stop".to_owned(),
                },
                StreamEvent::Done,
            ];
            Ok(
                Box::pin(stream::iter(events.into_iter().map(Ok::<_, CardinalError>)))
                    as BoxChatStream,
            )
        })
    }
}

#[tokio::test]
async fn has_active_tracks_any_session_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
    let db = Db::open_in_memory().await.expect("open db");
    let bus = EventBus::new(db.clone());
    let config = Arc::new(Config::default());
    let gate = Arc::new(Notify::new());
    let catalog = ox_llm::model_catalog::ModelCatalog::new(&config.llm);
    let runner = Runner::new_with_mcp_and_home(
        config,
        dir.path().to_path_buf(),
        db,
        bus.clone(),
        Arc::new(GatedChat {
            gate: Arc::clone(&gate),
        }),
        Arc::new(ToolRegistry::new()),
        catalog,
        Arc::new(ox_mcp::Hub::empty()),
        home.path().to_path_buf(),
    );

    assert!(!runner.has_active(), "an empty runner has no active turn");

    bus.publish(SessionEventData::SessionCreated {
        session_id: SESSION.to_owned(),
        project_id: "default".to_owned(),
        location: LocationRef {
            directory: dir.path().to_string_lossy().into_owned(),
            workspace_id: None,
        },
        subpath: None,
        parent_id: None,
        slug: "has-active".to_owned(),
        // Titled, so no title generation competes for the gated chat.
        title: Some("has active".to_owned()),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        version: "test".to_owned(),
    })
    .await
    .expect("create session");
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: SESSION.to_owned(),
        inbox_id: "msg_prompt".to_owned(),
        item: Item::User {
            payload: UserPayload {
                prompt: Prompt {
                    text: "go".to_owned(),
                    files: None,
                    agents: None,
                    skills: None,
                },
                metadata: None,
            },
            delivery: Delivery::Steer,
        },
    })
    .await
    .expect("enqueue prompt");
    runner
        .dispatch(SESSION, "msg_prompt")
        .await
        .expect("dispatch");

    assert!(runner.has_active(), "dispatched turn is in flight");

    // `notify_one` stores a permit, so release is safe even if the request
    // has not reached the gate yet.
    gate.notify_one();
    let deadline = Instant::now() + Duration::from_secs(20);
    while runner.has_active() {
        assert!(Instant::now() < deadline, "turn did not finish in 20s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
