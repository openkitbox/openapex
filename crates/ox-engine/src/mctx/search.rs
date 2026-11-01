//! `ctx_search` — one retrieval tool over everything ox keeps.
//!
//! # Sources
//!
//! Five labels, two of them indexed:
//!
//! - `memory` — durable project facts (`mc_memory`),
//! - `note` — session-scoped parked intentions (`mc_note`),
//! - `compartment` — folded conversation summaries (`mc_compartments`),
//! - `transcript` — raw `session_message` text the historian has NOT folded
//!   yet (the plan's 会话历史 LIKE 回退),
//! - `commit` — recent git history, read from the repository at query time
//!   (see [`search_commits`]; there is no commit index).
//!
//! Memories and notes are each served by their own Tantivy-backed FTS index
//! (migration V7, rebuilt with the `ngram` tokenizer in V17 so CJK text is
//! split into 2–3 character grams): `mc_memory_fts` and `mc_note_fts`.
//! The table itself is the source, so there is no shared shadow table and no
//! `source`/`ref` columns to join on: each lane queries its own table.
//!
//! Compartments had an index too (`mc_compartments_fts`) until V18 dropped it:
//! turso matches the whole index before the session filter applies and keeps
//! its segments resident, so a lane that only ever reads one session (or one
//! project) paid for every session ever folded. [`search_compartments`] reads
//! the scope's rows instead, matches case-insensitive substrings and ranks
//! with BM25 computed over exactly the rows it scanned.
//! The transcript is deliberately NOT indexed: it is the live, un-folded tail
//! that changes on every turn and belongs to `session_v2`, not to the
//! magic-context store. ox reads it directly with a case-insensitive `LIKE`
//! over the message JSON — the plan's 会话历史 LIKE 回退, and the same shape as
//! the reference's fallback when its raw-message index is unavailable.
//! Commits are not indexed either: git already stores them, and a derived
//! index would be a second source of truth to keep in sync for no benefit at
//! this corpus size (see the deviation note below).
//!
//! # Index maintenance: none
//!
//! turso's FTS indexes follow every INSERT / UPDATE / DELETE by themselves
//! (see `MIGRATION_V7` in [`crate::db`]), so no trigger, backfill query or
//! write-through in the store functions is needed: every write path, including
//! ones added later, is indexed without call-site discipline. The one derived
//! value is `mc_compartments.search_text`, written by the compartment store
//! because `tiers` is JSON rather than searchable columns; the compartment
//! scan matches against it.
//!
//! # Query hygiene
//!
//! The index's query grammar gives meaning to quotes, `*`, `-`, `:`, `^`,
//! parentheses and the bare words `AND`/`OR`/`NOT`/`NEAR`, so a query as
//! ordinary as `a OR b`, `"a b"`, or an unbalanced quote would raise a syntax
//! error. [`query_tokens`] therefore scans the raw query into
//! word tokens and the tokens are wrapped in double quotes before they reach
//! `fts_match`: nothing a model types can be interpreted as grammar. A token can
//! only contain alphanumerics and `_ . / : -`, so it can never contain the
//! quote that would break the wrapping and `a OR b` searches for the three
//! literal words `a`, `or`, `b`.
//!
//! Tokens are joined by the `OR` operator, not by the implicit AND: a
//! natural-language question carries words the corpus does not ("where is the
//! opencode source code path?"), and requiring all of them would return
//! nothing. Every hit is still ranked by BM25 (the FTS score for memories and
//! notes, ox's own for compartments), which orders rows matching more — and
//! rarer — query words first, so a row containing all of them outranks one
//! matching a single coincidental word. This also matches the reference's
//! note lane, whose `LIKE` predicates are joined by `OR`.
//!
//! # Automatic recall (the per-turn hint)
//!
//! A turn's opening request may be preceded by a context the model does not
//! expect: a memory, a parked note, a compartment folded out of the window.
//! [`render_recall_block`] turns the top few hits for the prompt's own typed
//! text into a compact `<recall>` block, and the step appends it to that same
//! user message — so the hint lives in the per-turn tail of the request rather
//! than in `system`/`m[0]`/`m[1]`, where it would rewrite the provider's cached
//! prefix on every turn. Snippets are capped and never carry a body: a hint is
//! a pointer, not an injection, and the model follows up with `ctx_search` /
//! `ctx_expand` when it wants the whole thing.
//!
//! # Deviations from the reference (documented, not accidental)
//!
//! - **Semantic search only where it pays: the memory lane.** When
//!   `[magic_context.embedding]` is configured, project memories are ranked by
//!   cosine similarity to the query; notes, compartments and the transcript
//!   stay lexical (FTS, substring scan, `LIKE`) because their corpora are
//!   small and session- or project-scoped. There is no blended score: cosine either replaces the
//!   memory lane's BM25 order or, with the embedder off, BM25 is what runs.
//!   Automatic recall reuses the same lanes (see [`render_recall_block`]) but
//!   runs without an embedder: the step that assembles a request has none, so
//!   a hint is always lexical.
//! - **Commit history is read at query time, never indexed.** The reference
//!   offers an opt-in git-commit index (embeddings in a store table, kept in
//!   sync as commits land). ox walks the newest
//!   [`COMMIT_SCAN_LIMIT`] commits on `HEAD` with libgit2 and matches the
//!   summary line lexically. Deriving the answer from the repository on demand
//!   costs one revwalk and cannot drift; an index would need a sync mechanism,
//!   a schema and a re-embedding path for a corpus that is already on disk.
//!   The cost is the bound itself: a commit older than the window is not
//!   found, and branches other than the checked-out one are not searched.
//! - **No result suppression.** The reference hides memories already visible
//!   in `<project-memory>` and messages still in the live tail; ox returns
//!   every match with its id so the model can decide.
//! - **Compartments are scanned, not indexed, and can span the project.**
//!   The reference also scans them (a session-scoped `LIKE`) but ranks by hit
//!   count; ox ranks by BM25 over the scanned rows. `ctx_search` takes
//!   `scope: "project"` to search every session in the current directory;
//!   automatic recall always stays on the current session.
//! - **Ranking is per-source and deterministic**, not a blended score:
//!   BM25 scores are not comparable across sources, so each source
//!   contributes hits in its own relevance order and they merge by descending
//!   band, then within-source rank, then id.
//! - **Dates are `YYYY-MM-DD` only** (the reference also accepts full ISO
//!   datetimes). `from` is UTC midnight, `to` the last millisecond of that
//!   UTC day; both are inclusive and checked against each row's `created_at`.
//! - **`limit` is clamped to 1..=25** (the reference is unbounded) to keep an
//!   accidental large `limit` from flooding the context window.

