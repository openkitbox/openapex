//! Static tool paths and nested-call lifecycle events at the public API.

use std::{
    sync::{
        Arc, Barrier, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

use ox_codemode::{
    CallEvent, CallStatus, ExecutionError, Limits, ToolCallRecord, ToolInvoker, execute,
};
use serde_json::{Value, json};

struct RecordingInvoker {
    events: Mutex<Vec<ToolCallRecord>>,
    fail: bool,
}

impl RecordingInvoker {
    fn new(fail: bool) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            fail,
        }
    }

    fn events(&self) -> Vec<ToolCallRecord> {
        self.events.lock().expect("event lock").clone()
    }
}

impl ToolInvoker for RecordingInvoker {
    fn invoke(&self, path: &str, args: &str) -> anyhow::Result<String> {
        if self.fail {
            anyhow::bail!("nested tool denied");
        }
        Ok(format!(r#"{{"path":"{path}","args":{args}}}"#))
    }

    fn on_call(&self, event: CallEvent<'_>) {
        self.events
            .lock()
            .expect("event lock")
            .push(ToolCallRecord {
                path: event.path.to_owned(),
                status: event.status,
                input: event.input.map(|input| input.to_owned()),
            });
    }
}

struct NoHookInvoker;

impl ToolInvoker for NoHookInvoker {
    fn invoke(&self, _path: &str, _args: &str) -> anyhow::Result<String> {
        Ok(r#"{"ok":true}"#.to_owned())
    }
}

/// Two calls cannot both pass the barrier unless the runtime lets them stay
/// in flight at the same time.
struct GateInvoker {
    events: Mutex<Vec<ToolCallRecord>>,
    barrier: Arc<Barrier>,
}

impl ToolInvoker for GateInvoker {
    fn invoke(&self, _path: &str, _args: &str) -> anyhow::Result<String> {
        Ok(r#"{"ok":true}"#.to_owned())
    }

    fn start(&self, path: &str, args: &str, done: ox_codemode::Completion) -> anyhow::Result<()> {
        let barrier = Arc::clone(&self.barrier);
        let path = path.to_owned();
        let args = args.to_owned();
        std::thread::spawn(move || {
            barrier.wait();
            done(Ok(format!(r#"{{"path":"{path}","args":{args}}}"#)));
        });
        Ok(())
    }

    fn on_call(&self, event: CallEvent<'_>) {
        self.events.lock().expect("event lock").push(record(
            event.path,
            event.status,
            event.input.expect("args"),
        ));
    }
}

struct NeverInvoker;

impl ToolInvoker for NeverInvoker {
    fn invoke(&self, _path: &str, _args: &str) -> anyhow::Result<String> {
        Ok(r#"{"ok":true}"#.to_owned())
    }

    fn start(
        &self,
        _path: &str,
        _args: &str,
        _done: ox_codemode::Completion,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

struct StartErrorInvoker;

impl ToolInvoker for StartErrorInvoker {
    fn invoke(&self, _path: &str, _args: &str) -> anyhow::Result<String> {
        Ok(r#"{"ok":true}"#.to_owned())
    }

    fn start(
        &self,
        _path: &str,
        _args: &str,
        _done: ox_codemode::Completion,
    ) -> anyhow::Result<()> {
        anyhow::bail!("start refused")
    }
}

/// An asynchronous call that stays in flight after the script returns.
struct LateInvoker {
    release_rx: Arc<Mutex<Receiver<()>>>,
    finished_tx: Sender<()>,
    events: Mutex<Vec<ToolCallRecord>>,
}

impl ToolInvoker for LateInvoker {
    fn invoke(&self, _path: &str, _args: &str) -> anyhow::Result<String> {
        Ok(r#"{"ok":true}"#.to_owned())
    }

    fn start(&self, _path: &str, _args: &str, done: ox_codemode::Completion) -> anyhow::Result<()> {
        let release_rx = Arc::clone(&self.release_rx);
        let finished_tx = self.finished_tx.clone();
        std::thread::spawn(move || {
            release_rx
                .lock()
                .expect("release lock")
                .recv()
                .expect("release signal");
            done(Ok(r#"{"ok":true}"#.to_owned()));
            finished_tx.send(()).expect("finished signal");
        });
        Ok(())
    }

    fn on_call(&self, event: CallEvent<'_>) {
        self.events.lock().expect("event lock").push(record(
            event.path,
            event.status,
            event.input.expect("args"),
        ));
    }
}

/// A host that reports cancellation from another thread once the VM has
/// submitted the call.
struct CancelInvoker {
    cancel_rx: Arc<Mutex<Receiver<()>>>,
    started_tx: Sender<()>,
}

impl ToolInvoker for CancelInvoker {
    fn invoke(&self, _path: &str, _args: &str) -> anyhow::Result<String> {
        Ok(r#"{"ok":true}"#.to_owned())
    }

    fn start(&self, _path: &str, _args: &str, done: ox_codemode::Completion) -> anyhow::Result<()> {
        let cancel_rx = Arc::clone(&self.cancel_rx);
        let started_tx = self.started_tx.clone();
        std::thread::spawn(move || {
            started_tx.send(()).expect("started signal");
            cancel_rx
                .lock()
                .expect("cancel lock")
                .recv()
                .expect("cancel signal");
            done(Err("nested call interrupted".to_owned()));
        });
        Ok(())
    }
}

fn record(path: &str, status: CallStatus, input: &str) -> ToolCallRecord {
    ToolCallRecord {
        path: path.to_owned(),
        status,
        input: Some(input.to_owned()),
    }
}

#[test]
fn execute_calls_dot_path_records_running_and_completed() {
    let invoker = Arc::new(RecordingInvoker::new(false));
    let output = execute(
        "const value = tools.ns.name({ a: 1 }); return { done: true, state: value };",
        "{}",
        invoker.clone(),
        &Limits::default(),
    )
    .expect("dot path executes");

    assert_eq!(
        output
            .result
            .as_ref()
            .and_then(|result| result.get("done").and_then(Value::as_bool)),
        Some(true),
        "the program result is decoded"
    );
    assert_eq!(
        output.calls,
        vec![record("ns.name", CallStatus::Completed, r#"{"a":1}"#)],
        "the output carries the terminal call record"
    );
    assert_eq!(
        invoker.events(),
        vec![
            record("ns.name", CallStatus::Running, r#"{"a":1}"#),
            record("ns.name", CallStatus::Completed, r#"{"a":1}"#),
        ],
        "the hook sees running before completion"
    );
}

#[test]
fn execute_calls_static_bracket_paths_match_dot_paths() {
    let invoker = Arc::new(RecordingInvoker::new(false));
    let output = execute(
        "await tools[\"ns\"][\"name\"]({ a: 2 }); await tools.ns[\"other\"]({}); await tools[\"third\"].name({}); return { done: true };",
        "{}",
        invoker.clone(),
        &Limits::default(),
    )
    .expect("static bracket paths execute");

    let paths = output
        .calls
        .iter()
        .map(|call| call.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(paths, ["ns.name", "ns.other", "third.name"]);
    assert!(
        output
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Completed)
    );
    let statuses = invoker
        .events()
        .into_iter()
        .map(|event| event.status)
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        [
            CallStatus::Running,
            CallStatus::Completed,
            CallStatus::Running,
            CallStatus::Completed,
            CallStatus::Running,
            CallStatus::Completed,
        ]
    );
}

#[test]
fn execute_calls_reject_dynamic_and_indirect_access() {
    const REJECTED: [&str; 6] = [
        "const key = \"name\"; tools.ns[key]({});",
        "const alias = tools.ns; alias.name({});",
        "const fn = tools.ns.name; fn({});",
        "tools.ns.name.call(null, {});",
        "tools[\"ns\"][key]({});",
        "tools?.[\"ns\"].name({});",
    ];
    for code in REJECTED {
        let invoker = Arc::new(RecordingInvoker::new(false));
        let error = execute(code, "{}", invoker.clone(), &Limits::default())
            .expect_err("non-static access must be rejected");
        assert!(
            matches!(error, ExecutionError::Scan(_)),
            "{code}: {error:?}"
        );
        assert!(invoker.events().is_empty(), "nothing ran: {code}");
    }
}

#[test]
fn execute_calls_failed_invocation_records_running_then_error() {
    let invoker = Arc::new(RecordingInvoker::new(true));
    let error = execute(
        "tools.ns.name({ a: 3 });",
        "{}",
        invoker.clone(),
        &Limits::default(),
    )
    .expect_err("a denied nested tool fails the program");
    assert!(matches!(error, ExecutionError::Runtime(_)), "{error:?}");
    assert_eq!(
        invoker.events(),
        vec![
            record("ns.name", CallStatus::Running, r#"{"a":3}"#),
            record("ns.name", CallStatus::Error, r#"{"a":3}"#),
        ]
    );
}

#[test]
fn execute_calls_invokers_without_a_hook_keep_working() {
    let output = execute(
        "tools.ns.name({}); return { done: true };",
        "{}",
        Arc::new(NoHookInvoker),
        &Limits::default(),
    )
    .expect("default hook is a no-op");
    assert_eq!(
        output
            .result
            .as_ref()
            .and_then(|result| result.get("done").and_then(Value::as_bool)),
        Some(true)
    );
    assert_eq!(output.calls.len(), 1);
    assert_eq!(output.calls[0].status, CallStatus::Completed);
}

#[test]
fn execute_calls_top_level_await_resolves() {
    let invoker = Arc::new(RecordingInvoker::new(false));
    let output = execute(
        "return { state: { v: await Promise.resolve(7) } };",
        "{}",
        invoker,
        &Limits::default(),
    )
    .expect("top-level await runs");
    assert_eq!(
        output
            .result
            .as_ref()
            .and_then(|result| result.get("state").cloned()),
        Some(json!({ "v": 7 }))
    );
}

#[test]
fn execute_calls_async_tools_run_concurrently() {
    let invoker = Arc::new(GateInvoker {
        events: Mutex::new(Vec::new()),
        barrier: Arc::new(Barrier::new(2)),
    });
    let limits = Limits {
        timeout: Duration::from_secs(2),
        ..Limits::default()
    };
    let output = execute(
        "const settled = await Promise.allSettled([tools.ns.first({ n: 1 }), tools.ns.second({ n: 2 })]);\n\
         return { state: { a: settled[0].status, b: settled[1].status } };",
        "{}",
        invoker.clone(),
        &limits,
    )
    .expect("both gated calls settle");
    assert_eq!(
        output
            .result
            .as_ref()
            .and_then(|result| result.get("state").cloned()),
        Some(json!({ "a": "fulfilled", "b": "fulfilled" }))
    );
    let events = invoker.events.lock().expect("event lock").clone();
    assert_eq!(events.len(), 4);
    assert!(
        events[..2]
            .iter()
            .all(|event| event.status == CallStatus::Running),
        "both calls are running before either completes: {events:?}"
    );
    assert!(
        events[2..]
            .iter()
            .all(|event| event.status == CallStatus::Completed)
    );
}

#[test]
fn execute_calls_timeout_while_a_tool_is_pending() {
    let limits = Limits {
        timeout: Duration::from_millis(50),
        ..Limits::default()
    };
    let started = Instant::now();
    let error = execute(
        "await tools.ns.slow({}); return { done: true };",
        "{}",
        Arc::new(NeverInvoker),
        &limits,
    )
    .expect_err("a pending tool cannot outlive the deadline");
    assert!(matches!(error, ExecutionError::Timeout(_)), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the run must not hang on the in-flight tool"
    );
}

#[test]
fn execute_calls_cancelled_tool_settles_without_hanging() {
    let (started_tx, started_rx) = mpsc::channel();
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let invoker = Arc::new(CancelInvoker {
        cancel_rx: Arc::new(Mutex::new(cancel_rx)),
        started_tx,
    });
    let canceller = std::thread::spawn(move || {
        started_rx.recv().expect("the tool started");
        cancel_tx.send(()).expect("cancel the tool");
    });
    let started = Instant::now();
    let error = execute(
        "await tools.ns.wait({}); return { done: true };",
        "{}",
        invoker,
        &Limits::default(),
    )
    .expect_err("cancellation rejects the awaiting program");
    canceller.join().expect("canceller finished");
    assert!(
        matches!(&error, ExecutionError::Runtime(text) if text.contains("interrupted")),
        "{error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancellation must not leave the VM waiting"
    );
}

#[test]
fn execute_calls_start_error_without_done_fails_closed() {
    let started = Instant::now();
    let error = execute(
        "await tools.ns.refuse({}); return { done: true };",
        "{}",
        Arc::new(StartErrorInvoker),
        &Limits::default(),
    )
    .expect_err("a refused start must not leave the promise pending");
    assert!(
        matches!(&error, ExecutionError::Runtime(text) if text.contains("start refused")),
        "{error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "no completion means an immediate synthesized error"
    );
}

#[test]
fn execute_calls_rejected_promise_is_a_runtime_error() {
    let error = execute(
        "await Promise.reject(new Error('boom')); return { done: true };",
        "{}",
        Arc::new(NoHookInvoker),
        &Limits::default(),
    )
    .expect_err("a rejected promise has no partial result");
    assert!(
        matches!(&error, ExecutionError::Runtime(text) if text.contains("boom")),
        "{error:?}"
    );
}

#[test]
fn execute_calls_unawaited_async_call_gets_a_terminal_record() {
    let (release_tx, release_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let invoker = Arc::new(LateInvoker {
        release_rx: Arc::new(Mutex::new(release_rx)),
        finished_tx,
        events: Mutex::new(Vec::new()),
    });
    let output = execute(
        "tools.ns.later({ n: 9 }); return { done: true };",
        "{}",
        invoker.clone(),
        &Limits::default(),
    )
    .expect("an unawaited call does not block the program");
    assert_eq!(
        output
            .result
            .as_ref()
            .and_then(|result| result.get("done").and_then(Value::as_bool)),
        Some(true)
    );
    assert_eq!(
        output.calls,
        vec![record("ns.later", CallStatus::Error, r#"{"n":9}"#)],
        "the in-flight call is closed when the script returns"
    );
    assert_eq!(
        invoker.events.lock().expect("event lock").clone(),
        vec![
            record("ns.later", CallStatus::Running, r#"{"n":9}"#),
            record("ns.later", CallStatus::Error, r#"{"n":9}"#),
        ]
    );

    release_tx.send(()).expect("release the late completion");
    finished_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the late host task finished");
    assert_eq!(
        invoker.events.lock().expect("event lock").len(),
        2,
        "a completion after the run cannot add another terminal event"
    );
}
