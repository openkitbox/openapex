//! Integration tests for the event bus + in-transaction projector.

use ox::db::Db;
use ox::event_bus::EventBus;
use ox::schema::event::SessionEventData;
use ox::schema::inbox::{Delivery, Item, UserPayload};
use ox::schema::support::{LocationRef, ModelRef, Prompt, Record, TokenCache, TokenUsage};

fn location() -> LocationRef {
    LocationRef {
        directory: "/tmp/ox".to_string(),
        workspace_id: None,
    }
}

fn model() -> ModelRef {
    ModelRef {
        id: "claude-test".to_string(),
        provider_id: "anthropic".to_string(),
        variant: None,
    }
}

fn tokens() -> TokenUsage {
    TokenUsage {
        input: 10,
        output: 5,
        reasoning: 2,
        cache: TokenCache { read: 1, write: 0 },
    }
}

fn user_item(text: &str) -> Item {
    Item::User {
        payload: UserPayload {
            prompt: Prompt {
                text: text.to_string(),
                files: None,
                agents: None,
                skills: None,
            },
            metadata: None,
        },
        delivery: Delivery::Queue,
    }
}

async fn bus() -> EventBus {
    EventBus::new(Db::open_in_memory().await.unwrap())
}

/// Run a query expected to return exactly one row.
async fn one_row(
    conn: &ox_db::turso::Connection,
    sql: &str,
    params: impl ox_db::turso::IntoParams,
) -> ox_db::turso::Row {
    let mut stmt = conn.prepare(sql).await.expect("prepare");
    let mut rows = stmt.query(params).await.expect("query");
    rows.next().await.expect("next").expect("one row")
}

