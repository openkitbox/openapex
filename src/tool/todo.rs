//! Port of oh-my-pi `packages/coding-agent/src/tools/todo.ts`: one `todo`
//! tool whose params object **is** a single op, over a two-level
//! phase/task model.
//!
//! Design decisions (see `.opencode/plans/ox-todo-tool.md` §4.4):
//!
//! - **The `content` is the rendering.** The tool result's model-facing text
//!   is oh-my-pi's `formatSummary` output — the summary line plus the
//!   per-phase tree. opencode's TUI has no todo renderer and shows a tool's
//!   text verbatim, so that same string is what the user sees.
//! - **The `metadata` is the structure.** The same phases also travel as
//!   `ToolOutput.metadata.phases`, i.e. the `metadata` field of the
//!   `session.tool.success` event, mirroring upstream `todowrite`'s
//!   `metadata: { todos }`. That is the only channel a client can read without
//!   parsing the rendered text, and it is what the companion TUI plugin
//!   renders as a two-level tree (`.opencode/plans/ox-todo-metadata.md`). The
//!   two stay in lockstep because both are built from the same `effective`
//!   list, after the error-is-rollback rule has been applied.
//! - **State lives in the session's `kv` row** (`todo:{sessionID}`), the same
//!   store magic-context keeps its own per-session facts in. oh-my-pi reads
//!   the latest `todo` tool-result `details` back off the transcript; ox's
//!   tool output is content plus client-facing metadata, so a durable side row
//!   is the equivalent — it also survives the compaction floor that drops the
//!   tool result itself.
//! - **Every op is ported exactly**, including the error-is-rollback rule
//!   (a batch with any error commits nothing), the single-active-task
//!   invariant (`normalize_in_progress_task`), content-as-identity targeting,
//!   and the `append`-only phase creation rule.
//!
//! Deviations, all deliberate:
//! - `details` (`phases`, `storage`, `completedTasks`, `op`) is reduced to
//!   `metadata.phases`. The storage key, the op and the completion transitions
//!   stay internal: the first two are things the client already knows from the
//!   call itself, and oh-my-pi's `completedTasks` only feeds its HUD/reminders,
//!   which ox does not have.
//! - No `lenientArgValidation` repair of a missing `op`: ox validates the
//!   arguments against the advertised schema before dispatch, so a stale or
//!   flattened payload gets the ordinary schema error and a retry.
//! - No completion reminder / HUD / `/todo` command (those need the todo
//!   state hooks and `session.command`, plan §5).

use std::pin::Pin;

use serde_json::{Value, json};

use crate::cardinal::wire::ToolDef;
use crate::db::Db;
use crate::runner::{Tool, ToolContext, ToolOutput};
use crate::tool::{fail, object_param, ok_text, param};

/// Session scope id for the durable todo row live in the `kv` table.
fn storage_key(session_id: &str) -> String {
    format!("todo:{session_id}")
}

/// One task's state. `content` is the identity: every targeting op resolves a
/// task by exact string equality (oh-my-pi `findTaskByContent`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    pub blocker: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Abandoned,
    Blocked,
}

impl TodoStatus {
    fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
            TodoStatus::Abandoned => "abandoned",
            TodoStatus::Blocked => "blocked",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(TodoStatus::Pending),
            "in_progress" => Some(TodoStatus::InProgress),
            "completed" => Some(TodoStatus::Completed),
            "abandoned" => Some(TodoStatus::Abandoned),
            "blocked" => Some(TodoStatus::Blocked),
            _ => None,
        }
    }

    /// Whether the task is open work (what `formatSummary` reports as
    /// remaining and what `block` may act on).
    fn is_open(self) -> bool {
        matches!(self, TodoStatus::Pending | TodoStatus::InProgress)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TodoPhase {
    pub name: String,
    pub tasks: Vec<TodoItem>,
}

/// The nine ops (the params discriminant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Init,
    Start,
    Done,
    Drop,
    Block,
    Unblock,
    Rm,
    Append,
    View,
}

impl Op {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "init" => Some(Op::Init),
            "start" => Some(Op::Start),
            "done" => Some(Op::Done),
            "drop" => Some(Op::Drop),
            "block" => Some(Op::Block),
            "unblock" => Some(Op::Unblock),
            "rm" => Some(Op::Rm),
            "append" => Some(Op::Append),
            "view" => Some(Op::View),
            _ => None,
        }
    }
}

/// One decoded op payload. Every field is optional at this layer: op-specific
/// requirements are validated by [`apply_entry`], which reports them as
/// recoverable errors (oh-my-pi validates the same way).
#[derive(Debug, Clone, Default)]
struct Params {
    op: Option<Op>,
    list: Option<Vec<(String, Vec<String>)>>,
    task: Option<String>,
    phase: Option<String>,
    items: Option<Vec<String>>,
    reason: Option<String>,
}

pub struct TodoTool {
    db: Db,
}

