use std::collections::BTreeMap;

use anyhow::{Context as _, Result};

use ox_db::Db;

use super::text::head_chars;

/// The classify task's prefixed completion prompt. The output is a strict JSON
/// object because the only effect is a numeric column: there is no manifest to
/// apply, no text to store, and nothing the model may write itself.
pub(super) const CLASSIFY_SYSTEM_PROMPT: &str = "You are a memory classifier for the magic-context system. You score a project's memories by importance.

IMPORTANCE is how much the fact deserves a permanent slot in every future request's prefix:
- 90-100: a rule the agent must never violate (a hard constraint, a safety rule, a stated directive).
- 60-89: an architectural fact or a hard-won gotcha that prevents real mistakes.
- 30-59: a useful convention, a config value, a path.
- 1-29: background that is nice to know.
- 0: trivia, restated context, or something the code already says.

Judge the fact itself, not how recently it was written, and use the whole range — a pool where everything scores 80 tells the host nothing.

Output ONE JSON object at the very end and NOTHING else — no narration, no reasoning:
{ \"12\": 90, \"13\": 20 }

Rules:
- Every id listed below appears exactly once, with a number between 0 and 100.
- Ids you were not given are ignored.";

/// Most memories one `classify` completion carries. Larger than the verify
/// batch because a score is one number per row and the prompt is the whole
/// cost: forty short facts still fit comfortably.
const CLASSIFY_BATCH: usize = 40;

/// Characters of one memory the classifier sees. A memory is already one fact;
/// the cap is a cost guard against a single verbose row eating the batch.
const CLASSIFY_CONTENT_CHARS: usize = 400;

// ---------------------------------------------------------------------------
// classify: the host-side gate
// ---------------------------------------------------------------------------

/// One unclassified memory as the classify task reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClassifyItem {
    id: String,
    category: String,
    content: String,
}

/// How many active memories of this project have no score yet. The cheap gate:
/// a fully classified pool costs one `count(*)` per tick and no model call.
pub(super) async fn unclassified_count(db: &Db, project: &str) -> Result<i64> {
    let context = format!("count unclassified memories for project {project}");
    let result: Result<i64> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT count(*) FROM mc_memory
                  WHERE project = ?1 AND status = 'active' AND importance = 0",
            )
            .await?;
        let row = stmt.query_row([project]).await?;
        Ok(row.get(0)?)
    }
    .await;
    result.with_context(|| context)
}

/// The memories one `classify` completion covers: the lowest ids first, so a
/// pool larger than the batch is scored in stable, resumable slices.
pub(super) async fn unclassified_batch(db: &Db, project: &str) -> Result<Vec<ClassifyItem>> {
    let context = format!("read unclassified memories for project {project}");
    let result: Result<Vec<ClassifyItem>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, category, content FROM mc_memory
                  WHERE project = ?1 AND status = 'active' AND importance = 0
                  ORDER BY CAST(id AS INTEGER)
                  LIMIT ?2",
            )
            .await?;
        let mut rows = stmt.query((project, CLASSIFY_BATCH as i64)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(ClassifyItem {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
            });
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// The `classify` user prompt: the batch, one handle per memory.
pub(super) fn render_classify_prompt(project: &str, batch: &[ClassifyItem]) -> String {
    let mut parts = vec![format!(
        "## Project: {project}\n\nThese memories have no importance score yet. Score every one of them.\n"
    )];
    for item in batch {
        parts.push(format!(
            "#{} [{}]\n{}",
            item.id,
            item.category,
            head_chars(&item.content, CLASSIFY_CONTENT_CHARS)
        ));
    }
    parts.join("\n")
}

/// Parse the `{"<id>": <number>}` manifest.
///
/// Strict where it matters and forgiving where it costs nothing: prose around
/// the object is tolerated (the first `{` to the last `}` is the manifest), but
/// every id of the batch must be scored — a partial answer is the shape of a
/// truncated response, and applying it would silently leave the rest at `0`
/// while banking a success. Ids outside the batch are dropped with a warning
/// (the reference drops unknown manifest ids the same way).
pub(super) fn parse_classify_manifest(
    text: &str,
    batch: &[ClassifyItem],
) -> Result<BTreeMap<String, f64>, String> {
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err("classify manifest carried no JSON object".to_owned());
    };
    if end < start {
        return Err("classify manifest carried no complete JSON object".to_owned());
    }
    let raw: BTreeMap<String, f64> = serde_json::from_str(&text[start..=end]).map_err(|error| {
        format!("classify manifest is not a JSON object of ids to numbers: {error}")
    })?;
    let mut manifest: BTreeMap<String, f64> = BTreeMap::new();
    let mut unknown = 0usize;
    for (id, score) in raw {
        if !batch.iter().any(|item| item.id == id) {
            unknown += 1;
            continue;
        }
        manifest.insert(id, clamp_importance(score));
    }
    if unknown > 0 {
        tracing::warn!(
            unknown,
            "classify manifest named memories outside the batch; dropped"
        );
    }
    for item in batch {
        if !manifest.contains_key(&item.id) {
            return Err(format!(
                "classify manifest is missing memory #{}; rejecting a partial manifest",
                item.id
            ));
        }
    }
    Ok(manifest)
}

/// A model score forced into the stored range. A non-finite value is a broken
/// answer, and `0` (unscored) is the safe reading: it leaves the row for the
/// next run instead of pinning it to the top of the selection.
fn clamp_importance(score: f64) -> f64 {
    if score.is_finite() {
        score.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// Apply a parsed manifest: one transaction, one `importance` write per id,
/// each guarded by `active` so an archived row cannot be scored through a
/// stale batch.
pub(super) async fn apply_classify_manifest(
    db: &Db,
    project: &str,
    manifest: &BTreeMap<String, f64>,
) -> Result<usize> {
    let context = format!("apply classify manifest for project {project}");
    let result: Result<usize> = async {
        let tx = db.begin().await?;
        let mut applied = 0;
        {
            let mut stmt = tx
                .prepare(
                    "UPDATE mc_memory SET importance = ?2
                      WHERE project = ?3 AND id = ?1 AND status = 'active'",
                )
                .await?;
            for (id, importance) in manifest {
                applied += stmt.execute((id.as_str(), *importance, project)).await? as usize;
            }
        }
        tx.commit().await?;
        Ok(applied)
    }
    .await;
    result.with_context(|| context)
}
