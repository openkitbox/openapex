//! Model-facing CBM tools: five thin adapters over [`Cbm::call`].
//!
//! The names are the binary's own bare CLI names (its error hints reference
//! them verbatim), so the registry entries need no prefix translation. Each
//! adapter validates the model's arguments, assembles the CLI's snake_case
//! JSON object, resolves `project` when the caller omitted it, and delegates
//! the spawn to the gateway — defaults and deeper validation stay with the
//! binary.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use ox_schema::wire::ToolDef;
use ox_tools::contract::{Tool, ToolContext, ToolOutput};
use ox_tools::{
    int_param, invalid_arguments, object_param, optional_bool, optional_positive_int, optional_str,
    param, required_str,
};

use super::Cbm;

const INDEX_MODES: &[&str] = &["full", "moderate", "fast"];
const TRACE_DIRECTIONS: &[&str] = &["inbound", "outbound", "both"];
const TRACE_MODES: &[&str] = &[
    "calls",
    "data_flow",
    "cross_service",
    "topology",
    "deploy",
    "run",
];
const SNIPPET_SOURCE_MODES: &[&str] = &["auto", "full", "outline"];

/// The indexed-project closing sentence shared by the four read tools'
/// descriptions; `index_repository` has its own closer.
const INDEXED_PROJECT_HINT: &str = "Requires an indexed project; omit `project` to use the project covering the workspace — call `index_repository` first if it reports none.";

/// The five CBM tools, in stable registration order.
pub fn all(cbm: Arc<Cbm>) -> [Arc<dyn Tool>; 5] {
    [
        Arc::new(IndexRepositoryTool {
            cbm: Arc::clone(&cbm),
        }) as Arc<dyn Tool>,
        Arc::new(SearchGraphTool {
            cbm: Arc::clone(&cbm),
        }),
        Arc::new(TracePathTool {
            cbm: Arc::clone(&cbm),
        }),
        Arc::new(GetCodeSnippetTool {
            cbm: Arc::clone(&cbm),
        }),
        Arc::new(GetArchitectureTool { cbm }),
    ]
}

/// A JSON-Schema string fragment restricted to `values`.
fn enum_param(values: &[&str], description: &str) -> Value {
    json!({ "type": "string", "enum": values, "description": description })
}

/// Validate an optional enum field. `None` means absent or `null`; a wrong
/// type or a value outside `allowed` is an invalid-arguments failure naming
/// the allowed values.
fn optional_enum<'a>(
    tool: &str,
    input: &'a Value,
    key: &str,
    allowed: &[&str],
) -> Result<Option<&'a str>, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Ok(None);
    };
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if allowed.contains(&value.as_str()) => Ok(Some(value)),
        Some(Value::String(value)) => Err(invalid_arguments(
            tool,
            &[(
                key,
                format!("Expected one of: {}; got \"{value}\"", allowed.join(", ")),
            )],
            input,
        )),
        Some(other) => Err(invalid_arguments(
            tool,
            &[(
                key,
                format!("Expected one of: {}; got {other}", allowed.join(", ")),
            )],
            input,
        )),
    }
}

/// Validate an optional array-of-strings field (only `aspects` needs it).
fn optional_string_array(
    tool: &str,
    input: &Value,
    key: &str,
) -> Result<Option<Vec<String>>, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Ok(None);
    };
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => {
            let mut strings = Vec::with_capacity(values.len());
            for (index, value) in values.iter().enumerate() {
                match value {
                    Value::String(text) => strings.push(text.clone()),
                    other => {
                        return Err(invalid_arguments(
                            tool,
                            &[(
                                key,
                                format!("Expected an array of strings; item {index} is {other}"),
                            )],
                            input,
                        ));
                    }
                }
            }
            Ok(Some(strings))
        }
        Some(other) => Err(invalid_arguments(
            tool,
            &[(key, format!("Expected an array of strings, got {other}"))],
            input,
        )),
    }
}