async fn create(bus: &EventBus, sid: &str) {
    bus.publish(SessionEventData::SessionCreated {
        session_id: sid.to_string(),
        project_id: "proj".to_string(),
        location: location(),
        subpath: None,
        parent_id: None,
        slug: sid.to_string(),
        title: Some("Test".to_string()),
        agent: Some("build".to_string()),
        model: Some(model()),
        metadata: None,
        permissions: None,
        version: "1.0.0".to_string(),
    })
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Sequencing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn monotonic_seq_per_aggregate() {
    let bus = bus().await;
    create(&bus, "s1").await;
    create(&bus, "s2").await;

    let e1 = bus
        .publish(SessionEventData::SessionRenamed {
            session_id: "s1".to_string(),
            title: "a".to_string(),
        })
        .await
        .unwrap();
    let e2 = bus
        .publish(SessionEventData::SessionRenamed {
            session_id: "s1".to_string(),
            title: "b".to_string(),
        })
        .await
        .unwrap();
    let e3 = bus
        .publish(SessionEventData::SessionRenamed {
            session_id: "s2".to_string(),
            title: "c".to_string(),
        })
        .await
        .unwrap();

    // s1: created=0, rename=1, rename=2. s2: created=0, rename=1.
    assert_eq!(e1.durable.as_ref().unwrap().seq, 1);
    assert_eq!(e2.durable.as_ref().unwrap().seq, 2);
    assert_eq!(e3.durable.as_ref().unwrap().seq, 1);
}

#[tokio::test]
async fn concurrent_publishes_serialize() {
    let bus = std::sync::Arc::new(bus().await);
    create(&bus, "s1").await;

    let n = 16;
    let mut handles = Vec::new();
    for i in 0..n {
        let b = std::sync::Arc::clone(&bus);
        handles.push(tokio::spawn(async move {
            b.publish(SessionEventData::SessionRenamed {
                session_id: "s1".to_string(),
                title: format!("t{i}"),
            })
            .await
            .unwrap()
            .durable
            .unwrap()
            .seq
        }));
    }
    let mut seqs: Vec<i64> = Vec::new();
    for h in handles {
        seqs.push(h.await.unwrap());
    }
    seqs.sort_unstable();
    // created is seq 0, so renames are seqs 1..=n.
    let expected: Vec<i64> = (1..=n as i64).collect();
    assert_eq!(seqs, expected, "no duplicate or missing seqs");
}

/// The connection carries one transaction at a time, so concurrent writers
/// must queue for it — different aggregates have no per-aggregate lock, and
/// turso answers a second `BEGIN` with "cannot start a transaction within a
/// transaction" rather than waiting.
#[tokio::test]
async fn concurrent_publishes_on_different_aggregates_queue() {
    let bus = std::sync::Arc::new(bus().await);
    create(&bus, "s1").await;
    create(&bus, "s2").await;

    let mut handles = Vec::new();
    for i in 0..16 {
        let b = std::sync::Arc::clone(&bus);
        let session = if i % 2 == 0 { "s1" } else { "s2" };
        handles.push(tokio::spawn(async move {
            b.publish(SessionEventData::SessionRenamed {
                session_id: session.to_string(),
                title: format!("t{i}"),
            })
            .await
            .map(|env| env.durable.unwrap().seq)
        }));
    }
    let mut seqs = Vec::new();
    for handle in handles {
        seqs.push(handle.await.unwrap().expect("publish"));
    }
    seqs.sort_unstable();
    // Each session is its own aggregate, so the two interleave 1..=8.
    let expected: Vec<i64> = (1..=8).flat_map(|seq| [seq, seq]).collect();
    assert_eq!(
        seqs, expected,
        "every publish landed, none lost to a BEGIN clash"
    );
}

/// A projector conflict aborts its transaction mid-way; the next writer must
/// not find that transaction still open.
#[tokio::test]
async fn a_failed_publish_leaves_no_open_transaction() {
    let bus = bus().await;
    create(&bus, "s1").await;

    // No such inbox row: project_delivery_changed updates zero rows and fails.
    let failed = bus
        .publish(SessionEventData::InboxDeliveryChanged {
            session_id: "s1".to_string(),
            inbox_id: "missing".to_string(),
            delivery: Delivery::Queue,
        })
        .await;
    assert!(
        failed.is_err(),
        "a missing inbox row is a lifecycle conflict"
    );

    let env = bus
        .publish(SessionEventData::SessionRenamed {
            session_id: "s1".to_string(),
            title: "after".to_string(),
        })
        .await
        .expect("publish after a rolled-back one");
    assert_eq!(
        env.durable.unwrap().seq,
        1,
        "the conflict did not consume a seq"
    );
}

// ---------------------------------------------------------------------------
// Rollback on projector error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn projector_error_rolls_back() {
    let bus = bus().await;
    create(&bus, "s1").await;

    // Deliver a nonexistent inbox row → projector error → rollback.
    let res = bus
        .publish(SessionEventData::InboxDelivered {
            session_id: "s1".to_string(),
            inbox_id: "nonexistent".to_string(),
        })
        .await;
    assert!(res.is_err());

    // event_sequence must be untouched (still at created's seq 0).
    let conn = bus.db().acquire().await.unwrap();
    let seq = one_row(
        &conn,
        "SELECT seq FROM event_sequence WHERE aggregate_id = 's1'",
        (),
    )
    .await
    .get::<i64>(0)
    .unwrap();
    assert_eq!(seq, 0);

    // No event rows beyond the created one.
    let count = one_row(
        &conn,
        "SELECT count(*) FROM event WHERE aggregate_id = 's1'",
        (),
    )
    .await
    .get::<i64>(0)
    .unwrap();
    assert_eq!(count, 1);
}

