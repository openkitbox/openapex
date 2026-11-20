use std::future::Future;
use std::pin::Pin;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::runner::{Tool, ToolContext, ToolOutput};
use ox_db::Db;
use ox_schema::id::now_millis;
use ox_schema::wire::ToolDef;
use ox_tools::{object_param, ok_text};

use super::ctx_memory_tool::plural;
use super::notes::{
    NoteFilter, NoteRow, dismiss_notes, get_notes, humanize_age, list_notes, update_note,
    write_note,
};
use super::tool_args::{internal_error, optional_text, parse_int_ids, required_text};
use super::{MAX_NOTE_PAGE, NOTE_ID_LIMIT, NOTE_PAGE, STALE_DAYS};

/// `ctx_note` — session-scoped pending intentions (see the module docs).
pub struct CtxNoteTool {
    db: Db,
}

impl CtxNoteTool {
    /// The tool over `db` (the `mc_note` table).
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for CtxNoteTool {
    fn name(&self) -> &str {
        "ctx_note"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(crate::mctx::CTX_GUIDANCE)
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "ctx_note",
            "Session notes are pending intentions: work you intend to return to, with its \
             findings attached. Use them for a finding to revisit when you return to the \
             intended work; a decision with its reasoning, when follow-up work remains; a \
             backlog item with evidence already found; or something the user explicitly \
             asks you to note. Do not use them for: the next few steps; a plan you are \
             actively executing; restart/fold insurance; a record of how things stand \
             (world-state, a design at a point in time) with nothing you intend to do \
             about it; a fact worth keeping is memory, the rest is nothing. First line is \
             the title (under 80 chars), followed by detail. If the detail already lives \
             in a file, record the path and what to inspect — don't copy the file into a \
             note. Durable project facts belong in ctx_memory, not notes.",
            object_param(
                vec![
                    (
                        "action",
                        json!({
                            "type": "string",
                            "enum": ["write", "read", "update", "dismiss"],
                            "description": "write: save a new note (content required). \
                                            read: one row per note — `#id · age · title` — \
                                            newest first; rows \
                                            untouched 30+ days are marked stale. Pass \
                                            note_ids to read full bodies; limit/offset \
                                            page; filter selects other statuses. update: \
                                            change one note (note_ids=[N]). dismiss: \
                                            retire 1-50 notes (note_ids).",
                        }),
                    ),
                    (
                        "content",
                        json!({
                            "type": "string",
                            "description": "Note text for write/update: first line is the \
                                            title (under 80 chars), then the detail.",
                        }),
                    ),
                    (
                        "note_ids",
                        json!({
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Note ids: one for update, 1-50 for read full \
                                            bodies or dismiss.",
                        }),
                    ),
                    (
                        "surface_condition",
                        json!({
                            "type": "string",
                            "description": "A condition that must hold before the note is \
                                            worth surfacing. The note is parked as pending and \
                                            a background task re-checks the condition against \
                                            this project's recent user turns; once it holds, \
                                            the note becomes ready and appears in the session \
                                            pinboard without being asked for. Omit it for an \
                                            ordinary note you read yourself.",
                        }),
                    ),
                    (
                        "filter",
                        json!({
                            "type": "string",
                            "enum": ["all", "active", "pending", "ready", "dismissed"],
                            "description": "Which notes read returns (default: active, parked \
                                            and surfaced smart notes).",
                        }),
                    ),
                    (
                        "limit",
                        json!({ "type": "integer", "description": "Rows per read (default 25, max 200)." }),
                    ),
                    (
                        "offset",
                        json!({ "type": "integer", "description": "Skip this many newest rows (default 0)." }),
                    ),
                ],
                &["action"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_note(&self.db, &ctx, &input).await })
    }
}

async fn run_note(db: &Db, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let Some(action) = input.get("action").and_then(Value::as_str) else {
        return ok_text("Error: 'action' must be provided.");
    };
    let session = ctx.session_id.as_str();

    match action {
        "write" => {
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            let condition = match optional_text(input, "surface_condition") {
                Ok(condition) => condition,
                Err(message) => return ok_text(message),
            };
            match write_note(db, session, &content, condition.as_deref()).await {
                Ok(id) => {
                    let summary = match active_summary(db, session).await {
                        Ok(summary) => summary,
                        Err(error) => {
                            tracing::warn!(error = %error, "note summary after write failed");
                            String::new()
                        }
                    };
                    ok_text(format!("Saved session note #{id}.{summary}"))
                }
                Err(error) => internal_error("note write", &error),
            }
        }
        "read" => {
            let filter = match NoteFilter::parse(input.get("filter").and_then(Value::as_str)) {
                Ok(filter) => filter,
                Err(message) => return ok_text(message),
            };
            let ids = match input.get("note_ids") {
                None | Some(Value::Null) => None,
                other => match parse_int_ids(other, "note_ids", NOTE_ID_LIMIT) {
                    Ok(ids) => Some(ids),
                    Err(message) => return ok_text(message),
                },
            };
            let now = now_millis();
            match ids {
                // Full bodies: the page arguments are not read for this
                // branch, so a bad `limit` cannot reject a valid read.
                Some(ids) => match get_notes(db, session, &ids).await {
                    Ok(rows) if rows.is_empty() => ok_text("Error: no such note."),
                    Ok(rows) => {
                        let lines: Vec<String> =
                            rows.iter().map(|row| full_note(row, now)).collect();
                        ok_text(lines.join("\n\n"))
                    }
                    Err(error) => internal_error("note read", &error),
                },
                // The `#id · age · title` page.
                None => {
                    let limit = match parse_page(input.get("limit"), "limit", NOTE_PAGE) {
                        Ok(limit) => limit,
                        Err(message) => return ok_text(message),
                    };
                    let offset = match parse_page(input.get("offset"), "offset", 0) {
                        Ok(offset) => offset,
                        Err(message) => return ok_text(message),
                    };
                    match list_notes(db, session, filter, limit, offset).await {
                        Ok(rows) if rows.is_empty() => ok_text("No notes."),
                        Ok(rows) => {
                            let lines: Vec<String> =
                                rows.iter().map(|row| note_row_line(row, now)).collect();
                            ok_text(lines.join("\n"))
                        }
                        Err(error) => internal_error("note read", &error),
                    }
                }
            }
        }
        "update" => {
            let ids = match parse_int_ids(input.get("note_ids"), "note_ids", NOTE_ID_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            let [id] = ids.as_slice() else {
                return ok_text("Error: 'note_ids' must carry exactly one id for update.");
            };
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            match update_note(db, session, id, &content).await {
                Ok(true) => ok_text(format!("Updated note #{id}.")),
                Ok(false) => ok_text(format!("Error: no note #{id}.")),
                Err(error) => internal_error("note update", &error),
            }
        }
        "dismiss" => {
            let ids = match parse_int_ids(input.get("note_ids"), "note_ids", NOTE_ID_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            match dismiss_notes(db, session, &ids).await {
                Ok(0) => ok_text("Error: no such note."),
                Ok(moved) => ok_text(format!("Dismissed {}.", plural(moved, "note", "notes"))),
                Err(error) => internal_error("note dismiss", &error),
            }
        }
        other => ok_text(format!(
            "Error: unknown action '{other}' (write, read, update, dismiss)."
        )),
    }
}

/// An optional non-negative integer page argument, clamped to
/// [`MAX_NOTE_PAGE`]. A huge value would otherwise reach SQLite as a negative
/// LIMIT, which SQLite reads as "no limit".
fn parse_page(value: Option<&Value>, field: &str, default: usize) -> Result<usize, String> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) => Ok(usize::try_from(value)
                .unwrap_or(usize::MAX)
                .min(MAX_NOTE_PAGE)),
            None => Err(format!("Error: '{field}' must be a non-negative integer.")),
        },
        Some(other) => Err(format!(
            "Error: '{field}' must be a non-negative integer (got {other})."
        )),
    }
}