/// Resolve the project argument: an explicit `project` wins; otherwise the
/// gateway picks the indexed project covering the session directory.
async fn project_arg(
    cbm: &Cbm,
    tool: &str,
    ctx: &ToolContext,
    input: &Value,
) -> Result<String, ToolOutput> {
    match optional_str(tool, input, "project")? {
        Some(project) => Ok(project.to_owned()),
        None => cbm.project_for(&ctx.directory, &ctx.cancellation).await,
    }
}

/// Insert an optional argument, leaving absent options out of the object so
/// the binary's own defaults apply.
fn put(args: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        args.insert(key.to_owned(), value);
    }
}

/// Append the shared indexed-project sentence.
fn described(summary: &str) -> String {
    format!("{summary} {INDEXED_PROJECT_HINT}")
}

// ---------------------------------------------------------------------------
// index_repository
// ---------------------------------------------------------------------------

struct IndexRepositoryTool {
    cbm: Arc<Cbm>,
}

impl Tool for IndexRepositoryTool {
    fn name(&self) -> &str {
        "index_repository"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "index_repository",
            "Index or refresh a repository's code graph so its symbols, routes, and call edges become searchable. `repo_path` chooses the root (defaults to the workspace), `mode` trades indexing depth for speed, and `status: true` reports the last job instead of starting one. Indexing writes the graph, may take a while on large repositories, is abortable by cancellation, and should be re-run after substantial edits.",
            object_param(
                vec![
                    (
                        "repo_path",
                        param(
                            "string",
                            "Repository root to index. Defaults to the session workspace directory.",
                        ),
                    ),
                    (
                        "name",
                        param("string", "Project name to record the index under."),
                    ),
                    (
                        "mode",
                        enum_param(
                            INDEX_MODES,
                            "Indexing depth: full, moderate, or fast (default: full).",
                        ),
                    ),
                    (
                        "status",
                        param(
                            "boolean",
                            "Report the last index job instead of starting a new one.",
                        ),
                    ),
                ],
                &[],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_index_repository(&self.cbm, &ctx, &input).await })
    }
}

async fn run_index_repository(cbm: &Cbm, ctx: &ToolContext, input: &Value) -> ToolOutput {
    const TOOL: &str = "index_repository";
    let repo_path = match optional_str(TOOL, input, "repo_path") {
        Ok(Some(path)) => path.to_owned(),
        Ok(None) => ctx.directory.to_string_lossy().into_owned(),
        Err(output) => return output,
    };
    let name = match optional_str(TOOL, input, "name") {
        Ok(name) => name,
        Err(output) => return output,
    };
    let mode = match optional_enum(TOOL, input, "mode", INDEX_MODES) {
        Ok(mode) => mode,
        Err(output) => return output,
    };
    let status = match optional_bool(TOOL, input, "status") {
        Ok(status) => status,
        Err(output) => return output,
    };
    let mut args = Map::new();
    args.insert("repo_path".to_owned(), Value::String(repo_path));
    put(
        &mut args,
        "name",
        name.map(|value| Value::String(value.to_owned())),
    );
    put(
        &mut args,
        "mode",
        mode.map(|value| Value::String(value.to_owned())),
    );
    put(&mut args, "status", status.map(Value::Bool));
    cbm.call(
        TOOL,
        &Value::Object(args),
        &ctx.directory,
        &ctx.cancellation,
    )
    .await
}

// ---------------------------------------------------------------------------
// search_graph
// ---------------------------------------------------------------------------

struct SearchGraphTool {
    cbm: Arc<Cbm>,
}

