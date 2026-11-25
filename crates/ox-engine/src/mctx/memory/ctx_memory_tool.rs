use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::mctx::redact;
use crate::runner::{Tool, ToolContext, ToolOutput};
use ox_db::Db;
use ox_schema::id::now_millis;
use ox_schema::wire::ToolDef;
use ox_tools::{object_param, ok_text};

use super::memories::{
    archive_memories, get_memories, merge_memories, record_use, store_embedding, update_memory,
    write_memory,
};
use super::tool_args::{internal_error, optional_text, parse_int_ids, required_text};
use super::{
    CATEGORIES, MEMORY_GET_LIMIT, MEMORY_MUTATE_LIMIT, USER_CATEGORIES, is_known_category,
};

/// `ctx_memory` — the project pinboard (see the module docs).
pub struct CtxMemoryTool {
    db: Db,
    /// `None` when semantic search is off: writes then skip embedding.
    embedder: Option<Arc<crate::mctx::embed::Embedder>>,
}

impl CtxMemoryTool {
    /// The tool over `db` (the `mc_memory` table).
    #[must_use]
    pub fn new(db: Db, embedder: Option<Arc<crate::mctx::embed::Embedder>>) -> Self {
        Self { db, embedder }
    }
}

impl Tool for CtxMemoryTool {
    fn name(&self) -> &str {
        "ctx_memory"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(crate::mctx::CTX_GUIDANCE)
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "ctx_memory",
            "Durable facts about this project, shared with every agent working on it and \
             kept for the months this work lasts. Your active memories are already in \
             <project-memory> as `#id: fact` lines. Write one when you learn something \
             that must not have to be found again — a project rule, an architectural \
             fact, a hard-won constraint, a config value, a naming convention — and \
             especially when it cost you turns to find. One standalone fact per memory, \
             phrased to make sense on its own. A pending intention with its evidence \
             (\"do X later, here is what we know\") is ctx_note, not memory. A fact about \
             the user rather than the project — how they like to work, what they keep \
             correcting — is category USER_PROFILE: it follows them into every project \
             (<user-profile>), so never put project specifics there.",
            object_param(
                vec![
                    (
                        "action",
                        json!({
                            "type": "string",
                            "enum": ["write", "update", "archive", "merge", "get"],
                            "description": "write: new memory (content + category). \
                                            update: rewrite one memory whose fact changed \
                                            (ids: [one], content; category optional to \
                                            recategorize). archive: retire wrong or \
                                            obsolete memories (ids: [one or more], \
                                            optional reason). merge: collapse duplicates \
                                            into one active memory (ids: [two or more] \
                                            active, content; the lowest id survives and \
                                            the rest are archived). get: fetch by id \
                                            (ids: 1-20), readable in every status.",
                        }),
                    ),
                    (
                        "content",
                        json!({
                            "type": "string",
                            "description": "The fact text (write, update, merge).",
                        }),
                    ),
                    (
                        "category",
                        json!({
                            "type": "string",
                            "enum": tool_categories(),
                            "description": "Kind of fact (required for write; optional on \
                                            update/merge to recategorize within the same \
                                            owner: a project fact stays a project fact, \
                                            a USER_PROFILE memory stays one).",
                        }),
                    ),
                    (
                        "ids",
                        json!({
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Memory ids from a `#id: fact` line or a \
                                            `<project-memory>` / `<user-profile>` entry.",
                        }),
                    ),
                    (
                        "reason",
                        json!({
                            "type": "string",
                            "description": "Why the memory is being archived (optional, \
                                            archive).",
                        }),
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
        Box::pin(async move { run_memory(&self.db, self.embedder.as_deref(), &ctx, &input).await })
    }
}

async fn run_memory(
    db: &Db,
    embedder: Option<&crate::mctx::embed::Embedder>,
    ctx: &ToolContext,
    input: &Value,
) -> ToolOutput {
    let Some(action) = input.get("action").and_then(Value::as_str) else {
        return ok_text("Error: 'action' must be provided.");
    };
    // The project key is the working directory the client sent: every
    // session of the same project shares its pinboard.
    let project = ctx.directory.to_string_lossy().into_owned();

    match action {
        "write" => {
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            let category = match required_category(input) {
                Ok(category) => category,
                Err(message) => return ok_text(message),
            };
            let content = redact::redact_secrets(&content);
            match write_memory(db, &project, &content, &category).await {
                Ok(id) => {
                    embed_inline(db, embedder, &id, &content).await;
                    ok_text(format!(
                        "Wrote memory #{id}{}.",
                        redaction_note(matches!(content, Cow::Owned(_)))
                    ))
                }
                Err(error) => internal_error("memory write", &error),
            }
        }
        "update" => {
            let ids = match parse_int_ids(input.get("ids"), "ids", MEMORY_MUTATE_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            let [id] = ids.as_slice() else {
                return ok_text("Error: 'ids' must carry exactly one id for update.");
            };
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            let category = match optional_category(input) {
                Ok(category) => category,
                Err(message) => return ok_text(message),
            };
            let content = redact::redact_secrets(&content);
            match update_memory(db, &project, id, &content, category.as_deref()).await {
                Ok(true) => {
                    embed_inline(db, embedder, id, &content).await;
                    ok_text(format!(
                        "Updated memory #{id}{}.",
                        redaction_note(matches!(content, Cow::Owned(_)))
                    ))
                }
                Ok(false) => ok_text(format!("Error: no memory #{id}.")),
                Err(error) => internal_error("memory update", &error),
            }
        }
        "archive" => {
            let ids = match parse_int_ids(input.get("ids"), "ids", MEMORY_MUTATE_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            if has_duplicates(&ids) {
                return ok_text("Error: 'ids' must not repeat an id.");
            }
            let reason = match optional_text(input, "reason") {
                Ok(reason) => reason,
                Err(message) => return ok_text(message),
            };
            match archive_memories(db, &project, &ids, reason.as_deref()).await {
                Ok(archived) if archived.is_empty() => ok_text("Error: no such active memory."),
                Ok(archived) => ok_text(format!(
                    "Archived {}: {}.",
                    plural(archived.len(), "memory", "memories"),
                    handles(&archived)
                )),
                Err(error) => internal_error("memory archive", &error),
            }
        }
        "merge" => {
            let ids = match parse_int_ids(input.get("ids"), "ids", MEMORY_MUTATE_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            if ids.len() < 2 {
                return ok_text("Error: 'ids' must carry at least two ids for merge.");
            }
            if has_duplicates(&ids) {
                return ok_text("Error: 'ids' must not repeat an id.");
            }
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            let category = match optional_category(input) {
                Ok(category) => category,
                Err(message) => return ok_text(message),
            };
            let content = redact::redact_secrets(&content);
            match merge_memories(db, &project, &ids, &content, category.as_deref()).await {
                Ok(Some(merged)) => {
                    embed_inline(db, embedder, &merged.survivor, &content).await;
                    ok_text(format!(
                        "Merged {} into #{}{}.",
                        handles(&merged.sources),
                        merged.survivor,
                        redaction_note(matches!(content, Cow::Owned(_)))
                    ))
                }
                Ok(None) => ok_text("Error: merge needs at least two active memories among 'ids'."),
                Err(error) => internal_error("memory merge", &error),
            }
        }
        "get" => {
            let ids = match parse_int_ids(input.get("ids"), "ids", MEMORY_GET_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            match get_memories(db, &project, &ids).await {
                Ok(rows) if rows.is_empty() => ok_text("Error: no such memory."),
                Ok(rows) => {
                    // Usage is a by-product: a failed write must not fail the read.
                    let used: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
                    if let Err(error) = record_use(db, &project, &used, now_millis()).await {
                        tracing::warn!(error = %error, "memory get: record use failed");
                    }
                    let lines: Vec<String> = rows
                        .iter()
                        .map(
                            |row| match (row.status.as_str(), row.superseded_by.as_deref()) {
                                ("archived", Some(replacement)) => format!(
                                    "#{} [{} {} -> #{}]\n{}",
                                    row.id, row.category, row.status, replacement, row.content
                                ),
                                _ => format!(
                                    "#{} [{} {}]\n{}",
                                    row.id, row.category, row.status, row.content
                                ),
                            },
                        )
                        .collect();
                    ok_text(lines.join("\n\n"))
                }
                Err(error) => internal_error("memory get", &error),
            }
        }
        other => ok_text(format!(
            "Error: unknown action '{other}' (write, update, archive, merge, get)."
        )),
    }
}

/// The success-message suffix telling the model its text was redacted, so it
/// does not retry a write whose secret came back as `[REDACTED_SECRET]`.
/// `redacted` is whether [`redact::redact_secrets`] returned `Cow::Owned`.
fn redaction_note(redacted: bool) -> &'static str {
    if redacted { " (secrets redacted)" } else { "" }
}

/// Embed a just-written fact and store its vector.
///
/// Best effort: a failed call logs and leaves the row without a vector, which
/// the background embedding sweep ([`embed_missing`]) picks up. A write must
/// never fail because the embedding endpoint is down.
async fn embed_inline(
    db: &Db,
    embedder: Option<&crate::mctx::embed::Embedder>,
    id: &str,
    content: &str,
) {
    let Some(embedder) = embedder else {
        return;
    };
    match embedder.embed_documents(&[content]).await {
        Ok(mut vectors) => {
            if let Some(vector) = vectors.pop()
                && let Err(error) =
                    store_embedding(db, id, content, embedder.identity(), &vector).await
            {
                tracing::warn!(memory = %id, error = %error, "store memory embedding failed");
            }
        }
        Err(error) => {
            tracing::warn!(memory = %id, error = %error, "embed memory failed; the embedding sweep backfills it");
        }
    }
}

/// Every category the tool accepts: the project set, then the user set.
fn tool_categories() -> Vec<&'static str> {
    CATEGORIES.iter().chain(&USER_CATEGORIES).copied().collect()
}

/// One category check for every arm: the closed sets live in [`CATEGORIES`]
/// and [`USER_CATEGORIES`], and the rejection message is written once.
fn check_category(category: &str) -> Result<(), String> {
    if is_known_category(category) {
        Ok(())
    } else {
        Err(format!(
            "Error: 'category' must be one of {}.",
            tool_categories().join(", ")
        ))
    }
}

/// A required `category` argument, validated.
fn required_category(input: &Value) -> Result<String, String> {
    let category = required_text(input, "category")?;
    check_category(&category)?;
    Ok(category)
}

/// An optional (omitted ⇒ `None`) `category` argument, validated when given.
fn optional_category(input: &Value) -> Result<Option<String>, String> {
    let category = optional_text(input, "category")?;
    if let Some(category) = category.as_deref() {
        check_category(category)?;
    }
    Ok(category)
}

/// Whether the same id appears twice (merge rejects repeats: merging an id
/// into itself is meaningless, and the reference reports `DuplicateSourceId`).
fn has_duplicates(ids: &[String]) -> bool {
    let mut seen = std::collections::HashSet::new();
    ids.iter().any(|id| !seen.insert(id))
}

/// `#a, #b, #c` from handle bodies.
fn handles(ids: &[String]) -> String {
    ids.iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `1 note` / `3 notes` (matching the module's other reporting).
pub(super) fn plural(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}
