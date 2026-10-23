//! Live smoke test against the real `codebase-memory-mcp` binary.
//!
//! Ignored by default: it needs the binary on `PATH` and an indexed
//! repository. `index_repository` writes the machine-global codebase-memory
//! cache and registry (nothing lands in the worktree) and registers this
//! checkout by its root path, so the read steps address the project the
//! index step just refreshed. The checks pin two repo symbols,
//! `ToolRegistry` and `dispatch`; update them if those move.
//!
//! Run with `cargo test --lib cbm::live -- --ignored --nocapture`; the
//! per-step timings and byte counts printed on stderr are the ExecPlan's
//! evidence.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::Cbm;
use super::tools::all;
use ox_tools::contract::{ToolContext, ToolOutput};

/// One step's ceiling, so a wedged daemon fails as a named step, not a hang.
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

/// The binary's remedy for files changing mid-index is a single rerun.
const RETRY_TOKEN: &str = "aborted_previous_preserved";

/// One execution-scoped context per step, named after the tool it runs.
fn context(tool: &str) -> ToolContext {
    ToolContext {
        session_id: "live".to_owned(),
        directory: env!("CARGO_MANIFEST_DIR").into(),
        call_id: tool.to_owned(),
        cancellation: CancellationToken::new(),
        registry: std::sync::Arc::new(ox_tools::contract::ToolRegistry::new()),
    }
}

/// Model content as text; a null/non-string body is a contract breach here.
fn text(output: &ToolOutput) -> &str {
    match output.content.as_str() {
        Some(text) => text,
        None => panic!("expected text content, got {}", output.content),
    }
}

/// Assert `body` contains every `needle`, quoting the whole body on failure.
fn require(body: &str, tool: &str, needles: &[&str]) {
    for needle in needles {
        assert!(
            body.contains(needle),
            "{tool} body does not contain {needle:?}: {body}"
        );
    }
}

/// `index_repository` must report a published index, not a refusal or a
/// partial run. `status` is authoritative; `state`/`outcome` are fallbacks
/// for older output shapes.
fn index_published(body: &str) {
    let parsed: Value = serde_json::from_str(body)
        .unwrap_or_else(|error| panic!("index_repository returned invalid JSON ({error}): {body}"));
    let status = match parsed.get("status") {
        Some(Value::String(status)) => status.as_str(),
        Some(other) => panic!("index_repository status is not a string ({other}): {body}"),
        None => ["state", "outcome"]
            .iter()
            .find_map(|key| parsed.get(*key).and_then(Value::as_str))
            .unwrap_or_else(|| {
                panic!("index_repository response has no status, state, or outcome: {body}")
            }),
    };
    assert_eq!(
        status, "indexed",
        "index_repository did not publish an index: {body}"
    );
}

fn search_graph_answers(body: &str) {
    require(body, "search_graph", &["ToolRegistry", "src/runner/mod.rs"]);
}

fn trace_path_answers(body: &str) {
    require(
        body,
        "trace_path",
        &["function: dispatch", "direction: inbound"],
    );
}

fn snippet_answers(body: &str) {
    require(
        body,
        "get_code_snippet",
        &["pub struct ToolRegistry", "src/runner/mod.rs"],
    );
}

fn architecture_answers(body: &str) {
    assert!(
        !body.trim().is_empty(),
        "get_architecture returned empty content: {body:?}"
    );
}

/// A live step: tool name, arguments, and the check its body must pass.
type Step = (&'static str, Value, fn(&str));

#[tokio::test]
#[ignore = "needs the real codebase-memory-mcp binary and an indexed repository"]
async fn live_smoke_through_the_model_facing_tools() {
    let cbm = Cbm::discover()
        .expect("live test needs codebase-memory-mcp on PATH (not found or not executable)");
    let tools = all(Arc::new(cbm));
    // Each step carries its own check, so reordering or inserting a step
    // cannot silently satisfy another step's assertion.
    let steps: [Step; 5] = [
        ("index_repository", json!({}), index_published),
        (
            "search_graph",
            json!({"name_pattern": "ToolRegistry"}),
            search_graph_answers,
        ),
        (
            "trace_path",
            json!({"function_name": "dispatch", "direction": "inbound"}),
            trace_path_answers,
        ),
        (
            "get_code_snippet",
            json!({"qualified_name": "ToolRegistry"}),
            snippet_answers,
        ),
        // Its argv shape is pinned by the stub suite; this step proves the
        // real subcommand runs and answers.
        ("get_architecture", json!({}), architecture_answers),
    ];

    for (name, args, check) in steps {
        let tool = tools
            .iter()
            .find(|tool| tool.name() == name)
            .unwrap_or_else(|| panic!("CBM tool `{name}` is not registered"));
        let mut retried = false;
        loop {
            let started = Instant::now();
            let output = timeout(STEP_TIMEOUT, tool.execute(context(name), args.clone()))
                .await
                .unwrap_or_else(|_| panic!("{name} timed out after {}s", STEP_TIMEOUT.as_secs()));
            let elapsed = started.elapsed();
            if !retried
                && let Some(error) = &output.error
                && error.contains(RETRY_TOKEN)
            {
                eprintln!(
                    "cbm live: {name} {} ms, retrying once after {RETRY_TOKEN}: {error}",
                    elapsed.as_millis()
                );
                retried = true;
                continue;
            }
            if let Some(error) = &output.error {
                eprintln!(
                    "cbm live: {name} {} ms, error: {error}",
                    elapsed.as_millis()
                );
            }
            assert!(output.error.is_none(), "{name} failed: {:?}", output.error);
            let body = text(&output);
            eprintln!(
                "cbm live: {name} {} ms, {} bytes",
                elapsed.as_millis(),
                body.len()
            );
            check(body);
            break;
        }
    }
}
