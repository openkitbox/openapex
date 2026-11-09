use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use regex::Regex;

use ox_db::Db;

use super::text::head_chars;

/// The surface-notes task's prefixed completion prompt. The answer is a list of
/// ids and nothing else: a note's text is the model's own writing from an
/// earlier turn, so the only decision here is "is the condition satisfied", and
/// nothing the model says about a note may be stored.
pub(super) const SURFACE_SYSTEM_PROMPT: &str = "You are a note-surfacing checker for the magic-context system. Each note below was parked with a condition that must hold before the note is worth showing again. You read the user's own recent turns from this project and decide which conditions hold NOW.

Rules:
1. A condition holds only on evidence in the turns below: something the user did, said, asked for, or finished. Do not infer progress from the note's own text, and do not surface a note because its topic came up.
2. A condition that is still ahead of the work, or that the turns cannot decide, does not hold. Leaving a note parked costs nothing; surfacing it early intrudes on work that is not ready for it.
3. A condition is a plain-language statement about state (\"after the schema migration lands\", \"when the user asks about pricing\"). Judge it as written.

Output ONE element at the very end for each satisfied note and NOTHING else — no narration, no per-note commentary:
<ready id=\"7\"/>
<ready id=\"9\"/>

Or, when no condition holds, emit no <ready> element at all — a one-line answer saying so is fine.";

/// Most parked notes one `surface_notes` completion carries. A note is a
/// paragraph rather than a fact, so the batch is half the classify batch.
const SURFACE_BATCH: usize = 20;

/// Characters of one note's body the checker sees. The condition is what is
/// judged; the body only says what would be surfaced.
const SURFACE_BODY_CHARS: usize = 300;

/// Characters of one user turn the checker sees: the same cap the retrospective
/// uses, for the same reason (a correction is a sentence; a paste is not).
const SURFACE_TURN_CHARS: usize = 600;

/// Directories whose pending notes have a condition to evaluate, independently
/// of whether the project has ever produced a memory.
pub(super) async fn projects_with_parked_notes(db: &Db) -> Result<Vec<String>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT s.directory FROM mc_note n
               JOIN session_v2 s ON s.id = n.session_id
              WHERE n.status = 'pending'
                AND n.surface_condition IS NOT NULL AND n.surface_condition <> ''
              ORDER BY s.directory",
        )
        .await?;
    let mut rows = stmt.query(()).await?;
    let mut owners = Vec::new();
    while let Some(row) = rows.next().await? {
        owners.push(row.get(0)?);
    }
    Ok(owners)
}

// ---------------------------------------------------------------------------
// surface_notes
// ---------------------------------------------------------------------------

/// One parked smart note as the checker reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SurfaceNote {
    /// `#N` handle body.
    pub(super) id: String,
    title: String,
    body: String,
    /// The condition the note was parked on, verbatim.
    condition: String,
}

/// The cheap gate: how many of this project's notes are parked on a usable
/// condition. A project with none costs one `count(*)` per tick and no model
/// call (`dreamer/task-gates.ts`).
///
/// The project key is the session directory (that is what the dreamer
/// enumerates by), so this joins the notes to their sessions. An empty
/// condition is not a condition: `write_note` drops a blank one, and a row that
/// somehow carries one has nothing to judge.
pub(super) async fn parked_note_count(db: &Db, project: &str) -> Result<i64> {
    let context = format!("count parked notes for project {project}");
    let result: Result<i64> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT count(*) FROM mc_note n
                   JOIN session_v2 s ON s.id = n.session_id
                  WHERE s.directory = ?1 AND n.status = 'pending'
                    AND n.surface_condition IS NOT NULL AND n.surface_condition <> ''",
            )
            .await?;
        let row = stmt.query_row([project]).await?;
        Ok(row.get(0)?)
    }
    .await;
    result.with_context(|| context)
}

