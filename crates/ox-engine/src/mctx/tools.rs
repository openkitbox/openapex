//! Registration of the `ctx_*` tools into the existing tool registry.
//!
//! The two tools here are the model-facing shells over
//! [`crate::mctx::tagger`]; the tag/drop semantics live there. They are
//! registered via [`ox_tools::ToolRegistry::register_mctx`], which the
//! app wiring calls only when `magic_context.enabled` — a default-off
//! config must keep the advertised toolset byte-identical.
//!
//! Session scoping needs no dispatch change: [`ToolContext`] already
//! carries `session_id`; the tools hold their [`Db`] handle from
//! construction (they are built where the DB exists, see `register_mctx`).
//!
//! The argument shapes mirror the reference `magic-context` tools:
//! `ctx_reduce` takes a single `drop` range string, `ctx_expand` a single
//! `tag` (number or copied handle). See [`parse_range_string`] for the
//! exact range grammar and normalization rules.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::runner::{Tool, ToolContext, ToolOutput};
use ox_db::Db;
use ox_schema::wire::ToolDef;
use ox_tools::{object_param, ok_text};

use super::tagger::{DropState, QueueOutcome, Recovered, queue_drop_outcome, recover};

/// The maximum number of tag numbers one range — or the whole drop list —
/// may expand to; mirrors the reference `parseRangeString` cap.
const MAX_RANGE_ELEMENTS: u64 = 1000;

/// The reason recorded on every drop row. `ctx_reduce` no longer takes a
/// model-supplied reason (the reference drops it from the interface too);
/// the queue's `reason` column keeps a fixed marker for diagnostics.
const DROP_REASON: &str = "ctx_reduce";

/// `ctx_reduce`'s missing/empty-`drop` message, verbatim from the
/// reference.
const DROP_REQUIRED: &str = "Error: 'drop' must be provided.";

// ---------------------------------------------------------------------------
// Range-string parsing (port of the reference `parseRangeString`)
// ---------------------------------------------------------------------------

/// The `§N§` handle-normalization patterns, compiled once. All three are
/// constant and valid (asserted by a unit test); matching the reference,
/// `[0-9]` is used instead of `\d` so only ASCII digits normalize.
fn handle_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // provider_error.rs convention: an invalid pattern degrades to a
        // no-op instead of panicking. Order is load-bearing (dropped
        // handles first, prose handles, then bare §N§ sections).
        [
            r"\[dropped\s+§([0-9]+)§\]",
            r"tag\s+([0-9]+)",
            r"§([0-9]+)§?",
        ]
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    })
}

/// Normalize the tag handles a model copies verbatim out of the transcript
/// (`§302§-§380§`, `tag 12`, `[dropped §302§]`) down to their bare numbers,
/// in the same order and with the same patterns as the reference.
fn normalize_tag_handles(input: &str) -> String {
    let mut text = input.to_owned();
    for pattern in handle_patterns() {
        text = pattern.replace_all(&text, "$1").into_owned();
    }
    text
}

/// One range endpoint: ASCII digits only, nothing parsed from the empty
/// string. Values beyond `u64` are rejected (tags are `u64`); the reference
/// would only reject them once a range/total cap applied.
fn parse_range_integer(raw: &str) -> Result<u64, String> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("Invalid integer: \"{raw}\""));
    }
    raw.parse::<u64>()
        .map_err(|_| format!("Invalid integer: \"{raw}\""))
}

/// Parse a range string into a sorted, deduplicated list of tag numbers.
///
/// Supported syntax: a single number (`"5"`), a range (`"3-5"`), a
/// comma-separated list (`"1,2,9"`), or a mix (`"1-5,8,12-15"`). Copied
/// transcript handles are normalized first — `§302§-§380§`, `tag 12` and
/// `[dropped §302§]` all parse as their bare numbers.
///
/// Errors on an empty string, non-numeric input, a reversed range, a range
/// over [`MAX_RANGE_ELEMENTS`], or a total list over that same cap.
fn parse_range_string(input: &str) -> Result<Vec<u64>, String> {
    let normalized = normalize_tag_handles(input);
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return Err("Range string must not be empty".to_owned());
    }

    let mut numbers = BTreeSet::new();
    for segment in trimmed.split(',') {
        let part = segment.trim();
        match part.split_once('-') {
            Some((start_str, end_str)) => {
                let start = parse_range_integer(start_str.trim())?;
                let end = parse_range_integer(end_str.trim())?;
                if start > end {
                    return Err(format!(
                        "Invalid range \"{part}\": start ({start}) must be <= end ({end})"
                    ));
                }
                let range_size = u128::from(end - start) + 1;
                if range_size > u128::from(MAX_RANGE_ELEMENTS) {
                    return Err(format!(
                        "Range \"{part}\" exceeds maximum size of {MAX_RANGE_ELEMENTS} elements \
                         (got {range_size})"
                    ));
                }
                numbers.extend(start..=end);
            }
            None => {
                numbers.insert(parse_range_integer(part)?);
            }
        }
    }

    if numbers.len() > MAX_RANGE_ELEMENTS as usize {
        return Err(format!(
            "Total range size exceeds maximum of {MAX_RANGE_ELEMENTS} elements (got {})",
            numbers.len()
        ));
    }

    Ok(numbers.into_iter().collect())
}

