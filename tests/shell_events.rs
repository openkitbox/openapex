//! The shell table's lifecycle notices: what the event bridge publishes.
//! Exercised on a private table so no bus, server or global is involved.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ox::tool::shell::events::frame;
use ox::tool::shell::sessions::{Notice, Observer, Owner, ShellSessions, SpawnRequest, Status};
use serde_json::json;

type Log = Arc<Mutex<Vec<(String, Notice)>>>;

fn recorder() -> (Observer, Log) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let observer: Observer = Arc::new(move |directory, notice| {
        sink.lock().unwrap().push((directory.to_owned(), notice));
    });
    (observer, log)
}

/// Poll (bounded) until the observer has seen `count` notices.
async fn wait_for_notices(log: &Log, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while log.lock().unwrap().len() < count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count} notices, saw {:?}",
            log.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn request(command: &str, owner: Option<Owner>) -> SpawnRequest {
    SpawnRequest {
        command: command.to_owned(),
        cwd: std::env::temp_dir(),
        timeout_ms: Some(10_000),
        completion: None,
        foreground: false,
        owner,
    }
}

fn owner() -> Owner {
    Owner {
        session_id: "ses_owner".to_owned(),
        directory: "/work/project".to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn created_precedes_exited_and_both_carry_the_owner_location() {
    let table = ShellSessions::new();
    let (observer, log) = recorder();
    table.set_observer(observer);

    let session = table.spawn(request("echo hi", Some(owner()))).unwrap();
    assert!(session.wait_settled(Duration::from_secs(10)).await);
    // `settled` is visible a moment before the exited notice goes out.
    wait_for_notices(&log, 2).await;

    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(
        log.iter()
            .all(|(directory, _)| directory == "/work/project")
    );
    let Notice::Created(info) = &log[0].1 else {
        panic!("first notice must be created: {:?}", log[0]);
    };
    assert_eq!(info.id, session.id);
    assert_eq!(info.status, Status::Running);
    assert_eq!(info.metadata["sessionID"], json!("ses_owner"));
    let Notice::Exited { id, exit, status } = &log[1].1 else {
        panic!("second notice must be exited: {:?}", log[1]);
    };
    assert_eq!(
        (id.as_str(), *exit, *status),
        (session.id.as_str(), Some(0), Status::Exited)
    );
    // Exited is published after the capture is final.
    assert!(session.recent(64).contains("hi"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_without_an_owner_is_never_announced() {
    let table = ShellSessions::new();
    let (observer, log) = recorder();
    table.set_observer(observer);
    let session = table.spawn(request("true", None)).unwrap();
    assert!(session.wait_settled(Duration::from_secs(10)).await);
    assert!(table.remove(&session.id));
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_holds_only_running_sessions_and_get_still_serves_settled_ones() {
    let table = ShellSessions::new();
    let running = table.spawn(request("sleep 30", Some(owner()))).unwrap();
    let done = table.spawn(request("true", Some(owner()))).unwrap();
    assert!(done.wait_settled(Duration::from_secs(10)).await);

    let ids: Vec<String> = table.list().into_iter().map(|info| info.id).collect();
    assert_eq!(ids, vec![running.id.clone()]);
    assert!(table.get(&done.id).is_some());

    running.stop(true).await;
    assert!(table.list().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_announces_deleted_once_and_refuses_a_running_session() {
    let table = ShellSessions::new();
    let (observer, log) = recorder();
    table.set_observer(observer);
    let session = table.spawn(request("sleep 30", Some(owner()))).unwrap();

    assert!(!table.remove(&session.id), "running sessions are kept");
    session.stop(true).await;
    wait_for_notices(&log, 2).await;
    assert!(table.remove(&session.id));
    assert!(!table.remove(&session.id), "second remove finds nothing");

    let log = log.lock().unwrap();
    let kinds: Vec<&str> = log
        .iter()
        .map(|(_, notice)| match notice {
            Notice::Created(_) => "created",
            Notice::Exited { .. } => "exited",
            Notice::Deleted { .. } => "deleted",
        })
        .collect();
    assert_eq!(kinds, ["created", "exited", "deleted"]);
    assert_eq!(log[2].0, "/work/project");
}

#[test]
fn frames_match_the_upstream_event_schemas() {
    let (kind, data) = frame(Notice::Exited {
        id: "sh_1".to_owned(),
        exit: Some(3),
        status: Status::Exited,
    });
    assert_eq!(kind, "shell.exited");
    assert_eq!(data, json!({ "id": "sh_1", "exit": 3, "status": "exited" }));

    let (_, data) = frame(Notice::Exited {
        id: "sh_1".to_owned(),
        exit: None,
        status: Status::Killed,
    });
    assert_eq!(data, json!({ "id": "sh_1", "status": "killed" }));

    let (kind, data) = frame(Notice::Deleted {
        id: "sh_1".to_owned(),
    });
    assert_eq!((kind, data), ("shell.deleted", json!({ "id": "sh_1" })));
}
