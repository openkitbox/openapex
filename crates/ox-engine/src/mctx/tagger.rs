//! §N§ part tagging, drop queueing and original-content recovery.
//!
//! Every taggable item in the live session — assistant text/reasoning/tool
//! parts and user prompt text — carries a `§N§` tag. Tags are per-session,
//! minted in arrival order, and STABLE across passes: the same item keeps
//! its tag forever. The agent stamps items it no longer needs with
//! `ctx_reduce`; the marks are QUEUED (`mc_drop` rows) and only take effect
//! when a later pass rebuilds the cached prefix. Recovery stays possible via
//! `ctx_expand`, which reads the ORIGINAL content back from the raw
//! `session_message` store — nothing is ever lost.
//!
//! Module layout: the tag identity store, the drop queue and the request-side
//! overlay live here together (one module owns the whole tag lifecycle: mint →
//! render → queue → drain → recover); the `ctx_reduce`/`ctx_expand` tool shells
//! are in [`crate::mctx::tools`], and [`apply_overlay`] is the request-assembly
//! half — it renders the tag beside each item and substitutes an applied drop's
//! content with its placeholder.
//!
//! ## `part_path` — the stable addressing scheme
//!
//! A tag addresses an item inside a `session_message` row's `data` JSON
//! (the decoded message minus `id`/`type`, per
//! [`ox_schema::message::Message::to_row_data`]). The grammar is
//! minimal and closed — only two shapes are ever minted:
//!
//! - `"text"` — the user message's prompt text (`data.text`; upstream
//!   `Prompt` is flattened into the message object).
//! - `"content[N]"` — one assistant `text`/`reasoning` content part
//!   (`data.content[N]`).
//! - `"tool:{id}"` — one assistant tool part, addressed by its stable part
//!   `id` (the content array is rewritten whole when a tool state settles,
//!   so positional paths can shift and would silently retag unrelated
//!   parts); parts without an `id` fall back to `"content[N]"`. A tool
//!   part recovers its serialized `state` object (input + output
//!   content), which is also what its digest covers.
//!
//! Other message types (system/synthetic/shell/…) are not tagged for now;
//! `taggable_items` is the single place to extend.
//!
//! ## Drop lifecycle (the `state` column)
//!
//! `mc_drop` rows are never deleted. `queued` = ctx_reduce accepted the
//! mark; `applied` = [`mark_drops_applied`] committed it after the render
//! pass that consumed it succeeded (Task 5 peeks with
//! [`peek_queued_drops`] into `RenderedPass.queued`; it lands when the
//! next bust pass rebuilds the prefix). Keeping the rows lets `ctx_expand`
//! report whether content is currently dropped, and re-reducing an applied
//! tag is rejected — the content already left the live array.

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use serde_json::Value;

use ox_core::copied_tags::strip_copied_tag_notation;
use ox_db::{Db, optional};
use ox_schema::id::now_millis;

/// One tagged item: the tag handle plus where the content lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedItem {
    /// The `§N§` number shown to the model. Stable across passes.
    pub tag: u64,
    /// The `session_message` row id backing the item.
    pub message_id: String,
    /// Where the content sits inside the row's `data` JSON (see the module
    /// docs for the addressing grammar).
    pub part_path: String,
}

/// A drop row's lifecycle state (the `mc_drop.state` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropState {
    /// ctx_reduce accepted the mark; not yet consumed by a render pass.
    Queued,
    /// Handed to the render pipeline; the content left the live array (or
    /// will, at the next bust pass).
    Applied,
    /// An unrecognized `state` value (forward compat): not queued, but its
    /// lifecycle is unknown — reported verbatim instead of reading as live.
    Unknown,
}

/// Which half of the overlay a caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMode {
    /// The request side: prepend every tag and replace applied drops, so the
    /// model can address what it reads.
    Request,
    /// The read side (session API): replace applied drops only. A human reads
    /// the same transcript the model does, without `§N§` on every part.
    Transcript,
}

