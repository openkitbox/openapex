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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teststub::{MAIN_STUB, spawns, stdin_json, stub, write_projects};
    use tempfile::{TempDir, tempdir};
    use tokio_util::sync::CancellationToken;

    fn fixture() -> (TempDir, Arc<Cbm>) {
        let directory = tempdir().unwrap();
        let cbm = Arc::new(stub(
            &directory.path().join("codebase-memory-mcp"),
            MAIN_STUB,
        ));
        (directory, cbm)
    }

    /// Make the stub's `list_projects` answer cover the workspace as `ox`.
    fn cover_workspace(directory: &TempDir) {
        write_projects(
            directory.path(),
            json!({"projects": [
                {"name": "ox", "root_path": directory.path().to_string_lossy()},
            ]}),
        );
    }

    fn ctx(directory: &TempDir) -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: directory.path().to_path_buf(),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
            registry: std::sync::Arc::new(ox_tools::contract::ToolRegistry::new()),
        }
    }

    fn tool(tools: &[Arc<dyn Tool>; 5], name: &str) -> Arc<dyn Tool> {
        tools
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
            .unwrap_or_else(|| panic!("missing tool {name}"))
    }

    fn parameters(tools: &[Arc<dyn Tool>; 5], name: &str) -> Value {
        tool(tools, name).definition().function.parameters
    }

    async fn execute(
        tools: &[Arc<dyn Tool>; 5],
        name: &str,
        ctx: ToolContext,
        input: Value,
    ) -> ToolOutput {
        let tool = tool(tools, name);
        tool.execute(ctx, input).await
    }

    #[test]
    fn all_returns_five_tools_in_stable_order() {
        let (_directory, cbm) = fixture();
        let tools = all(cbm);
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(
            names,
            vec![
                "index_repository",
                "search_graph",
                "trace_path",
                "get_code_snippet",
                "get_architecture",
            ]
        );
        for tool in &tools {
            assert_eq!(
                tool.name(),
                tool.definition().function.name,
                "registry key must equal the advertised function name"
            );
        }
    }

    #[test]
    fn definitions_have_exact_schemas() {
        let (_directory, cbm) = fixture();
        let tools = all(cbm);
        let required: [(&str, Option<Value>); 5] = [
            ("index_repository", None),
            ("search_graph", None),
            ("trace_path", Some(json!(["function_name"]))),
            ("get_code_snippet", Some(json!(["qualified_name"]))),
            ("get_architecture", None),
        ];
        for (name, expected) in required {
            let params = parameters(&tools, name);
            assert_eq!(params["type"], json!("object"), "{name}");
            assert_eq!(params["additionalProperties"], json!(false), "{name}");
            match expected {
                Some(expected) => assert_eq!(params["required"], expected, "{name}"),
                None => assert!(
                    params.get("required").is_none(),
                    "{name} must not declare a required array"
                ),
            }
            for field in params["required"].as_array().into_iter().flatten() {
                let field = field.as_str().expect("required string");
                assert!(
                    params["properties"].get(field).is_some(),
                    "required field {field} of {name} missing from properties"
                );
            }
            let definition = tool(&tools, name).definition();
            assert!(!definition.function.description.is_empty(), "{name}");
        }

        // The read tools share the indexed-project closer; the tool that
        // creates the index does not.
        for name in [
            "search_graph",
            "trace_path",
            "get_code_snippet",
            "get_architecture",
        ] {
            let description = tool(&tools, name).definition().function.description;
            assert!(
                description.contains("Requires an indexed project"),
                "{name}: {description}"
            );
            assert!(
                description.contains("omit `project`"),
                "{name}: {description}"
            );
            assert!(
                description.contains("call `index_repository` first"),
                "{name}: {description}"
            );
        }
        let index_description = tool(&tools, "index_repository")
            .definition()
            .function
            .description;
        assert!(
            !index_description.contains("Requires an indexed project"),
            "{index_description}"
        );
        assert!(
            !index_description.contains("omit `project`"),
            "{index_description}"
        );
        assert!(
            index_description.contains("may take a while on large repositories"),
            "{index_description}"
        );
        assert!(
            index_description.contains("abortable by cancellation"),
            "{index_description}"
        );
        assert!(
            index_description.contains("re-run after substantial edits"),
            "{index_description}"
        );

        // Pinning each property set catches a dropped or renamed forwarding
        // key: the binary silently ignores unknown arguments.
        let properties: [(&str, &[&str]); 5] = [
            ("index_repository", &["mode", "name", "repo_path", "status"]),
            (
                "search_graph",
                &[
                    "file_pattern",
                    "label",
                    "limit",
                    "max_output_tokens",
                    "name_pattern",
                    "project",
                    "qn_pattern",
                    "query",
                ],
            ),
            (
                "trace_path",
                &[
                    "depth",
                    "direction",
                    "function_name",
                    "include_tests",
                    "limit",
                    "mode",
                    "project",
                    "risk_labels",
                ],
            ),
            (
                "get_code_snippet",
                &[
                    "include_neighbors",
                    "max_lines",
                    "member_limit",
                    "project",
                    "qualified_name",
                    "source_mode",
                    "start_line",
                ],
            ),
            ("get_architecture", &["aspects", "path", "project"]),
        ];
        for (name, expected) in properties {
            let params = parameters(&tools, name);
            let mut actual: Vec<&str> = params["properties"]
                .as_object()
                .expect("properties object")
                .keys()
                .map(String::as_str)
                .collect();
            actual.sort_unstable();
            assert_eq!(actual, expected, "{name} property keys");
        }

        // Every integer parameter rejects zero explicitly.
        let integer_params: [(&str, &[&str]); 3] = [
            ("search_graph", &["limit", "max_output_tokens"]),
            ("trace_path", &["depth", "limit"]),
            (
                "get_code_snippet",
                &["start_line", "max_lines", "member_limit"],
            ),
        ];
        for (name, fields) in integer_params {
            let params = parameters(&tools, name);
            for field in fields {
                assert_eq!(
                    params["properties"][*field]["type"],
                    json!("integer"),
                    "{name}.{field}"
                );
                assert_eq!(
                    params["properties"][*field]["minimum"],
                    json!(1),
                    "{name}.{field}"
                );
            }
        }

        assert_eq!(
            parameters(&tools, "index_repository")["properties"]["mode"]["enum"],
            json!(["full", "moderate", "fast"])
        );
        assert_eq!(
            parameters(&tools, "trace_path")["properties"]["direction"]["enum"],
            json!(["inbound", "outbound", "both"])
        );
        assert_eq!(
            parameters(&tools, "trace_path")["properties"]["mode"]["enum"],
            json!([
                "calls",
                "data_flow",
                "cross_service",
                "topology",
                "deploy",
                "run"
            ])
        );
        assert_eq!(
            parameters(&tools, "get_code_snippet")["properties"]["source_mode"]["enum"],
            json!(["auto", "full", "outline"])
        );
        let aspects = &parameters(&tools, "get_architecture")["properties"]["aspects"];
        assert_eq!(aspects["type"], json!("array"));
        assert_eq!(aspects["items"], json!({"type": "string"}));
    }

    #[tokio::test]
    async fn index_repository_defaults_repo_path_and_passes_status() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);

        let output = execute(
            &tools,
            "index_repository",
            ctx(&directory),
            json!({"status": true}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({
                "repo_path": directory.path().to_string_lossy(),
                "status": true,
            })
        );

        // An explicit repo_path replaces the workspace default.
        let output = execute(
            &tools,
            "index_repository",
            ctx(&directory),
            json!({"repo_path": "/tmp/elsewhere"}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({"repo_path": "/tmp/elsewhere"})
        );
    }

    #[tokio::test]
    async fn search_graph_forwards_validated_arguments() {
        let (directory, cbm) = fixture();
        cover_workspace(&directory);
        let tools = all(cbm);
        let output = execute(
            &tools,
            "search_graph",
            ctx(&directory),
            json!({"query": "Registry", "limit": 5}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({"query": "Registry", "limit": 5, "project": "ox"})
        );
    }

    #[tokio::test]
    async fn trace_path_forwards_required_and_depth() {
        let (directory, cbm) = fixture();
        cover_workspace(&directory);
        let tools = all(cbm);
        let output = execute(
            &tools,
            "trace_path",
            ctx(&directory),
            json!({"function_name": "Registry::get", "depth": 3}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({"function_name": "Registry::get", "depth": 3, "project": "ox"})
        );
    }

    #[tokio::test]
    async fn get_code_snippet_omits_absent_options() {
        let (directory, cbm) = fixture();
        cover_workspace(&directory);
        let tools = all(cbm);
        let output = execute(
            &tools,
            "get_code_snippet",
            ctx(&directory),
            json!({"qualified_name": "ox::Registry"}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({"qualified_name": "ox::Registry", "project": "ox"})
        );
    }

    #[tokio::test]
    async fn get_architecture_forwards_aspects_and_path() {
        let (directory, cbm) = fixture();
        cover_workspace(&directory);
        let tools = all(cbm);
        let output = execute(
            &tools,
            "get_architecture",
            ctx(&directory),
            json!({"aspects": ["dependencies", "entry_points"], "path": "src"}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({
                "aspects": ["dependencies", "entry_points"],
                "path": "src",
                "project": "ox",
            })
        );
    }

    #[tokio::test]
    async fn index_repository_forwards_all_arguments() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "index_repository",
            ctx(&directory),
            json!({
                "repo_path": "/tmp/repo",
                "name": "ox",
                "mode": "moderate",
                "status": true,
            }),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({
                "repo_path": "/tmp/repo",
                "name": "ox",
                "mode": "moderate",
                "status": true,
            })
        );
    }

    #[tokio::test]
    async fn search_graph_forwards_all_arguments() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "search_graph",
            ctx(&directory),
            json!({
                "query": "Registry",
                "name_pattern": "^Registry",
                "qn_pattern": "ox::.*",
                "file_pattern": "src/**/*.rs",
                "label": "Function",
                "project": "ox",
                "limit": 7,
                "max_output_tokens": 900,
            }),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(spawns(directory.path()), vec!["search_graph"]);
        assert_eq!(
            stdin_json(directory.path()),
            json!({
                "query": "Registry",
                "name_pattern": "^Registry",
                "qn_pattern": "ox::.*",
                "file_pattern": "src/**/*.rs",
                "label": "Function",
                "project": "ox",
                "limit": 7,
                "max_output_tokens": 900,
            })
        );
    }

    #[tokio::test]
    async fn trace_path_forwards_all_arguments() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "trace_path",
            ctx(&directory),
            json!({
                "function_name": "Registry::get",
                "project": "ox",
                "direction": "inbound",
                "depth": 4,
                "mode": "data_flow",
                "include_tests": true,
                "risk_labels": false,
                "limit": 9,
            }),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({
                "function_name": "Registry::get",
                "project": "ox",
                "direction": "inbound",
                "depth": 4,
                "mode": "data_flow",
                "include_tests": true,
                "risk_labels": false,
                "limit": 9,
            })
        );
    }

    #[tokio::test]
    async fn get_code_snippet_forwards_all_arguments() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "get_code_snippet",
            ctx(&directory),
            json!({
                "qualified_name": "ox::Registry",
                "project": "ox",
                "source_mode": "outline",
                "start_line": 3,
                "max_lines": 12,
                "member_limit": 5,
                "include_neighbors": false,
            }),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({
                "qualified_name": "ox::Registry",
                "project": "ox",
                "source_mode": "outline",
                "start_line": 3,
                "max_lines": 12,
                "member_limit": 5,
                "include_neighbors": false,
            })
        );
    }

    #[tokio::test]
    async fn get_architecture_forwards_all_arguments() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "get_architecture",
            ctx(&directory),
            json!({
                "project": "ox",
                "aspects": ["dependencies", "entry_points", "layers"],
                "path": "src/registry.rs",
            }),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            stdin_json(directory.path()),
            json!({
                "project": "ox",
                "aspects": ["dependencies", "entry_points", "layers"],
                "path": "src/registry.rs",
            })
        );
    }

    #[tokio::test]
    async fn omitted_project_resolves_before_tool_spawn() {
        let (directory, cbm) = fixture();
        cover_workspace(&directory);
        let tools = all(cbm);
        let output = execute(
            &tools,
            "search_graph",
            ctx(&directory),
            json!({"name_pattern": "Registry"}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            spawns(directory.path()),
            vec!["list_projects", "search_graph"]
        );
        assert_eq!(
            stdin_json(directory.path()),
            json!({"name_pattern": "Registry", "project": "ox"})
        );
    }

    #[tokio::test]
    async fn explicit_project_skips_list_projects() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "search_graph",
            ctx(&directory),
            json!({"project": "given", "query": "Registry"}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(spawns(directory.path()), vec!["search_graph"]);
        assert_eq!(
            stdin_json(directory.path()),
            json!({"project": "given", "query": "Registry"})
        );
    }

    #[tokio::test]
    async fn missing_project_match_returns_project_for_failure() {
        let (directory, cbm) = fixture();
        let elsewhere = directory.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        write_projects(
            directory.path(),
            json!({"projects": [
                {"name": "other", "root_path": elsewhere.to_string_lossy()},
            ]}),
        );
        let expected = cbm
            .project_for(directory.path(), &CancellationToken::new())
            .await
            .unwrap_err();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "search_graph",
            ctx(&directory),
            json!({"query": "Registry"}),
        )
        .await;
        assert_eq!(output.error, expected.error);
        let message = output.error.expect("no-match failure");
        assert!(message.contains("no indexed project"), "{message}");
        assert_eq!(
            spawns(directory.path()).last().map(String::as_str),
            Some("list_projects")
        );
        assert!(
            !spawns(directory.path())
                .iter()
                .any(|spawn| spawn == "search_graph"),
            "the tool must not spawn when project resolution fails"
        );
    }

    #[tokio::test]
    async fn invalid_enum_names_allowed_values() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "trace_path",
            ctx(&directory),
            json!({"function_name": "f", "direction": "sideways"}),
        )
        .await;
        let message = output.error.expect("enum failure");
        assert!(message.contains("inbound"), "{message}");
        assert!(message.contains("outbound"), "{message}");
        assert!(message.contains("both"), "{message}");
        assert!(spawns(directory.path()).is_empty());
    }

    #[tokio::test]
    async fn wrongly_typed_arguments_are_rejected() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let output = execute(
            &tools,
            "trace_path",
            ctx(&directory),
            json!({"function_name": "f", "depth": "3"}),
        )
        .await;
        let message = output.error.expect("type failure");
        assert!(message.contains("depth"), "{message}");
        assert!(message.contains("Expected integer"), "{message}");
        assert!(spawns(directory.path()).is_empty());
    }

    #[tokio::test]
    async fn get_architecture_rejects_malformed_aspects() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);

        let output = execute(
            &tools,
            "get_architecture",
            ctx(&directory),
            json!({"aspects": "layers"}),
        )
        .await;
        let message = output.error.expect("wrong-type failure");
        assert!(message.contains("aspects"), "{message}");
        assert!(
            message.contains("Expected an array of strings, got \"layers\""),
            "{message}"
        );
        assert!(spawns(directory.path()).is_empty());

        let output = execute(
            &tools,
            "get_architecture",
            ctx(&directory),
            json!({"aspects": ["ok", 5]}),
        )
        .await;
        let message = output.error.expect("non-string element failure");
        assert!(message.contains("aspects"), "{message}");
        assert!(
            message.contains("Expected an array of strings; item 1 is 5"),
            "{message}"
        );
        assert!(spawns(directory.path()).is_empty());
    }

    #[tokio::test]
    async fn pre_cancelled_tool_never_spawns() {
        let (directory, cbm) = fixture();
        let tools = all(cbm);
        let ctx = ctx(&directory);
        ctx.cancellation.cancel();
        let output = execute(
            &tools,
            "search_graph",
            ctx,
            json!({"project": "ox", "query": "Registry"}),
        )
        .await;
        assert_eq!(output.error.as_deref(), Some("command cancelled"));
        assert!(spawns(directory.path()).is_empty());
        assert!(!directory.path().join(".cbm-stub-stdin.json").exists());
    }
}
