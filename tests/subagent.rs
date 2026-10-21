//! `subagent` end to end: a parent session delegates through the tool, a
//! child session runs its own turn against the same chat transport, and the
//! parent's tool result folds the child's reply. A second scenario checks
//! that a zero depth limit rejects every delegation.
//!
//! This binary deliberately holds ONE `#[tokio::test]` covering both
//! scenarios in sequence: `register_subagent` binds the process-global
//! `runner::SUBAGENT_HANDLE` (`OnceLock`, first setter wins, and an
//! immutable static cannot be reset), so a second test in this process
//! would silently drive the first test's db/runner no matter how the tests
//! were locked. The delegation scenario registers first and owns the slot;
//! the depth-limit scenario never touches the handle (its check rejects
//! before any db/runner access), so the stale binding is harmless there.

use std::sync::{Arc, Mutex};

use ox::config::Config;
use ox::runner::ToolRegistry;
use serde_json::json;

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
async fn subagent_delegation_folds_reply_and_depth_zero_rejects() {
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

    // --- depth limit: a zero limit rejects every delegation ---------------
    // This scenario runs second in the SAME process, so `register_subagent`
    // cannot install another global handle (the slot keeps the one bound
    // above) and this tool carries the first scenario's handle. That is
    // harmless here: the depth check rejects before any db/runner access,
    // which the assertions against this scenario's own fresh db confirm.
    let dir2 = tempfile::tempdir().unwrap();
    let config2 = Config {
        db_path: dir2.path().join("ox.db"),
        listen: "127.0.0.1:0".to_owned(),
        ..Config::default()
    };
    let db2 = ox::db::Db::open(&config2.db_path).await.unwrap();
    let bus2 = ox::event_bus::EventBus::new(db2.clone());
    let mut tools2 = ToolRegistry::new();
    tools2.register_subagent(db2.clone(), bus2.clone(), 0);
    let chat2 = Arc::new(ScriptedChat {
        scripts: Mutex::new(std::collections::VecDeque::new()),
    });
    let _state2 = ox::build_state_with(config2.clone(), chat2, ToolRegistry::new())
        .await
        .expect("build state");

    bus2.publish(ox::schema::event::SessionEventData::SessionCreated {
        session_id: parent_id.clone(),
        project_id: "proj_test".to_owned(),
        location: ox::schema::support::LocationRef {
            directory: dir2.path().to_string_lossy().to_string(),
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

    let ctx2 = ox::runner::ToolContext {
        session_id: parent_id.clone(),
        directory: dir2.path().to_path_buf(),
        call_id: "call_sub_depth".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let tool2 = tools2.get("subagent").expect("subagent registered");
    let output2 = tool2
        .execute(
            ctx2,
            json!({ "description": "research", "prompt": "Find the answer." }),
        )
        .await;

    let error = output2.error.expect("rejected");
    assert!(
        error.contains("Subagent depth limit reached (0)"),
        "{error}"
    );
    // No child session was created in this scenario's db.
    let conn = db2.acquire().await.unwrap();
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