/// What a tag's drop row says, or `None` when nothing is queued on it.
fn drop_state_of(raw: Option<String>) -> Option<DropState> {
    match raw.as_deref() {
        Some("queued") => Some(DropState::Queued),
        Some("applied") => Some(DropState::Applied),
        Some(_) => Some(DropState::Unknown),
        None => None,
    }
}

/// The §N§ tagger. Stateless: the tag counter and the identity mapping are
/// derived from the durable `mc_tag` table on every `assign` (restart-safe;
/// ox's one-actor-per-session model makes the max-derivation race-free).
#[derive(Debug, Clone, Copy, Default)]
pub struct Tagger;

impl Tagger {
    /// Walk `messages` (the session's decoded message JSONs, in arrival /
    /// seq order) and return the full §N§ mapping for the pass. Items seen
    /// in a previous pass KEEP their tag (`mc_tag` is the source of truth);
    /// only previously-unseen items mint fresh tags. A tagged item whose
    /// content changed in place keeps its tag too — its digest is refreshed
    /// (the tag addresses the slot, not the bytes).
    ///
    /// Messages without an `id` field are skipped (they cannot be addressed
    /// in the store); non-taggable types yield nothing.
    pub async fn assign(
        &self,
        db: &Db,
        session_id: &str,
        messages: &[Value],
    ) -> Result<Vec<TaggedItem>> {
        // Extract only the taggable items before entering the async body —
        // cloning the whole decoded history per pass is wasted work on long
        // sessions.
        let items: Vec<(String, Vec<(String, String)>)> = messages
            .iter()
            .filter_map(|message| {
                let id = message.get("id").and_then(Value::as_str)?.to_owned();
                let taggables = taggable_items(message);
                (!taggables.is_empty()).then_some((id, taggables))
            })
            .collect();
        let result: Result<Vec<TaggedItem>> = async {
            let conn = db.acquire().await?;
            // Existing identity mapping for the session, keyed by
            // (message_id, part_path) → (tag, digest).
            let mut existing: HashMap<(String, String), (u64, String)> = HashMap::new();
            {
                let mut stmt = conn
                    .prepare(
                        "SELECT tag, message_id, part_path, digest
                         FROM mc_tag WHERE session_id = ?1",
                    )
                    .await?;
                let mut rows = stmt.query([session_id]).await?;
                while let Some(row) = rows.next().await? {
                    // Tags are small monotonic counters; the i64
                    // narrowing matches the SQLite column type.
                    let (tag, message_id, part_path, digest) = (
                        row.get::<i64>(0)?,
                        row.get::<String>(1)?,
                        row.get::<String>(2)?,
                        row.get::<String>(3)?,
                    );
                    existing.insert((message_id, part_path), (tag as u64, digest));
                }
            }
            // Durable counter: one past the session's highest tag.
            let mut next: u64 = {
                let mut stmt = conn
                    .prepare("SELECT COALESCE(MAX(tag), 0) FROM mc_tag WHERE session_id = ?1")
                    .await?;
                stmt.query_row([session_id]).await?.get::<i64>(0)? as u64
            };
            next += 1;

            let mut out = Vec::new();
            for (message_id, taggables) in &items {
                for (part_path, content) in taggables {
                    let content_digest = digest_hex(content);
                    let key = (message_id.clone(), part_path.clone());
                    if let Some(&(tag, ref stored)) = existing.get(&key) {
                        if *stored != content_digest {
                            conn.execute(
                                "UPDATE mc_tag SET digest = ?3
                                     WHERE session_id = ?1 AND tag = ?2",
                                (session_id, tag as i64, content_digest),
                            )
                            .await?;
                        }
                        out.push(TaggedItem {
                            tag,
                            message_id: message_id.clone(),
                            part_path: part_path.clone(),
                        });
                        continue;
                    }
                    let tag = next;
                    next += 1;
                    conn.execute(
                        "INSERT INTO mc_tag (session_id, tag, message_id, part_path, digest)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                        (
                            session_id,
                            tag as i64,
                            message_id.as_str(),
                            part_path.as_str(),
                            content_digest.as_str(),
                        ),
                    )
                    .await?;
                    out.push(TaggedItem {
                        tag,
                        message_id: message_id.clone(),
                        part_path: part_path.clone(),
                    });
                }
            }
            Ok(out)
        }
        .await;
        result.with_context(|| format!("assign mctx tags for session {session_id}"))
    }
}