use std::collections::HashSet;
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::runner::{Tool, ToolContext, ToolOutput};
use ox_db::Db;
use ox_llm::cardinal::wire::ToolDef;
use ox_tools::{object_param, ok_text};

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
/// Seconds in one UTC day; the commit lane works in git's seconds.
const SECONDS_PER_DAY: i64 = 86_400;
/// Commits one search may walk back from `HEAD`. The walk is the whole cost of
/// the lane, so it is bounded: history older than this window is not searched
/// (see [`search_commits`]).
const COMMIT_SCAN_LIMIT: usize = 500;
/// The commit lane's score band. Commits rank below every indexed lane
/// (memory 100, compartments 95/90, notes 80) and above the raw transcript
/// (0): "why was this changed" is a secondary question next to the project's
/// own facts and decisions, so an old commit is a hint rather than an answer.
const COMMIT_SCORE: i64 = 50;

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
    /// Recent git history on `HEAD`, read from the repository at query time.
    Commit,
}

impl Source {
    /// The label printed in a result line and accepted in `sources`.
    fn label(self) -> &'static str {
        match self {
            Source::Memory => "memory",
            Source::Note => "note",
            Source::Compartment => "compartment",
            Source::Transcript => "transcript",
            Source::Commit => "commit",
        }
    }

    /// Parse a `sources` entry.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "memory" => Some(Source::Memory),
            "note" => Some(Source::Note),
            "compartment" => Some(Source::Compartment),
            "transcript" => Some(Source::Transcript),
            "commit" => Some(Source::Commit),
            _ => None,
        }
    }
}

/// The set of sources a query may draw from, indexed by [`Source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceFilter([bool; 5]);

impl SourceFilter {
    /// Every source (the default when `sources` is omitted or empty).
    pub(crate) const ALL: Self = Self([true; 5]);

    /// The lanes automatic recall draws from: memories, notes and folded
    /// compartments — context that can drift out of the live window without
    /// the model noticing. Not the transcript (whatever is in the window is
    /// already in the request) and not commit history (a different corpus,
    /// and one the model asks about explicitly).
    pub(crate) const RECALL: Self = Self([true, true, true, false, false]);

    fn contains(self, source: Source) -> bool {
        self.0[source as usize]
    }

    fn enable(&mut self, source: Source) {
        self.0[source as usize] = true;
    }
}

/// Which sessions the compartment lane searches. Only compartments have a
/// scope choice: memories are project-wide already, and notes and the
/// transcript always belong to the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompartmentScope {
    /// The current session only (the default, and what recall uses).
    Session,
    /// Every session whose directory equals the current project directory,
    /// child sessions included.
    Project,
}

impl CompartmentScope {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "session" => Some(CompartmentScope::Session),
            "project" => Some(CompartmentScope::Project),
            _ => None,
        }
    }
}

/// One search request. Borrowed so the tool need not clone its inputs.
pub(crate) struct SearchRequest<'a> {
    /// Project scope (the working directory key memories are written under).
    pub(crate) project: &'a str,
    /// The current session: scopes notes and the transcript, and compartments
    /// unless `compartment_scope` widens them to the project.
    pub(crate) session_id: &'a str,
    /// Whether compartments come from this session or the whole project.
    pub(crate) compartment_scope: CompartmentScope,
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
    /// compartments; the message id for a transcript row; the 7-character
    /// short oid for a commit).
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
    /// a model needs to follow up with `ctx_expand` or another tool. A
    /// compartment from a session other than `current_session` names its
    /// session; one from the current session prints exactly as before.
    fn header(&self, index: usize, current_session: &str) -> String {
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
                let mut header = format!(
                    "[{index}] [{source}] id={} range={start}-{end} title={}",
                    self.id,
                    self.title.as_deref().unwrap_or("")
                );
                if self.scope != current_session {
                    header.push_str(" session=");
                    header.push_str(&self.scope);
                }
                header
            }
            Source::Transcript => format!(
                "[{index}] [{source}] id={} ordinal={}",
                self.id,
                self.ordinal.unwrap_or_default()
            ),
            Source::Commit => format!("[{index}] [{source}] id={} scope={}", self.id, self.scope),
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

