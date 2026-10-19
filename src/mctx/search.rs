//! `ctx_search` — one retrieval tool over everything ox keeps.
//!
//! # Sources
//!
//! Four labels, three of them indexed:
//!
//! - `memory` — durable project facts (`mc_memory`),
//! - `note` — session-scoped parked intentions (`mc_note`),
//! - `compartment` — folded conversation summaries (`mc_compartments`),
//! - `transcript` — raw `session_message` text the historian has NOT folded
//!   yet (the plan's 会话历史 LIKE 回退).
//!
//! The first three are served by the FTS5 table `mc_search_fts` (migration
//! V7): one index row per source row, `source`/`ref` carrying the origin.
//! The transcript is deliberately NOT indexed: it is the live, un-folded tail
//! that changes on every turn and belongs to `session_v2`, not to the
//! magic-context store. ox reads it directly with a case-insensitive `LIKE`
//! over the message JSON — the plan's 会话历史 LIKE 回退, and the same shape as
//! the reference's fallback when its raw-message index is unavailable.
//!
//! # Index maintenance: triggers, not write-through
//!
//! The index is maintained by SQL triggers (see `MIGRATION_V7` in
//! [`crate::db`]). The alternative — writing to the index from the store
//! functions in [`crate::mctx::memory`] and
//! [`crate::mctx::compartments`] — was rejected because those functions
//! already open their own transactions and know nothing about search; a
//! trigger keeps the index in step with *every* write path, including ones
//! added later, with no call-site discipline to forget. (SQLite forbids
//! `CREATE INDEX` on a virtual table, so no unique index can backstop the
//! one-row-per-`(source, ref)` invariant; the triggers hold it instead.)
//!
//! # Query hygiene
//!
//! FTS5's `MATCH` grammar gives meaning to quotes, `*`, `-`, `:`, `^`,
//! parentheses and the bare words `AND`/`OR`/`NOT`/`NEAR`, so a query as
//! ordinary as `a OR b`, `"a b"`, or an unbalanced quote would raise
//! `fts5: syntax error`. [`query_tokens`] therefore scans the raw query into
//! word tokens and the tokens are wrapped in double quotes before they reach
//! `MATCH`: nothing a model types can be interpreted as grammar. A token can
//! only contain alphanumerics and `_ . / : -`, so it can never contain the
//! quote that would break the wrapping and `a OR b` searches for the three
//! literal words `a`, `or`, `b`.
//!
//! Tokens are joined by the `OR` operator, not by FTS5's implicit AND: a
//! natural-language question carries words the corpus does not ("where is the
//! opencode source code path?"), and requiring all of them would return
//! nothing. Every hit is still ranked by BM25, which orders rows matching more
//! of the query's words first, so a row containing all of them outranks one
//! matching a single coincidental word. This also matches the reference's note
//! lane, whose `LIKE` predicates are joined by `OR`.
//!
//! # Deviations from the reference (documented, not accidental)
//!
//! - **No semantic search, auto-search hints, git-commit index or primers.**
//!   ox has no embedding provider for those; the plan scopes search to the
//!   three indexed corpora plus the transcript fallback.
//! - **No result suppression.** The reference hides memories already visible
//!   in `<project-memory>` and messages still in the live tail; ox returns
//!   every match with its id so the model can decide.
//! - **Ranking is per-source and deterministic**, not a blended BM25 score:
//!   FTS5's `bm25` is not comparable across sources, so each source
//!   contributes hits in its own relevance order and they merge by descending
//!   score, then within-source rank, then id.
//! - **Dates are `YYYY-MM-DD` only** (the reference also accepts full ISO
//!   datetimes). `from` is UTC midnight, `to` the last millisecond of that
//!   UTC day; both are inclusive and checked against each row's `created_at`.
//! - **`limit` is clamped to 1..=25** (the reference is unbounded) to keep an
//!   accidental large `limit` from flooding the context window.

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::cardinal::wire::ToolDef;
use crate::db::Db;
use crate::runner::{Tool, ToolContext, ToolOutput};
use crate::tool::{object_param, ok_text};

use super::compartments::Tiers;
use super::memory::internal_error;
use super::tagger::taggable_items;

/// Default `limit` when the model omits it (the reference's default).
const DEFAULT_LIMIT: usize = 10;
/// ox's cap on `limit`; the reference is unbounded (see the module docs).
const MAX_LIMIT: usize = 25;
/// Rows fetched from one indexed source before date filtering. Bounds each
/// source's work; matches the reference's per-source `LIMIT 100`.
const SOURCE_FETCH_LIMIT: i64 = 100;
/// Milliseconds in one UTC day; `to` is the last millisecond of its day.
const DAY_MS: i64 = 86_400_000;

// ---------------------------------------------------------------------------
// Result model
// ---------------------------------------------------------------------------

/// Which corpus a hit came from. The discriminant indexes [`SourceFilter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// Durable project facts (`mc_memory`).
    Memory,
    /// Session-scoped parked intentions (`mc_note`).
    Note,
    /// Folded conversation summaries (`mc_compartments`).
    Compartment,
    /// Raw, not-yet-folded transcript (`session_message`).
    Transcript,
}

impl Source {
    /// The label printed in a result line and accepted in `sources`.
    fn label(self) -> &'static str {
        match self {
            Source::Memory => "memory",
            Source::Note => "note",
            Source::Compartment => "compartment",
            Source::Transcript => "transcript",
        }
    }

    /// Parse a `sources` entry.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "memory" => Some(Source::Memory),
            "note" => Some(Source::Note),
            "compartment" => Some(Source::Compartment),
            "transcript" => Some(Source::Transcript),
            _ => None,
        }
    }
}

/// The set of sources a query may draw from, indexed by [`Source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceFilter([bool; 4]);

impl SourceFilter {
    /// Every source (the default when `sources` is omitted or empty).
    pub(crate) const ALL: Self = Self([true; 4]);

    fn contains(self, source: Source) -> bool {
        self.0[source as usize]
    }

    fn enable(&mut self, source: Source) {
        self.0[source as usize] = true;
    }
}