/// The taggable items of one decoded message: `(part_path, content)` pairs
/// in part order. Assistant text/reasoning parts contribute their `text`;
/// tool parts contribute their serialized `state` (input + output);
/// user messages contribute their prompt text. [`crate::mctx::search`] reuses
/// this to render a transcript hit from the same text the tagger tags.
pub(crate) fn taggable_items(message: &Value) -> Vec<(String, String)> {
    let mut items = Vec::new();
    match message.get("type").and_then(Value::as_str) {
        Some("user") => {
            if let Some(text) = message.get("text").and_then(Value::as_str) {
                items.push(("text".to_owned(), text.to_owned()));
            }
        }
        Some("assistant") => {
            if let Some(parts) = message.get("content").and_then(Value::as_array) {
                for (index, part) in parts.iter().enumerate() {
                    let Some(content) = part_content(part) else {
                        continue;
                    };
                    items.push((part_path(part, index), content));
                }
            }
        }
        _ => {}
    }
    items
}

/// The `part_path` of one assistant content part. Tool parts are addressed
/// by their stable part `id` when present — the row's content array is
/// rewritten whole when a tool state settles, so array indexes can shift
/// and would silently retag unrelated parts. Parts without an `id` fall
/// back to the positional path.
fn part_path(part: &Value, index: usize) -> String {
    match part.get("type").and_then(Value::as_str) {
        Some("tool") => part
            .get("id")
            .and_then(Value::as_str)
            .map(|id| format!("tool:{id}"))
            .unwrap_or_else(|| format!("content[{index}]")),
        _ => format!("content[{index}]"),
    }
}

/// The taggable content string of one assistant content part, or `None`
/// when the part is not taggable (unknown type, missing `text`/`state`).
fn part_content(part: &Value) -> Option<String> {
    match part.get("type").and_then(Value::as_str) {
        Some("text") | Some("reasoning") => Some(part.get("text")?.as_str()?.to_owned()),
        Some("tool") => {
            let state = part.get("state")?;
            if !state.is_object() {
                return None;
            }
            Some(serde_json::to_string(state).ok()?)
        }
        _ => None,
    }
}

/// FNV-1a 64-bit of `content`, hex — a cheap stable fingerprint whose only
/// consumer is in-place-change detection on re-tag. Not cryptographic: a
/// collision at worst skips one digest refresh (the tag still addresses
/// the same slot), and a cross-version hash change at worst triggers one
/// spurious refresh per row.
fn digest_hex(content: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Every tag a session has *applied a drop on*, with the tag's addressing.
///
/// [`Tagger::assign`]'s read half without the minting: a consumer that must not
/// write (the session read API) still needs the `part_path` addressing to find
/// what an applied drop covers. The join is what keeps the read path off the
/// full tag table — a long session mints thousands of tags per poll interval,
/// and only the dropped handful is ever rendered. Rows for messages that no
/// longer exist are harmless — [`apply_overlay`] walks the live messages and
/// skips unknown ids.
async fn applied_tags(db: &Db, session_id: &str) -> Result<Vec<TaggedItem>> {
    let result: Result<Vec<TaggedItem>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT t.tag, t.message_id, t.part_path FROM mc_tag t
                   JOIN mc_drop d ON d.session_id = t.session_id AND d.tag = t.tag
                  WHERE t.session_id = ?1 AND d.state = 'applied'",
            )
            .await?;
        let mut rows = stmt.query([session_id]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(TaggedItem {
                tag: row.get::<i64>(0)? as u64,
                message_id: row.get::<String>(1)?,
                part_path: row.get::<String>(2)?,
            });
        }
        Ok(out)
    }
    .await;
    result.context("load applied mctx tags")
}

