//! `execute`: the model-visible Code Mode tool.
//!
//! A call submits one plain JavaScript program. The program runs in a fresh
//! `ox-codemode` QuickJS world, calls other registered tools through the static
//! `tools` object, and returns an upstream-shaped `{output, toolCalls, files}`
//! result. Nested calls go through the host installed on the registry, so they
//! inherit the session's permission gate and cancellation; this module only
//! converts their lifecycle events into wire rows and progress metadata.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use ox_codemode::{CallEvent, CallStatus, CodeOutput, Limits, ToolInvoker, execute_scoped};
use ox_schema::wire::ToolDef;

use crate::contract::{ExecuteHost, NestedCallContext, Tool, ToolContext, ToolOutput};
use crate::{fail, invalid_arguments, object_param, param};

/// Upstream publishes at most one replacement row per 100 ms; the terminal
/// tool result carries the complete rows and is never throttled.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Tools withheld from Code Mode scripts: `execute` (no recursion), host/agent
/// controls that own their own lifecycle (`shell`, `subagent`, automations),
/// and `question` (a form a script cannot settle). The prompt catalog and the
/// runtime authorization both consult this one list.
const CODEMODE_DENIED_TOOLS: [&str; 4] = ["execute", "shell", "subagent", "question"];

/// Whether `name` must not be callable from a Code Mode script.
#[must_use]
pub fn is_codemode_denied(name: &str) -> bool {
    CODEMODE_DENIED_TOOLS.contains(&name) || name.starts_with("automation_")
}

/// Mirrors upstream `packages/core/src/codemode/tool.ts` default-mode
/// description, with ox's documented sandbox deviations spelled out.
const DESCRIPTION: &str = concat!(
    "Run JavaScript in a confined Code Mode runtime to script tool calls and HTTP requests and compose their results.\n",
    "Write plain JavaScript and send it exactly as written: no code fence, and no JSON or other wrapper around it.\n",
    "Each call is a fresh program; nothing carries over from earlier calls.\n",
    "`fetch` is unavailable in ox; use the catalog's `webfetch` tool for HTTP requests. Imports, direct filesystem access, and timers are unavailable: there is no `setTimeout` or sleep. All other external access goes through `tools`.\n",
    "Inside `execute`, the only callable tools are those explicitly listed in the Code Mode catalog instructions. Ignore tools shown outside the Code Mode catalog. They are not available in the Code Mode runtime.\n",
    "Call tools through `tools` using only exact paths and signatures from the catalog: `tools.name(...)` or `tools[\"name\"](...)`. Keep those paths static; do not infer, normalize, or build tool names dynamically.\n",
    "Prefer an explicit `return`; if omitted, the final top-level expression becomes the result.\n",
    "Await every call whose completion matters; pending calls are interrupted when execution ends. Run independent calls concurrently with `Promise.allSettled`.",
);

pub struct ExecuteTool;