/// One scanned compartment row that matched at least one query token. Rows
/// that match nothing are dropped as they are read; `tiers` holds the title,
/// paraphrases and covered range; `search_text` identifies a fork's copy.
struct CompartmentRow {
    id: String,
    session_id: String,
    tiers: String,
    search_text: String,
    created_at: i64,
    stats: DocStats,
}

/// BM25 `k1`: how quickly repeated occurrences of a term saturate.
const BM25_K1: f64 = 1.2;
/// BM25 `b`: how strongly a document's length normalizes its score.
const BM25_B: f64 = 0.75;

/// One document's BM25 inputs: the occurrence count of each query token (in
/// token order) and the document length in characters. Characters, not words:
/// CJK text has no spaces, and BM25 only needs one consistent unit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DocStats {
    term_frequency: Vec<u32>,
    length: usize,
}

impl DocStats {
    /// Count every needle in `lowered`, which the caller lowercased once.
    /// `needles` are the lowercased query tokens (never empty strings).
    fn new(lowered: &str, needles: &[String]) -> Self {
        let term_frequency = needles
            .iter()
            .map(|needle| {
                u32::try_from(lowered.matches(needle.as_str()).count()).unwrap_or(u32::MAX)
            })
            .collect();
        Self {
            term_frequency,
            length: lowered.chars().count(),
        }
    }

    /// Whether any query token occurs in the document.
    fn matches(&self) -> bool {
        self.term_frequency.iter().any(|&tf| tf > 0)
    }
}

/// Corpus statistics for BM25, taken from exactly the rows one scan read: the
/// rarity of a term and the average length are relative to the searched scope
/// (one session, or one project), not to the whole database.
#[derive(Debug, Clone, PartialEq)]
struct Bm25Corpus {
    /// Rows scanned, matched or not (`N`).
    documents: usize,
    /// Mean document length in characters (`avgdl`).
    average_length: f64,
    /// Per-token count of matched documents containing it (`df`).
    document_frequency: Vec<usize>,
}

impl Bm25Corpus {
    /// `scanned`/`total_length` cover every row read; `matched` are the
    /// documents kept. A dropped row contains no token, so it adds to no
    /// document frequency.
    fn new<'a>(
        scanned: usize,
        total_length: usize,
        matched: impl IntoIterator<Item = &'a DocStats>,
        tokens: usize,
    ) -> Self {
        let mut document_frequency = vec![0; tokens];
        for doc in matched {
            for (df, &tf) in document_frequency.iter_mut().zip(&doc.term_frequency) {
                if tf > 0 {
                    *df += 1;
                }
            }
        }
        let average_length = if scanned == 0 {
            0.0
        } else {
            total_length as f64 / scanned as f64
        };
        Self {
            documents: scanned,
            average_length,
            document_frequency,
        }
    }

    /// Lucene's BM25: `Σ idf · tf·(k1+1) / (tf + k1·(1 − b + b·dl/avgdl))`
    /// with `idf = ln(1 + (N − df + 0.5) / (df + 0.5))`, never negative.
    #[must_use]
    fn score(&self, doc: &DocStats) -> f64 {
        let documents = self.documents as f64;
        let relative_length = if self.average_length > 0.0 {
            doc.length as f64 / self.average_length
        } else {
            1.0
        };
        let norm = BM25_K1 * (1.0 - BM25_B + BM25_B * relative_length);
        doc.term_frequency
            .iter()
            .zip(&self.document_frequency)
            .filter(|(tf, _)| **tf > 0)
            .map(|(&tf, &df)| {
                let df = df as f64;
                let idf = (1.0 + (documents - df + 0.5) / (df + 0.5)).ln();
                let tf = f64::from(tf);
                idf * tf * (BM25_K1 + 1.0) / (tf + norm)
            })
            .sum()
    }
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
pub(crate) async fn search(
    db: &Db,
    embedder: Option<&super::embed::Embedder>,
    request: &SearchRequest<'_>,
) -> Result<Vec<SearchHit>> {
    let tokens = query_tokens(request.query);
    if tokens.is_empty() || request.limit == 0 {
        return Ok(Vec::new());
    }
    let mut ranked: Vec<RankedHit> = Vec::new();
    if request.sources.contains(Source::Memory) {
        // Semantic when configured, lexical otherwise. An endpoint failure
        // degrades this lane to its index rather than failing the search.
        let hits = match embedder {
            Some(embedder) => match semantic_memories(db, embedder, request, &tokens).await {
                Ok(hits) => hits,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "semantic memory search failed; falling back to the index"
                    );
                    search_memories(db, request, &tokens).await?
                }
            },
            None => search_memories(db, request, &tokens).await?,
        };
        ranked.extend(hits);
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
    if request.sources.contains(Source::Commit) {
        ranked.extend(search_commits(request));
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

/// Active project memories matching every token, in FTS-score order.
async fn search_memories(
    db: &Db,
    request: &SearchRequest<'_>,
    tokens: &[String],
) -> Result<Vec<RankedHit>> {
    let Some(match_expr) = fts_match_expression(tokens) else {
        return Ok(Vec::new());
    };
    // The source table is the index: the project and status predicates stay in
    // SQL, so the fetch limit applies to the rows this lane may return.
    let rows = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, category, content, created_at
                   FROM mc_memory
                  WHERE fts_match(content, ?1)
                    AND project = ?2
                    AND status = 'active'
                  ORDER BY fts_score(content, ?1) DESC, rowid
                  LIMIT ?3",
            )
            .await?;
        let mut candidates = stmt
            .query((match_expr.as_str(), request.project, SOURCE_FETCH_LIMIT))
            .await?;
        let mut rows = Vec::new();
        while let Some(row) = candidates.next().await? {
            rows.push(MemoryRow {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                created_at: row.get(3)?,
            });
        }
        Ok::<_, anyhow::Error>(rows)
    }
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