/// Replace the content of every *applied* drop with its placeholder, leaving
/// tags alone: the transcript a human reads then matches the transcript the
/// model gets.
///
/// Read-only (no minting, no writes), so it is safe on the session API's read
/// path. A session with nothing dropped costs one indexed join and no walk.
pub async fn apply_transcript_overlay(
    db: &Db,
    session_id: &str,
    messages: &mut [Value],
) -> Result<()> {
    let tags = applied_tags(db, session_id).await?;
    if tags.is_empty() {
        return Ok(());
    }
    let drops: HashMap<u64, DropState> = tags
        .iter()
        .map(|item| (item.tag, DropState::Applied))
        .collect();
    apply_overlay(messages, &tags, &drops, OverlayMode::Transcript);
    Ok(())
}

/// Every drop row for a session: tag → lifecycle state.
pub async fn drops(db: &Db, session_id: &str) -> Result<HashMap<u64, DropState>> {
    let result: Result<HashMap<u64, DropState>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT tag, state FROM mc_drop WHERE session_id = ?1")
            .await?;
        let mut rows = stmt.query([session_id]).await?;
        let mut out = HashMap::new();
        while let Some(row) = rows.next().await? {
            let tag = row.get::<i64>(0)?;
            let state = row.get::<String>(1)?;
            if let Some(state) = drop_state_of(Some(state)) {
                out.insert(tag as u64, state);
            }
        }
        Ok(out)
    }
    .await;
    result.context("load mctx drops")
}

/// The placeholder that stands in for a dropped item's content.
fn dropped_placeholder(tag: u64) -> String {
    format!("[dropped §{tag}§]")
}

/// Apply the §N§ overlay to decoded message JSONs, in place — the request-side
/// half of the tag lifecycle.
///
/// Every taggable item gets its tag prepended (`§N§ text`), so the model can
/// address it with `ctx_reduce`. An item under an APPLIED drop has its content
/// replaced by the placeholder instead: the content left the live array and
/// `ctx_expand` is the way back. A QUEUED drop changes nothing here — stamping
/// only queues, the item stays fully readable until the mark materializes.
///
/// This is the inverse of [`taggable_items`]; the two must stay in step (each
/// `part_path` shape is resolved by both).
///
/// ## Deviations from the reference
///
/// - The reference strips line-leading `§N§` imitations from an assistant
///   completion before prepending the official tag (models echo the notation).
///   ox strips at the persistence boundary instead: the projector's
///   `TextEnded` / `ReasoningEnded` handlers run
///   [`crate::mctx::copied_tags::strip_copied_tag_notation`] before a part is
///   written, so stored transcripts never carry the copies. Only legacy rows
///   written before that change can still render as `§5§ §1§ …`.
/// - The reference reduces a dropped tool CALL to a skeleton carrying
///   `{"dropped": …}`. ox keeps the call's real arguments and reduces only the
///   result — the same shape as the reference's recency window (`SkeletonReal`),
///   which is also what keeps the tool chain readable.
pub fn apply_overlay(
    messages: &mut [Value],
    tags: &[TaggedItem],
    drops: &HashMap<u64, DropState>,
    mode: OverlayMode,
) {
    // Group by message so each row is walked once.
    let mut by_message: HashMap<&str, Vec<&TaggedItem>> = HashMap::new();
    for item in tags {
        by_message
            .entry(item.message_id.as_str())
            .or_default()
            .push(item);
    }
    for message in messages.iter_mut() {
        let Some(id) = message.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(items) = by_message.get(id) else {
            continue;
        };
        for item in items {
            let dropped = matches!(drops.get(&item.tag), Some(DropState::Applied));
            if !dropped && mode == OverlayMode::Transcript {
                continue;
            }
            let placeholder = dropped.then(|| dropped_placeholder(item.tag));
            let is_assistant_text = mode == OverlayMode::Request
                && message.get("type").and_then(Value::as_str) == Some("assistant");
            let Some(part) = resolve_part(message, &item.part_path) else {
                continue;
            };
            match part {
                Addressed::Text(text) => {
                    // Request-side strip (upstream OC2's hook point): copied
                    // §N§ runs in assistant text — including rows persisted
                    // before the storage-side strip — must not keep flowing
                    // back into the model's context, where they reinforce the
                    // copying loop. Runs BEFORE the official prefix is
                    // prepended, so the addressing notation itself survives;
                    // user and tool content stay verbatim.
                    if is_assistant_text {
                        *text = strip_copied_tag_notation(text).into_owned();
                    }
                    *text = match placeholder {
                        Some(placeholder) => placeholder,
                        None => format!("§{}§ {text}", item.tag),
                    };
                }
                Addressed::ToolState(state) => {
                    if let Some(placeholder) = placeholder {
                        // Keep the call's arguments (the reference keeps the call
                        // with real arguments and reduces only the result), and
                        // keep the outcome: a failed tool did not succeed merely
                        // because its result was dropped, and the human reading
                        // the transcript must see the same failure the model did.
                        let errored = state.get("status").and_then(Value::as_str) == Some("error");
                        let mut reduced = serde_json::json!({
                            "status": if errored { "error" } else { "completed" },
                            "input": state.get("input").cloned().unwrap_or(Value::Null),
                            "content": [ { "type": "text", "text": placeholder } ],
                        });
                        if errored {
                            reduced["error"] = state.get("error").cloned().unwrap_or(Value::Null);
                        }
                        // Whatever the provider attached (timing, exit code) does
                        // not describe the result any more, but it is not ours to
                        // throw away when it costs nothing to keep.
                        if let Some(metadata) = state.get("metadata") {
                            reduced["metadata"] = metadata.clone();
                        }
                        *state = reduced;
                        continue;
                    }
                    // Tools are tagged on their serialized state; a readable
                    // prefix on the result text is what the model needs.
                    let Some(slot) = state
                        .get_mut("content")
                        .and_then(Value::as_array_mut)
                        .and_then(|content| content.first_mut())
                        .and_then(|part| part.get_mut("text"))
                    else {
                        continue;
                    };
                    let Some(text) = slot.as_str() else { continue };
                    let tagged = format!("§{}§ {text}", item.tag);
                    *slot = Value::String(tagged);
                }
            }
        }
    }
}