impl Tool for ExecuteTool {
    fn name(&self) -> &str {
        "execute"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "execute",
            DESCRIPTION,
            object_param(
                vec![(
                    "code",
                    param("string", "Plain JavaScript program to run in Code Mode."),
                )],
                &["code"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move {
            let code = match parse_code(&input) {
                Ok(code) => code.to_owned(),
                Err(output) => return output,
            };
            if ctx.cancellation.is_cancelled() {
                return fail("Tool execution interrupted");
            }
            let Some(host) = ctx.registry.execute_host() else {
                return fail(
                    "execute is unavailable: no nested tool host is installed for this session",
                );
            };
            // The session's live toolset is the authority; derive paths once at
            // this call boundary and never keep a second cached tool list.
            let allowed: BTreeSet<String> = ctx
                .registry
                .definitions()
                .into_iter()
                .map(|definition| definition.function.name)
                .filter(|name| !is_codemode_denied(name))
                .collect();
            let handle = tokio::runtime::Handle::current();
            let ids = CallIds {
                session_id: ctx.session_id.clone(),
                assistant_message_id: ctx.assistant_message_id.clone(),
                parent_call_id: ctx.call_id.clone(),
            };
            let directory = ctx.directory.clone();
            let cancellation = ctx.cancellation.clone();

            let task = tokio::task::spawn_blocking(move || {
                let progress = Arc::new(Progress::new(PROGRESS_INTERVAL));
                let invoker = Arc::new(HostInvoker {
                    host: Arc::clone(&host),
                    handle,
                    ids,
                    directory,
                    cancellation,
                    progress: Arc::clone(&progress),
                    last_error: Mutex::new(None),
                });
                let limits = Limits::default();
                let bridge: Arc<dyn ToolInvoker> = invoker.clone();
                let outcome = execute_scoped(&code, &allowed, "{}", bridge, &limits);
                // Terminal metadata comes from the runtime's own final records
                // on success; a failed run has no `CodeOutput`, so use the
                // invoker's last observed rows (including its error status).
                let rows = match &outcome {
                    Ok(output) => output
                        .calls
                        .iter()
                        .map(|call| wire_row(&call.path, call.status, call.input.as_deref()))
                        .collect(),
                    Err(_) => progress.rows(),
                };
                let (output_text, failed) = match outcome {
                    Ok(output) => (format_output(&output), false),
                    Err(error) => {
                        // QuickJS reports a thrown bridge error generically;
                        // keep the host's own reason for the model.
                        let text = match invoker.last_error() {
                            Some(reason) if !error.to_string().contains(&reason) => {
                                format!("{error}: {reason}")
                            }
                            _ => error.to_string(),
                        };
                        (text, true)
                    }
                };
                let output_text = bounded(output_text, limits.output_bytes);
                let mut content = Map::new();
                content.insert("output".to_owned(), Value::String(output_text));
                content.insert("toolCalls".to_owned(), json!(rows));
                content.insert("files".to_owned(), json!([]));
                let mut metadata = Map::new();
                metadata.insert("toolCalls".to_owned(), json!(rows));
                metadata.insert("files".to_owned(), json!([]));
                if failed {
                    content.insert("error".to_owned(), json!(true));
                    metadata.insert("error".to_owned(), json!(true));
                }
                ToolOutput {
                    content: Value::Object(content),
                    // Upstream settles the tool call successfully and flags a
                    // failed script inside content/metadata; the client renders
                    // the cross from that flag.
                    error: None,
                    metadata: Some(Value::Object(metadata)),
                    attachments: None,
                }
            });
            match task.await {
                Ok(output) => output,
                Err(error) => fail(format!("execute task failed: {error}")),
            }
        })
    }
}

/// The `code` field is required, but a bare JSON string is also accepted:
/// upstream's `freeform` input sends the program as one text field, and models
/// ported from it sometimes send that text as the whole argument.
fn parse_code(input: &Value) -> Result<&str, ToolOutput> {
    match input {
        Value::String(code) => Ok(code),
        Value::Object(object) => match object.get("code") {
            Some(Value::String(code)) => Ok(code),
            Some(other) => Err(invalid_arguments(
                "execute",
                &[("code", format!("Expected string, got {other}"))],
                input,
            )),
            None => Err(invalid_arguments(
                "execute",
                &[("code", "Missing required field".to_owned())],
                input,
            )),
        },
        _ => Err(invalid_arguments(
            "execute",
            &[("code", "Expected an object or a code string".to_owned())],
            input,
        )),
    }
}

/// Session identity for progress and nested calls. The nested tool gets its
/// own call ID from the host; progress always addresses the parent `execute`
/// call so the client can attach rows to the running tool part.
struct CallIds {
    session_id: String,
    assistant_message_id: String,
    parent_call_id: String,
}

/// Synchronous bridge from the QuickJS blocking task to the async host.
struct HostInvoker {
    host: Arc<dyn ExecuteHost>,
    handle: tokio::runtime::Handle,
    ids: CallIds,
    directory: PathBuf,
    cancellation: CancellationToken,
    progress: Arc<Progress>,
    last_error: Mutex<Option<String>>,
}

impl ToolInvoker for HostInvoker {
    fn invoke(&self, path: &str, args: &str) -> anyhow::Result<String> {
        let nested = NestedCallContext {
            session_id: &self.ids.session_id,
            directory: &self.directory,
            assistant_message_id: &self.ids.assistant_message_id,
            parent_call_id: &self.ids.parent_call_id,
            cancellation: self.cancellation.clone(),
        };
        match self
            .handle
            .block_on(self.host.run_nested(nested, path, args))
        {
            // The JS wrapper JSON-parses the bridge result. The host returns
            // model-facing text, so quote it as a JSON string here rather than
            // letting a plain word fail the script at the wrapper boundary.
            Ok(text) => serde_json::to_string(&text).map_err(|error| anyhow!(error.to_string())),
            Err(message) => {
                *self.last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(message.clone());
                Err(anyhow!(message))
            }
        }
    }