/// One search request. Borrowed so the tool need not clone its inputs.
pub(crate) struct SearchRequest<'a> {
    /// Project scope (the working directory key memories are written under).
    pub(crate) project: &'a str,
    /// Session scope: notes, compartments and the transcript.
    pub(crate) session_id: &'a str,
    /// The raw user query.
    pub(crate) query: &'a str,
    /// Which sources to search.
    pub(crate) sources: SourceFilter,
    /// Maximum number of merged hits to return.
    pub(crate) limit: usize,
    /// Inclusive lower bound on `created_at` (ms), from `from`.
    pub(crate) from_ms: Option<i64>,
    /// Inclusive upper bound on `created_at` (ms), from `to`.
    pub(crate) to_ms: Option<i64>,
}

/// One unified hit, with the context needed to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchHit {
    /// Which corpus the hit came from.
    pub(crate) source: Source,
    /// The source row's handle (`#N` bodies for memory/note; opaque id for
    /// compartments; the message id for a transcript row).
    pub(crate) id: String,
    /// A short window around the match.
    pub(crate) snippet: String,
    /// The within-source relevance band (see the module docs on ranking).
    pub(crate) score: i64,
    /// Owning scope: the project (memories) or the session (all else).
    pub(crate) scope: String,
    /// Memory category.
    pub(crate) category: Option<String>,
    /// Note status (`active`/`pending`/`dismissed`).
    pub(crate) status: Option<String>,
    /// Compartment title.
    pub(crate) title: Option<String>,
    /// Covered ordinal range (compartments).
    pub(crate) range: Option<(i64, i64)>,
    /// Message ordinal (transcript).
    pub(crate) ordinal: Option<i64>,
}

impl SearchHit {
    /// The header line: `[n] [source] id=… ` plus the source-specific context
    /// a model needs to follow up with `ctx_expand` or another tool.
    fn header(&self, index: usize) -> String {
        let source = self.source.label();
        match self.source {
            Source::Memory => format!(
                "[{index}] [{source}] id=#{} category={}",
                self.id,
                self.category.as_deref().unwrap_or("uncategorized")
            ),
            Source::Note => format!(
                "[{index}] [{source}] id=#{} status={}",
                self.id,
                self.status.as_deref().unwrap_or("active")
            ),
            Source::Compartment => {
                let (start, end) = self.range.unwrap_or((0, 0));
                format!(
                    "[{index}] [{source}] id={} range={start}-{end} title={}",
                    self.id,
                    self.title.as_deref().unwrap_or("")
                )
            }
            Source::Transcript => format!(
                "[{index}] [{source}] id={} ordinal={}",
                self.id,
                self.ordinal.unwrap_or_default()
            ),
        }
    }
}

/// A hit plus its within-source relevance key (lower is better).
struct RankedHit {
    rank: usize,
    hit: SearchHit,
}

/// One indexed memory row as the search reads it.
struct MemoryRow {
    id: String,
    category: String,
    content: String,
    created_at: i64,
}

/// One indexed note row: `surface_condition` is part of the index body, so it
/// is part of the searchable text too.
struct NoteRow {
    id: String,
    title: String,
    body: String,
    status: String,
    surface_condition: Option<String>,
    created_at: i64,
}

/// One indexed compartment row: the tiers JSON holds title, paraphrases and
/// the covered ordinal range.
struct CompartmentRow {
    id: String,
    tiers: String,
    created_at: i64,
}

/// One raw transcript row. `kind` is `session_message.type`; with `id` it
/// completes the message envelope that `data` is stored without.
struct TranscriptRow {
    id: String,
    kind: String,
    seq: i64,
    data: String,
}

// ---------------------------------------------------------------------------
// The search itself
// ---------------------------------------------------------------------------

/// Run one search: query every selected source and return the merged,
/// deterministically ordered hits (at most `request.limit`).
pub(crate) async fn search(db: &Db, request: &SearchRequest<'_>) -> Result<Vec<SearchHit>> {
    let tokens = query_tokens(request.query);
    if tokens.is_empty() || request.limit == 0 {
        return Ok(Vec::new());
    }
    let mut ranked: Vec<RankedHit> = Vec::new();
    if request.sources.contains(Source::Memory) {
        ranked.extend(search_memories(db, request, &tokens).await?);
    }
    if request.sources.contains(Source::Note) {
        ranked.extend(search_notes(db, request, &tokens).await?);
    }
    if request.sources.contains(Source::Compartment) {
        ranked.extend(search_compartments(db, request, &tokens).await?);
    }
    if request.sources.contains(Source::Transcript) {
        ranked.extend(search_transcript(db, request, &tokens).await?);
    }
    // Score is a per-source band, so `rank` is only ever compared between
    // two hits of the same source; `id` is the total tiebreak.
    ranked.sort_by(|a, b| {
        b.hit
            .score
            .cmp(&a.hit.score)
            .then_with(|| a.rank.cmp(&b.rank))
            .then_with(|| a.hit.id.cmp(&b.hit.id))
    });
    ranked.truncate(request.limit);
    Ok(ranked.into_iter().map(|ranked| ranked.hit).collect())
}