/// A mutable handle to the text an item's `part_path` addresses.
enum Addressed<'a> {
    /// A plain text slot (`data.text`, or a `text`/`reasoning` part's `text`).
    Text(&'a mut String),
    /// A tool part's `state` object.
    ToolState(&'a mut Value),
}

/// Resolve a `part_path` against one decoded message (the set-side inverse of
/// [`taggable_items`]).
fn resolve_part<'a>(message: &'a mut Value, part_path: &str) -> Option<Addressed<'a>> {
    if part_path == "text" {
        return message.get_mut("text").and_then(|value| match value {
            Value::String(text) => Some(Addressed::Text(text)),
            _ => None,
        });
    }
    let parts = message.get_mut("content").and_then(Value::as_array_mut)?;
    if let Some(id) = part_path.strip_prefix("tool:") {
        let part = parts
            .iter_mut()
            .find(|part| part.get("id").and_then(Value::as_str) == Some(id))?;
        return part.get_mut("state").map(Addressed::ToolState);
    }
    let index: usize = part_path
        .strip_prefix("content[")?
        .strip_suffix(']')?
        .parse()
        .ok()?;
    let part = parts.get_mut(index)?;
    part.get_mut("text").and_then(|value| match value {
        Value::String(text) => Some(Addressed::Text(text)),
        _ => None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOutcome {
    /// The mark is (now) queued.
    Queued,
    /// Rejected: the tag was already applied — its content already left the
    /// live array (recovery via `ctx_expand` instead).
    AlreadyApplied,
    /// No such tag for this session.
    UnknownTag,
}

/// Queue a drop mark for `tag` (ctx_reduce). Marks are QUEUED: nothing is
/// removed here — the Task-5 renderer applies them when the next pass
/// rebuilds the cached prefix.
///
/// `false` (the collapsed [`QueueOutcome`]) means nothing was queued: the
/// tag is unknown for the session, or it was already applied. Re-queueing a
/// still-queued tag is idempotent: the reason is overwritten (the last
/// stated reason wins) and the original `created_at` is kept.
pub async fn queue_drop(db: &Db, session_id: &str, tag: u64, reason: &str) -> Result<bool> {
    Ok(queue_drop_outcome(db, session_id, tag, reason).await? == QueueOutcome::Queued)
}

/// [`queue_drop`] with the full outcome, for per-tag tool reporting.
pub async fn queue_drop_outcome(
    db: &Db,
    session_id: &str,
    tag: u64,
    reason: &str,
) -> Result<QueueOutcome> {
    let now = now_millis();
    let tag_id = tag as i64;
    let result: Result<QueueOutcome> = async {
        let conn = db.acquire().await?;
        let known = {
            let mut stmt = conn
                .prepare("SELECT tag FROM mc_tag WHERE session_id = ?1 AND tag = ?2")
                .await?;
            optional(&mut stmt, (session_id, tag_id)).await?
        };
        let Some(_) = known else {
            return Ok(QueueOutcome::UnknownTag);
        };
        let state: Option<String> = {
            let mut stmt = conn
                .prepare("SELECT state FROM mc_drop WHERE session_id = ?1 AND tag = ?2")
                .await?;
            match optional(&mut stmt, (session_id, tag_id)).await? {
                Some(row) => row.get(0)?,
                None => None,
            }
        };
        match state.as_deref() {
            None => {
                conn.execute(
                    "INSERT INTO mc_drop (session_id, tag, reason, state, created_at)
                         VALUES (?1, ?2, ?3, 'queued', ?4)",
                    (session_id, tag_id, reason, now),
                )
                .await?;
                Ok(QueueOutcome::Queued)
            }
            Some("queued") => {
                conn.execute(
                    "UPDATE mc_drop SET reason = ?3 WHERE session_id = ?1 AND tag = ?2",
                    (session_id, tag_id, reason),
                )
                .await?;
                Ok(QueueOutcome::Queued)
            }
            Some(_) => Ok(QueueOutcome::AlreadyApplied),
        }
    }
    .await;
    result.with_context(|| format!("queue drop of tag {tag} for session {session_id}"))
}

/// Peek the session's queued drops: return them (tag, reason) in tag order
/// WITHOUT mutating their state. The caller hands them to the render pass
/// and marks them applied with [`mark_drops_applied`] only after that pass
/// commits — marking at hand-off would lose every drop to a failed pass.
pub async fn peek_queued_drops(db: &Db, session_id: &str) -> Result<Vec<(u64, String)>> {
    let result: Result<Vec<(u64, String)>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT tag, reason FROM mc_drop
                 WHERE session_id = ?1 AND state = 'queued' ORDER BY tag",
            )
            .await?;
        let mut rows = stmt.query([session_id]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((row.get::<i64>(0)? as u64, row.get(1)?));
        }
        Ok(out)
    }
    .await;
    result.with_context(|| format!("peek mc_drop queue for session {session_id}"))
}