// ---------------------------------------------------------------------------
// Happy-path projection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_projection() {
    let bus = bus().await;
    create(&bus, "s1").await;

    // inbox.enqueued
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
        item: user_item("hello"),
    })
    .await
    .unwrap();

    // inbox.delivered → user message row.
    let del = bus
        .publish(SessionEventData::InboxDelivered {
            session_id: "s1".to_string(),
            inbox_id: "ib1".to_string(),
        })
        .await
        .unwrap();
    let del_seq = del.durable.as_ref().unwrap().seq;

    let conn = bus.db().acquire().await.unwrap();
    let row = one_row(
        &conn,
        "SELECT type, seq FROM session_message WHERE id = 'ib1'",
        (),
    )
    .await;
    let mtype = row.get::<String>(0).unwrap();
    let mseq = row.get::<i64>(1).unwrap();
    assert_eq!(mtype, "user");
    assert_eq!(mseq, del_seq, "promoted message seq = deliver event seq");

    // step.started → assistant row
    bus.publish(SessionEventData::StepStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        agent: "build".to_string(),
        model: model(),
        snapshot: None,
        started: 1000,
    })
    .await
    .unwrap();

    // text.started / ended
    bus.publish(SessionEventData::TextStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        ordinal: 0,
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::TextEnded {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        ordinal: 0,
        text: "world".to_string(),
        state: None,
    })
    .await
    .unwrap();

    // step.ended → usage accumulated
    bus.publish(SessionEventData::StepEnded {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        finish: ox::schema::support::FinishReason::Stop,
        raw_finish: None,
        provider_state: None,
        cost: ox::schema::support::Finite::Float(0.5),
        tokens: tokens(),
        snapshot: None,
        files: None,
    })
    .await
    .unwrap();

    // execution.succeeded → time_idle + idle_outcome
    bus.publish(SessionEventData::ExecutionSucceeded {
        session_id: "s1".to_string(),
    })
    .await
    .unwrap();

    let row = one_row(
        &conn,
        "SELECT cost, tokens_input, tokens_output, idle_outcome, time_idle \
         FROM session_v2 WHERE id = 's1'",
        (),
    )
    .await;
    let cost = row.get::<f64>(0).unwrap();
    let tin = row.get::<i64>(1).unwrap();
    let tout = row.get::<i64>(2).unwrap();
    let idle_outcome = row.get::<String>(3).unwrap();
    let time_idle = row.get::<i64>(4).unwrap();
    assert!((cost - 0.5).abs() < 1e-9);
    assert_eq!(tin, 10);
    assert_eq!(tout, 5);
    assert_eq!(idle_outcome, "succeeded");
    assert!(time_idle > 0);

    // Idle marker message exists.
    let idle_count = one_row(
        &conn,
        "SELECT count(*) FROM session_message WHERE session_id = 's1' AND type = 'idle'",
        (),
    )
    .await
    .get::<i64>(0)
    .unwrap();
    assert_eq!(idle_count, 1);
}

// ---------------------------------------------------------------------------
// Inbox promote / cancel / delivery-change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbox_cancel_removes_row() {
    let bus = bus().await;
    create(&bus, "s1").await;
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
        item: user_item("hi"),
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::InboxCancelled {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
    })
    .await
    .unwrap();

    let conn = bus.db().acquire().await.unwrap();
    let n = one_row(
        &conn,
        "SELECT count(*) FROM session_inbox WHERE id = 'ib1'",
        (),
    )
    .await
    .get::<i64>(0)
    .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn inbox_delivery_change_updates() {
    let bus = bus().await;
    create(&bus, "s1").await;
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
        item: user_item("hi"),
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::InboxDeliveryChanged {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
        delivery: Delivery::Steer,
    })
    .await
    .unwrap();

    let conn = bus.db().acquire().await.unwrap();
    let d = one_row(
        &conn,
        "SELECT delivery FROM session_inbox WHERE id = 'ib1'",
        (),
    )
    .await
    .get::<String>(0)
    .unwrap();
    assert_eq!(d, "steer");
}

// ---------------------------------------------------------------------------
// event_log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_log_ordering_and_versions() {
    let bus = bus().await;
    create(&bus, "s1").await;
    bus.publish(SessionEventData::SessionRenamed {
        session_id: "s1".to_string(),
        title: "x".to_string(),
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::SessionDeleted {
        session_id: "s1".to_string(),
    })
    .await
    .unwrap();

    let log = bus.event_log("s1", -1).await.unwrap();
    assert_eq!(log.len(), 3);
    let seqs: Vec<i64> = log
        .iter()
        .map(|e| e.durable.as_ref().unwrap().seq)
        .collect();
    assert_eq!(seqs, vec![0, 1, 2], "ordered by seq");

    // created & renamed are v1; deleted is v2.
    assert_eq!(log[0].durable.as_ref().unwrap().version, 1);
    assert_eq!(log[1].durable.as_ref().unwrap().version, 1);
    assert_eq!(log[2].durable.as_ref().unwrap().version, 2);
    assert_eq!(log[2].kind, "session.deleted");

    // after filter
    let after = bus.event_log("s1", 0).await.unwrap();
    assert_eq!(after.len(), 2);
}