/// Active project memories ranked by cosine similarity to the query.
///
/// Rows whose vector is missing or was produced under another identity are
/// backfilled first (bounded per search), so the corpus converges without a
/// scheduled sweep. No blended score: when semantic search is configured it
/// replaces the memory lane's BM25 ranking rather than competing with it — the
/// two scales are not comparable, and a blend would need a weight nobody can
/// justify.
async fn semantic_memories(
    db: &Db,
    embedder: &super::embed::Embedder,
    request: &SearchRequest<'_>,
    tokens: &[String],
) -> Result<Vec<RankedHit>> {
    backfill_embeddings(db, embedder, request.project).await;
    let query = embedder.embed_query(request.query).await?;
    let memories = super::memory::embedded_memories(db, request.project, embedder.identity())
        .await
        .context("load embedded memories")?;
    let mut scored: Vec<(f32, super::memory::EmbeddedMemory)> = memories
        .into_iter()
        .map(|memory| (super::embed::cosine(&query, &memory.vector), memory))
        .collect();
    scored.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.id.cmp(&right.1.id))
    });
    Ok(scored
        .into_iter()
        .take(SOURCE_FETCH_LIMIT as usize)
        .filter(|(_, memory)| in_range(memory.created_at, request.from_ms, request.to_ms))
        .enumerate()
        .map(|(rank, (_, memory))| RankedHit {
            rank,
            hit: SearchHit {
                source: Source::Memory,
                id: memory.id,
                snippet: snippet_around_tokens(&memory.content, tokens),
                score: 100,
                scope: request.project.to_owned(),
                category: Some(memory.category),
                status: None,
                title: None,
                range: None,
                ordinal: None,
            },
        })
        .collect())
}

/// Embed up to [`BACKFILL_LIMIT`] memories of `project` that carry no vector
/// for the current identity. Best effort: a failure logs and leaves the rows
/// for the next search.
async fn backfill_embeddings(db: &Db, embedder: &super::embed::Embedder, project: &str) {
    const BACKFILL_LIMIT: i64 = 20;
    let pending = match super::memory::memories_needing_embedding(
        db,
        project,
        embedder.identity(),
        BACKFILL_LIMIT,
    )
    .await
    {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(error = %error, "find memories to embed failed");
            return;
        }
    };
    if pending.is_empty() {
        return;
    }
    let texts: Vec<&str> = pending
        .iter()
        .map(|(_, content)| content.as_str())
        .collect();
    let vectors = match embedder.embed_documents(&texts).await {
        Ok(vectors) => vectors,
        Err(error) => {
            tracing::warn!(error = %error, "backfill embeddings failed");
            return;
        }
    };
    for ((id, _), vector) in pending.iter().zip(vectors) {
        if let Err(error) =
            super::memory::store_embedding(db, id, embedder.identity(), &vector).await
        {
            tracing::warn!(memory = %id, error = %error, "store backfilled embedding failed");
        }
    }
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
    // index's column list, migration V7); the snippet is taken from the same
    // text.
    let rows = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, title, body, status, surface_condition, created_at
                   FROM mc_note
                  WHERE fts_match(title, body, surface_condition, ?1)
                    AND session_id = ?2
                    AND status != 'dismissed'
                  ORDER BY fts_score(title, body, surface_condition, ?1) DESC, rowid
                  LIMIT ?3",
            )
            .await?;
        let mut candidates = stmt
            .query((match_expr.as_str(), request.session_id, SOURCE_FETCH_LIMIT))
            .await?;
        let mut rows = Vec::new();
        while let Some(row) = candidates.next().await? {
            rows.push(NoteRow {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                status: row.get(3)?,
                surface_condition: row.get(4)?,
                created_at: row.get(5)?,
            });
        }
        Ok::<_, anyhow::Error>(rows)
    }
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