impl TodoTool {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "todo",
            // Ported from oh-my-pi `prompts/tools/todo.md`: the rules the model
            // needs to use the tool correctly (content-as-identity, the
            // single-active invariant, when to init, and that a todo call must
            // ride along with real work rather than take a turn of its own).
            "Write a structured todo list to track progress within a session. Applies one \
             mutation per call and returns a text summary plus the full phase/task state.\n\n\
             `todo` NEVER takes a turn of its own: emit it in the same response as your next \
             real tool call (parallel tool calls) — `init` beside the first read or action, \
             `done`/`start` beside the next action or check. A response whose only call is \
             `todo` wastes a full model round trip.\n\
             Tasks are identified by verbatim content, NEVER generated IDs. Task and phase \
             names must be unique and stable; if you lost the text, use `view`, NEVER guess.\n\
             Before work, init for 3+ steps, requested task sets, or new instructions. List \
             EVERY user item separately; never omit leftovers.\n\
             After a successful mutation: with no active task the earliest pending task \
             starts (phase order); with several active tasks only the earliest stays. \
             `blocked` tasks never start automatically; `unblock` returns them to pending. \
             Mark tasks done immediately, in phase order.",
            object_param(
                vec![
                    (
                        "op",
                        json!({
                            "type": "string",
                            "enum": [
                                "init", "start", "done", "drop", "block",
                                "unblock", "rm", "append", "view"
                            ],
                            "description": "Operation discriminator. init replaces the whole \
                                            list; start marks one task in_progress; done / \
                                            drop / rm target a task, a phase, or everything; \
                                            block / unblock require a task or phase; append \
                                            adds tasks to a phase (creating it); view only \
                                            echoes the current list.",
                        }),
                    ),
                    (
                        "list",
                        json!({
                            "type": "array",
                            "description": "Full replacement payload for init: one entry per \
                                            phase with at least one task. An explicit empty \
                                            array clears all phases.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "phase": { "type": "string" },
                                    "items": {
                                        "type": "array",
                                        "items": { "type": "string" },
                                        "minItems": 1,
                                    },
                                },
                                "required": ["phase", "items"],
                                "additionalProperties": false,
                            },
                        }),
                    ),
                    (
                        "task",
                        param("string", "Verbatim task content (exact match)."),
                    ),
                    (
                        "phase",
                        param(
                            "string",
                            "Exact phase name. Required by append (creates the phase when \
                             missing); optional for a flat init (defaults to `Tasks`).",
                        ),
                    ),
                    (
                        "items",
                        json!({
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Tasks to append, or the full task list for a \
                                            flat init.",
                        }),
                    ),
                    (
                        "reason",
                        param("string", "Optional blocker note for block."),
                    ),
                ],
                &["op"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move {
            let params = match decode(&input) {
                Ok(params) => params,
                // A schema-level failure commits nothing, exactly like an
                // op-level error: the model retries with a valid payload.
                Err(message) => return fail(format!("Invalid todo arguments: {message}")),
            };
            let previous = match load(&self.db, &ctx.session_id).await {
                Ok(phases) => phases,
                Err(error) => {
                    tracing::error!(session = %ctx.session_id, error = %error, "load todo state failed");
                    return fail(format!("Failed to load the todo list: {error}"));
                }
            };
            run_op(&self.db, &ctx.session_id, previous, params).await
        })
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decode the arguments object. Every field is optional here; the op-specific
/// requirements are enforced (with oh-my-pi's messages) by [`apply_entry`].
fn decode(input: &Value) -> Result<Params, String> {
    let Some(object) = input.as_object() else {
        return Err("Expected an object".to_owned());
    };
    let op = match object.get("op") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => match Op::from_str(raw) {
            Some(op) => Some(op),
            None => return Err(format!("Unknown op `{raw}`")),
        },
        Some(_) => return Err("`op` must be a string".to_owned()),
    };
    let list = match object.get("list") {
        None | Some(Value::Null) => None,
        Some(Value::Array(entries)) => {
            let mut phases = Vec::with_capacity(entries.len());
            for entry in entries {
                let Some(row) = entry.as_object() else {
                    return Err("`list` entries must be objects".to_owned());
                };
                let Some(name) = row.get("phase").and_then(Value::as_str) else {
                    return Err("`list` entries need a string `phase`".to_owned());
                };
                let Some(items) = row.get("items").and_then(Value::as_array) else {
                    return Err("`list` entries need an `items` array".to_owned());
                };
                let items = items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| "`list` items must be strings".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                phases.push((name.to_owned(), items));
            }
            Some(phases)
        }
        Some(_) => return Err("`list` must be an array".to_owned()),
    };
    let task = optional_string(object, "task")?;
    let phase = optional_string(object, "phase")?;
    let reason = optional_string(object, "reason")?;
    let items = match object.get("items") {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "`items` must be strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err("`items` must be an array".to_owned()),
    };
    Ok(Params {
        op,
        list,
        task,
        phase,
        items,
        reason,
    })
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

// ---------------------------------------------------------------------------
// Durable state
// ---------------------------------------------------------------------------

/// Load the session's phases. A missing or unparseable row is an empty list:
/// a corrupt side row must not wedge the tool.
async fn load(db: &Db, session_id: &str) -> anyhow::Result<Vec<TodoPhase>> {
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare("SELECT value FROM kv WHERE key = ?1").await?;
    let Some(row) = crate::db::optional(&mut stmt, [storage_key(session_id)]).await? else {
        return Ok(Vec::new());
    };
    let raw: String = row.get(0)?;
    Ok(decode_phases(&raw))
}

/// Store the phases, replacing any previous snapshot.
async fn store(db: &Db, session_id: &str, phases: &[TodoPhase]) -> anyhow::Result<()> {
    let value = serde_json::to_string(&encode_phases(phases))?;
    let now = crate::id::now_millis();
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?3",
        (storage_key(session_id), value.as_str(), now),
    )
    .await?;
    Ok(())
}

/// Drop the session's row. Used when the list finishes: a completed list that
/// lingers makes every later `view` and every client panel report stale work.
async fn clear(db: &Db, session_id: &str) -> anyhow::Result<()> {
    let conn = db.acquire().await?;
    conn.execute("DELETE FROM kv WHERE key = ?1", [storage_key(session_id)])
        .await?;
    Ok(())
}

/// Open work for a session, as `(phase, task, status)`. The runner's completion
/// reminder reads this; it is the same durable row the tool writes, so a
/// reminder can never disagree with what the tool last reported.
pub(crate) async fn open_tasks(
    db: &Db,
    session_id: &str,
) -> anyhow::Result<Vec<(String, String, &'static str)>> {
    Ok(load(db, session_id)
        .await?
        .into_iter()
        .flat_map(|phase| {
            phase
                .tasks
                .into_iter()
                .filter(|task| task.status.is_open())
                .map(move |task| (phase.name.clone(), task.content, task.status.as_str()))
        })
        .collect())
}

/// Whether every task is closed and there was work to begin with. An empty list
/// is not "finished" — there is nothing to announce or clear.
fn is_finished(phases: &[TodoPhase]) -> bool {
    let mut any = false;
    for phase in phases {
        for task in &phase.tasks {
            any = true;
            if task.status.is_open() {
                return false;
            }
        }
    }
    any
}