impl Tool for SearchGraphTool {
    fn name(&self) -> &str {
        "search_graph"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "search_graph",
            described(
                "Search the code graph for functions, classes, routes, and variables. Pass at least one of `query` (free-text intent), `name_pattern`, or `qn_pattern` (regex filters on symbol or qualified names); without one the binary returns an arbitrary page of the whole graph. Results are a single page — raise `limit` if truncated.",
            ),
            object_param(
                vec![
                    (
                        "query",
                        param(
                            "string",
                            "Free-text search query matched against the graph.",
                        ),
                    ),
                    (
                        "name_pattern",
                        param("string", "Pattern to match against symbol names."),
                    ),
                    (
                        "qn_pattern",
                        param("string", "Pattern to match against qualified names."),
                    ),
                    (
                        "file_pattern",
                        param("string", "Restrict results to files matching this pattern."),
                    ),
                    (
                        "label",
                        param(
                            "string",
                            "Restrict results to a node label, such as Function.",
                        ),
                    ),
                    (
                        "project",
                        param(
                            "string",
                            "Indexed project name. Defaults to the project covering the workspace.",
                        ),
                    ),
                    (
                        "limit",
                        int_param(1, "Maximum number of results to return."),
                    ),
                    (
                        "max_output_tokens",
                        int_param(1, "Maximum size of the returned output in tokens."),
                    ),
                ],
                &[],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_search_graph(&self.cbm, &ctx, &input).await })
    }
}

async fn run_search_graph(cbm: &Cbm, ctx: &ToolContext, input: &Value) -> ToolOutput {
    const TOOL: &str = "search_graph";
    let mut args = Map::new();
    for key in [
        "query",
        "name_pattern",
        "qn_pattern",
        "file_pattern",
        "label",
    ] {
        match optional_str(TOOL, input, key) {
            Ok(Some(value)) => {
                args.insert(key.to_owned(), Value::String(value.to_owned()));
            }
            Ok(None) => {}
            Err(output) => return output,
        }
    }
    for key in ["limit", "max_output_tokens"] {
        match optional_positive_int(TOOL, input, key) {
            Ok(Some(value)) => {
                args.insert(key.to_owned(), json!(value));
            }
            Ok(None) => {}
            Err(output) => return output,
        }
    }
    let project = match project_arg(cbm, TOOL, ctx, input).await {
        Ok(project) => project,
        Err(output) => return output,
    };
    args.insert("project".to_owned(), Value::String(project));
    cbm.call(
        TOOL,
        &Value::Object(args),
        &ctx.directory,
        &ctx.cancellation,
    )
    .await
}

// ---------------------------------------------------------------------------
// trace_path
// ---------------------------------------------------------------------------

struct TracePathTool {
    cbm: Arc<Cbm>,
}

impl Tool for TracePathTool {
    fn name(&self) -> &str {
        "trace_path"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "trace_path",
            described(
                "Trace who calls `function_name` or what it calls. `direction` selects inbound, outbound, or both, `depth` bounds the traversal (the binary caps it at 15), and `mode` chooses call, data-flow, or architecture views.",
            ),
            object_param(
                vec![
                    (
                        "function_name",
                        param("string", "Function whose call path should be traced."),
                    ),
                    (
                        "project",
                        param(
                            "string",
                            "Indexed project name. Defaults to the project covering the workspace.",
                        ),
                    ),
                    (
                        "direction",
                        enum_param(
                            TRACE_DIRECTIONS,
                            "Trace callers (inbound), callees (outbound), or both (default: both).",
                        ),
                    ),
                    (
                        "depth",
                        int_param(
                            1,
                            "Maximum traversal depth (default: binary-chosen; the binary caps it at 15).",
                        ),
                    ),
                    (
                        "mode",
                        enum_param(
                            TRACE_MODES,
                            "Traversal view: calls, data_flow, cross_service, topology, deploy, or run (default: calls).",
                        ),
                    ),
                    (
                        "include_tests",
                        param("boolean", "Include test code in the trace."),
                    ),
                    (
                        "risk_labels",
                        param("boolean", "Annotate edges with risk labels."),
                    ),
                    (
                        "limit",
                        int_param(1, "Maximum number of nodes or edges to return."),
                    ),
                ],
                &["function_name"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_trace_path(&self.cbm, &ctx, &input).await })
    }
}