/// Compartments in the request's scope (this session, or every session in
/// the project directory) matching any token. A title match outranks a
/// body-only match (the reference's two bands); within a band rows order by
/// BM25 over the scanned scope.
///
/// No full-text index: the scope's rows are read through
/// `mc_compartments_session_idx` (project scope finds its sessions through
/// `session_v2_directory_idx` first) and matched as case-insensitive substrings
/// here (see `MIGRATION_V18` for why the FTS index was dropped). Rows stream;
/// one that matches nothing contributes to the corpus statistics and is
/// dropped at once, so a query holds one row plus its hits.
async fn search_compartments(
    db: &Db,
    request: &SearchRequest<'_>,
    tokens: &[String],
) -> Result<Vec<RankedHit>> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let needles: Vec<String> = tokens.iter().map(|token| token.to_lowercase()).collect();
    // Session scope reads through `mc_compartments_session_idx`; project
    // scope finds the project's sessions through `session_v2_directory_idx`.
    // The date filter is in SQL so the BM25 corpus is exactly the rows the
    // scope and range make visible.
    let (sql, key) = match request.compartment_scope {
        CompartmentScope::Session => (
            "SELECT id, session_id, tiers, search_text, created_at
               FROM mc_compartments
              WHERE session_id = ?1
                AND (?2 IS NULL OR created_at >= ?2)
                AND (?3 IS NULL OR created_at <= ?3)",
            request.session_id,
        ),
        CompartmentScope::Project => (
            "SELECT c.id, c.session_id, c.tiers, c.search_text, c.created_at
               FROM session_v2 s
               JOIN mc_compartments c ON c.session_id = s.id
              WHERE s.directory = ?1
                AND (?2 IS NULL OR c.created_at >= ?2)
                AND (?3 IS NULL OR c.created_at <= ?3)",
            request.project,
        ),
    };
    let (rows, corpus) = async {
        let conn = db.acquire().await?;
        let mut stmt = conn.prepare(sql).await?;
        let mut candidates = stmt.query((key, request.from_ms, request.to_ms)).await?;
        let mut rows = Vec::new();
        let mut scanned = 0_usize;
        let mut total_length = 0_usize;
        while let Some(row) = candidates.next().await? {
            let search_text: String = row.get(3)?;
            let stats = DocStats::new(&search_text.to_lowercase(), &needles);
            scanned += 1;
            total_length += stats.length;
            if !stats.matches() {
                continue;
            }
            rows.push(CompartmentRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                tiers: row.get(2)?,
                search_text,
                created_at: row.get(4)?,
                stats,
            });
        }
        let corpus = Bm25Corpus::new(
            scanned,
            total_length,
            rows.iter().map(|row| &row.stats),
            needles.len(),
        );
        Ok::<_, anyhow::Error>((rows, corpus))
    }
    .await
    .context("scan compartments")?;

    let mut scored: Vec<ScoredCompartment> = rows
        .into_iter()
        .filter_map(|row| {
            // The search text is built from the same `tiers` fields the
            // renderer reads; a malformed row degrades to empty text exactly
            // as the store does, so it is skipped rather than failing the
            // query.
            let tiers = serde_json::from_str::<Tiers>(&row.tiers).ok()?;
            let title_hit = needles
                .iter()
                .any(|needle| first_match(&tiers.title, needle).is_some());
            Some(ScoredCompartment {
                band: if title_hit { 95 } else { 90 },
                bm25: corpus.score(&row.stats),
                current_session: row.session_id == request.session_id,
                row,
                tiers,
            })
        })
        .collect();
    scored.sort_by(ScoredCompartment::order);
    // A fork copies its parent's compartments verbatim, so project scope can
    // see the same text twice. Identical text scores identically, so the
    // order above keeps the current session's copy, else the newest (a fork
    // keeps `created_at`, so "newest" falls to the later, larger id).
    // The text moves into the set; nothing reads it afterwards.
    let mut seen: HashSet<String> = HashSet::new();
    let unique = scored.into_iter().filter_map(|mut candidate| {
        seen.insert(mem::take(&mut candidate.row.search_text))
            .then_some(candidate)
    });

    Ok(unique
        .take(SOURCE_FETCH_LIMIT as usize)
        .enumerate()
        .map(|(rank, scored)| {
            let body = compartment_body(&scored.tiers);
            RankedHit {
                rank,
                hit: SearchHit {
                    source: Source::Compartment,
                    id: scored.row.id,
                    snippet: snippet_around_tokens(&body, tokens),
                    score: scored.band,
                    scope: scored.row.session_id,
                    category: None,
                    status: None,
                    title: Some(scored.tiers.title),
                    range: Some((scored.tiers.start_message, scored.tiers.end_message)),
                    ordinal: None,
                },
            }
        })
        .collect())
}

/// A matched compartment with its sort keys.
struct ScoredCompartment {
    band: i64,
    bm25: f64,
    current_session: bool,
    row: CompartmentRow,
    tiers: Tiers,
}