fn encode_phases(phases: &[TodoPhase]) -> Value {
    Value::Array(
        phases
            .iter()
            .map(|phase| {
                json!({
                    "name": phase.name,
                    "tasks": phase.tasks.iter().map(|task| match &task.blocker {
                        Some(blocker) => json!({
                            "content": task.content,
                            "status": task.status.as_str(),
                            "blocker": blocker,
                        }),
                        None => json!({
                            "content": task.content,
                            "status": task.status.as_str(),
                        }),
                    }).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// Decode persisted phases, skipping malformed entries rather than failing the
/// whole list (the row is a cache of the model's own state).
fn decode_phases(raw: &str) -> Vec<TodoPhase> {
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let mut phases = Vec::new();
    for row in rows {
        let Some(name) = row.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(tasks) = row.get("tasks").and_then(Value::as_array) else {
            continue;
        };
        let tasks = tasks
            .iter()
            .filter_map(|task| {
                let content = task.get("content")?.as_str()?.to_owned();
                let status = TodoStatus::from_str(task.get("status")?.as_str()?)?;
                let blocker = task
                    .get("blocker")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Some(TodoItem {
                    content,
                    status,
                    blocker,
                })
            })
            .collect();
        phases.push(TodoPhase {
            name: name.to_owned(),
            tasks,
        });
    }
    phases
}

// ---------------------------------------------------------------------------
// The op itself
// ---------------------------------------------------------------------------

async fn run_op(db: &Db, session_id: &str, previous: Vec<TodoPhase>, params: Params) -> ToolOutput {
    // `view` is a pure read: no normalization, no state write. It still
    // carries `metadata`, so a client can treat it as a refresh.
    if params.op == Some(Op::View) {
        return render(&previous, &[], true, false);
    }

    let (updated, errors) = match params.op {
        Some(op) => {
            let mut working = previous.clone();
            let mut errors = Vec::new();
            apply_entry(op, &params, &mut working, &mut errors);
            normalize_in_progress_task(&mut working);
            (working, errors)
        }
        // Without an op the payload cannot be applied. oh-my-pi repairs the
        // unambiguous flattened shapes; ox's schema requires `op`, so this is
        // unreachable through the normal dispatch path.
        None => (
            previous.clone(),
            vec!["Missing op — pass one of init, start, done, drop, block, unblock, rm, append, view".to_owned()],
        ),
    };

    // Any error discards the whole batch: persisting a half-applied op would
    // make the natural retry hit "already exists" for the parts that landed.
    let failed = !errors.is_empty();
    let effective = if failed { previous } else { updated };
    if !failed {
        // A finished list is dropped rather than kept: leaving it behind makes
        // every later `view` and every client panel report work that is done.
        // The result still shows the completed tree (that is the useful
        // confirmation), but the stored state — and the next `view` — is empty.
        let finished = is_finished(&effective);
        let stored = if finished {
            clear(db, session_id).await
        } else {
            store(db, session_id, &effective).await
        };
        if let Err(error) = stored {
            tracing::error!(session = %session_id, error = %error, "save todo state failed");
            return fail(format!("Failed to save the todo list: {error}"));
        }
        // Any successful mutation re-arms the completion reminder budget
        // (upstream `todo-tracker`: touching the list resets `reminderCount`).
        crate::runner::reset_todo_reminder_budget(db, session_id).await;
        return render(&effective, &[], false, finished);
    }
    // oh-my-pi reports op errors as a successful tool result whose content
    // starts with `Errors: …` (details carry `isError` instead); ox mirrors
    // that in the content and lets the rendered summary carry the failure.
    // `effective` is the pre-op list whenever anything failed, so both the text
    // and the metadata describe the state that actually persisted.
    render(&effective, &errors, false, false)
}

/// Build the tool result: the rendered summary the model reads plus the
/// structured phases a client renders.
///
/// oh-my-pi's `details` carries `{ op, phases, storage, completedTasks }`; only
/// the list is published, under `phases`, because ox's model is two-level. The
/// wire type is a JSON *object* (`step::tool_metadata` drops anything else), so
/// the array is wrapped. `cleared` marks a list that finished and was dropped,
/// so a client can tell "everything is done" from "there was never a list".
fn render(phases: &[TodoPhase], errors: &[String], read_only: bool, cleared: bool) -> ToolOutput {
    let mut metadata = json!({ "phases": encode_phases(phases) });
    if let Some(object) = metadata.as_object_mut()
        && cleared
    {
        object.insert("cleared".to_owned(), Value::Bool(true));
    }
    ok_text(format_summary(phases, errors, read_only)).with_metadata(metadata)
}

/// Apply one op to `phases` in place, recording recoverable errors.
fn apply_entry(op: Op, params: &Params, phases: &mut Vec<TodoPhase>, errors: &mut Vec<String>) {
    match op {
        Op::Init => {
            let list = match &params.list {
                Some(list) => list.clone(),
                None => match &params.items {
                    // A flat `items` list synthesizes one phase; models send
                    // this instead of the canonical `list` shape routinely.
                    Some(items) if !items.is_empty() => vec![(
                        params.phase.clone().unwrap_or_else(|| "Tasks".to_owned()),
                        items.clone(),
                    )],
                    _ => {
                        errors.push("Missing list for init operation".to_owned());
                        return;
                    }
                },
            };
            // Duplicate names would be permanently unaddressable (targeting
            // resolves the first match), so reject them up front.
            let mut seen_phases = std::collections::HashSet::new();
            let mut seen_tasks = std::collections::HashSet::new();
            for (name, items) in &list {
                if !seen_phases.insert(name.clone()) {
                    errors.push(format!("Duplicate phase \"{name}\" in init list"));
                }
                for content in items {
                    if !seen_tasks.insert(content.clone()) {
                        errors.push(format!("Duplicate task \"{content}\" in init list"));
                    }
                }
            }
            *phases = list
                .into_iter()
                .map(|(name, items)| TodoPhase {
                    name,
                    tasks: items
                        .into_iter()
                        .map(|content| TodoItem {
                            content,
                            status: TodoStatus::Pending,
                            blocker: None,
                        })
                        .collect(),
                })
                .collect();
        }

        Op::Start => {
            let Some((phase_index, task_index)) =
                resolve_task(phases, params.task.as_deref(), errors)
            else {
                return;
            };
            for phase in phases.iter_mut() {
                for task in phase.tasks.iter_mut() {
                    if task.status == TodoStatus::InProgress {
                        task.status = TodoStatus::Pending;
                    }
                }
            }
            phases[phase_index].tasks[task_index].status = TodoStatus::InProgress;
        }

        Op::Done | Op::Drop => {
            let status = if op == Op::Done {
                TodoStatus::Completed
            } else {
                TodoStatus::Abandoned
            };
            for (phase_index, task_index) in targets(phases, params, errors) {
                phases[phase_index].tasks[task_index].status = status;
            }
        }

        Op::Block => {
            if params.task.is_none() && params.phase.is_none() {
                errors.push("block requires a task or phase target".to_owned());
                return;
            }
            // A blocker note rides one Markdown line and one summary line, so a
            // multi-line external error is collapsed to a single trimmed line.
            let reason = params
                .reason
                .as_deref()
                .map(|raw| raw.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|note| !note.is_empty());
            for (phase_index, task_index) in targets(phases, params, errors) {
                let task = &mut phases[phase_index].tasks[task_index];
                // Only actionable open work may be blocked: blocking a phase
                // must not reopen completed/abandoned tasks. An already-blocked
                // task stays eligible so a later block can refine the note.
                if !matches!(
                    task.status,
                    TodoStatus::Pending | TodoStatus::InProgress | TodoStatus::Blocked
                ) {
                    continue;
                }
                task.status = TodoStatus::Blocked;
                task.blocker = reason.clone();
            }
        }

        Op::Unblock => {
            if params.task.is_none() && params.phase.is_none() {
                errors.push("unblock requires a task or phase target".to_owned());
                return;
            }
            for (phase_index, task_index) in targets(phases, params, errors) {
                let task = &mut phases[phase_index].tasks[task_index];
                if task.status == TodoStatus::Blocked {
                    task.status = TodoStatus::Pending;
                    task.blocker = None;
                }
            }
        }

        Op::Rm => {
            if params.task.is_some() {
                if let Some((phase_index, task_index)) =
                    resolve_task(phases, params.task.as_deref(), errors)
                {
                    phases[phase_index].tasks.remove(task_index);
                }
                return;
            }
            if let Some(name) = params.phase.as_deref() {
                match phases.iter_mut().find(|phase| phase.name == name) {
                    Some(phase) => phase.tasks.clear(),
                    None => errors.push(format!("Phase \"{name}\" not found")),
                }
                return;
            }
            for phase in phases.iter_mut() {
                phase.tasks.clear();
            }
        }

        Op::Append => {
            let Some(name) = params.phase.as_deref() else {
                errors.push("Missing phase name for append operation".to_owned());
                return;
            };
            let Some(items) = params.items.as_deref() else {
                errors.push("Missing items for append operation".to_owned());
                return;
            };
            if items.is_empty() {
                errors.push("Missing items for append operation".to_owned());
                return;
            }
            // Validate the whole batch before mutating so a failing op reports
            // every duplicate and leaves nothing half-applied.
            let mut seen = std::collections::HashSet::new();
            let mut duplicate = false;
            for content in items {
                if !seen.insert(content.clone()) || find_task(phases, content).is_some() {
                    errors.push(format!("Task \"{content}\" already exists"));
                    duplicate = true;
                }
            }
            if duplicate {
                return;
            }
            // `append` is the only op that creates a missing phase.
            let phase_index = match phases.iter().position(|phase| phase.name == name) {
                Some(index) => index,
                None => {
                    phases.push(TodoPhase {
                        name: name.to_owned(),
                        tasks: Vec::new(),
                    });
                    phases.len() - 1
                }
            };
            for content in items {
                phases[phase_index].tasks.push(TodoItem {
                    content: content.clone(),
                    status: TodoStatus::Pending,
                    blocker: None,
                });
            }
        }

        Op::View => {}
    }
}

/// Resolve a task by exact content, recording oh-my-pi's error strings.
fn resolve_task(
    phases: &[TodoPhase],
    content: Option<&str>,
    errors: &mut Vec<String>,
) -> Option<(usize, usize)> {
    let Some(content) = content.filter(|text| !text.is_empty()) else {
        errors.push("Missing task content".to_owned());
        return None;
    };
    if let Some(hit) = find_task(phases, content) {
        return Some(hit);
    }
    // A generated `task-N` id is the most common wrong reference; say so.
    if is_generated_id(content) {
        errors.push(format!(
            "Task \"{content}\" not found. Tasks are referenced by content, not by IDs — \
             pass the task's full text from the previous result."
        ));
    } else {
        let total: usize = phases.iter().map(|phase| phase.tasks.len()).sum();
        let hint = if total == 0 {
            " (todo list is empty — was it replaced or not yet created?)"
        } else {
            ""
        };
        errors.push(format!("Task \"{content}\" not found{hint}"));
    }
    None
}

fn is_generated_id(content: &str) -> bool {
    content
        .strip_prefix("task-")
        .is_some_and(|tail| !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()))
}

fn find_task(phases: &[TodoPhase], content: &str) -> Option<(usize, usize)> {
    for (phase_index, phase) in phases.iter().enumerate() {
        if let Some(task_index) = phase.tasks.iter().position(|task| task.content == content) {
            return Some((phase_index, task_index));
        }
    }
    None
}

/// Every `(phase, task)` a mutating op targets: one task, one phase's tasks, or
/// everything. Errors are recorded for unresolved targets (oh-my-pi
/// `getTaskTargets`).
fn targets(phases: &[TodoPhase], params: &Params, errors: &mut Vec<String>) -> Vec<(usize, usize)> {
    if params.task.is_some() {
        return resolve_task(phases, params.task.as_deref(), errors)
            .into_iter()
            .collect();
    }
    if let Some(name) = params.phase.as_deref() {
        return match phases.iter().position(|phase| phase.name == name) {
            Some(phase_index) => (0..phases[phase_index].tasks.len())
                .map(|task_index| (phase_index, task_index))
                .collect(),
            None => {
                errors.push(format!("Phase \"{name}\" not found"));
                Vec::new()
            }
        };
    }
    phases
        .iter()
        .enumerate()
        .flat_map(|(phase_index, phase)| {
            (0..phase.tasks.len()).map(move |task_index| (phase_index, task_index))
        })
        .collect()
}

/// The single-active-task invariant: at most one `in_progress` afterwards, and
/// when none is active the first `pending` task (phase order) is promoted.
/// Blocked tasks are skipped, so a list whose open work is all blocked has no
/// active task.
fn normalize_in_progress_task(phases: &mut [TodoPhase]) {
    let mut seen_active = false;
    let mut promoted = false;
    for phase in phases.iter_mut() {
        for task in phase.tasks.iter_mut() {
            if task.status == TodoStatus::InProgress {
                if seen_active {
                    task.status = TodoStatus::Pending;
                } else {
                    seen_active = true;
                }
            }
        }
    }
    if seen_active {
        return;
    }
    for phase in phases.iter_mut() {
        for task in phase.tasks.iter_mut() {
            if task.status == TodoStatus::Pending {
                task.status = TodoStatus::InProgress;
                promoted = true;
                break;
            }
        }
        if promoted {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// oh-my-pi `formatSummary`: remaining items, an overall line, the active
/// phase, then the per-phase tree this tool's content *is*.
fn format_summary(phases: &[TodoPhase], errors: &[String], read_only: bool) -> String {
    let all: Vec<&TodoItem> = phases.iter().flat_map(|phase| phase.tasks.iter()).collect();
    if all.is_empty() {
        if !errors.is_empty() {
            return format!("Errors: {}", errors.join("; "));
        }
        return if read_only {
            "Todo list is empty.".to_owned()
        } else {
            "Todo list cleared.".to_owned()
        };
    }

    let remaining: Vec<(&str, &TodoItem)> = phases
        .iter()
        .flat_map(|phase| {
            phase
                .tasks
                .iter()
                .filter(|task| task.status.is_open())
                .map(move |task| (phase.name.as_str(), task))
        })
        .collect();

    // The active phase is the earliest holding open work; fall back to the last
    // phase when every task is closed.
    let current_index = phases
        .iter()
        .position(|phase| phase.tasks.iter().any(|task| task.status.is_open()))
        .unwrap_or_else(|| phases.len().saturating_sub(1));
    let current = &phases[current_index];
    let current_done = current
        .tasks
        .iter()
        .filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
        .count();

    let mut lines = Vec::new();
    if !errors.is_empty() {
        lines.push(format!("Errors: {}", errors.join("; ")));
    }
    if remaining.is_empty() {
        lines.push("Remaining items: none.".to_owned());
    } else {
        lines.push(format!("Remaining items ({}):", remaining.len()));
        for (phase, task) in &remaining {
            lines.push(format!(
                "  - {} [{}] ({phase})",
                task.content,
                task.status.as_str()
            ));
        }
    }

    let closed = all
        .iter()
        .filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
        .count();
    let blocked = all
        .iter()
        .filter(|task| task.status == TodoStatus::Blocked)
        .count();
    // Out-of-order completion leaves the pointer in an earlier phase than work
    // that already finished; name that case so the pointer is not read as a
    // task reverting.
    let worked_ahead = phases.iter().enumerate().any(|(index, phase)| {
        index > current_index
            && phase
                .tasks
                .iter()
                .any(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
    });
    lines.push(format!(
        "Overall: {closed}/{} done, {} open{}.",
        all.len(),
        remaining.len(),
        if blocked > 0 {
            format!(", {blocked} blocked")
        } else {
            String::new()
        }
    ));
    lines.push(format!(
        "Active phase {}/{} \"{}\" ({}/{}){}",
        current_index + 1,
        phases.len(),
        current.name,
        current_done,
        current.tasks.len(),
        if worked_ahead {
            " — earliest phase with open tasks; the in-progress pointer auto-advances to the \
             earliest open task on each completion, so it can sit behind out-of-order work \
             (nothing was un-completed)."
        } else {
            "."
        }
    ));
    for phase in phases {
        lines.push(format!("  {}:", phase.name));
        for task in &phase.tasks {
            let checkbox = if task.status == TodoStatus::Completed {
                "[X]"
            } else {
                "[ ]"
            };
            let tag = match task.status {
                TodoStatus::InProgress => " (in progress)".to_owned(),
                TodoStatus::Abandoned => " (dropped)".to_owned(),
                TodoStatus::Blocked => match &task.blocker {
                    Some(blocker) => format!(" (blocked: {blocker})"),
                    None => " (blocked)".to_owned(),
                },
                TodoStatus::Pending | TodoStatus::Completed => String::new(),
            };
            lines.push(format!("    - {checkbox} {}{tag}", task.content));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases_of(list: &[(&str, &[&str])]) -> Vec<TodoPhase> {
        list.iter()
            .map(|(name, items)| TodoPhase {
                name: (*name).to_owned(),
                tasks: items
                    .iter()
                    .map(|content| TodoItem {
                        content: (*content).to_owned(),
                        status: TodoStatus::Pending,
                        blocker: None,
                    })
                    .collect(),
            })
            .collect()
    }

    fn apply(phases: &mut Vec<TodoPhase>, params: Params) -> Vec<String> {
        let mut errors = Vec::new();
        apply_entry(params.op.expect("op"), &params, phases, &mut errors);
        normalize_in_progress_task(phases);
        errors
    }

    fn init(items: &[(&str, &[&str])]) -> Params {
        Params {
            op: Some(Op::Init),
            list: Some(
                items
                    .iter()
                    .map(|(name, list)| {
                        (
                            (*name).to_owned(),
                            list.iter().map(|s| (*s).to_owned()).collect(),
                        )
                    })
                    .collect(),
            ),
            ..Params::default()
        }
    }

    fn target(op: Op, task: &str) -> Params {
        Params {
            op: Some(op),
            task: Some(task.to_owned()),
            ..Params::default()
        }
    }

    fn statuses(phases: &[TodoPhase]) -> Vec<(&str, TodoStatus)> {
        phases
            .iter()
            .flat_map(|phase| {
                phase
                    .tasks
                    .iter()
                    .map(|task| (task.content.as_str(), task.status))
            })
            .collect()
    }

    #[test]
    fn init_promotes_the_first_pending_task() {
        let mut phases = Vec::new();
        let errors = apply(&mut phases, init(&[("Phase 1", &["a", "b"])]));
        assert!(errors.is_empty());
        assert_eq!(
            statuses(&phases),
            vec![("a", TodoStatus::InProgress), ("b", TodoStatus::Pending)],
            "normalization promotes the first pending task"
        );
    }

    #[test]
    fn init_rejects_duplicates_without_applying() {
        let mut phases = phases_of(&[("Keep", &["existing"])]);
        let errors = apply(
            &mut phases,
            init(&[("Phase", &["dup", "dup"]), ("Phase", &["other"])]),
        );
        assert_eq!(
            errors,
            vec![
                "Duplicate task \"dup\" in init list",
                "Duplicate phase \"Phase\" in init list",
            ],
            "errors follow the list order: the first entry's task duplicate is seen first"
        );
        // The failing op is discarded wholesale by the caller; the working copy
        // it built here is irrelevant, so assert on the reported errors only.
    }

    #[test]
    fn start_demotes_the_previous_active_task() {
        let mut phases = Vec::new();
        apply(&mut phases, init(&[("P", &["a", "b"])]));
        assert_eq!(statuses(&phases)[0].1, TodoStatus::InProgress);

        let errors = apply(&mut phases, target(Op::Start, "b"));
        assert!(errors.is_empty());
        assert_eq!(
            statuses(&phases),
            vec![("a", TodoStatus::Pending), ("b", TodoStatus::InProgress)]
        );
    }

    #[test]
    fn done_targets_a_phase_and_advances_the_pointer() {
        let mut phases = Vec::new();
        apply(&mut phases, init(&[("P1", &["a"]), ("P2", &["b"])]));
        let errors = apply(
            &mut phases,
            Params {
                op: Some(Op::Done),
                phase: Some("P1".to_owned()),
                ..Params::default()
            },
        );
        assert!(errors.is_empty());
        assert_eq!(
            statuses(&phases),
            vec![("a", TodoStatus::Completed), ("b", TodoStatus::InProgress)],
            "finishing P1 promotes P2's task"
        );
    }

    #[test]
    fn block_keeps_closed_tasks_and_unblock_restores_pending() {
        let mut phases = Vec::new();
        apply(&mut phases, init(&[("P", &["a", "b"])]));
        apply(&mut phases, target(Op::Done, "a"));
        let errors = apply(
            &mut phases,
            Params {
                op: Some(Op::Block),
                phase: Some("P".to_owned()),
                reason: Some("waiting  on\n  upstream".to_owned()),
                ..Params::default()
            },
        );
        assert!(errors.is_empty());
        assert_eq!(statuses(&phases)[0], ("a", TodoStatus::Completed));
        assert_eq!(statuses(&phases)[1], ("b", TodoStatus::Blocked));
        assert_eq!(
            phases[0].tasks[1].blocker.as_deref(),
            Some("waiting on upstream"),
            "whitespace is collapsed to one line"
        );

        let errors = apply(
            &mut phases,
            Params {
                op: Some(Op::Unblock),
                phase: Some("P".to_owned()),
                ..Params::default()
            },
        );
        assert!(errors.is_empty());
        assert_eq!(statuses(&phases)[1], ("b", TodoStatus::InProgress));
        assert!(phases[0].tasks[1].blocker.is_none());
    }

    #[test]
    fn append_creates_a_missing_phase_and_rejects_duplicates() {
        let mut phases = phases_of(&[("P", &["a"])]);
        let errors = apply(
            &mut phases,
            Params {
                op: Some(Op::Append),
                phase: Some("New".to_owned()),
                items: Some(vec!["b".to_owned(), "c".to_owned()]),
                ..Params::default()
            },
        );
        assert!(errors.is_empty());
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[1].name, "New");
        assert_eq!(phases[1].tasks.len(), 2);

        let errors = apply(
            &mut phases,
            Params {
                op: Some(Op::Append),
                phase: Some("New".to_owned()),
                items: Some(vec!["b".to_owned()]),
                ..Params::default()
            },
        );
        assert_eq!(errors, vec!["Task \"b\" already exists"]);
    }

    #[test]
    fn rm_clears_one_phase_or_everything() {
        let mut phases = phases_of(&[("P1", &["a"]), ("P2", &["b"])]);
        apply(
            &mut phases,
            Params {
                op: Some(Op::Rm),
                phase: Some("P1".to_owned()),
                ..Params::default()
            },
        );
        assert!(phases[0].tasks.is_empty());
        assert_eq!(phases[1].tasks.len(), 1);

        apply(
            &mut phases,
            Params {
                op: Some(Op::Rm),
                ..Params::default()
            },
        );
        assert!(phases.iter().all(|phase| phase.tasks.is_empty()));
    }

    #[test]
    fn unknown_targets_report_oh_my_pi_messages() {
        let mut phases = Vec::new();
        let errors = apply(&mut phases, target(Op::Done, "task-1"));
        assert!(errors[0].contains("Tasks are referenced by content, not by IDs"));

        let errors = apply(&mut phases, target(Op::Done, "ghost"));
        assert_eq!(
            errors,
            vec![
                "Task \"ghost\" not found (todo list is empty — was it replaced or not yet created?)"
            ]
        );

        let errors = apply(
            &mut phases,
            Params {
                op: Some(Op::Done),
                phase: Some("Nope".to_owned()),
                ..Params::default()
            },
        );
        assert_eq!(errors, vec!["Phase \"Nope\" not found"]);
    }

    #[test]
    fn block_and_unblock_require_a_target() {
        let mut phases = Vec::new();
        let errors = apply(
            &mut phases,
            Params {
                op: Some(Op::Block),
                ..Params::default()
            },
        );
        assert_eq!(errors, vec!["block requires a task or phase target"]);
        let errors = apply(
            &mut phases,
            Params {
                op: Some(Op::Unblock),
                ..Params::default()
            },
        );
        assert_eq!(errors, vec!["unblock requires a task or phase target"]);
    }

    #[test]
    fn summary_renders_the_tree_and_counts() {
        let mut phases = Vec::new();
        apply(&mut phases, init(&[("P1", &["a", "b"]), ("P2", &["c"])]));
        apply(&mut phases, target(Op::Done, "a"));
        apply(
            &mut phases,
            Params {
                op: Some(Op::Block),
                task: Some("c".to_owned()),
                reason: Some("external".to_owned()),
                ..Params::default()
            },
        );
        let summary = format_summary(&phases, &[], false);
        assert!(
            summary.contains("Remaining items (1):"),
            "summary: {summary}"
        );
        assert!(summary.contains("  - b [in_progress] (P1)"));
        assert!(
            !summary.contains("  - c [blocked]"),
            "blocked tasks are not open work: {summary}"
        );
        assert!(summary.contains("Overall: 1/3 done, 1 open, 1 blocked."));
        assert!(summary.contains("Active phase 1/2 \"P1\" (1/2)."));
        assert!(summary.contains("    - [X] a"));
        assert!(summary.contains("    - [ ] c (blocked: external)"));
    }

    #[test]
    fn empty_summary_distinguishes_view_from_mutation() {
        assert_eq!(format_summary(&[], &[], true), "Todo list is empty.");
        assert_eq!(format_summary(&[], &[], false), "Todo list cleared.");
        assert_eq!(
            format_summary(&[], &["boom".to_owned()], false),
            "Errors: boom"
        );
    }

    #[test]
    fn encode_decode_round_trips_blockers() {
        let phases = phases_of(&[("P", &["a", "b"])]);
        let mut phases = phases;
        phases[0].tasks[1].status = TodoStatus::Blocked;
        phases[0].tasks[1].blocker = Some("note".to_owned());
        let encoded = serde_json::to_string(&encode_phases(&phases)).unwrap();
        assert_eq!(decode_phases(&encoded), phases);
        assert_eq!(
            decode_phases("not json"),
            Vec::new(),
            "a corrupt row degrades to an empty list"
        );
    }

    // -----------------------------------------------------------------------
    // End-to-end through the tool seam (real Db, real persisted row)
    // -----------------------------------------------------------------------

    fn context(session_id: &str) -> ToolContext {
        ToolContext {
            session_id: session_id.to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_1".to_owned(),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    async fn run(tool: &TodoTool, session_id: &str, input: Value) -> ToolOutput {
        tool.execute(context(session_id), input).await
    }

    #[tokio::test]
    async fn state_persists_across_calls_and_is_session_scoped() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db.clone());

        let output = run(
            &tool,
            "ses_a",
            json!({"op": "init", "items": ["read the code", "write the fix"]}),
        )
        .await;
        assert!(output.error.is_none(), "init must succeed");
        let text = output.content.as_str().expect("text content").to_owned();
        assert!(text.contains("- [X]") || text.contains("- [ ]"), "{text}");
        assert!(text.contains("read the code"), "{text}");

        // A fresh call sees the stored list: `start` resolves by content.
        let output = run(
            &tool,
            "ses_a",
            json!({"op": "start", "task": "write the fix"}),
        )
        .await;
        assert!(output.error.is_none());
        let text = output.content.as_str().unwrap();
        assert!(text.contains("write the fix [in_progress]"), "{text}");

        // Another session starts empty.
        let output = run(&tool, "ses_b", json!({"op": "view"})).await;
        assert_eq!(output.content, json!("Todo list is empty."));
    }

    #[tokio::test]
    async fn failed_op_commits_nothing() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db.clone());

        run(&tool, "ses", json!({"op": "init", "items": ["a"]})).await;
        // `done` on an unknown task leaves the list untouched.
        let output = run(&tool, "ses", json!({"op": "done", "task": "ghost"})).await;
        let text = output.content.as_str().unwrap();
        assert!(
            text.starts_with("Errors: Task \"ghost\" not found"),
            "{text}"
        );
        assert!(text.contains("    - [ ] a (in progress)"), "{text}");

        // The stored row is unchanged, not the failed op's working copy.
        let output = run(&tool, "ses", json!({"op": "view"})).await;
        assert!(
            output
                .content
                .as_str()
                .unwrap()
                .contains("    - [ ] a (in progress)")
        );
    }

    #[tokio::test]
    async fn malformed_arguments_fail_without_touching_state() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db.clone());
        run(&tool, "ses", json!({"op": "init", "items": ["a"]})).await;

        let output = run(&tool, "ses", json!({"op": "explode"})).await;
        assert_eq!(
            output.error.as_deref(),
            Some("Invalid todo arguments: Unknown op `explode`")
        );
        let output = run(
            &tool,
            "ses",
            json!({"op": "init", "list": [{"phase": "P"}]}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some("Invalid todo arguments: `list` entries need an `items` array")
        );
        let output = run(&tool, "ses", json!({"op": "view"})).await;
        assert!(
            output
                .content
                .as_str()
                .unwrap()
                .contains("    - [ ] a (in progress)")
        );
    }

    #[tokio::test]
    async fn rendered_content_is_the_markdown_tree() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db);
        let output = run(
            &tool,
            "ses",
            json!({"op": "init", "list": [
                {"phase": "Investigate", "items": ["read", "grep"]},
                {"phase": "Fix", "items": ["edit"]}
            ]}),
        )
        .await;
        let text = output.content.as_str().unwrap();
        // The tree the TUI's generic tool card renders verbatim.
        assert!(text.contains("  Investigate:"), "{text}");
        assert!(text.contains("    - [ ] read (in progress)"), "{text}");
        assert!(text.contains("    - [ ] grep"), "{text}");
        assert!(text.contains("  Fix:"), "{text}");
        assert!(text.contains("    - [ ] edit"), "{text}");
        assert!(text.contains("Overall: 0/3 done, 3 open."), "{text}");
    }

    // -----------------------------------------------------------------------
    // The metadata channel a client renders instead of parsing the text
    // -----------------------------------------------------------------------

    /// The `phases` array of a result's metadata, or a panic naming the shape
    /// the projector requires (an object, see `step::tool_metadata`).
    fn phases_of_output(output: &ToolOutput) -> &Vec<Value> {
        let metadata = output
            .metadata
            .as_ref()
            .expect("every todo result carries metadata");
        metadata
            .get("phases")
            .and_then(Value::as_array)
            .expect("metadata.phases must be an array")
    }

    #[tokio::test]
    async fn metadata_carries_the_same_tree_as_the_content() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db);
        let output = run(
            &tool,
            "ses",
            json!({"op": "init", "list": [
                {"phase": "Investigate", "items": ["read", "grep"]},
                {"phase": "Fix", "items": ["edit"]}
            ]}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);

        let phases = phases_of_output(&output);
        assert_eq!(phases.len(), 2, "{phases:?}");
        assert_eq!(phases[0]["name"], json!("Investigate"));
        assert_eq!(
            phases[0]["tasks"],
            json!([
                {"content": "read", "status": "in_progress"},
                {"content": "grep", "status": "pending"}
            ]),
            "the single-active-task invariant lands in metadata too"
        );
        assert_eq!(phases[1]["name"], json!("Fix"));
        assert_eq!(
            phases[1]["tasks"],
            json!([{"content": "edit", "status": "pending"}])
        );
    }

    #[tokio::test]
    async fn metadata_tracks_blocked_tasks_with_their_note() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db);
        run(&tool, "ses", json!({"op": "init", "items": ["a", "b"]})).await;
        let output = run(
            &tool,
            "ses",
            json!({"op": "block", "task": "b", "reason": "waiting on upstream"}),
        )
        .await;
        assert!(output.error.is_none());
        let phases = phases_of_output(&output);
        let tasks = phases[0]["tasks"].as_array().unwrap();
        // `b` was the active task; blocking it releases the pointer to `a`.
        assert_eq!(
            tasks[1],
            json!({
                "content": "b",
                "status": "blocked",
                "blocker": "waiting on upstream"
            })
        );
    }

    #[tokio::test]
    async fn failed_op_metadata_reflects_the_rollback() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db);
        run(&tool, "ses", json!({"op": "init", "items": ["a"]})).await;
        // `done` on an unknown task discards the batch, so metadata must still
        // describe the pre-op list — the same state the text shows.
        let output = run(&tool, "ses", json!({"op": "done", "task": "ghost"})).await;
        assert!(output.content.as_str().unwrap().starts_with("Errors:"));
        let phases = phases_of_output(&output);
        assert_eq!(
            phases[0]["tasks"],
            json!([{"content": "a", "status": "in_progress"}]),
            "a failed op publishes the state that persisted, not the working copy"
        );
    }

    #[tokio::test]
    async fn view_carries_metadata_so_a_client_can_refresh() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db);
        run(&tool, "ses", json!({"op": "init", "items": ["a"]})).await;
        let output = run(&tool, "ses", json!({"op": "view"})).await;
        let text = output.content.as_str().expect("view content");
        assert!(text.starts_with("Remaining items"), "{text}");
        let phases = phases_of_output(&output);
        assert_eq!(
            phases[0]["tasks"],
            json!([{"content": "a", "status": "in_progress"}])
        );

        // An empty list is still a well-formed object: `{ "phases": [] }`.
        let output = run(&tool, "other", json!({"op": "view"})).await;
        assert!(phases_of_output(&output).is_empty());
    }

    // -----------------------------------------------------------------------
    // A finished list is cleared, and says so
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn completing_the_last_task_clears_the_stored_list() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db.clone());
        run(&tool, "ses", json!({"op": "init", "items": ["a", "b"]})).await;

        // Closing one of two leaves the list in place.
        let output = run(&tool, "ses", json!({"op": "done", "task": "a"})).await;
        assert!(output.error.is_none());
        assert_eq!(phases_of_output(&output).len(), 1);
        assert!(
            output.metadata.as_ref().unwrap().get("cleared").is_none(),
            "an unfinished list is not marked cleared"
        );
        assert!(
            run(&tool, "ses", json!({"op": "view"})).await.content != json!("Todo list is empty.")
        );

        // Closing the last one reports the finished tree and drops the row.
        let output = run(&tool, "ses", json!({"op": "done", "task": "b"})).await;
        assert!(output.error.is_none());
        assert_eq!(
            output.metadata.as_ref().unwrap().get("cleared"),
            Some(&Value::Bool(true)),
            "the result announces the clear"
        );
        let text = output.content.as_str().unwrap();
        assert!(text.contains("Overall: 2/2 done"), "{text}");

        // The stored state is gone: `view` reports an empty list.
        let output = run(&tool, "ses", json!({"op": "view"})).await;
        assert_eq!(output.content, json!("Todo list is empty."));
        assert!(phases_of_output(&output).is_empty());
    }

    #[tokio::test]
    async fn dropping_the_last_task_also_clears() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db);
        run(&tool, "ses", json!({"op": "init", "items": ["a"]})).await;
        let output = run(&tool, "ses", json!({"op": "drop", "task": "a"})).await;
        assert!(output.error.is_none());
        assert_eq!(
            output.metadata.as_ref().unwrap().get("cleared"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            run(&tool, "ses", json!({"op": "view"})).await.content,
            json!("Todo list is empty.")
        );
    }

    #[tokio::test]
    async fn a_finished_list_is_not_reported_as_cleared_on_failure() {
        // A failed op never reaches the clear path, even when the pre-op list
        // happened to be finished (it is dropped, so this is the empty case).
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db);
        let output = run(&tool, "ses", json!({"op": "done", "task": "ghost"})).await;
        assert!(
            output.metadata.as_ref().unwrap().get("cleared").is_none(),
            "a rollback is not a clear"
        );
        assert!(output.content.as_str().unwrap().starts_with("Errors:"));
    }

    // -----------------------------------------------------------------------
    // Open tasks, as the completion reminder reads them
    // -----------------------------------------------------------------------

    // A cleared list must not swallow the next one: the same session keeps
    // working, inits a fresh list, and the new list is what clients show.
    #[tokio::test]
    async fn a_new_list_after_a_cleared_one_is_stored_again() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db.clone());

        // First list, finished -> cleared.
        run(&tool, "ses", json!({"op": "init", "items": ["a"]})).await;
        let output = run(&tool, "ses", json!({"op": "done", "task": "a"})).await;
        assert_eq!(
            output.metadata.as_ref().unwrap().get("cleared"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            run(&tool, "ses", json!({"op": "view"})).await.content,
            json!("Todo list is empty.")
        );

        // A second list in the same session starts from nothing and is stored.
        let output = run(
            &tool,
            "ses",
            json!({"op": "init", "list": [{"phase": "Next", "items": ["x", "y"]}]}),
        )
        .await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert!(
            output.metadata.as_ref().unwrap().get("cleared").is_none(),
            "a fresh list is not marked cleared"
        );
        assert_eq!(
            phases_of_output(&output),
            &vec![json!({
                "name": "Next",
                "tasks": [
                    {"content": "x", "status": "in_progress"},
                    {"content": "y", "status": "pending"}
                ]
            })]
        );

        // And it is durable: a later call sees it.
        let output = run(&tool, "ses", json!({"op": "done", "task": "x"})).await;
        assert!(output.error.is_none());
        let phases = phases_of_output(&output);
        assert_eq!(phases[0]["tasks"][0]["status"], json!("completed"));
        assert_eq!(phases[0]["tasks"][1]["status"], json!("in_progress"));
    }

    #[tokio::test]
    async fn open_tasks_reports_only_unfinished_work() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db.clone());
        run(
            &tool,
            "ses",
            json!({"op": "init", "list": [
                {"phase": "One", "items": ["a", "b"]},
                {"phase": "Two", "items": ["c"]}
            ]}),
        )
        .await;
        run(&tool, "ses", json!({"op": "done", "task": "a"})).await;
        run(&tool, "ses", json!({"op": "drop", "task": "c"})).await;

        let open = super::open_tasks(&db, "ses").await.unwrap();
        assert_eq!(
            open,
            vec![("One".to_owned(), "b".to_owned(), "in_progress")]
        );
    }

    #[tokio::test]
    async fn open_tasks_is_empty_once_everything_closes() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoTool::new(db.clone());
        run(&tool, "ses", json!({"op": "init", "items": ["a"]})).await;
        run(&tool, "ses", json!({"op": "done", "task": "a"})).await;

        // The finished list was cleared, so there is nothing to report.
        assert!(super::open_tasks(&db, "ses").await.unwrap().is_empty());
    }
}
