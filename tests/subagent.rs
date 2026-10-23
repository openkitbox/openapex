//! `subagent` end to end: a parent session delegates through the tool, a
//! child session runs its own turn against the same chat transport, and the
//! parent's tool result folds the child's reply. A second scenario checks
//! that a zero depth limit rejects every delegation. Two more scenarios
//! cover `sessionID` resume: continuing the same child (no new row, no
//! spawn prefix) and rejecting a target that is not this session's child.
//! The last two cover `background`: the tool returns while the child is
//! held mid-turn and the parent is notified on completion, and a completion
//! whose parent was deleted meanwhile is dropped without panicking.
//!
//! This binary deliberately holds ONE `#[tokio::test]` covering all
//! scenarios in sequence: they share one database and one state (the runner
//! that dispatches child turns), which is the shape a sequential walk needs.
//! The subagent handle is per-registry now, so each registry attaches to the
//! runner it is handed; scenario 2 (depth zero) needs no runner because the
//! depth guard rejects before any dispatch.

use std::sync::{Arc, Mutex};

use ox::config::Config;
use ox::runner::ToolRegistry;
use serde_json::json;

/// A chat whose every turn first waits for the gate — lets a background child
/// be held mid-turn while the tool call returns. Starts OPEN (behaves like
/// ScriptedChat); close it before launching a background child.
struct GatedChat {
    scripts: Mutex<std::collections::VecDeque<Vec<ox::cardinal::stream::StreamEvent>>>,
    gate: tokio::sync::watch::Receiver<bool>,
    /// One row per completion issued, so the test can assert what the child
    /// session's request carried (tools + the registry-driven static layer)
    /// without reaching into the engine.
    recorded: Mutex<Vec<Recorded>>,
}

/// The facts a request carries that distinguish a child turn from a parent
/// one: the spawn prefix in the user text, the advertised tool names, and
/// whether the static layer advertises the `question` guidance.
#[derive(Default)]
struct Recorded {
    subagent_prompt: bool,
    /// The two output-contract lines the spawn prefix must carry.
    spawn_contract: bool,
    tools: Vec<String>,
    question_guidance: bool,
}

impl Recorded {
    fn of(request: &ox::cardinal::wire::ChatRequest) -> Self {
        let mut recorded = Self::default();
        for message in &request.messages {
            match message {
                ox::cardinal::wire::ChatMessage::System { content } => {
                    if content.contains("Use the question tool for preferences") {
                        recorded.question_guidance = true;
                    }
                }
                ox::cardinal::wire::ChatMessage::User { content } => {
                    let text = content.as_str();
                    if text.contains("You are a subagent spawned by another session.") {
                        recorded.subagent_prompt = true;
                    }
                    if text.contains(
                        "Do not call the question tool; the user cannot see forms from a subagent.",
                    ) && text.contains(
                        "Your final message is the only thing the parent session receives; make it self-contained.",
                    ) {
                        recorded.spawn_contract = true;
                    }
                }
                _ => {}
            }
        }
        recorded.tools = request
            .tools
            .iter()
            .map(|tool| tool.function.name.clone())
            .collect();
        recorded
    }
}