// ---------------------------------------------------------------------------
// Ephemeral broadcast
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ephemeral_events_broadcast_not_persisted() {
    let bus = bus().await;
    let mut rx = bus.subscribe();
    create(&bus, "s1").await;
    bus.publish(SessionEventData::StepStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        agent: "build".to_string(),
        model: model(),
        snapshot: None,
        started: 1,
    })
    .await
    .unwrap();

    // text.delta is ephemeral: no durable envelope, but broadcast.
    let env = bus
        .publish(SessionEventData::TextDelta {
            session_id: "s1".to_string(),
            assistant_message_id: "am1".to_string(),
            ordinal: 0,
            delta: "d".to_string(),
        })
        .await
        .unwrap();
    assert!(env.durable.is_none());

    // It should arrive on the subscriber stream.
    let mut got = None;
    while let Ok(ev) = rx.try_recv() {
        if ev.kind == "session.text.delta" {
            got = Some(ev);
        }
    }
    assert!(got.is_some(), "ephemeral event broadcast");

    // And not persisted.
    let conn = bus.db().acquire().await.unwrap();
    let n = one_row(
        &conn,
        "SELECT count(*) FROM event WHERE aggregate_id = 's1' AND type LIKE 'session.text.delta%'",
        (),
    )
    .await
    .get::<i64>(0)
    .unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------------------
// Spec-review regressions
// ---------------------------------------------------------------------------

/// `getCurrentAssistant` semantics: the latest assistant row is selected by
/// `seq DESC`, then discarded if it has `time.completed`. A completed
/// assistant must therefore never be reset/closed by a later `step.started`.
#[tokio::test]
async fn completed_assistant_is_not_current() {
    let bus = bus().await;
    create(&bus, "s1").await;

    bus.publish(SessionEventData::StepStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        agent: "build".to_string(),
        model: model(),
        snapshot: None,
        started: 100,
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::StepEnded {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        finish: ox::schema::support::FinishReason::Stop,
        raw_finish: None,
        provider_state: None,
        cost: ox::schema::support::Finite::Float(0.0),
        tokens: TokenUsage {
            input: 0,
            output: 0,
            reasoning: 0,
            cache: TokenCache { read: 0, write: 0 },
        },
        snapshot: None,
        files: None,
    })
    .await
    .unwrap();

    let conn = bus.db().acquire().await.unwrap();
    let before = one_row(
        &conn,
        "SELECT data FROM session_message WHERE id = 'am1'",
        (),
    )
    .await
    .get::<String>(0)
    .unwrap();

    // A second step starts while am1 is complete. The "close current
    // incomplete assistant" path must not touch it (getCurrentAssistant ->
    // None), so am1 is not mutated.
    bus.publish(SessionEventData::StepStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am2".to_string(),
        agent: "build".to_string(),
        model: model(),
        snapshot: None,
        started: 200,
    })
    .await
    .unwrap();

    let after = one_row(
        &conn,
        "SELECT data FROM session_message WHERE id = 'am1'",
        (),
    )
    .await
    .get::<String>(0)
    .unwrap();
    assert_eq!(before, after, "completed assistant must not be mutated");

    // The new assistant exists and the completed one still carries its finish.
    let count = one_row(
        &conn,
        "SELECT count(*) FROM session_message WHERE session_id = 's1' AND type = 'assistant'",
        (),
    )
    .await
    .get::<i64>(0)
    .unwrap();
    let finish = one_row(
        &conn,
        "SELECT json_extract(data, '$.finish') FROM session_message WHERE id = 'am1'",
        (),
    )
    .await
    .get::<Option<String>>(0)
    .unwrap();
    assert_eq!(count, 2);
    assert_eq!(finish.as_deref(), Some("stop"));
}