async fn run_trace_path(cbm: &Cbm, ctx: &ToolContext, input: &Value) -> ToolOutput {
    const TOOL: &str = "trace_path";
    let function_name = match required_str(TOOL, input, "function_name") {
        Ok(name) => name,
        Err(output) => return output,
    };
    let direction = match optional_enum(TOOL, input, "direction", TRACE_DIRECTIONS) {
        Ok(direction) => direction,
        Err(output) => return output,
    };
    let depth = match optional_positive_int(TOOL, input, "depth") {
        Ok(depth) => depth,
        Err(output) => return output,
    };
    let mode = match optional_enum(TOOL, input, "mode", TRACE_MODES) {
        Ok(mode) => mode,
        Err(output) => return output,
    };
    let include_tests = match optional_bool(TOOL, input, "include_tests") {
        Ok(value) => value,
        Err(output) => return output,
    };
    let risk_labels = match optional_bool(TOOL, input, "risk_labels") {
        Ok(value) => value,
        Err(output) => return output,
    };
    let limit = match optional_positive_int(TOOL, input, "limit") {
        Ok(limit) => limit,
        Err(output) => return output,
    };
    let mut args = Map::new();
    args.insert(
        "function_name".to_owned(),
        Value::String(function_name.to_owned()),
    );
    put(
        &mut args,
        "direction",
        direction.map(|value| Value::String(value.to_owned())),
    );
    put(&mut args, "depth", depth.map(|value| json!(value)));
    put(
        &mut args,
        "mode",
        mode.map(|value| Value::String(value.to_owned())),
    );
    put(&mut args, "include_tests", include_tests.map(Value::Bool));
    put(&mut args, "risk_labels", risk_labels.map(Value::Bool));
    put(&mut args, "limit", limit.map(|value| json!(value)));
    let project = match project_arg(cbm, TOOL, ctx, input).await {
        Ok(project) => project,
        Err(output) => return output,
    };
    args.insert("project".to_owned(), Value::String(project));
    cbm.call(
        TOOL,
        &Value::Object(args),
        &ctx.directory,
        &ctx.cancellation,
    )
    .await
}

// ---------------------------------------------------------------------------
// get_code_snippet
// ---------------------------------------------------------------------------

struct GetCodeSnippetTool {
    cbm: Arc<Cbm>,
}

impl Tool for GetCodeSnippetTool {
    fn name(&self) -> &str {
        "get_code_snippet"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "get_code_snippet",
            described(
                "Read one symbol's source by `qualified_name`. `source_mode` selects auto, full, or outline, while `start_line` and `max_lines` narrow a full read.",
            ),
            object_param(
                vec![
                    (
                        "qualified_name",
                        param("string", "Qualified name of the symbol to read."),
                    ),
                    (
                        "project",
                        param(
                            "string",
                            "Indexed project name. Defaults to the project covering the workspace.",
                        ),
                    ),
                    (
                        "source_mode",
                        enum_param(
                            SNIPPET_SOURCE_MODES,
                            "Source form: auto, full, or outline (default: auto).",
                        ),
                    ),
                    (
                        "start_line",
                        int_param(1, "First line of the returned window (1-based)."),
                    ),
                    (
                        "max_lines",
                        int_param(1, "Maximum number of lines to return."),
                    ),
                    (
                        "member_limit",
                        int_param(1, "Maximum number of members to include for a container."),
                    ),
                    (
                        "include_neighbors",
                        param(
                            "boolean",
                            "Include adjacent declarations around the symbol.",
                        ),
                    ),
                ],
                &["qualified_name"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_get_code_snippet(&self.cbm, &ctx, &input).await })
    }
}