    fn on_call(&self, event: CallEvent<'_>) {
        self.progress.observe(self.host.as_ref(), &self.ids, event);
    }
}

impl HostInvoker {
    fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

#[derive(Default)]
struct ProgressState {
    rows: Vec<Value>,
    published_at: Option<Instant>,
}

/// Call rows and the publish clock live together so a row can never be
/// observed without the throttle seeing the same state.
struct Progress {
    interval: Duration,
    state: Mutex<ProgressState>,
}

impl Progress {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(ProgressState::default()),
        }
    }

    fn observe(&self, host: &dyn ExecuteHost, ids: &CallIds, event: CallEvent<'_>) {
        let status = event.status;
        let row = wire_row(event.path, status, event.input);
        let rows = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            match status {
                CallStatus::Running => state.rows.push(row),
                CallStatus::Completed | CallStatus::Error => {
                    let tool = row["tool"].as_str().unwrap_or_default();
                    let running = state.rows.iter().rposition(|existing| {
                        existing["status"] == "running" && existing["tool"] == tool
                    });
                    match running {
                        Some(index) => state.rows[index] = row,
                        None => state.rows.push(row),
                    }
                }
            }
            let now = Instant::now();
            let due = state
                .published_at
                .is_none_or(|at| now.duration_since(at) >= self.interval);
            if due {
                state.published_at = Some(now);
                Some(state.rows.clone())
            } else {
                None
            }
        };
        if let Some(rows) = rows {
            let metadata = json!({ "toolCalls": rows });
            host.publish_progress(
                &ids.session_id,
                &ids.assistant_message_id,
                &ids.parent_call_id,
                metadata,
            );
        }
    }

    fn rows(&self) -> Vec<Value> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .rows
            .clone()
    }
}

fn wire_row(path: &str, status: CallStatus, input: Option<&str>) -> Value {
    let status = match status {
        CallStatus::Running => "running",
        CallStatus::Completed => "completed",
        CallStatus::Error => "error",
    };
    let mut row = Map::new();
    row.insert("tool".to_owned(), Value::String(path.to_owned()));
    row.insert("status".to_owned(), Value::String(status.to_owned()));
    // An unparsable argument string is not worth failing a nested call over:
    // the client simply gets a row without `input`.
    if let Some(value) = input.and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        && let Some(shown) = display_input(value)
    {
        row.insert("input".to_owned(), shown);
    }
    Value::Object(row)
}

/// Upstream `displayInput`: objects are shown as-is (an empty object says
/// nothing), primitives are wrapped under `input`, null is omitted.
fn display_input(input: Value) -> Option<Value> {
    match input {
        Value::Null => None,
        Value::Object(ref object) if object.is_empty() => None,
        Value::Object(_) => Some(input),
        other => Some(json!({ "input": other })),
    }
}

/// Upstream `formatResult`: the script result first, then captured console
/// logs. Both stay inside the runtime's output budget.
fn format_output(output: &CodeOutput) -> String {
    let result = output
        .result
        .as_ref()
        .and_then(|result| match &result.state {
            Some(Value::String(text)) => Some(text.clone()),
            Some(value) => serde_json::to_string(value).ok(),
            None if result.done.is_some() || result.send.is_some() => {
                serde_json::to_string(result).ok()
            }
            None => None,
        });
    let logs = (!output.logs.is_empty()).then(|| format!("Logs:\n{}", output.logs));
    [result, logs]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n")
}

const TRUNCATION_MARKER: &str = "[truncated]";