/// The `drop` argument as a tag list. The reference advertises a string
/// only; for tolerance we also accept the older array-of-integers shape a
/// model may still send, reducing it to the same deduplicated, sorted list.
fn parse_drop_argument(value: &Value) -> Result<Vec<u64>, String> {
    match value {
        Value::String(text) => parse_range_string(text),
        Value::Array(items) => {
            let mut numbers = BTreeSet::new();
            for item in items {
                match item.as_u64() {
                    Some(tag) => {
                        numbers.insert(tag);
                    }
                    None => {
                        return Err(format!("Expected a non-negative integer, got {item}"));
                    }
                }
            }
            Ok(numbers.into_iter().collect())
        }
        other => Err(format!("Expected a range string, got {other}")),
    }
}

/// Model-facing `§N§` form for a list of tags.
fn tag_list(tags: &[u64]) -> String {
    tags.iter()
        .map(|tag| format!("§{tag}§"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// ctx_reduce
// ---------------------------------------------------------------------------

/// Mark conversation items as no longer needed. Marks are QUEUED — content
/// leaves the live array only when a later pass rebuilds the cached prefix,
/// a `[dropped §N§]` placeholder remains, and `ctx_expand` still recovers
/// the original.
pub struct CtxReduceTool {
    db: Db,
}

impl CtxReduceTool {
    /// The tool over `db` (tag/drop tables).
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for CtxReduceTool {
    fn name(&self) -> &str {
        "ctx_reduce"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(super::CTX_GUIDANCE)
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "ctx_reduce",
            "Stamp conversation items (their §N§ tags are shown beside them) that you no \
             longer need on your desk. Marks are queued, not immediate: the content leaves \
             the context at the next prefix rebuild and a `[dropped §N§]` placeholder \
             remains. Nothing is lost — ctx_expand(tag) can always recover the original. \
             Pass the tags to drop as a range string: \"3-5\", \"1,2,9\", \"1-5,8,12-15\". \
             Stamp: file reads, search results and tool outputs the work ahead no longer \
             needs; build/test output after you acted on it; repeated or redundant dumps; \
             status output that only confirmed what you expected. Keep: user messages, \
             your own conversation text, unresolved errors, raw evidence you have not \
             extracted yet, outputs whose exact wording may still matter. Never \
             blanket-stamp a range like \"1-50\" — many small targeted stamps beat one \
             sweep.",
            object_param(
                vec![(
                    "drop",
                    json!({
                        "type": "string",
                        "description": "Tag IDs to drop: \"3-5\", \"1,2,9\", \"1-5,8,12-15\".",
                    }),
                )],
                &[],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_reduce(&self.db, &ctx, &input).await })
    }
}

async fn run_reduce(db: &Db, ctx: &ToolContext, input: &Value) -> ToolOutput {
    // The reference returns the provided-error as the tool's content, not a
    // tool failure; `drop: ""` is falsy there too.
    let drop = match input.get("drop") {
        Some(value) if !value.is_null() && value.as_str() != Some("") => value,
        _ => return ok_text(DROP_REQUIRED),
    };
    let tags = match parse_drop_argument(drop) {
        Ok(tags) => tags,
        Err(message) => return ok_text(format!("Error: Invalid range syntax. {message}")),
    };
    let mut queued = Vec::new();
    let mut applied = Vec::new();
    let mut unknown = Vec::new();
    let mut failed = Vec::new();
    for tag in tags {
        let outcome = match queue_drop_outcome(db, &ctx.session_id, tag, DROP_REASON).await {
            Ok(outcome) => outcome,
            // Per-tag failure: report it, keep processing the other tags.
            Err(error) => {
                failed.push((tag, error.to_string()));
                continue;
            }
        };
        match outcome {
            QueueOutcome::Queued => queued.push(tag),
            QueueOutcome::AlreadyApplied => applied.push(tag),
            QueueOutcome::UnknownTag => unknown.push(tag),
        }
    }
    let mut lines = Vec::new();
    if !queued.is_empty() {
        lines.push(format!(
            "Queued for drop: {} ({} — content leaves the context at the next prefix \
             rebuild; ctx_expand recovers it).",
            tag_list(&queued),
            plural(queued.len(), "item"),
        ));
    }
    if !applied.is_empty() {
        lines.push(format!("Already dropped (no-op): {}.", tag_list(&applied)));
    }
    if !unknown.is_empty() {
        lines.push(format!("Unknown tag: {}.", tag_list(&unknown)));
    }
    for (tag, error) in failed {
        lines.push(format!("§{tag}§ failed: {error}"));
    }
    ok_text(lines.join("\n"))
}

/// `"{n} {noun}"` with a pluralized noun.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

// ---------------------------------------------------------------------------
// ctx_expand
// ---------------------------------------------------------------------------

/// Recover the original content behind §N§ tags from the raw history store.
pub struct CtxExpandTool {
    db: Db,
}

impl CtxExpandTool {
    /// The tool over `db` (tag/drop tables).
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for CtxExpandTool {
    fn name(&self) -> &str {
        "ctx_expand"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(super::CTX_GUIDANCE)
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "ctx_expand",
            "Recover the original content behind a §N§ tag or a `[dropped §N§]` placeholder \
             from history. Pass the tag number or a copied handle (e.g. \"§12§\", \"tag 12\", \
             \"[dropped §12§]\"). Returns that one item whole — a text, or a tool call with \
             its full input and output. Items already dropped via ctx_reduce are reported as \
             [dropped] but their content is still recovered.",
            object_param(
                vec![(
                    "tag",
                    json!({
                        "type": ["integer", "string"],
                        "description": "Tag number from a §N§ tag or a [dropped §N§] placeholder, not a message ordinal. Returns that one item in full. Use alone.",
                    }),
                )],
                &[],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_expand(&self.db, &ctx, &input).await })
    }
}

/// Resolve the `tag` argument: an integer is itself; a string is
/// normalized and must resolve to exactly one tag. Every failure is the
/// model-facing `Error: …` text (content, not a tool failure, mirroring the
/// reference returning a string).
fn parse_expand_tag(input: &Value) -> Result<u64, String> {
    let Some(value) = input.get("tag") else {
        return Err("Error: 'tag' must be provided.".to_owned());
    };
    match value {
        Value::Null => Err("Error: 'tag' must be provided.".to_owned()),
        Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| "Error: 'tag' must be a non-negative integer.".to_owned()),
        Value::String(text) => {
            if text.trim().is_empty() {
                return Err("Error: 'tag' must be provided.".to_owned());
            }
            let tags = parse_range_string(text)
                .map_err(|message| format!("Error: Invalid tag. {message}"))?;
            match tags.as_slice() {
                [tag] => Ok(*tag),
                other => Err(format!(
                    "Error: 'tag' must resolve to exactly one tag (got {}).",
                    other.len()
                )),
            }
        }
        other => Err(format!(
            "Error: 'tag' must be a tag number or a copied handle, got {other}."
        )),
    }
}

async fn run_expand(db: &Db, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let tag = match parse_expand_tag(input) {
        Ok(tag) => tag,
        Err(message) => return ok_text(message),
    };
    let section = match recover(db, &ctx.session_id, tag).await {
        Ok(None) => format!("§{tag}§ unknown tag for this session."),
        Ok(Some(Recovered::Missing)) => {
            format!("§{tag}§ raw content no longer recoverable (message removed or reshaped).")
        }
        Ok(Some(Recovered::Found { content, dropped })) => {
            let state = match dropped {
                Some(DropState::Applied) => "dropped",
                Some(DropState::Queued) => "drop queued",
                Some(DropState::Unknown) => "unknown drop state",
                None => "live",
            };
            // Tag-mode recovery is WHOLE: no truncation (the reference's
            // 15000-token budget governs its ordinal-range mode, which ox
            // does not implement). The char count is informational only.
            let total = content.chars().count();
            format!("§{tag}§ [{state}] ({total} chars)\n{content}")
        }
        Err(error) => format!("§{tag}§ error: {error}"),
    };
    ok_text(section)
}
