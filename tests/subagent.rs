//! `subagent` end to end: a parent session delegates through the tool, a
//! child session runs its own turn against the same chat transport, and the
//! parent's tool result folds the child's reply.

use std::sync::{Arc, Mutex};

use ox::config::Config;
use ox::runner::ToolRegistry;
use serde_json::json;

/// `register_subagent` binds a process-wide `OnceLock` slot (`runner::
/// SUBAGENT_HANDLE`): the first registrant wins and every later tool silently
/// reuses that handle's db/runner. Tests in this binary each build their own
/// db + runner, so they must not overlap — same remedy as `config`'s
/// `ENV_LOCK`.
static SUBAGENT_SLOT: Mutex<()> = Mutex::new(());

/// A scripted chat transport handing out event lists in order.
struct ScriptedChat {
    scripts: Mutex<std::collections::VecDeque<Vec<ox::cardinal::stream::StreamEvent>>>,
}

impl ox::runner::Chat for ScriptedChat {
    fn stream_chat<'a>(
        &'a self,
        _request: &'a ox::cardinal::wire::ChatRequest,
    ) -> ox::cardinal::chat::ChatFuture<'a> {
        let script = self
            .scripts
            .lock()
            .expect("scripts")
            .pop_front()
            .unwrap_or_else(|| {
                vec![
                    ox::cardinal::stream::StreamEvent::TextDelta("(no script)".to_owned()),
                    ox::cardinal::stream::StreamEvent::Finish {
                        reason: "stop".to_owned(),
                    },
                ]
            });
        let events: std::collections::VecDeque<
            Result<ox::cardinal::stream::StreamEvent, ox::cardinal::CardinalError>,
        > = script.into_iter().map(Ok).collect();
        Box::pin(
            async move { Ok(Box::pin(stream::Iter(events)) as ox::cardinal::chat::BoxChatStream) },
        )
    }
}

/// A minimal owned stream over a VecDeque of events (futures-core is not a
/// dependency of this test, so it is written by hand).
mod stream {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Iter<T>(pub VecDeque<T>);

    impl<T: Unpin> futures_core::Stream for Iter<T> {
        type Item = T;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<T>> {
            Poll::Ready(self.get_mut().0.pop_front())
        }
    }
}

/// One model turn for the child: text then stop.
fn child_reply(text: &'static str) -> Vec<ox::cardinal::stream::StreamEvent> {
    vec![
        ox::cardinal::stream::StreamEvent::TextDelta(text.to_owned()),
        ox::cardinal::stream::StreamEvent::Finish {
            reason: "stop".to_owned(),
        },
    ]
}

#[tokio::test]
async fn a_parent_delegation_runs_a_child_and_folds_its_reply() {
    let _slot = SUBAGENT_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        db_path: dir.path().join("ox.db"),
        listen: "127.0.0.1:0".to_owned(),
        ..Config::default()
    };
    let db = ox::db::Db::open(&config.db_path).await.unwrap();
    let bus = ox::event_bus::EventBus::new(db.clone());
    let mut tools = ToolRegistry::new();
    tools.register_subagent(db.clone(), bus.clone(), 1);
    let chat = Arc::new(ScriptedChat {
        scripts: Mutex::new(std::collections::VecDeque::from(vec![child_reply(
            "Child report: done.",
        )])),
    });
    // The runner must exist so the tool can dispatch the child. Its registry
    // needs no tools — the child replies in one turn.
    let _state = ox::build_state_with(config.clone(), chat, ToolRegistry::new())
        .await
        .expect("build state");

    // The parent session row the tool reads.
    let parent_id = "ses_parent".to_owned();
    bus.publish(ox::schema::event::SessionEventData::SessionCreated {
        session_id: parent_id.clone(),
        project_id: "proj_test".to_owned(),
        location: ox::schema::support::LocationRef {
            directory: dir.path().to_string_lossy().to_string(),
            workspace_id: None,
        },
        subpath: None,
        parent_id: None,
        slug: ox::slug::create(),
        title: Some("parent".to_owned()),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .await
    .unwrap();

    let ctx = ox::runner::ToolContext {
        session_id: parent_id.clone(),
        directory: dir.path().to_path_buf(),
        call_id: "call_sub".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let tool = tools.get("subagent").expect("subagent registered");
    let output = tool
        .execute(
            ctx,
            json!({
                "description": "research",
                "prompt": "Find the answer."
            }),
        )
        .await;

    assert_eq!(output.error, None, "{output:?}");
    let text = output.content.as_str().expect("text");
    assert!(
        text.contains("Subagent report:") && text.contains("Child report: done."),
        "{text}"
    );
    let metadata = output.metadata.expect("metadata");
    let child_id = metadata["sessionId"].as_str().expect("child id");

    // The child session row exists with parent_id = parent.
    let conn = db.acquire().await.unwrap();
    let mut stmt = conn
        .prepare("SELECT parent_id FROM session_v2 WHERE id = ?1")
        .await
        .unwrap();
    let row = ox::db::optional(&mut stmt, (child_id,))
        .await
        .unwrap()
        .expect("child row");
    assert_eq!(row.get::<String>(0).unwrap(), parent_id);
}

#[tokio::test]
async fn a_depth_limit_of_zero_rejects_every_delegation() {
    let _slot = SUBAGENT_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        db_path: dir.path().join("ox.db"),
        listen: "127.0.0.1:0".to_owned(),
        ..Config::default()
    };
    let db = ox::db::Db::open(&config.db_path).await.unwrap();
    let bus = ox::event_bus::EventBus::new(db.clone());
    let mut tools = ToolRegistry::new();
    tools.register_subagent(db.clone(), bus.clone(), 0);
    let chat = Arc::new(ScriptedChat {
        scripts: Mutex::new(std::collections::VecDeque::new()),
    });
    let _state = ox::build_state_with(config.clone(), chat, ToolRegistry::new())
        .await
        .expect("build state");

    let parent_id = "ses_parent".to_owned();
    bus.publish(ox::schema::event::SessionEventData::SessionCreated {
        session_id: parent_id.clone(),
        project_id: "proj_test".to_owned(),
        location: ox::schema::support::LocationRef {
            directory: dir.path().to_string_lossy().to_string(),
            workspace_id: None,
        },
        subpath: None,
        parent_id: None,
        slug: ox::slug::create(),
        title: Some("parent".to_owned()),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .await
    .unwrap();

    let ctx = ox::runner::ToolContext {
        session_id: parent_id.clone(),
        directory: dir.path().to_path_buf(),
        call_id: "call_sub".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let tool = tools.get("subagent").expect("subagent registered");
    let output = tool
        .execute(
            ctx,
            json!({ "description": "research", "prompt": "Find the answer." }),
        )
        .await;

    let error = output.error.expect("rejected");
    assert!(
        error.contains("Subagent depth limit reached (0)"),
        "{error}"
    );
    // No child session was created.
    let conn = db.acquire().await.unwrap();
    let mut stmt = conn
        .prepare("SELECT COUNT(*) FROM session_v2 WHERE parent_id = ?1")
        .await
        .unwrap();
    let row = ox::db::optional(&mut stmt, (parent_id,))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), 0);
}