impl ScoredCompartment {
    /// Band, then BM25 (descending), then the current session first, then
    /// newest first, then id descending: a total, deterministic order. Ids
    /// ascend with creation time, so when a fork copy shares its parent's
    /// `created_at` the later copy still sorts first.
    fn order(a: &Self, b: &Self) -> std::cmp::Ordering {
        b.band
            .cmp(&a.band)
            .then_with(|| b.bm25.total_cmp(&a.bm25))
            .then_with(|| b.current_session.cmp(&a.current_session))
            .then_with(|| b.row.created_at.cmp(&a.row.created_at))
            .then_with(|| b.row.id.cmp(&a.row.id))
    }
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
    let needles: Vec<String> = tokens.iter().map(|token| token.to_lowercase()).collect();
    let rows = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, type, seq, data FROM session_message
                  WHERE session_id = ?1
                  ORDER BY seq ASC",
            )
            .await?;
        let mut candidates = stmt.query([request.session_id]).await?;
        let mut rows = Vec::new();
        while let Some(row) = candidates.next().await? {
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
        Ok::<_, anyhow::Error>(rows)
    }
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

/// The commit lane: the newest [`COMMIT_SCAN_LIMIT`] commits on `HEAD` whose
/// summary line (the first line of the message) contains every query token.
///
/// Work is bounded by the walk, not by the repository's age: at most
/// [`COMMIT_SCAN_LIMIT`] commits are visited and at most
/// [`SOURCE_FETCH_LIMIT`] matches are kept, so a call costs one revwalk and one
/// snippet allocation per hit. Two documented limits follow from deriving
/// instead of indexing:
///
/// - a commit older than the window is never found;
/// - only `HEAD`'s history is walked, so a branch that has not been merged
///   into the checked-out branch is not searched.
///
/// A project that is not a repository, or whose repository cannot be read, has
/// no commits to search: the lane returns no hits rather than an error, since
/// "no history" is an answer, not a failure.
fn search_commits(request: &SearchRequest<'_>) -> Vec<RankedHit> {
    let needles = commit_tokens(request.query);
    if needles.is_empty() {
        return Vec::new();
    }
    let Ok(repo) = git2::Repository::discover(request.project) else {
        return Vec::new();
    };
    let mut walk = match repo.revwalk() {
        Ok(walk) => walk,
        Err(_) => return Vec::new(),
    };
    // Newest first, so the within-source rank is recency order. Sorting is set
    // before the walk starts (libgit2 rejects a change once it has).
    if walk.set_sorting(git2::Sort::TIME).is_err() || walk.push_head().is_err() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for oid in walk.take(COMMIT_SCAN_LIMIT) {
        let Ok(oid) = oid else { continue };
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        let time = commit.time().seconds();
        if !in_range(time.saturating_mul(1000), request.from_ms, request.to_ms) {
            continue;
        }
        let summary = commit_summary(commit.message().unwrap_or(""));
        if !summary_matches(summary, &needles) {
            continue;
        }
        let author = commit.author();
        hits.push(SearchHit {
            source: Source::Commit,
            id: short_oid(oid),
            snippet: format!(
                "{} {}: {summary}",
                author.name().unwrap_or("unknown"),
                utc_date(time)
            ),
            score: COMMIT_SCORE,
            scope: request.project.to_owned(),
            category: None,
            status: None,
            title: Some(summary.to_owned()),
            range: None,
            ordinal: None,
        });
        if hits.len() == SOURCE_FETCH_LIMIT as usize {
            break;
        }
    }
    hits.into_iter()
        .enumerate()
        .map(|(rank, hit)| RankedHit { rank, hit })
        .collect()
}

/// The commit lane's tokens: the query split on every non-alphanumeric
/// character and lowercased.
///
/// Commits are matched lexically against one English summary line, so the
/// punctuation that carries meaning in the FTS lanes (`_ . / : -`, see
/// [`query_tokens`]) is a separator here, and one-character tokens are dropped
/// because they match nearly every summary. Duplicates are dropped, preserving
/// first-seen order.
fn commit_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for word in query.split(|ch: char| !ch.is_alphanumeric()) {
        if word.chars().count() < 2 {
            continue;
        }
        let token = word.to_lowercase();
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    tokens
}

/// Whether every token appears in `summary`, case-insensitively. An empty token
/// list matches nothing: there is no query to answer.
fn summary_matches(summary: &str, needles: &[String]) -> bool {
    if needles.is_empty() {
        return false;
    }
    let lowered = summary.to_lowercase();
    needles
        .iter()
        .all(|needle| lowered.contains(needle.as_str()))
}

/// The summary: the first line of a commit message, without trailing
/// whitespace (git's `%s` for the common single-paragraph case).
fn commit_summary(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim_end()
}

/// Git's abbreviated oid: the first seven hex characters.
fn short_oid(oid: git2::Oid) -> String {
    oid.to_string()[..7].to_owned()
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
/// can never contain a character that is meaningful to the query grammar.
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

/// Whether `ch` may appear inside a token. Alphanumeric is Unicode-wide, so a
/// CJK run stays one token here. The memory and note indexes' ngram tokenizer
/// (migration V17) splits it into 2–3 character grams on both the indexing
/// and query side; the compartment scan matches the whole token as one
/// contiguous substring, so a single CJK character is a usable query there.
fn is_token_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '.' | '/' | ':' | '-')
}