/// Active project memories matching every token, in BM25 order.
async fn search_memories(
    db: &Db,
    request: &SearchRequest<'_>,
    tokens: &[String],
) -> Result<Vec<RankedHit>> {
    let Some(match_expr) = fts_match_expression(tokens) else {
        return Ok(Vec::new());
    };
    let project = request.project.to_owned();
    let rows = db
        .conn()
        .call(move |c| -> rusqlite::Result<Vec<MemoryRow>> {
            let mut stmt = c.prepare(
                "SELECT m.id, m.category, m.content, m.created_at
                   FROM mc_search_fts
                   JOIN mc_memory m ON m.id = mc_search_fts.ref
                  WHERE mc_search_fts MATCH ?1
                    AND mc_search_fts.source = 'memory'
                    AND m.project = ?2
                    AND m.status = 'active'
                  ORDER BY bm25(mc_search_fts), mc_search_fts.rowid
                  LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![match_expr, project, SOURCE_FETCH_LIMIT],
                |row| {
                    Ok(MemoryRow {
                        id: row.get(0)?,
                        category: row.get(1)?,
                        content: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )?;
            rows.collect()
        })
        .await
        .context("search indexed memories")?;

    Ok(rows
        .into_iter()
        .enumerate()
        .filter_map(|(rank, row)| {
            in_range(row.created_at, request.from_ms, request.to_ms).then(|| RankedHit {
                rank,
                hit: SearchHit {
                    source: Source::Memory,
                    id: row.id,
                    snippet: snippet_around_tokens(&row.content, tokens),
                    score: 100,
                    scope: request.project.to_owned(),
                    category: Some(row.category),
                    status: None,
                    title: None,
                    range: None,
                    ordinal: None,
                },
            })
        })
        .collect())
}

/// Undismissed notes of the session matching every token, newest first.
async fn search_notes(
    db: &Db,
    request: &SearchRequest<'_>,
    tokens: &[String],
) -> Result<Vec<RankedHit>> {
    let Some(match_expr) = fts_match_expression(tokens) else {
        return Ok(Vec::new());
    };
    // The note's searchable text is title + body + surface condition (the
    // index body, migration V7); the snippet is taken from the same text.
    let session = request.session_id.to_owned();
    let rows = db
        .conn()
        .call(move |c| -> rusqlite::Result<Vec<NoteRow>> {
            let mut stmt = c.prepare(
                "SELECT n.id, n.title, n.body, n.status, n.surface_condition, n.created_at
                   FROM mc_search_fts
                   JOIN mc_note n ON n.id = mc_search_fts.ref
                  WHERE mc_search_fts MATCH ?1
                    AND mc_search_fts.source = 'note'
                    AND n.session_id = ?2
                    AND n.status != 'dismissed'
                  ORDER BY bm25(mc_search_fts), mc_search_fts.rowid
                  LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![match_expr, session, SOURCE_FETCH_LIMIT],
                |row| {
                    Ok(NoteRow {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        body: row.get(2)?,
                        status: row.get(3)?,
                        surface_condition: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                },
            )?;
            rows.collect()
        })
        .await
        .context("search indexed notes")?;

    Ok(rows
        .into_iter()
        .enumerate()
        .filter_map(|(rank, row)| {
            if !in_range(row.created_at, request.from_ms, request.to_ms) {
                return None;
            }
            let mut text = row.title;
            if !row.body.is_empty() {
                text.push('\n');
                text.push_str(&row.body);
            }
            if let Some(condition) = row.surface_condition.as_deref() {
                text.push('\n');
                text.push_str(condition);
            }
            Some(RankedHit {
                rank,
                hit: SearchHit {
                    source: Source::Note,
                    id: row.id,
                    snippet: snippet_around_tokens(&text, tokens),
                    score: 80,
                    scope: request.session_id.to_owned(),
                    category: None,
                    status: Some(row.status),
                    title: None,
                    range: None,
                    ordinal: None,
                },
            })
        })
        .collect())
}

/// Compartments of the session matching every token. A title match outranks
/// a body-only match (the reference's two bands).
async fn search_compartments(
    db: &Db,
    request: &SearchRequest<'_>,
    tokens: &[String],
) -> Result<Vec<RankedHit>> {
    let Some(match_expr) = fts_match_expression(tokens) else {
        return Ok(Vec::new());
    };
    let session = request.session_id.to_owned();
    let rows = db
        .conn()
        .call(move |c| -> rusqlite::Result<Vec<CompartmentRow>> {
            let mut stmt = c.prepare(
                "SELECT c.id, c.tiers, c.created_at
                   FROM mc_search_fts
                   JOIN mc_compartments c ON c.id = mc_search_fts.ref
                  WHERE mc_search_fts MATCH ?1
                    AND mc_search_fts.source = 'compartment'
                    AND c.session_id = ?2
                  ORDER BY bm25(mc_search_fts), mc_search_fts.rowid
                  LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![match_expr, session, SOURCE_FETCH_LIMIT],
                |row| {
                    Ok(CompartmentRow {
                        id: row.get(0)?,
                        tiers: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )?;
            rows.collect()
        })
        .await
        .context("search indexed compartments")?;

    Ok(rows
        .into_iter()
        .enumerate()
        .filter_map(|(rank, row)| {
            if !in_range(row.created_at, request.from_ms, request.to_ms) {
                return None;
            }
            // The index is built from the same `tiers` fields the renderer
            // reads; a malformed row degrades to empty text exactly as the
            // store does, so it is skipped rather than failing the query.
            let Ok(tiers) = serde_json::from_str::<Tiers>(&row.tiers) else {
                return None;
            };
            let body = compartment_body(&tiers);
            let title_hit = tokens
                .iter()
                .any(|token| first_match(&tiers.title, token).is_some());
            Some(RankedHit {
                rank,
                hit: SearchHit {
                    source: Source::Compartment,
                    id: row.id,
                    snippet: snippet_around_tokens(&body, tokens),
                    score: if title_hit { 95 } else { 90 },
                    scope: request.session_id.to_owned(),
                    category: None,
                    status: None,
                    title: Some(tiers.title),
                    range: Some((tiers.start_message, tiers.end_message)),
                    ordinal: None,
                },
            })
        })
        .collect())
}

/// The transcript fallback: a case-insensitive `LIKE` over `session_message`
/// rows, so a query reaches text the historian has not folded into a
/// compartment yet.
async fn search_transcript(
    db: &Db,
    request: &SearchRequest<'_>,
    tokens: &[String],
) -> Result<Vec<RankedHit>> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    // The filter runs in Rust over the extracted text, not in SQL over the raw
    // JSON: SQLite's `LOWER()`/`LIKE` fold ASCII case only, so a non-ASCII term
    // whose case differs from the stored text would match in the FTS lanes and
    // silently not here. Rust's folding is Unicode-correct, and filtering the
    // same extraction the tagger tags means a hit is always text the model could
    // have seen (never a JSON key). Rows stream, so a long session costs one row
    // at a time and stops at the fetch limit.
    let session = request.session_id.to_owned();
    let needles: Vec<String> = tokens.iter().map(|token| token.to_lowercase()).collect();
    let rows = db
        .conn()
        .call(move |c| -> rusqlite::Result<Vec<TranscriptRow>> {
            let mut stmt = c.prepare(
                "SELECT id, type, seq, data FROM session_message
                  WHERE session_id = ?1
                  ORDER BY seq ASC",
            )?;
            let mut candidates = stmt.query([&session])?;
            let mut rows = Vec::new();
            while let Some(row) = candidates.next()? {
                let candidate = TranscriptRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    seq: row.get(2)?,
                    data: row.get(3)?,
                };
                let text = message_text(&candidate).to_lowercase();
                if needles.iter().all(|needle| text.contains(needle.as_str())) {
                    rows.push(candidate);
                    if rows.len() as i64 == SOURCE_FETCH_LIMIT {
                        break;
                    }
                }
            }
            Ok(rows)
        })
        .await
        .context("search the raw transcript")?;

    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(rank, row)| {
            let snippet = snippet_around_tokens(&message_text(&row), tokens);
            RankedHit {
                rank,
                hit: SearchHit {
                    source: Source::Transcript,
                    id: row.id,
                    snippet,
                    score: 0,
                    scope: request.session_id.to_owned(),
                    category: None,
                    status: None,
                    title: None,
                    range: None,
                    ordinal: Some(row.seq),
                },
            }
        })
        .collect())
}

/// The searchable text of one stored message row: the taggable parts joined
/// by newlines, in part order — the same extraction [`crate::mctx::tagger`]
/// tags, so a transcript hit covers exactly the text the model could have
/// seen.
///
/// `session_message.data` holds the message *without* its envelope, and the
/// tagger dispatches on `type`, so `id` and `type` are put back first
/// (upstream's `decode({ ...data, id, type })`). A row whose data is not a
/// JSON object yields no text.
fn message_text(row: &TranscriptRow) -> String {
    let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(&row.data) else {
        return String::new();
    };
    message.insert("id".to_owned(), Value::String(row.id.clone()));
    message.insert("type".to_owned(), Value::String(row.kind.clone()));
    taggable_items(&Value::Object(message))
        .into_iter()
        .map(|(_, content)| content)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The compartment's searchable text: title first, then each paraphrase tier,
/// skipping empty and repeated field values (the reference's
/// `compartment_body_text`).
fn compartment_body(tiers: &Tiers) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for text in [
        tiers.title.as_str(),
        tiers.p1.as_deref().unwrap_or(""),
        tiers.p2.as_deref().unwrap_or(""),
        tiers.p3.as_deref().unwrap_or(""),
        tiers.p4.as_deref().unwrap_or(""),
    ] {
        if !text.is_empty() && !parts.contains(&text) {
            parts.push(text);
        }
    }
    parts.join("\n")
}

/// Whether `timestamp` falls inside the inclusive bounds.
fn in_range(timestamp: i64, from_ms: Option<i64>, to_ms: Option<i64>) -> bool {
    from_ms.is_none_or(|from| timestamp >= from) && to_ms.is_none_or(|to| timestamp <= to)
}

// ---------------------------------------------------------------------------
// Query sanitization
// ---------------------------------------------------------------------------

/// Most terms one query may carry. A search is a handful of words; past this
/// bound the query is a document, and the term list is what every lane's
/// expression is built from.
const MAX_QUERY_TERMS: usize = 64;

/// Split a raw query into word tokens.
///
/// A token is a maximal run of alphanumeric characters plus the word-ish
/// punctuation that appears in paths and symbols (`_ . / : -`), which mirrors
/// the reference's `keyword_search_terms`. Nothing else survives, so a token
/// can never contain a character that is meaningful to the FTS5 grammar.
/// Duplicates are dropped, preserving first-seen order.
fn query_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, tokens: &mut Vec<String>| {
        if !current.is_empty() {
            let token = std::mem::take(current);
            if !tokens.contains(&token) {
                tokens.push(token);
            }
        }
    };
    for ch in query.chars() {
        if is_token_char(ch) {
            current.push(ch);
        } else {
            flush(&mut current, &mut tokens);
        }
    }
    flush(&mut current, &mut tokens);
    tokens
}

/// Whether `ch` may appear inside a token. Alphanumeric is Unicode-wide so
/// non-ASCII words (e.g. CJK) stay searchable.
fn is_token_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '.' | '/' | ':' | '-')
}

/// A safe FTS5 `MATCH` expression: every token double-quoted (a phrase of one
/// token, which FTS5 treats as a literal term) and joined by `OR`, so a hit is
/// any row containing any term and BM25 ranks the rows containing more of
/// them ahead. `None` when the query has no usable token.
fn fts_match_expression(tokens: &[String]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|token| format!("\"{token}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

// ---------------------------------------------------------------------------
// Snippets
// ---------------------------------------------------------------------------

/// A window of `text` around its earliest occurrence of any token, with `…`
/// markers on the trimmed sides. Falls back to a short preview when no token
/// is present (possible for the transcript, whose raw-JSON filter can match a
/// key rather than a value).
fn snippet_around_tokens(text: &str, tokens: &[String]) -> String {
    match tokens
        .iter()
        .filter_map(|token| first_match(text, token))
        .min_by_key(|(start, _)| *start)
    {
        Some((hit, hit_end)) => match_window(text, hit, hit_end),
        None => preview_text(text),
    }
}

/// The snippet window itself: up to 100 characters each side of the match,
/// trimmed to whole characters at both ends and capped at 200 characters.
fn match_window(text: &str, hit: usize, hit_end: usize) -> String {
    const CONTEXT: usize = 100;
    const MAX_CHARS: usize = 200;
    let mut start = hit.saturating_sub(CONTEXT);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (hit_end + CONTEXT).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let snippet: String = text[start..end].chars().take(MAX_CHARS).collect();
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < text.len() { "…" } else { "" };
    format!("{prefix}{}{suffix}", snippet.trim())
}

/// Whitespace-collapsed, truncated head of `text` (the reference's
/// `preview_text`).
fn preview_text(text: &str) -> String {
    const LIMIT: usize = 220;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= LIMIT {
        return normalized;
    }
    let mut preview = normalized
        .chars()
        .take(LIMIT - 1)
        .collect::<String>()
        .trim_end()
        .to_string();
    preview.push('…');
    preview
}

/// The byte range in `text` of the first case-insensitive occurrence of
/// `query`.
///
/// Matching runs on the lowercased text, whose byte offsets are not the
/// original's: some characters change length when lowercased (`İ` grows from
/// two bytes to three, the Kelvin sign shrinks from three to one). Each
/// lowercased byte is mapped back to the original character it came from, so
/// the returned range always lands on the matched text.
fn first_match(text: &str, query: &str) -> Option<(usize, usize)> {
    let lowered = text.to_lowercase();
    let needle = query.to_lowercase();
    let hit = lowered.find(&needle)?;
    let hit_end = hit + needle.len();
    // `str::to_lowercase` lowercases character by character (its one context
    // rule, the final sigma, never changes a length), so walking the
    // characters reproduces its offsets.
    let mut lowered_at = 0;
    let mut start = None;
    for (original_at, ch) in text.char_indices() {
        let next = lowered_at + ch.to_lowercase().map(char::len_utf8).sum::<usize>();
        if start.is_none() && hit < next {
            start = Some(original_at);
        }
        if hit_end <= next {
            return Some((start.unwrap_or(original_at), original_at + ch.len_utf8()));
        }
        lowered_at = next;
    }
    start.map(|start| (start, text.len()))
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// Render the merged hits for the model: `Found N results` then one block per
/// hit (a header line plus its snippet).
fn render_hits(query: &str, hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return format!(
            "No results found for \"{query}\" across memories, notes, compartments, or transcript."
        );
    }
    let mut lines = vec![format!(
        "Found {} result{} for \"{query}\":",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    )];
    for (index, hit) in hits.iter().enumerate() {
        lines.push(String::new());
        lines.push(hit.header(index + 1));
        lines.push(hit.snippet.clone());
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// ctx_search
// ---------------------------------------------------------------------------

/// `ctx_search` — unified retrieval over the magic-context store.
pub struct CtxSearchTool {
    db: Db,
}

impl CtxSearchTool {
    /// The tool over `db` (the `mc_search_fts` index and its sources).
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for CtxSearchTool {
    fn name(&self) -> &str {
        "ctx_search"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "ctx_search",
            "Search the archive — everything that ever happened in this project, not just \
             what is on your desk: project memories, session notes, folded conversation \
             summaries, and raw transcript text the historian has not folded yet.\n\n\
             Retrieval matches exact words (FTS5 over memories, notes and compartments; a \
             case-insensitive substring match over the raw transcript for anything not yet \
             indexed). Phrase `query` as a natural-language question that still carries the \
             exact terms you expect in the answer (paths, symbols, config keys, error \
             strings); a bare keyword stack finds less.\n\n\
             Sources (omit for all):\n\
             - memory — durable project facts; \"what's our convention for X\"\n\
             - note — parked follow-ups with their recorded text; \"did we leave a follow-up\"\n\
             - compartment — folded conversation summaries; \"what did we decide earlier\"\n\
             - transcript — raw messages not yet folded; \"did we discuss this\"\n\
             Use from/to to restrict every source to an inclusive UTC date range.",
            object_param(
                vec![
                    (
                        "query",
                        json!({
                            "type": "string",
                            "description": "A natural-language question carrying the exact \
                                            terms you expect in the answer.",
                        }),
                    ),
                    (
                        "limit",
                        json!({
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_LIMIT,
                            "description": "Maximum results (default 10).",
                        }),
                    ),
                    (
                        "sources",
                        json!({
                            "type": "array",
                            "items": {
                                "type": "string",
                                "enum": ["memory", "note", "compartment", "transcript"],
                            },
                            "description": "Restrict to these sources; omitting it or \
                                            passing [] searches every source.",
                        }),
                    ),
                    (
                        "from",
                        json!({
                            "type": "string",
                            "description": "Earliest date, YYYY-MM-DD (inclusive).",
                        }),
                    ),
                    (
                        "to",
                        json!({
                            "type": "string",
                            "description": "Latest date, YYYY-MM-DD (inclusive; default open).",
                        }),
                    ),
                ],
                &["query"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_search(&self.db, &ctx, &input).await })
    }
}

async fn run_search(db: &Db, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let query = match input.get("query") {
        Some(Value::String(query)) if !query.trim().is_empty() => query.trim(),
        _ => return ok_text("Error: 'query' is required."),
    };
    // A term count is bounded so an unbounded query cannot turn into an
    // unbounded FTS expression; rejecting is louder than silently dropping
    // terms (which would change what "found" means).
    let tokens = query_tokens(query);
    if tokens.len() > MAX_QUERY_TERMS {
        return ok_text(format!(
            "Error: 'query' has too many terms ({}; max {MAX_QUERY_TERMS}).",
            tokens.len()
        ));
    }
    let limit = match parse_limit(input) {
        Ok(limit) => limit,
        Err(message) => return ok_text(message),
    };
    let sources = match parse_sources(input) {
        Ok(sources) => sources,
        Err(message) => return ok_text(message),
    };
    let (from_ms, to_ms) = match parse_date_range(input) {
        Ok(range) => range,
        Err(message) => return ok_text(message),
    };
    let project = ctx.directory.to_string_lossy().into_owned();
    let request = SearchRequest {
        project: &project,
        session_id: &ctx.session_id,
        query,
        sources,
        limit,
        from_ms,
        to_ms,
    };
    match search(db, &request).await {
        Ok(hits) => ok_text(render_hits(query, &hits)),
        Err(error) => internal_error("search", &error),
    }
}

/// `limit`: absent, null or non-positive means the default; otherwise clamped
/// to `1..=MAX_LIMIT`. A non-integer is a schema violation and is rejected.
fn parse_limit(input: &Value) -> std::result::Result<usize, String> {
    match input.get("limit") {
        None | Some(Value::Null) => Ok(DEFAULT_LIMIT),
        Some(value) => match value.as_i64() {
            Some(number) if number <= 0 => Ok(DEFAULT_LIMIT),
            Some(number) => Ok((number as usize).clamp(1, MAX_LIMIT)),
            None => Err(format!(
                "Error: 'limit' must be a positive integer (got {value})."
            )),
        },
    }
}

/// `sources`: absent, null or `[]` means every source; otherwise the named
/// ones, with unknown names rejected.
fn parse_sources(input: &Value) -> std::result::Result<SourceFilter, String> {
    match input.get("sources") {
        None | Some(Value::Null) => Ok(SourceFilter::ALL),
        Some(Value::Array(items)) if items.is_empty() => Ok(SourceFilter::ALL),
        Some(Value::Array(items)) => {
            let mut filter = SourceFilter([false; 4]);
            for item in items {
                let Some(name) = item.as_str() else {
                    return Err(format!(
                        "Error: 'sources' must contain source names (got {item})."
                    ));
                };
                match Source::parse(name) {
                    Some(source) => filter.enable(source),
                    None => {
                        return Err(format!(
                            "Error: unknown source '{name}' (memory, note, compartment, transcript)."
                        ));
                    }
                }
            }
            Ok(filter)
        }
        Some(other) => Err(format!(
            "Error: 'sources' must be an array of source names (got {other})."
        )),
    }
}

/// `from`/`to`: optional inclusive `YYYY-MM-DD` UTC dates, `to` widened to the
/// end of its day; a reversed pair is rejected.
fn parse_date_range(input: &Value) -> std::result::Result<(Option<i64>, Option<i64>), String> {
    let from = parse_date_arg(input, "from")?;
    let to = parse_date_arg(input, "to")?;
    if let (Some(from), Some(to)) = (from, to)
        && from > to
    {
        return Err("Error: Invalid date range; 'from' must be on or before 'to'.".to_owned());
    }
    Ok((from, to))
}

fn parse_date_arg(input: &Value, field: &str) -> std::result::Result<Option<i64>, String> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => match parse_utc_date_ms(text.trim()) {
            Some(start) => Ok(Some(if field == "to" {
                start + DAY_MS - 1
            } else {
                start
            })),
            None => Err(format!("Error: Invalid '{field}' date; use YYYY-MM-DD.")),
        },
        Some(other) => Err(format!(
            "Error: Invalid '{field}' date; use YYYY-MM-DD (got {other})."
        )),
    }
}

/// UTC midnight (ms) for a `YYYY-MM-DD` date, or `None` when malformed or
/// impossible (bad month/day, `2026-02-30`, `2026-13-01`). Years are not
/// range-checked beyond four digits.
fn parse_utc_date_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = text[0..4].parse().ok()?;
    let month: i64 = text[5..7].parse().ok()?;
    let day: i64 = text[8..10].parse().ok()?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day) * DAY_MS)
}

/// Days in `month` of `year` (Gregorian leap rule).
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (Howard Hinnant's
/// `days_from_civil`). Exact for every date, including pre-1970 ones.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::mctx::compartments::{NewCompartment, Tiers, insert};
    use crate::mctx::memory::{update_memory, write_memory, write_note};
    use crate::mctx::tagger::test_support::store_messages;
    use crate::runner::ToolContext;

    const SESSION: &str = "ses_search";
    const PROJECT: &str = "/tmp/search-project";

    async fn db() -> Db {
        Db::open_in_memory().await.unwrap()
    }

    fn tool_ctx(directory: std::path::PathBuf) -> ToolContext {
        ToolContext {
            session_id: SESSION.to_owned(),
            directory,
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    /// Drive the tool shell exactly as the runner does, and return its text.
    async fn run(db: &Db, directory: &str, input: Value) -> String {
        let tool = CtxSearchTool::new(db.clone());
        tool.execute(tool_ctx(std::path::PathBuf::from(directory)), input)
            .await
            .content
            .as_str()
            .expect("tool content is text")
            .to_owned()
    }

    /// A direct store-level request over the fixture project/session.
    fn request<'a>(query: &'a str, sources: SourceFilter) -> SearchRequest<'a> {
        SearchRequest {
            project: PROJECT,
            session_id: SESSION,
            query,
            sources,
            limit: DEFAULT_LIMIT,
            from_ms: None,
            to_ms: None,
        }
    }

    /// A compartment carrying `needle` in its body, in the session.
    async fn publish(db: &Db, id: &str, title: &str, body: &str) {
        let tiers = Tiers {
            title: title.to_owned(),
            start_message: 1,
            end_message: 9,
            start_date: None,
            end_date: None,
            p1: Some(body.to_owned()),
            p2: None,
            p3: None,
            p4: None,
        };
        insert(
            db,
            &NewCompartment {
                id,
                session_id: SESSION,
                importance: 50.0,
                tiers: &tiers,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn finds_one_hit_per_indexed_source_with_its_label() {
        let db = db().await;
        write_memory(&db, PROJECT, "zebrafish migration ledger", "ARCHITECTURE")
            .await
            .unwrap();
        write_note(&db, SESSION, "zebrafish follow-up", None)
            .await
            .unwrap();
        publish(
            &db,
            "mc_a",
            "Historian design",
            "we chose zebrafish indexing",
        )
        .await;

        let hits = search(&db, &request("zebrafish", SourceFilter::ALL))
            .await
            .unwrap();
        assert_eq!(hits.len(), 3, "one hit per indexed source: {hits:#?}");
        let labels: Vec<&str> = hits.iter().map(|hit| hit.source.label()).collect();
        assert_eq!(labels, vec!["memory", "compartment", "note"]);
        assert!(hits[0].snippet.contains("zebrafish"), "{:#?}", hits[0]);
        assert_eq!(hits[0].category.as_deref(), Some("ARCHITECTURE"));
        assert_eq!(hits[1].title.as_deref(), Some("Historian design"));
        assert_eq!(hits[1].range, Some((1, 9)));
        assert_eq!(hits[2].status.as_deref(), Some("active"));
        // Project and session scoping reach the header the model reads.
        let rendered = render_hits("zebrafish", &hits);
        assert!(
            rendered.starts_with("Found 3 results for \"zebrafish\":"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[1] [memory] id=#1 category=ARCHITECTURE"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[3] [note] id=#1 status=active"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn transcript_fallback_finds_text_that_is_not_indexed() {
        let db = db().await;
        store_messages(
            &db,
            SESSION,
            &[json!({ "id": "msg_1", "type": "user", "text": "seahorse only in the raw tail" })],
        )
        .await;
        // Nothing is in the index, so the raw transcript is the only hit.
        let indexed: i64 = db
            .conn()
            .call(|c| c.query_row("SELECT count(*) FROM mc_search_fts", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(indexed, 0);

        let hits = search(&db, &request("seahorse", SourceFilter::ALL))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert_eq!(hits[0].source, Source::Transcript);
        assert_eq!(hits[0].id, "msg_1");
        assert_eq!(hits[0].ordinal, Some(0));
        assert_eq!(hits[0].scope, SESSION);
        assert!(hits[0].snippet.contains("seahorse"), "{}", hits[0].snippet);
        // The fallback is case-insensitive, and LIKE/SQL metacharacters are
        // inert (nothing reaches SQL as a pattern any more): `%` separates terms
        // instead of matching anything, and `_` is an ordinary term character.
        for (query, expected) in [("SEAHORSE", 1), ("sea%horse", 1), ("sea_horse", 0)] {
            let hits = search(&db, &request(query, SourceFilter::ALL))
                .await
                .unwrap();
            assert_eq!(hits.len(), expected, "query {query}");
        }
    }

    /// `LOWER()`/`LIKE` fold ASCII case only, so the transcript lane carries a
    /// second, raw-case pattern; without it a non-ASCII term whose case differs
    /// from the stored text would match in the index lanes and vanish here.
    #[tokio::test]
    async fn transcript_lane_matches_non_ascii_terms_in_either_case() {
        let db = db().await;
        store_messages(
            &db,
            SESSION,
            &[json!({ "id": "msg_1", "type": "user", "text": "die Ökologie des Waldes" })],
        )
        .await;

        for query in ["Ökologie", "ökologie"] {
            let mut only_transcript = SourceFilter([false; 4]);
            only_transcript.enable(Source::Transcript);
            let hits = search(&db, &request(query, only_transcript)).await.unwrap();
            assert_eq!(hits.len(), 1, "query {query}: {hits:#?}");
            assert!(hits[0].snippet.contains("kologie"), "{}", hits[0].snippet);
        }
    }

    /// A query with more terms than the transcript lane can bind is rejected
    /// loudly; silently dropping terms would change what "found" means.
    #[tokio::test]
    async fn an_absurd_term_count_is_rejected_not_partially_searched() {
        let db = db().await;
        let query = (0..=MAX_QUERY_TERMS)
            .map(|n| format!("t{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let output = run(&db, "dir", json!({ "query": query })).await;
        assert!(
            output.starts_with("Error: 'query' has too many terms"),
            "{output}"
        );

        // Exactly at the bound the search still runs.
        let query = (0..MAX_QUERY_TERMS)
            .map(|n| format!("t{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let output = run(&db, "dir", json!({ "query": query })).await;
        assert!(!output.starts_with("Error:"), "{output}");
    }

    /// The acceptance's index-maintenance test: every write path (memory,
    /// note, compartment) must be visible to a search immediately.
    #[tokio::test]
    async fn index_follows_every_write_path() {
        let db = db().await;
        write_memory(&db, PROJECT, "narwhal fact", "CONSTRAINTS")
            .await
            .unwrap();
        write_note(&db, SESSION, "narwhal follow-up", None)
            .await
            .unwrap();
        publish(&db, "mc_n", "Narwhal", "narwhal summary").await;

        let hits = search(&db, &request("narwhal", SourceFilter::ALL))
            .await
            .unwrap();
        assert_eq!(hits.len(), 3, "every write path is indexed: {hits:#?}");

        // An update must replace the indexed text, not append a second row.
        update_memory(&db, PROJECT, "1", "axolotl fact", None)
            .await
            .unwrap();
        let memory_only = SourceFilter([true, false, false, false]);
        assert!(
            search(&db, &request("narwhal", memory_only))
                .await
                .unwrap()
                .is_empty(),
            "the updated memory must no longer match its old text"
        );
        let hits = search(&db, &request("axolotl", SourceFilter::ALL))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, Source::Memory);
        assert!(hits[0].snippet.contains("axolotl"));
    }

    #[tokio::test]
    async fn merged_hits_are_ordered_by_score_then_id_deterministically() {
        let db = db().await;
        publish(&db, "mc_z", "Quokka", "quokka in a compartment").await;
        write_note(&db, SESSION, "quokka note", None).await.unwrap();
        write_memory(&db, PROJECT, "quokka memory", "NAMING")
            .await
            .unwrap();
        store_messages(
            &db,
            SESSION,
            &[json!({ "id": "msg_1", "type": "user", "text": "quokka transcript" })],
        )
        .await;

        let first = search(&db, &request("quokka", SourceFilter::ALL))
            .await
            .unwrap();
        let labels: Vec<&str> = first.iter().map(|hit| hit.source.label()).collect();
        assert_eq!(
            labels,
            vec!["memory", "compartment", "note", "transcript"],
            "{first:#?}"
        );
        let second = search(&db, &request("quokka", SourceFilter::ALL))
            .await
            .unwrap();
        assert_eq!(first, second, "ordering is deterministic");
    }

    #[tokio::test]
    async fn source_filter_and_limit_apply() {
        let db = db().await;
        write_memory(&db, PROJECT, "pangolin memory", "PROJECT_RULES")
            .await
            .unwrap();
        write_note(&db, SESSION, "pangolin note", None)
            .await
            .unwrap();
        publish(&db, "mc_p", "Pangolin", "pangolin compartment").await;
        store_messages(
            &db,
            SESSION,
            &[json!({ "id": "msg_1", "type": "user", "text": "pangolin transcript" })],
        )
        .await;

        let mut only_notes = request("pangolin", SourceFilter([false; 4]));
        only_notes.sources.enable(Source::Note);
        let hits = search(&db, &only_notes).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, Source::Note);

        let mut limited = request("pangolin", SourceFilter::ALL);
        limited.limit = 2;
        let hits = search(&db, &limited).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source, Source::Memory);
        assert_eq!(hits[1].source, Source::Compartment);

        // A project memory is invisible to another project, even though the
        // index row exists.
        let mut other = request("pangolin", SourceFilter::ALL);
        other.project = "/tmp/other-project";
        let hits = search(&db, &other).await.unwrap();
        assert!(
            hits.iter().all(|hit| hit.source != Source::Memory),
            "{hits:#?}"
        );
    }

    #[tokio::test]
    async fn date_range_filters_every_source_inclusively() {
        let db = db().await;
        write_memory(&db, PROJECT, "otter fact", "NAMING")
            .await
            .unwrap();
        // `write_memory` stamps `now`, which is long after 2000.
        let narrowed = SearchRequest {
            from_ms: Some(parse_utc_date_ms("2000-01-01").unwrap()),
            to_ms: Some(parse_utc_date_ms("2000-01-01").unwrap() + DAY_MS - 1),
            ..request("otter", SourceFilter::ALL)
        };
        assert!(search(&db, &narrowed).await.unwrap().is_empty());
        let open = SearchRequest {
            from_ms: Some(parse_utc_date_ms("2000-01-01").unwrap()),
            ..request("otter", SourceFilter::ALL)
        };
        assert_eq!(search(&db, &open).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn tool_shell_reports_missing_and_malformed_arguments() {
        let db = db().await;
        let directory = std::env::temp_dir().to_string_lossy().into_owned();
        assert_eq!(
            run(&db, &directory, json!({})).await,
            "Error: 'query' is required."
        );
        assert_eq!(
            run(&db, &directory, json!({ "query": "   " })).await,
            "Error: 'query' is required."
        );
        assert!(
            run(&db, &directory, json!({ "query": "x", "limit": "many" }))
                .await
                .starts_with("Error: 'limit' must be a positive integer")
        );
        assert!(
            run(
                &db,
                &directory,
                json!({ "query": "x", "sources": ["git_commit"] })
            )
            .await
            .starts_with("Error: unknown source 'git_commit'")
        );
        assert!(
            run(
                &db,
                &directory,
                json!({ "query": "x", "from": "01-01-2000" })
            )
            .await
            .starts_with("Error: Invalid 'from' date; use YYYY-MM-DD.")
        );
        assert!(
            run(
                &db,
                &directory,
                json!({ "query": "x", "from": "2026-02-01", "to": "2026-01-01" })
            )
            .await
            .starts_with("Error: Invalid date range; 'from' must be on or before 'to'.")
        );
        // The happy path through the shell, scoped to the working directory.
        write_memory(&db, &directory, "capybara fact", "CONFIG_VALUES")
            .await
            .unwrap();
        let rendered = run(&db, &directory, json!({ "query": "capybara" })).await;
        assert!(
            rendered.starts_with("Found 1 result for \"capybara\":"),
            "{rendered}"
        );
        assert!(rendered.contains("[memory]"), "{rendered}");
        let rendered = run(&db, &directory, json!({ "query": "armadillo" })).await;
        assert!(
            rendered.starts_with("No results found for \"armadillo\""),
            "{rendered}"
        );
    }

    /// FTS5's grammar treats quotes, `*`, `-`, `:`, `^`, parentheses and the
    /// words AND/OR/NOT/NEAR as operators; every one of these must be a literal
    /// token instead of a syntax error.
    #[tokio::test]
    async fn query_hygiene_survives_operators_and_unbalanced_quotes() {
        let db = db().await;
        write_memory(&db, PROJECT, "alpha beta gamma", "NAMING")
            .await
            .unwrap();
        for query in [
            "\"alpha\"",
            "\"alpha beta\"",
            "\"unbalanced",
            "alpha OR beta",
            "alpha NEAR(beta)",
            "-alpha",
            "alpha*",
            "NOT(alpha)",
            "alpha^2",
            "alpha:beta",
            "***",
            "()",
            "a\"b\"c",
        ] {
            let rendered = run(
                &db,
                std::env::temp_dir().to_str().unwrap(),
                json!({ "query": query }),
            )
            .await;
            assert!(!rendered.starts_with("Error"), "{query:?} -> {rendered}");
        }
        // The operator word is searched as a word (`OR` matches nothing here),
        // and the terms around it still match.
        let hits = search(&db, &request("alpha OR beta", SourceFilter::ALL))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        let hits = search(&db, &request("\"alpha beta gamma\"", SourceFilter::ALL))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn malformed_compartment_tiers_degrade_without_error() {
        let db = db().await;
        publish(&db, "mc_ok", "Valid", "quokka here").await;
        db.conn()
            .call(|c| {
                c.execute(
                    "INSERT INTO mc_compartments
                         (id, session_id, coverage_end, importance, tiers, created_at)
                     VALUES ('mc_bad', ?1, '3', 50.0, '{not json', 1)",
                    rusqlite::params![SESSION],
                )
            })
            .await
            .unwrap();
        let hits = search(&db, &request("quokka", SourceFilter::ALL))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "mc_ok");
    }

    #[test]
    fn query_tokens_strip_fts_grammar() {
        assert_eq!(query_tokens("a OR b"), vec!["a", "OR", "b"]);
        assert_eq!(query_tokens("\"a b\""), vec!["a", "b"]);
        assert_eq!(
            query_tokens("src/mctx/search.rs"),
            vec!["src/mctx/search.rs"]
        );
        assert_eq!(query_tokens("dup dup dup"), vec!["dup"]);
        assert_eq!(query_tokens("  ***  "), Vec::<String>::new());
        assert_eq!(query_tokens("含义"), vec!["含义"]);
        assert_eq!(
            fts_match_expression(&query_tokens("a OR b")).unwrap(),
            "\"a\" OR \"OR\" OR \"b\""
        );
        assert_eq!(fts_match_expression(&[]), None);
    }

    #[test]
    fn utc_dates_parse_and_reject_impossible_days() {
        assert_eq!(parse_utc_date_ms("1970-01-01"), Some(0));
        assert_eq!(
            parse_utc_date_ms("2026-10-19").unwrap() / DAY_MS,
            days_from_civil(2026, 10, 19)
        );
        assert_eq!(parse_utc_date_ms("2026-02-30"), None);
        assert_eq!(parse_utc_date_ms("2026-13-01"), None);
        assert_eq!(parse_utc_date_ms("2026-1-01"), None);
        assert_eq!(parse_utc_date_ms("2026-01-01T00:00:00Z"), None);
        assert_eq!(parse_utc_date_ms("yesterday"), None);
        assert_eq!(
            parse_utc_date_ms("2024-02-29"),
            Some(days_from_civil(2024, 2, 29) * DAY_MS)
        );
    }

    /// Characters that change length when lowercased must not move a snippet
    /// off its match: `İ` grows, the Kelvin sign shrinks (the reference's own
    /// regression test).
    #[test]
    fn snippets_stay_on_the_match_when_lowercasing_changes_lengths() {
        for filler in ["\u{130}", "\u{212a}"] {
            let text = format!("{}needle{}", filler.repeat(150), "x".repeat(10));
            let snippet = snippet_around_tokens(&text, &["NEEDLE".to_owned()]);
            assert!(snippet.contains("needle"), "{filler:?}: {snippet:?}");
            let (start, end) = first_match(&text, "needle").unwrap();
            assert_eq!(&text[start..end], "needle");
        }
        let (start, end) = first_match("a\u{130}b", "i").unwrap();
        assert_eq!(&"a\u{130}b"[start..end], "\u{130}");
    }

    #[test]
    fn snippet_is_a_window_around_the_earliest_match() {
        let text = format!("{}needle{}", "x".repeat(500), "y".repeat(500));
        let snippet = snippet_around_tokens(&text, &["needle".to_owned()]);
        assert!(snippet.starts_with('…') && snippet.ends_with('…'));
        // 100 before + 6 matched + 100 after, capped at 200 characters.
        assert_eq!(snippet.chars().count(), 202, "window plus two markers");
        assert!(snippet.contains("needle"));
        // No match at all: a whitespace-collapsed preview, no panic.
        let preview = snippet_around_tokens("  a   b  ", &["zzz".to_owned()]);
        assert_eq!(preview, "a b");
        assert_eq!(
            preview_text(&"w".repeat(300)),
            format!("{}…", "w".repeat(219))
        );
    }
}