/// The parked notes one `surface_notes` completion covers: the lowest handles
/// first, so a backlog larger than the batch is swept in stable, resumable
/// slices. `session_id` is deliberately not selected — handles are unique across
/// the table and the apply is guarded by id alone.
pub(super) async fn parked_notes(db: &Db, project: &str) -> Result<Vec<SurfaceNote>> {
    let context = format!("read parked notes for project {project}");
    let result: Result<Vec<SurfaceNote>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT n.id, n.title, n.body, n.surface_condition FROM mc_note n
                   JOIN session_v2 s ON s.id = n.session_id
                  WHERE s.directory = ?1 AND n.status = 'pending'
                    AND n.surface_condition IS NOT NULL AND n.surface_condition <> ''
                  ORDER BY CAST(n.id AS INTEGER)
                  LIMIT ?2",
            )
            .await?;
        let mut rows = stmt.query((project, SURFACE_BATCH as i64)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(SurfaceNote {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                condition: row.get(3)?,
            });
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// The `surface_notes` user prompt: the parked notes, then the user's turns.
///
/// `turns` arrives newest-first (the query's order) and the prompt reads them in
/// the order they happened, so a condition about "after X" is read against a
/// transcript that runs forwards.
pub(super) fn render_surface_prompt(
    project: &str,
    batch: &[SurfaceNote],
    turns: &[String],
) -> String {
    let mut parts = vec![format!(
        "## Project: {project}\n\nThese notes are parked on a condition. Decide which conditions hold NOW, on the evidence of the user's own turns below.\n\n<notes>"
    )];
    for note in batch {
        parts.push(format!("<note id=\"{}\">", note.id));
        parts.push(format!("<condition>{}</condition>", note.condition));
        parts.push(format!("<title>{}</title>", note.title));
        let body = head_chars(note.body.trim(), SURFACE_BODY_CHARS);
        if !body.is_empty() {
            parts.push(body.to_owned());
        }
        parts.push("</note>".to_owned());
    }
    parts.push("</notes>".to_owned());
    parts.push("<turns>".to_owned());
    for turn in turns.iter().rev() {
        parts.push(format!(
            "<turn>{}</turn>",
            head_chars(turn.trim(), SURFACE_TURN_CHARS)
        ));
    }
    parts.push("</turns>".to_owned());
    parts.join("\n")
}

/// Parse the `<ready id="N"/>` list.
///
/// The tolerant half mirrors the other manifests (prose around the elements is
/// ignored, ids outside the batch are dropped with a warning) and adds one
/// concession the answer invites: `id='7'` is accepted as well as `id="7"`,
/// because a one-attribute list is exactly the shape a model switches quoting
/// on. Duplicates collapse — a note is either surfaced or not.
///
/// The strict half is fail-closed: a `<ready>` element the host cannot address,
/// or an unclosed `<ready` (truncation), rejects the whole answer instead of
/// surfacing a guessed subset. No `<ready` at all is the contract's "no
/// condition holds".
pub(super) fn parse_ready_ids(text: &str, batch_ids: &[&str]) -> Result<Vec<String>, String> {
    // A `<ready` opener without a complete element is a truncated answer: the
    // ids that did arrive would be a prefix of the verdict, never the verdict.
    if ready_open_regex().find_iter(text).count() > ready_element_regex().find_iter(text).count() {
        return Err("surface answer carries an unclosed <ready> element".to_owned());
    }
    let mut ids: Vec<String> = Vec::new();
    let mut unknown = 0usize;
    let mut unaddressed = false;
    for captures in ready_element_regex().captures_iter(text) {
        let attrs = captures.get(1).map_or("", |group| group.as_str());
        let Some(id) = ready_id_attr(attrs) else {
            unaddressed = true;
            continue;
        };
        if !batch_ids.contains(&id.as_str()) {
            unknown += 1;
            continue;
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if unaddressed {
        return Err("surface answer carries a <ready> element without an id".to_owned());
    }
    if unknown > 0 {
        tracing::warn!(
            unknown,
            "surface answer named notes outside the batch; dropped"
        );
    }
    Ok(ids)
}

/// The `id` attribute of one `<ready>` element, in either quote style. `None`
/// covers a missing attribute and an empty value: neither addresses a note.
fn ready_id_attr(attrs: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // A clone drops its lazy-DFA cache with this call (see
    // `historian_validate.rs`'s regex section).
    let re = RE
        .get_or_init(|| {
            Regex::new(r#"(?i)\bid\s*=\s*(?:"([^"]*)"|'([^']*)')"#).expect("ready id attr regex")
        })
        .clone();
    let captures = re.captures(attrs)?;
    let value = captures.get(1).or_else(|| captures.get(2))?.as_str().trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Apply a surface verdict: one transaction, one status flip per id, each
/// guarded by `pending` so a note that was dismissed, edited into `active` or
/// surfaced by an earlier run cannot be flipped through a stale batch.
///
/// `at` is the run's start (see [`crate::mctx::dreamer`]'s watermark rules); it
/// becomes the row's `updated_at`, which is what the `ctx_note read` page ages.
pub(super) async fn apply_surfaced(db: &Db, ids: &[String], at: i64) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let context = format!("surface {} notes", ids.len());
    let result: Result<usize> = async {
        let tx = db.begin().await?;
        let mut applied = 0;
        {
            let mut stmt = tx
                .prepare(
                    "UPDATE mc_note SET status = 'ready', updated_at = ?2
                      WHERE id = ?1 AND status = 'pending'",
                )
                .await?;
            for id in ids {
                applied += stmt.execute((id.as_str(), at)).await? as usize;
            }
        }
        tx.commit().await?;
        Ok(applied)
    }
    .await;
    result.with_context(|| context)
}

// Regex accessors return a clone so the lazy-DFA cache is dropped after the
// call instead of living on per worker thread (see `historian_validate.rs`).
/// The `<ready` opener (any of `/>`, `>`, or a truncated element).
fn ready_open_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<ready\b").expect("ready open regex"))
        .clone()
}

/// One complete `<ready …>` element's attribute string.
fn ready_element_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<ready\b([^>]*?)/?>").expect("ready element regex"))
        .clone()
}