/// `#12 · 3d · title` (+ ` (stale)` past [`STALE_DAYS`]).
fn note_row_line(row: &NoteRow, now: i64) -> String {
    let age = humanize_age(now, row.touched_at);
    let stale = if (now - row.touched_at) / 86_400_000 >= STALE_DAYS {
        " (stale)"
    } else {
        ""
    };
    let marker = if row.status == "pending" {
        " [parked]"
    } else {
        ""
    };
    format!("#{} · {age} · {}{marker}{stale}", row.id, row.title)
}

/// Full body view for `read` with `note_ids`.
fn full_note(row: &NoteRow, now: i64) -> String {
    let age = humanize_age(now, row.touched_at);
    let mut out = format!("#{} [{}] ({age})\n{}", row.id, row.status, row.title);
    if !row.body.is_empty() {
        out.push_str("\n\n");
        out.push_str(&row.body);
    }
    out
}

/// ` 3 active, oldest 3d.` — the write confirmation's tail; empty when the
/// session has no active notes. "Oldest" is the earliest last touch, matching
/// how rows are aged everywhere else.
async fn active_summary(db: &Db, session_id: &str) -> Result<String> {
    let result: Result<(i64, Option<i64>)> = async {
        let conn = db.acquire().await?;
        // An aggregate without GROUP BY always yields exactly one row.
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(*), MIN(COALESCE(updated_at, created_at)) FROM mc_note
                 WHERE session_id = ?1 AND status = 'active'",
            )
            .await?;
        let row = stmt.query_row([session_id]).await?;
        Ok((row.get(0)?, row.get(1)?))
    }
    .await;
    let (count, oldest) = result.context("active note summary")?;
    Ok(match oldest {
        Some(oldest) if count > 0 => {
            format!(
                " {count} active, oldest {}.",
                humanize_age(now_millis(), oldest)
            )
        }
        _ => String::new(),
    })
}