impl ox::runner::Chat for GatedChat {
    fn stream_chat<'a>(
        &'a self,
        request: &'a ox::cardinal::wire::ChatRequest,
    ) -> ox::cardinal::chat::ChatFuture<'a> {
        self.recorded
            .lock()
            .expect("recorded")
            .push(Recorded::of(request));
        Box::pin(async move {
            let mut gate = self.gate.clone();
            if !*gate.borrow_and_update() {
                let _ = gate.changed().await;
            }
            let script = self
                .scripts
                .lock()
                .expect("scripts")
                .pop_front()
                .unwrap_or_else(|| {
                    vec![
                        ox::cardinal::stream::StreamEvent::TextDelta("(ack)".to_owned()),
                        ox::cardinal::stream::StreamEvent::Finish {
                            reason: "stop".to_owned(),
                        },
                    ]
                });
            let events: std::collections::VecDeque<
                Result<ox::cardinal::stream::StreamEvent, ox::cardinal::CardinalError>,
            > = script.into_iter().map(Ok).collect();
            Ok(Box::pin(stream::Iter(events)) as ox::cardinal::chat::BoxChatStream)
        })
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
    let mut tools = ToolRegistry::with_builtins();
    tools.register_subagent(db.clone(), bus.clone(), 1);
    // The gate starts OPEN: every scenario before the background ones runs
    // exactly as it did against ScriptedChat.
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(true);
    let chat = Arc::new(GatedChat {
        scripts: Mutex::new(std::collections::VecDeque::from(vec![child_reply(
            "Child report: done.",
        )])),
        gate: gate_rx,
        recorded: Mutex::new(Vec::new()),
    });
    // The runner must exist so the tool can dispatch the child, and it must
    // see the SAME registry: `Runner::new` attaches itself to the subagent
    // handle the registry holds (no process-global slot any more).
    let tool = tools.get("subagent").expect("subagent registered");
    let _state = ox::build_state_with(config.clone(), chat.clone(), tools)
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
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    };
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
        text.starts_with("<subagent sessionID=\"ses_")
            && text.contains("state=\"completed\"")
            && text.contains("Child report: done."),
        "{text}"
    );
    let metadata = output.metadata.expect("metadata");
    let child_id = metadata["sessionID"].as_str().expect("child id");
    assert_eq!(metadata["status"], "completed");

    // --- a child session must not inherit `question` (R6) -----------------
    // No form UI watches a subagent, and a foreground tool call has no
    // timeout, so a child `question` would hang the parent's delegation until
    // the user interrupts. The child's request must therefore carry neither
    // the tool nor its guidance line (the static layer is registry-driven).
    {
        let recorded = chat.recorded.lock().expect("recorded");
        let child = recorded
            .iter()
            .find(|row| row.subagent_prompt)
            .expect("the child turn was issued");
        assert!(
            !child.tools.iter().any(|name| name == "question"),
            "child tools still advertise question: {:?}",
            child.tools
        );
        assert!(
            !child.question_guidance,
            "child system layer still carries the question guidance"
        );
        assert!(
            child.spawn_contract,
            "the child prompt is missing the output-contract lines"
        );
        // Everything else survives: the exclusion is exactly one tool.
        assert!(
            child.tools.iter().any(|name| name == "shell"),
            "{:?}",
            child.tools
        );
        assert!(
            child.tools.iter().any(|name| name == "subagent"),
            "{:?}",
            child.tools
        );
    }

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

    // --- resume: `sessionID` continues the same child conversation --------
    // The resumed child needs a script for its second turn; queue it before
    // executing so the transport hands it to that turn.
    {
        let mut scripts = chat.scripts.lock().expect("scripts");
        scripts.push_back(child_reply("Resumed: continued."));
    }
    let ctx_resume = ox::runner::ToolContext {
        session_id: parent_id.clone(),
        directory: dir.path().to_path_buf(),
        call_id: "call_sub_resume".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    };
    let output_resume = tool
        .execute(
            ctx_resume,
            json!({
                "description": "follow-up",
                "prompt": "Second question.",
                "sessionID": child_id,
            }),
        )
        .await;

    assert_eq!(output_resume.error, None, "{output_resume:?}");
    let resume_text = output_resume.content.as_str().expect("resume text");
    assert!(
        resume_text.contains("Resumed: continued.") && resume_text.contains("state=\"completed\""),
        "{resume_text}"
    );
    let resume_metadata = output_resume.metadata.expect("resume metadata");
    assert_eq!(resume_metadata["sessionID"].as_str(), Some(child_id));

    // The resume did NOT create another child row under the parent.
    let conn = db.acquire().await.unwrap();
    let mut stmt = conn
        .prepare("SELECT COUNT(*) FROM session_v2 WHERE parent_id = ?1")
        .await
        .unwrap();
    let row = ox::db::optional(&mut stmt, (parent_id.clone(),))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), 1);

    // The resumed prompt reached the SAME child transcript.
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*) FROM session_message \
             WHERE session_id = ?1 AND data LIKE '%Second question.%'",
        )
        .await
        .unwrap();
    let row = ox::db::optional(&mut stmt, (child_id,))
        .await
        .unwrap()
        .unwrap();
    assert!(row.get::<i64>(0).unwrap() >= 1, "resumed prompt missing");
    drop(stmt);
    drop(conn);

    // --- resume rejects a session that is not this session's child --------
    // An unrelated root session in the same db: known, but parentless, so
    // it can never pass the child-of-current-session check.
    let other = "ses_other".to_owned();
    bus.publish(ox::schema::event::SessionEventData::SessionCreated {
        session_id: other.clone(),
        project_id: "proj_test".to_owned(),
        location: ox::schema::support::LocationRef {
            directory: dir.path().to_string_lossy().to_string(),
            workspace_id: None,
        },
        subpath: None,
        parent_id: None,
        slug: ox::slug::create(),
        title: Some("unrelated".to_owned()),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .await
    .unwrap();

    let ctx_other = ox::runner::ToolContext {
        session_id: parent_id.clone(),
        directory: dir.path().to_path_buf(),
        call_id: "call_sub_other".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    };
    let output_other = tool
        .execute(
            ctx_other,
            json!({
                "description": "x",
                "prompt": "y",
                "sessionID": other,
            }),
        )
        .await;
    let error = output_other.error.expect("rejected");
    assert!(
        error.contains("is not a child of the current session"),
        "{error}"
    );

    // --- depth limit: a zero limit rejects every delegation ---------------
    // The depth guard runs before any dispatch, so this registry needs no
    // runner attached: the zero limit rejects every delegation. It shares the
    // scenario's db so the parent chain the depth walk reads is real.
    let mut tools2 = ToolRegistry::new();
    tools2.register_subagent(db.clone(), bus.clone(), 0);

    // A dedicated parent row in the db the tool's handle points at, so the
    // depth walk really finds it and the zero-child count below is meaningful.
    let depth_parent = "ses_parent_depth".to_owned();
    bus.publish(ox::schema::event::SessionEventData::SessionCreated {
        session_id: depth_parent.clone(),
        project_id: "proj_test".to_owned(),
        location: ox::schema::support::LocationRef {
            directory: dir.path().to_string_lossy().to_string(),
            workspace_id: None,
        },
        subpath: None,
        parent_id: None,
        slug: ox::slug::create(),
        title: Some("depth parent".to_owned()),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .await
    .unwrap();

    let ctx2 = ox::runner::ToolContext {
        session_id: depth_parent.clone(),
        directory: dir.path().to_path_buf(),
        call_id: "call_sub_depth".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
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
    // No child session was created in the db the tool's handle drives.
    let conn = db.acquire().await.unwrap();
    let mut stmt = conn
        .prepare("SELECT COUNT(*) FROM session_v2 WHERE parent_id = ?1")
        .await
        .unwrap();
    let row = ox::db::optional(&mut stmt, (depth_parent,))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), 0);

    // --- background: the call returns at once, the parent is woken later --
    // The gated child holds mid-turn while the tool call must already have
    // returned; releasing the gate lets the watcher deliver.
    {
        let mut scripts = chat.scripts.lock().expect("scripts");
        scripts.push_back(child_reply("Background findings: 42."));
    }
    gate_tx.send(false).unwrap();
    let ctx_bg = ox::runner::ToolContext {
        session_id: parent_id.clone(),
        directory: dir.path().to_path_buf(),
        call_id: "call_sub_bg".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    };
    let output_bg = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.execute(
            ctx_bg,
            json!({
                "description": "background research",
                "prompt": "Find the answer.",
                "background": true,
            }),
        ),
    )
    .await
    .expect("background call must return before the 5s timeout");
    assert_eq!(output_bg.error, None, "{output_bg:?}");
    let bg_text = output_bg.content.as_str().expect("bg text");
    assert!(
        bg_text.contains("working in the background (sessionID: ses_"),
        "{bg_text}"
    );
    let bg_metadata = output_bg.metadata.expect("bg metadata");
    assert_eq!(bg_metadata["status"], "running");
    let bg_child = bg_metadata["sessionID"].as_str().expect("bg child id");

    gate_tx.send(true).unwrap();

    // The watcher delivers a synthetic into the parent; the wake turn
    // consumes the transport's "(ack)" fallback.
    let mut delivered = false;
    for _ in 0..200 {
        {
            let conn = db.acquire().await.unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT COUNT(*) FROM session_message \
                     WHERE session_id = ?1 AND data LIKE '%<subagent sessionID=%'",
                )
                .await
                .unwrap();
            let row = ox::db::optional(&mut stmt, (parent_id.clone(),))
                .await
                .unwrap()
                .unwrap();
            delivered = row.get::<i64>(0).unwrap() > 0;
            drop(stmt);
            drop(conn);
        }
        if delivered {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        delivered,
        "background completion never reached the parent within 10s"
    );

    // The delivered synthetic carries the child's report and its state
    // (quotes are JSON-escaped in the raw `data` column).
    let conn = db.acquire().await.unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT data FROM session_message \
             WHERE session_id = ?1 AND data LIKE '%<subagent sessionID=%'",
        )
        .await
        .unwrap();
    let row = ox::db::optional(&mut stmt, (parent_id.clone(),))
        .await
        .unwrap()
        .unwrap();
    let data = row.get::<String>(0).unwrap();
    assert!(
        data.contains("state=\\\"completed\\\"")
            && data.contains("Background findings: 42.")
            && data.contains(bg_child),
        "{data}"
    );
    drop(stmt);
    drop(conn);

    // --- background completion for a deleted parent is dropped quietly ---
    {
        let mut scripts = chat.scripts.lock().expect("scripts");
        scripts.push_back(child_reply("orphan work"));
    }
    gate_tx.send(false).unwrap();
    let orphan = "ses_parent_orphan".to_owned();
    bus.publish(ox::schema::event::SessionEventData::SessionCreated {
        session_id: orphan.clone(),
        project_id: "proj_test".to_owned(),
        location: ox::schema::support::LocationRef {
            directory: dir.path().to_string_lossy().to_string(),
            workspace_id: None,
        },
        subpath: None,
        parent_id: None,
        slug: ox::slug::create(),
        title: Some("orphan parent".to_owned()),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .await
    .unwrap();
    let ctx_orphan = ox::runner::ToolContext {
        session_id: orphan.clone(),
        directory: dir.path().to_path_buf(),
        call_id: "call_sub_orphan".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    };
    let output_orphan = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.execute(
            ctx_orphan,
            json!({
                "description": "orphan delegation",
                "prompt": "Work alone.",
                "background": true,
            }),
        ),
    )
    .await
    .expect("background call must return before the 5s timeout");
    assert_eq!(output_orphan.error, None, "{output_orphan:?}");

    // Delete the parent while its child is still gated mid-turn.
    {
        let conn = db.acquire().await.unwrap();
        conn.execute("DELETE FROM session_v2 WHERE id = ?1", (orphan,))
            .await
            .unwrap();
    }
    gate_tx.send(true).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // Reaching here IS the assertion: the watcher took the parent-gone
    // warn+drop path instead of panicking on the missing session.
}