/// A safe `fts_match` query: every token double-quoted (a phrase of one
/// token, which the index treats as a literal term) and joined by `OR`, so a
/// hit is any row containing any term and the score ranks the rows containing
/// more of them ahead. `None` when the query has no usable token.
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
fn render_hits(query: &str, hits: &[SearchHit], current_session: &str) -> String {
    if hits.is_empty() {
        return format!(
            "No results found for \"{query}\" across memories, notes, compartments, transcript, or commits."
        );
    }
    let mut lines = vec![format!(
        "Found {} result{} for \"{query}\":",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    )];
    for (index, hit) in hits.iter().enumerate() {
        lines.push(String::new());
        lines.push(hit.header(index + 1, current_session));
        lines.push(hit.snippet.clone());
    }
    lines.join("\n")
}

/// The most hits one recall block lists; the caller asks for at most this
/// many, and the cap here keeps the line budget even if it does not.
const RECALL_LINES: usize = 3;
/// Characters of a hit's snippet a recall line keeps. A hint is a pointer: it
/// is enough to recognize the match, never enough to stand in for it.
const RECALL_SNIPPET_CHARS: usize = 200;

/// Render the automatic-recall block for this turn's hits (`None` when there
/// is nothing to recall).
///
/// The format is fixed: one `<recall>` element, one explanatory line, one
/// `- [source] handle: snippet` line per hit, and nothing after the closing
/// tag. The prose says the context was retrieved rather than quoted, so the
/// model treats it as a weak hint instead of something the user asked about.
pub(crate) fn render_recall_block(hits: &[SearchHit]) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let hits = hits.iter().take(RECALL_LINES);
    let mut lines: Vec<String> = Vec::with_capacity(RECALL_LINES + 3);
    lines.push("<recall>".to_owned());
    lines.push(
        "Possibly relevant context retrieved automatically for this turn (the user did not quote it):"
            .to_owned(),
    );
    for hit in hits {
        // Memory and note ids print with the `#` handle `ctx_expand` takes
        // (the `ctx_search` header's form); a compartment's id is already an
        // opaque handle and is printed bare.
        let mut line = format!("- [{}] ", hit.source.label());
        if matches!(hit.source, Source::Memory | Source::Note) {
            line.push('#');
        }
        line.push_str(&hit.id);
        line.push_str(": ");
        line.push_str(&recall_snippet(&hit.snippet));
        lines.push(line);
    }
    lines.push("</recall>".to_owned());
    Some(lines.join("\n"))
}

/// One recall line's snippet: whitespace (newlines included) collapsed to
/// single spaces, the first [`RECALL_SNIPPET_CHARS`] characters kept, then
/// XML-escaped — escaped last, so a memory body containing `<` or `&` can
/// neither close the element nor read as markup, and the cap counts source
/// characters rather than escape expansions.
fn recall_snippet(snippet: &str) -> String {
    let collapsed = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(RECALL_SNIPPET_CHARS).collect();
    super::historian::escape_xml_content(&capped)
}

// ---------------------------------------------------------------------------
// ctx_search
// ---------------------------------------------------------------------------

/// `ctx_search` — unified retrieval over the magic-context store.
pub struct CtxSearchTool {
    db: Db,
    /// `None` when semantic search is off: the memory lane then uses its
    /// full-text index like the note lane.
    embedder: Option<Arc<super::embed::Embedder>>,
}

impl CtxSearchTool {
    /// The tool over `db` (the `mc_*` tables and their search indexes).
    #[must_use]
    pub fn new(db: Db, embedder: Option<Arc<super::embed::Embedder>>) -> Self {
        Self { db, embedder }
    }
}