/// Mark the given tags' drop rows `'applied'` (rows are kept so `recover`
/// can report dropped-state). Called after the pass that consumed the
/// drops commits. The `state = 'queued'` guard makes this idempotent and
/// keeps a row re-queued between peek and mark from being flipped.
pub async fn mark_drops_applied(db: &Db, session_id: &str, tags: &[u64]) -> Result<()> {
    if tags.is_empty() {
        return Ok(());
    }
    let placeholders = tags.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    // Dynamic arity needs `Vec<Value>`; the tuple form only covers fixed
    // parameter lists.
    let mut params: Vec<ox_db::turso::Value> = Vec::with_capacity(tags.len() + 1);
    params.push(ox_db::turso::Value::Text(session_id.to_owned()));
    params.extend(
        tags.iter()
            .map(|tag| ox_db::turso::Value::Integer(*tag as i64)),
    );
    let conn = db.acquire().await?;
    conn.execute(
        &format!(
            "UPDATE mc_drop SET state = 'applied'
                 WHERE session_id = ? AND tag IN ({placeholders}) AND state = 'queued'"
        ),
        params,
    )
    .await
    .with_context(|| format!("mark mc_drop rows applied for session {session_id}"))?;
    Ok(())
}

/// What a tag resolves to when recovering its original content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovered {
    /// The tag maps to a resolvable part: its current raw-store content and
    /// the drop row's state (`None` = nothing ever queued on the tag).
    Found {
        /// The content as it exists NOW in the raw store. (ox rewrites
        /// message parts in place when tool states settle; recovery reads
        /// the store, not a cached copy.)
        content: String,
        /// The drop row's lifecycle state, if any.
        dropped: Option<DropState>,
    },
    /// The tag exists but its content is gone: the backing message row was
    /// removed (revert/cleanup) or the part no longer resolves.
    Missing,
}

