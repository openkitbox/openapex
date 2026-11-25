//! Static tool paths and nested-call lifecycle events at the public API.

use std::sync::{Arc, Mutex};

use ox_codemode::{
    CallEvent, CallStatus, ExecutionError, Limits, ToolCallRecord, ToolInvoker, execute,
};

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
        output.result.and_then(|result| result.done),
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
        "tools[\"ns\"][\"name\"]({ a: 2 }); tools.ns[\"other\"]({}); tools[\"third\"].name({}); return { done: true };",
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
    assert_eq!(output.result.and_then(|result| result.done), Some(true));
    assert_eq!(output.calls.len(), 1);
}