async fn run_get_code_snippet(cbm: &Cbm, ctx: &ToolContext, input: &Value) -> ToolOutput {
    const TOOL: &str = "get_code_snippet";
    let qualified_name = match required_str(TOOL, input, "qualified_name") {
        Ok(name) => name,
        Err(output) => return output,
    };
    let source_mode = match optional_enum(TOOL, input, "source_mode", SNIPPET_SOURCE_MODES) {
        Ok(mode) => mode,
        Err(output) => return output,
    };
    let start_line = match optional_positive_int(TOOL, input, "start_line") {
        Ok(value) => value,
        Err(output) => return output,
    };
    let max_lines = match optional_positive_int(TOOL, input, "max_lines") {
        Ok(value) => value,
        Err(output) => return output,
    };
    let member_limit = match optional_positive_int(TOOL, input, "member_limit") {
        Ok(value) => value,
        Err(output) => return output,
    };
    let include_neighbors = match optional_bool(TOOL, input, "include_neighbors") {
        Ok(value) => value,
        Err(output) => return output,
    };
    let mut args = Map::new();
    args.insert(
        "qualified_name".to_owned(),
        Value::String(qualified_name.to_owned()),
    );
    put(
        &mut args,
        "source_mode",
        source_mode.map(|value| Value::String(value.to_owned())),
    );
    put(
        &mut args,
        "start_line",
        start_line.map(|value| json!(value)),
    );
    put(&mut args, "max_lines", max_lines.map(|value| json!(value)));
    put(
        &mut args,
        "member_limit",
        member_limit.map(|value| json!(value)),
    );
    put(
        &mut args,
        "include_neighbors",
        include_neighbors.map(Value::Bool),
    );
    let project = match project_arg(cbm, TOOL, ctx, input).await {
        Ok(project) => project,
        Err(output) => return output,
    };
    args.insert("project".to_owned(), Value::String(project));
    cbm.call(
        TOOL,
        &Value::Object(args),
        &ctx.directory,
        &ctx.cancellation,
    )
    .await
}

// ---------------------------------------------------------------------------
// get_architecture
// ---------------------------------------------------------------------------

struct GetArchitectureTool {
    cbm: Arc<Cbm>,
}

impl Tool for GetArchitectureTool {
    fn name(&self) -> &str {
        "get_architecture"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "get_architecture",
            described(
                "Summarize an indexed project's architecture, optionally narrowed to `path`. `aspects` selects specific views such as `dependencies`, `entry_points`, or `layers`.",
            ),
            object_param(
                vec![
                    (
                        "project",
                        param(
                            "string",
                            "Indexed project name. Defaults to the project covering the workspace.",
                        ),
                    ),
                    (
                        "aspects",
                        json!({
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Architecture aspects to include, such as `dependencies`, `entry_points`, or `layers`.",
                        }),
                    ),
                    (
                        "path",
                        param(
                            "string",
                            "Restrict the summary to this path within the project.",
                        ),
                    ),
                ],
                &[],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_get_architecture(&self.cbm, &ctx, &input).await })
    }
}

async fn run_get_architecture(cbm: &Cbm, ctx: &ToolContext, input: &Value) -> ToolOutput {
    const TOOL: &str = "get_architecture";
    let aspects = match optional_string_array(TOOL, input, "aspects") {
        Ok(aspects) => aspects,
        Err(output) => return output,
    };
    let path = match optional_str(TOOL, input, "path") {
        Ok(path) => path,
        Err(output) => return output,
    };
    let mut args = Map::new();
    put(&mut args, "aspects", aspects.map(|value| json!(value)));
    put(
        &mut args,
        "path",
        path.map(|value| Value::String(value.to_owned())),
    );
    let project = match project_arg(cbm, TOOL, ctx, input).await {
        Ok(project) => project,
        Err(output) => return output,
    };
    args.insert("project".to_owned(), Value::String(project));
    cbm.call(
        TOOL,
        &Value::Object(args),
        &ctx.directory,
        &ctx.cancellation,
    )
    .await
}