/// The raw recover payload loaded in one shot: (tag mapping, drop row
/// state, backing message row `data`).
type RecoverRow = (Option<(String, String)>, Option<String>, Option<String>);

/// Recover the original content behind a tag (`ctx_expand`): load the
/// backing `session_message` row by the tag's mapping, navigate the stored
/// `part_path`, and return the raw content. `None` = the tag is unknown for
/// the session.
pub async fn recover(db: &Db, session_id: &str, tag: u64) -> Result<Option<Recovered>> {
    let tag_id = tag as i64;
    let (mapping, dropped, data): RecoverRow = async {
        let conn = db.acquire().await?;
        let mapping: Option<(String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT message_id, part_path FROM mc_tag
                     WHERE session_id = ?1 AND tag = ?2",
                )
                .await?;
            match optional(&mut stmt, (session_id, tag_id)).await? {
                Some(row) => Some((row.get(0)?, row.get(1)?)),
                None => None,
            }
        };
        let dropped: Option<String> = match &mapping {
            Some(_) => {
                let mut stmt = conn
                    .prepare("SELECT state FROM mc_drop WHERE session_id = ?1 AND tag = ?2")
                    .await?;
                match optional(&mut stmt, (session_id, tag_id)).await? {
                    Some(row) => row.get(0)?,
                    None => None,
                }
            }
            None => None,
        };
        let data: Option<String> = match &mapping {
            Some((message_id, _)) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT data FROM session_message
                         WHERE id = ?1 AND session_id = ?2",
                    )
                    .await?;
                match optional(&mut stmt, (message_id.as_str(), session_id)).await? {
                    Some(row) => row.get(0)?,
                    None => None,
                }
            }
            None => None,
        };
        Ok::<_, anyhow::Error>((mapping, dropped, data))
    }
    .await
    .with_context(|| format!("recover tag {tag} for session {session_id}"))?;
    let Some((_message_id, part_path)) = mapping else {
        return Ok(None);
    };
    let dropped = drop_state_of(dropped);
    let Some(data) = data else {
        return Ok(Some(Recovered::Missing));
    };
    let value: Value = serde_json::from_str(&data)
        .with_context(|| format!("parse session_message data for tag {tag}"))?;
    let Some(content) = resolve_part_path(&value, &part_path) else {
        return Ok(Some(Recovered::Missing));
    };
    Ok(Some(Recovered::Found { content, dropped }))
}

/// Navigate a stored `part_path` (`"text"` | `"content[N]"` |
/// `"tool:{id}"`) inside a message row's `data` JSON and return the item's
/// taggable content. Returns `None` for anything else (the grammar is
/// closed — see the module docs) or when the path no longer resolves.
fn resolve_part_path(data: &Value, part_path: &str) -> Option<String> {
    if part_path == "text" {
        return data.get("text").and_then(Value::as_str).map(str::to_owned);
    }
    if let Some(id) = part_path.strip_prefix("tool:") {
        // Id-addressed tool part: scan the content array for the part with
        // this id (order-independent — that is the point of the shape).
        let parts = data.get("content")?.as_array()?;
        return parts
            .iter()
            .find(|part| part.get("id").and_then(Value::as_str) == Some(id))
            .and_then(part_content);
    }
    let rest = part_path.strip_prefix("content[")?;
    let index: usize = rest.strip_suffix(']')?.parse().ok()?;
    let part = data.get("content")?.get(index)?;
    part_content(part)
}