impl Tool for CtxSearchTool {
    fn name(&self) -> &str {
        "ctx_search"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(super::CTX_GUIDANCE)
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "ctx_search",
            "Search the archive — everything that ever happened in this project, not just \
             what is on your desk: project memories, session notes, folded conversation \
             summaries, and raw transcript text the historian has not folded yet.\n\n\
             Retrieval matches exact words (full-text indexes over memories and notes; \
             a case-insensitive substring match over compartments and the raw \
             transcript). Phrase `query` as a natural-language question \
             that still carries the exact terms you expect in the answer (paths, symbols, \
             config keys, error strings); a bare keyword stack finds less.\n\n\
             Sources (omit for all):\n\
             - memory — durable project facts; \"what's our convention for X\"\n\
             - note — parked follow-ups with their recorded text; \"did we leave a follow-up\"\n\
             - compartment — folded conversation summaries; \"what did we decide earlier\"\n\
             - transcript — raw messages not yet folded; \"did we discuss this\"\n\
             - commit — git history: scans the last 500 commits on HEAD, newest first, \
             matching each commit's summary line; \"why was this changed\"\n\
             Use from/to to restrict every source to an inclusive UTC date range. \
             Compartments come from this session unless scope is \"project\".",
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
                                "enum": ["memory", "note", "compartment", "transcript", "commit"],
                            },
                            "description": "Restrict to these sources; omitting it or \
                                            passing [] searches every source.",
                        }),
                    ),
                    (
                        "scope",
                        json!({
                            "type": "string",
                            "enum": ["session", "project"],
                            "description": "Compartments only: \"session\" (default) searches \
                                            this session's summaries, \"project\" every \
                                            session in this directory. Memories are \
                                            project-wide; notes and transcript are always \
                                            this session.",
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
        Box::pin(async move { run_search(&self.db, self.embedder.as_deref(), &ctx, &input).await })
    }
}

async fn run_search(
    db: &Db,
    embedder: Option<&super::embed::Embedder>,
    ctx: &ToolContext,
    input: &Value,
) -> ToolOutput {
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
    let compartment_scope = match parse_scope(input) {
        Ok(scope) => scope,
        Err(message) => return ok_text(message),
    };
    let request = SearchRequest {
        project: &project,
        session_id: &ctx.session_id,
        compartment_scope,
        query,
        sources,
        limit,
        from_ms,
        to_ms,
    };
    match search(db, embedder, &request).await {
        Ok(hits) => ok_text(render_hits(query, &hits, &ctx.session_id)),
        Err(error) => internal_error("search", &error),
    }
}

/// `scope`: absent or null means the current session; otherwise `"session"`
/// or `"project"`, anything else rejected.
fn parse_scope(input: &Value) -> std::result::Result<CompartmentScope, String> {
    match input.get("scope") {
        None | Some(Value::Null) => Ok(CompartmentScope::Session),
        Some(value) => value
            .as_str()
            .and_then(CompartmentScope::parse)
            .ok_or_else(|| {
                format!("Error: 'scope' must be \"session\" or \"project\" (got {value}).")
            }),
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
            let mut filter = SourceFilter([false; 5]);
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
                            "Error: unknown source '{name}' (memory, note, compartment, transcript, commit)."
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

/// The UTC calendar date of a unix timestamp (seconds), as `YYYY-MM-DD`.
///
/// Used only for display, never for parsing, so integer arithmetic is enough:
/// no calendar crate and no timezone database is involved. The timestamp is
/// UTC by definition; git stores the author's offset, which the summary line
/// deliberately does not show.
fn utc_date(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(SECONDS_PER_DAY));
    format!("{year:04}-{month:02}-{day:02}")
}

/// The proleptic-Gregorian calendar date of a day count since 1970-01-01
/// (Howard Hinnant's `civil_from_days`, the inverse of [`days_from_civil`]).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needles(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| token.to_lowercase()).collect()
    }

    /// Score every document against a corpus made of exactly those documents.
    fn scores(docs: &[&str], tokens: &[&str]) -> Vec<f64> {
        let needles = needles(tokens);
        let stats: Vec<DocStats> = docs
            .iter()
            .map(|doc| DocStats::new(&doc.to_lowercase(), &needles))
            .collect();
        let total_length = stats.iter().map(|doc| doc.length).sum();
        let corpus = Bm25Corpus::new(docs.len(), total_length, &stats, needles.len());
        stats.iter().map(|doc| corpus.score(doc)).collect()
    }

    #[test]
    fn rare_term_outranks_common_term() {
        let docs = [
            "turso FTS 缓存 overview",
            "session session notes",
            "session handling",
            "session cleanup",
            "session restore",
        ];
        let scores = scores(&docs, &["turso", "FTS", "缓存", "session"]);
        assert!(
            scores[0] > scores[1],
            "rare terms must outrank the common one: {scores:?}"
        );
    }

    #[test]
    fn term_frequency_saturates() {
        let once = format!("lock {}", "x".repeat(45));
        let ten = "lock ".repeat(10);
        let scores = scores(
            &[&once, &ten, "unrelated text of similar size here...."],
            &["lock"],
        );
        assert!(
            scores[1] > scores[0],
            "more occurrences score higher: {scores:?}"
        );
        assert!(
            scores[1] < 10.0 * scores[0],
            "ten occurrences must score under ten times one: {scores:?}"
        );
    }

    #[test]
    fn shorter_document_wins_at_equal_frequency() {
        let short = "lock held";
        let long = format!("lock held {}", "padding ".repeat(30));
        let scores = scores(&[short, &long, "nothing here"], &["lock"]);
        assert!(scores[0] > scores[1], "length must normalize: {scores:?}");
    }

    #[test]
    fn single_cjk_character_matches_and_counts() {
        let stats = DocStats::new("获取锁失败，锁被占用", &needles(&["锁"]));
        assert_eq!(stats.term_frequency, vec![2], "both 锁 occurrences count");
        assert_eq!(
            stats.length, 10,
            "length is in characters, got {}",
            stats.length
        );
        assert!(stats.matches(), "a single CJK character is a usable token");
    }

    #[test]
    fn matching_is_case_insensitive() {
        let text = "Turso FTS and turso again";
        let stats = DocStats::new(&text.to_lowercase(), &needles(&["TURSO", "fts"]));
        assert_eq!(
            stats.term_frequency,
            vec![2, 1],
            "case must not affect counts"
        );
    }
}