/// Projected message ids derive deterministically from the event id by
/// swapping the `evt_` prefix for `msg_` (upstream `ID.fromEvent`). Two
/// distinct events therefore yield two rows whose ids are exactly the
/// prefix-swapped event ids — no random ids inside projection.
#[tokio::test]
async fn projected_message_ids_derive_from_event_id() {
    let bus = bus().await;
    create(&bus, "s1").await;

    let first = bus
        .publish(SessionEventData::ExecutionSucceeded {
            session_id: "s1".to_string(),
        })
        .await
        .unwrap();
    let second = bus
        .publish(SessionEventData::ExecutionSucceeded {
            session_id: "s1".to_string(),
        })
        .await
        .unwrap();

    assert!(first.id.starts_with("evt_"));
    let conn = bus.db().acquire().await.unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id FROM session_message WHERE session_id = 's1' \
             AND type = 'idle' ORDER BY seq ASC",
        )
        .await
        .unwrap();
    let mut rows = stmt.query(()).await.unwrap();
    let mut ids: Vec<String> = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        ids.push(row.get::<String>(0).unwrap());
    }

    assert_eq!(
        ids,
        vec![
            ox::id::from_event(&first.id),
            ox::id::from_event(&second.id)
        ]
    );
    assert!(ids.iter().all(|id| id.starts_with("msg_")));
    // Determinism: the derivation is a pure prefix swap.
    assert_eq!(
        ox::id::from_event(&first.id),
        format!("msg_{}", &first.id[4..])
    );
}

/// `session.moved` updates the session row's location and appends a
/// `location-switched` message carrying the previous location.
#[tokio::test]
async fn session_moved_projects_location_and_message() {
    let bus = bus().await;
    create(&bus, "s1").await;

    let moved = bus
        .publish(SessionEventData::SessionMoved {
            session_id: "s1".to_string(),
            payload: ox::schema::inbox::MovePayload {
                location: LocationRef {
                    directory: "/tmp/moved".to_string(),
                    workspace_id: Some("ws1".to_string()),
                },
                project_id: "proj2".to_string(),
                subpath: Some("packages/app".to_string()),
            },
        })
        .await
        .unwrap();
    let moved_id = ox::id::from_event(&moved.id);

    let created = moved.created;
    let conn = bus.db().acquire().await.unwrap();
    let row = one_row(
        &conn,
        "SELECT directory, path, project_id, workspace_id, time_updated \
         FROM session_v2 WHERE id = 's1'",
        (),
    )
    .await;
    let directory = row.get::<String>(0).unwrap();
    let path = row.get::<Option<String>>(1).unwrap();
    let project_id = row.get::<String>(2).unwrap();
    let workspace_id = row.get::<Option<String>>(3).unwrap();
    let time_updated = row.get::<i64>(4).unwrap();
    assert_eq!(directory, "/tmp/moved");
    assert_eq!(path.as_deref(), Some("packages/app"));
    assert_eq!(project_id, "proj2");
    assert_eq!(workspace_id.as_deref(), Some("ws1"));
    assert_eq!(time_updated, created);

    let data = one_row(
        &conn,
        "SELECT data FROM session_message WHERE id = ?1 AND type = 'location-switched'",
        [moved_id.as_str()],
    )
    .await
    .get::<String>(0)
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(value["location"]["directory"], "/tmp/moved");
    assert_eq!(value["projectID"], "proj2");
    assert_eq!(value["previous"]["location"]["directory"], "/tmp/ox");
}