fn bounded(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    if limit <= TRUNCATION_MARKER.len() {
        return TRUNCATION_MARKER.chars().take(limit).collect();
    }
    let mut cut = limit - TRUNCATION_MARKER.len();
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{TRUNCATION_MARKER}", &text[..cut])
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use serde_json::{Value, json};

    use super::*;
    use crate::contract::{Tool, ToolOutput};
    use crate::ok_text;

    struct FakeHost {
        nested: Mutex<Vec<(String, String)>>,
        publishes: Mutex<Vec<Value>>,
        fail_nested: Mutex<bool>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                nested: Mutex::new(Vec::new()),
                publishes: Mutex::new(Vec::new()),
                fail_nested: Mutex::new(false),
            }
        }

        fn fail_nested(&self) {
            *self.fail_nested.lock().unwrap_or_else(|p| p.into_inner()) = true;
        }

        fn publish_count(&self) -> usize {
            self.publishes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len()
        }
    }

    impl ExecuteHost for FakeHost {
        fn run_nested<'a>(
            &'a self,
            ctx: NestedCallContext<'a>,
            path: &'a str,
            input: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            self.nested
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((path.to_owned(), input.to_owned()));
            let failed = *self.fail_nested.lock().unwrap_or_else(|p| p.into_inner());
            let parent = ctx.parent_call_id.to_owned();
            Box::pin(async move {
                if failed {
                    Err(format!("nested tool denied under {parent}"))
                } else {
                    Ok("nested result".to_owned())
                }
            })
        }

        fn publish_progress(
            &self,
            _session_id: &str,
            _assistant_message_id: &str,
            call_id: &str,
            metadata: Value,
        ) {
            assert_eq!(call_id, "call_parent", "progress addresses the parent call");
            self.publishes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(metadata);
        }
    }

    struct ProbeTool;

    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            "probe"
        }

        fn definition(&self) -> ToolDef {
            ToolDef::function("probe", "A test tool.", json!({"type": "object"}))
        }

        fn execute(
            &self,
            _ctx: ToolContext,
            _input: Value,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            Box::pin(async { ok_text("unused") })
        }
    }

    /// Registered on purpose: authorization must deny this tool even though
    /// the session has it.
    struct DeniedTool;

    impl Tool for DeniedTool {
        fn name(&self) -> &str {
            "shell"
        }

        fn definition(&self) -> ToolDef {
            ToolDef::function(
                "shell",
                "A registered but denied tool.",
                json!({"type": "object"}),
            )
        }

        fn execute(
            &self,
            _ctx: ToolContext,
            _input: Value,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            Box::pin(async { ok_text("should not run") })
        }
    }

    fn context(host: Option<Arc<FakeHost>>) -> ToolContext {
        let mut registry = crate::contract::ToolRegistry::new();
        registry
            .register(Arc::new(ProbeTool) as Arc<dyn Tool>)
            .expect("probe registers once");
        registry
            .register(Arc::new(DeniedTool) as Arc<dyn Tool>)
            .expect("shell registers once");
        if let Some(host) = host {
            registry.set_execute_host(host);
        }
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: PathBuf::from("/tmp"),
            assistant_message_id: "msg_test".to_owned(),
            call_id: "call_parent".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(registry),
        }
    }

    async fn run(host: &Arc<FakeHost>, code: &str) -> ToolOutput {
        ExecuteTool
            .execute(context(Some(Arc::clone(host))), json!({ "code": code }))
            .await
    }

    #[test]
    fn codemode_denylist_is_exact() {
        for name in [
            "execute",
            "shell",
            "subagent",
            "question",
            "automation_create",
        ] {
            assert!(is_codemode_denied(name), "{name} must be denied");
        }
        for name in ["probe", "read", "automation", "shell_script"] {
            assert!(!is_codemode_denied(name), "{name} must stay callable");
        }
    }

    #[tokio::test]
    async fn denied_tool_is_absent_from_the_runtime_allowed_set() {
        let host = Arc::new(FakeHost::new());
        let output = run(&host, "tools.shell({ command: 'echo hi' }); return 1;").await;
        assert_eq!(
            output.content["error"],
            json!(true),
            "a denied tool cannot be called by a script"
        );
        assert!(
            host.nested
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
            "the host never sees a denied path"
        );
    }

    #[test]
    fn definition_and_input_parsing_accept_object_and_bare_string() {
        assert_eq!(ExecuteTool.name(), "execute");
        let definition = ExecuteTool.definition();
        assert_eq!(definition.function.name, "execute");
        assert_eq!(
            definition.function.parameters["required"][0], "code",
            "the schema has one required code field"
        );

        assert_eq!(
            parse_code(&json!({ "code": "return 1;" })).expect("object input"),
            "return 1;"
        );
        assert_eq!(
            parse_code(&json!("return 1;")).expect("bare string input"),
            "return 1;"
        );
        assert!(parse_code(&json!({})).is_err(), "missing code fails");
        assert!(parse_code(&json!(42)).is_err(), "a number is not code");
    }

    #[tokio::test]
    async fn missing_host_fails_closed() {
        let output = ExecuteTool
            .execute(context(None), json!({ "code": "return 1;" }))
            .await;
        let error = output.error.expect("no host is an error");
        assert!(
            error.contains("no nested tool host"),
            "the error names the missing host: {error}"
        );
        assert!(output.metadata.is_none());
    }

    #[tokio::test]
    async fn success_rows_are_complete_in_terminal_metadata() {
        let host = Arc::new(FakeHost::new());
        let output = run(
            &host,
            "tools.probe({ n: 1 }); tools.probe({ n: 2 }); return { done: true, state: { ok: 1 } };",
        )
        .await;

        assert!(
            output.error.is_none(),
            "a successful script settles normally"
        );
        let metadata = output.metadata.expect("terminal metadata");
        let rows = metadata["toolCalls"].as_array().expect("toolCalls array");
        assert_eq!(rows.len(), 2, "every nested call has a terminal row");
        assert_eq!(rows[0]["tool"], "probe");
        assert_eq!(rows[0]["status"], "completed");
        assert_eq!(rows[0]["input"], json!({ "n": 1 }));
        assert_eq!(rows[1]["status"], "completed");
        assert_eq!(metadata["files"], json!([]));
        assert!(metadata.get("error").is_none(), "no error flag on success");
        assert_eq!(output.content["toolCalls"], json!(rows));
        assert_eq!(output.content["files"], json!([]));
        assert!(
            output.content["output"]
                .as_str()
                .expect("output string")
                .contains("{\"ok\":1}"),
            "the JSON result is in the model-visible output"
        );
        assert!(
            host.publish_count() >= 1,
            "the first live call publishes at least once"
        );
    }

    #[tokio::test]
    async fn failed_script_flags_error_and_keeps_error_row() {
        let host = Arc::new(FakeHost::new());
        host.fail_nested();
        let output = run(&host, "tools.probe({ n: 3 }); return { done: true };").await;

        assert!(
            output.error.is_none(),
            "upstream settles execute successfully"
        );
        assert_eq!(output.content["error"], json!(true));
        let metadata = output.metadata.expect("terminal metadata");
        assert_eq!(metadata["error"], json!(true));
        assert_eq!(metadata["toolCalls"][0]["status"], "error");
        assert_eq!(metadata["toolCalls"][0]["input"], json!({ "n": 3 }));
        assert_eq!(output.content["toolCalls"], metadata["toolCalls"]);
        assert!(
            output.content["output"]
                .as_str()
                .expect("output string")
                .contains("nested tool denied"),
            "the nested failure is JS-visible: {}",
            output.content
        );
    }

    #[test]
    fn progress_is_throttled_without_dropping_final_rows() {
        assert_eq!(PROGRESS_INTERVAL, Duration::from_millis(100));
        let host = Arc::new(FakeHost::new());
        let progress = Progress::new(Duration::from_secs(60));
        let ids = CallIds {
            session_id: "ses_test".to_owned(),
            assistant_message_id: "msg_test".to_owned(),
            parent_call_id: "call_parent".to_owned(),
        };
        let event = |status, input: &'static str| CallEvent {
            path: "probe",
            status,
            input: Some(input),
        };

        progress.observe(host.as_ref(), &ids, event(CallStatus::Running, "{\"n\":1}"));
        progress.observe(
            host.as_ref(),
            &ids,
            event(CallStatus::Completed, "{\"n\":1}"),
        );
        progress.observe(host.as_ref(), &ids, event(CallStatus::Running, "{\"n\":2}"));
        progress.observe(host.as_ref(), &ids, event(CallStatus::Error, "{\"n\":2}"));

        assert_eq!(host.publish_count(), 1, "rapid events publish at most once");
        let rows = progress.rows();
        assert_eq!(rows.len(), 2, "terminal state is kept for every call");
        assert_eq!(rows[0]["status"], "completed");
        assert_eq!(rows[1]["status"], "error");
        assert_eq!(
            host.publishes.lock().unwrap_or_else(|p| p.into_inner())[0]["toolCalls"][0]["status"],
            "running",
            "the first publish shows the live row"
        );
    }

    #[test]
    fn unparsable_input_is_omitted_and_output_is_bounded() {
        let row = wire_row("probe", CallStatus::Error, Some("not json"));
        assert_eq!(row["tool"], "probe");
        assert!(row.get("input").is_none(), "bad JSON is not a wire input");

        let text = "x".repeat(50);
        let cut = bounded(text, 20);
        assert_eq!(cut.len(), 20, "the budget is respected");
        assert!(cut.ends_with(TRUNCATION_MARKER));
        assert_eq!(bounded("x".repeat(50), 4).len(), 4, "tiny budgets hold too");
    }
}