/// The ephemeral `session.usage.updated` is broadcast only after the
/// committing transaction: the totals it carries are read back from the
/// committed row on a separate connection.
#[tokio::test]
async fn usage_updated_broadcast_after_commit() {
    let bus = bus().await;
    create(&bus, "s1").await;
    let mut rx = bus.subscribe();

    bus.publish(SessionEventData::SessionUsageRecorded {
        session_id: "s1".to_string(),
        source: ox::schema::event::UsageSource::Title,
        cost: ox::schema::support::Finite::Float(0.25),
        tokens: TokenUsage {
            input: 3,
            output: 4,
            reasoning: 1,
            cache: TokenCache { read: 2, write: 0 },
        },
    })
    .await
    .unwrap();

    let mut seen_recorded = false;
    let mut usage = None;
    while let Ok(ev) = rx.try_recv() {
        if ev.kind == "session.usage.recorded" {
            seen_recorded = true;
        }
        if ev.kind == "session.usage.updated" {
            usage = Some(ev);
        }
    }
    assert!(seen_recorded, "durable usage.recorded was broadcast");
    let usage = usage.expect("ephemeral usage.updated was broadcast after commit");
    assert!(usage.durable.is_none());
    let typed = usage.parse_typed();
    match typed.data {
        SessionEventData::SessionUsageUpdated {
            session_id,
            cost,
            tokens,
        } => {
            assert_eq!(session_id, "s1");
            assert_eq!(cost, ox::schema::support::Finite::Float(0.25));
            assert_eq!(tokens.input, 3);
            assert_eq!(tokens.output, 4);
            assert_eq!(tokens.reasoning, 1);
            assert_eq!(tokens.cache.read, 2);
        }
        other => panic!("unexpected payload: {other:?}"),
    }

    // Ephemeral events are not persisted.
    let conn = bus.db().acquire().await.unwrap();
    let n = one_row(
        &conn,
        "SELECT count(*) FROM event WHERE aggregate_id = 's1' \
         AND type LIKE 'session.usage.updated%'",
        (),
    )
    .await
    .get::<i64>(0)
    .unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------------------
// Copied §N§ tag notation
// ---------------------------------------------------------------------------

/// A model that learned the overlay's `§N§ ` prefix echoes it at the start of
/// its own reply (and stutters the same number). The projector must strip it
/// before the text part is persisted — the stored transcript is what later
/// requests and the TUI read back.
#[tokio::test]
async fn copied_tags_are_stripped_from_persisted_text() {
    let bus = bus().await;
    create(&bus, "s1").await;

    bus.publish(SessionEventData::StepStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        agent: "build".to_string(),
        model: model(),
        snapshot: None,
        started: 1000,
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::TextStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        ordinal: 0,
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::TextEnded {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        ordinal: 0,
        text: "§194§ §194§ §194§ Task 2 实现完成".to_string(),
        state: None,
    })
    .await
    .unwrap();

    let conn = bus.db().acquire().await.unwrap();
    let data = one_row(
        &conn,
        "SELECT data FROM session_message WHERE id = 'am1'",
        (),
    )
    .await
    .get::<String>(0)
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(
        value["content"][0]["text"].as_str().unwrap(),
        "Task 2 实现完成",
        "the copied tag run never reaches the store"
    );
}

/// Reasoning parts stutter too (observed: `§73§ §73§ §73§ §73§ Let me look
/// at …`); the reference only guards text, ox guards both persistence points.
#[tokio::test]
async fn copied_tags_are_stripped_from_persisted_reasoning() {
    let bus = bus().await;
    create(&bus, "s1").await;

    bus.publish(SessionEventData::StepStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        agent: "build".to_string(),
        model: model(),
        snapshot: None,
        started: 1000,
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::ReasoningStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        ordinal: 0,
        state: None,
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::ReasoningEnded {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        ordinal: 0,
        text: "§73§ §73§ §73§ §73§ Let me look at the drop order".to_string(),
        state: None,
    })
    .await
    .unwrap();

    let conn = bus.db().acquire().await.unwrap();
    let data = one_row(
        &conn,
        "SELECT data FROM session_message WHERE id = 'am1'",
        (),
    )
    .await
    .get::<String>(0)
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(
        value["content"][0]["text"].as_str().unwrap(),
        "Let me look at the drop order"
    );
}

#[tokio::test]
async fn instructions_updated_projects_a_system_row() {
    let db = Db::open_in_memory().await.unwrap();
    let bus = EventBus::new(db.clone());
    create(&bus, "s1").await;
    let delta: Record =
        serde_json::from_value(serde_json::json!({"core/date": "abc123"})).expect("delta record");
    // The event data carries no `metadata`: the row metadata is projector-built.
    bus.publish(SessionEventData::SessionInstructionsUpdated {
        session_id: "s1".to_string(),
        delta,
        sources: Vec::new(),
        text: Some("Today's date is now: Fri Oct 23 2026".to_string()),
    })
    .await
    .unwrap();
    // text 为 None = 纯簿记，不落行
    bus.publish(SessionEventData::SessionInstructionsUpdated {
        session_id: "s1".to_string(),
        delta: Default::default(),
        sources: Vec::new(),
        text: None,
    })
    .await
    .unwrap();
    let conn = db.acquire().await.expect("conn");
    let row = one_row(
        &conn,
        "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'system'",
        ["s1"],
    )
    .await;
    let data: serde_json::Value =
        serde_json::from_str(&row.get::<String>(0).expect("data")).expect("json");
    assert_eq!(data["text"], "Today's date is now: Fri Oct 23 2026");
    assert_eq!(data["description"], "Instructions updated: core/date");
    assert_eq!(data["metadata"]["notice"], "instructions");
    assert_eq!(
        data["metadata"]["instructionSources"],
        serde_json::json!(["core/date"])
    );
    assert_eq!(data["metadata"]["instructions"]["core/date"], "abc123");
    let mut stmt = conn
        .prepare("SELECT COUNT(*) FROM session_message WHERE session_id = ?1 AND type = 'system'")
        .await
        .expect("prepare");
    let mut rows = stmt.query(["s1"]).await.expect("query");
    let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert_eq!(count, 1, "bookkeeping event must not add a row");
}

/// Batch I / L6: an event carrying `sources` projects one machine-readable
/// entry per instruction file — `global` for the home file, `project` for
/// every ancestor-chain file — so replay/audit tooling never string-matches
/// paths. Events without `sources` (the test above) keep the legacy
/// key-string form.
#[tokio::test]
async fn instructions_sources_kind_tag_the_projected_row() {
    let db = Db::open_in_memory().await.unwrap();
    let bus = EventBus::new(db.clone());
    create(&bus, "s1").await;
    let delta: Record = serde_json::from_value(serde_json::json!({"core/instructions": "abc123"}))
        .expect("delta record");
    let sources = vec![
        ox_schema::event::InstructionSource {
            key: "core/instructions".to_owned(),
            path: Some("/home/.ox/AGENTS.md".to_owned()),
            kind: Some(ox_schema::event::InstructionSourceKind::Global),
        },
        ox_schema::event::InstructionSource {
            key: "core/instructions".to_owned(),
            path: Some("/repo/AGENTS.md".to_owned()),
            kind: Some(ox_schema::event::InstructionSourceKind::Project),
        },
        ox_schema::event::InstructionSource {
            key: "core/date".to_owned(),
            path: None,
            kind: Some(ox_schema::event::InstructionSourceKind::Config),
        },
    ];
    bus.publish(SessionEventData::SessionInstructionsUpdated {
        session_id: "s1".to_string(),
        delta,
        sources,
        text: Some("Instructions from: /repo/AGENTS.md".to_string()),
    })
    .await
    .unwrap();
    let conn = db.acquire().await.expect("conn");
    let row = one_row(
        &conn,
        "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'system'",
        ["s1"],
    )
    .await;
    let data: serde_json::Value =
        serde_json::from_str(&row.get::<String>(0).expect("data")).expect("json");
    assert_eq!(
        data["metadata"]["instructionSources"],
        serde_json::json!([
            {"key": "core/instructions", "path": "/home/.ox/AGENTS.md", "kind": "global"},
            {"key": "core/instructions", "path": "/repo/AGENTS.md", "kind": "project"},
            {"key": "core/date", "kind": "config"},
        ])
    );
}

// ---------------------------------------------------------------------------
// session.deleted + magic-context tables
// ---------------------------------------------------------------------------

/// `session.deleted` must clear the magic-context tables too. They carry no
/// foreign key to `session_v2`, so the cascade that clears `session_message`,
/// `session_inbox` and `session_pending` never reaches them — measured on the
/// production database: 28 orphan `mc_tag` rows left by earlier deletions.
#[tokio::test]
async fn session_deleted_purges_magic_context_rows() {
    let bus = bus().await;
    create(&bus, "s1").await;
    create(&bus, "s2").await;
    {
        let conn = bus.db().acquire().await.unwrap();
        for sid in ["s1", "s2"] {
            exec(
                &conn,
                &format!(
                    "INSERT INTO mc_compartments
                       (id, session_id, coverage_end, importance, tiers, created_at, search_text)
                     VALUES ('c-{sid}', '{sid}', 'm1', 0.5, 't', 1, '')"
                ),
            )
            .await;
            exec(
                &conn,
                &format!(
                    "INSERT INTO mc_tag (session_id, tag, message_id, part_path, digest)
                     VALUES ('{sid}', 1, 'm', 'p', 'd')"
                ),
            )
            .await;
            exec(
                &conn,
                &format!(
                    "INSERT INTO mc_drop (session_id, tag, reason, state, created_at)
                     VALUES ('{sid}', 1, 'r', 'queued', 1)"
                ),
            )
            .await;
            exec(
                &conn,
                &format!(
                    "INSERT INTO mc_historian_run
                       (id, session_id, state, attempt, created_at, updated_at)
                     VALUES ('h-{sid}', '{sid}', 'done', 1, 1, 1)"
                ),
            )
            .await;
            exec(
                &conn,
                &format!(
                    "INSERT INTO mc_frozen_state (session_id, state, version)
                     VALUES ('{sid}', '{{}}', 1)"
                ),
            )
            .await;
            exec(
                &conn,
                &format!(
                    "INSERT INTO mc_note
                       (id, session_id, title, body, status, surface_condition,
                        created_at, updated_at)
                     VALUES ('n-{sid}', '{sid}', 't', 'b', 'active', NULL, 1, 1)"
                ),
            )
            .await;
        }
    }

    bus.publish(SessionEventData::SessionDeleted {
        session_id: "s1".to_string(),
    })
    .await
    .unwrap();

    let conn = bus.db().acquire().await.unwrap();
    for table in [
        "mc_compartments",
        "mc_tag",
        "mc_drop",
        "mc_historian_run",
        "mc_frozen_state",
        "mc_note",
    ] {
        let dead = one_row(
            &conn,
            &format!("SELECT count(*) FROM {table} WHERE session_id = 's1'"),
            (),
        )
        .await
        .get::<i64>(0)
        .unwrap();
        assert_eq!(dead, 0, "{table} still holds rows of the deleted session");
        let live = one_row(
            &conn,
            &format!("SELECT count(*) FROM {table} WHERE session_id = 's2'"),
            (),
        )
        .await
        .get::<i64>(0)
        .unwrap();
        assert_eq!(live, 1, "{table} lost a live session's rows");
    }
}

async fn exec(conn: &ox_db::turso::Connection, sql: &str) {
    conn.execute(sql, ()).await.expect("exec");
}

// ---------------------------------------------------------------------------
// archive / unarchive
// ---------------------------------------------------------------------------

/// Upstream `SessionEvent.Archived/Unarchived`: the archive watermark is set
/// (and `time_updated` bumped) on archive, cleared on unarchive.
#[tokio::test]
async fn archive_sets_and_clears_the_archive_watermark() {
    let bus = bus().await;
    create(&bus, "s1").await;

    bus.publish(SessionEventData::SessionArchived {
        session_id: "s1".to_string(),
    })
    .await
    .unwrap();
    {
        let conn = bus.db().acquire().await.unwrap();
        let row = one_row(
            &conn,
            "SELECT time_archived, time_updated FROM session_v2 WHERE id = 's1'",
            (),
        )
        .await;
        let archived = row.get::<Option<i64>>(0).unwrap();
        assert!(archived.is_some(), "archive must stamp time_archived");
        assert_eq!(
            row.get::<i64>(1).unwrap(),
            archived.unwrap(),
            "time_updated follows the archive stamp"
        );
    }

    bus.publish(SessionEventData::SessionUnarchived {
        session_id: "s1".to_string(),
    })
    .await
    .unwrap();
    let conn = bus.db().acquire().await.unwrap();
    let cleared = one_row(
        &conn,
        "SELECT time_archived FROM session_v2 WHERE id = 's1'",
        (),
    )
    .await
    .get::<Option<i64>>(0)
    .unwrap();
    assert!(cleared.is_none(), "unarchive must clear time_archived");
}
