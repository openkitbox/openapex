//! Registration of the `ctx_*` tools into the existing tool registry.
//!
//! The two tools here are the model-facing shells over
//! [`crate::mctx::tagger`]; the tag/drop semantics live there. They are
//! registered via [`crate::tool::ToolRegistry::register_mctx`], which the
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

use crate::cardinal::wire::ToolDef;
use crate::db::Db;
use crate::runner::{Tool, ToolContext, ToolOutput};
use crate::tool::{object_param, ok_text};

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::mctx::tagger::Tagger;
    use crate::mctx::tagger::test_support::store_messages;

    const SESSION: &str = "ses_test";

    async fn db() -> Db {
        Db::open_in_memory().await.unwrap()
    }

    fn tool_ctx() -> ToolContext {
        ToolContext {
            session_id: SESSION.to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    /// A tagged session: one user message and one assistant message (text +
    /// tool part), both stored in `session_message` and tagged 1..4.
    async fn tagged_session(db: &Db) -> Vec<u64> {
        let messages = vec![
            json!({
                "id": "msg_u1", "type": "user", "time": { "created": 1 },
                "text": "hello world",
            }),
            json!({
                "id": "msg_a1", "type": "assistant", "time": { "created": 2 },
                "agent": "build",
                "model": { "id": "m", "providerID": "p" },
                "content": [
                    { "type": "text", "text": "thinking out loud" },
                    {
                        "type": "tool", "id": "call_1", "name": "read",
                        "state": {
                            "status": "completed",
                            "input": { "path": "a.txt" },
                            "content": [ { "type": "text", "text": "file body" } ],
                        },
                        "time": { "created": 3 },
                    },
                ],
            }),
        ];
        // Store the raw rows so ctx_expand can recover from history.
        store_messages(db, SESSION, &messages).await;
        let items = Tagger.assign(db, SESSION, &messages).await.unwrap();
        items.into_iter().map(|item| item.tag).collect()
    }

    // -- parse_range_string --------------------------------------------------

    #[test]
    fn range_patterns_compile() {
        // The lazy statics use `expect`; a typo would panic on first use.
        assert_eq!(handle_patterns().len(), 3);
    }

    #[test]
    fn parse_range_string_handles_all_documented_shapes() {
        assert_eq!(parse_range_string("5").unwrap(), vec![5]);
        assert_eq!(parse_range_string("3-5").unwrap(), vec![3, 4, 5]);
        assert_eq!(parse_range_string("1,2,9").unwrap(), vec![1, 2, 9]);
        assert_eq!(
            parse_range_string("1-5,8,12-15").unwrap(),
            vec![1, 2, 3, 4, 5, 8, 12, 13, 14, 15]
        );
    }

    #[test]
    fn parse_range_string_normalizes_copied_tag_handles() {
        assert_eq!(
            parse_range_string("§302§-§380§").unwrap(),
            (302..=380).collect::<Vec<u64>>()
        );
        assert_eq!(parse_range_string("tag 12").unwrap(), vec![12]);
        assert_eq!(parse_range_string("[dropped §302§]").unwrap(), vec![302]);
        // The trailing § is optional and surrounding whitespace is tolerated.
        assert_eq!(parse_range_string("  §12  ").unwrap(), vec![12]);
        assert_eq!(parse_range_string("1, §2§, tag 3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parse_range_string_dedupes_and_sorts() {
        assert_eq!(parse_range_string("3,1,2,3,1").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_range_string("2-4,3,4,5").unwrap(), vec![2, 3, 4, 5]);
    }

    #[test]
    fn parse_range_string_rejects_empty_reversed_and_oversized() {
        assert!(parse_range_string("").is_err());
        assert!(parse_range_string("   ").is_err());
        assert!(parse_range_string("abc").is_err());
        assert!(parse_range_string("5-3").is_err());
        // A single range over the cap.
        assert!(parse_range_string("1-1001").is_err());
        // The total over the cap even though each range is within it.
        assert!(parse_range_string("1-600,700-1300").is_err());
        // Exactly at the cap is accepted.
        assert_eq!(parse_range_string("1-1000").unwrap().len(), 1000);
    }

    // -- ctx_reduce ----------------------------------------------------------

    #[tokio::test]
    async fn ctx_reduce_queues_valid_tags_and_reports_unknown() {
        let db = db().await;
        let tags = tagged_session(&db).await;
        let output = CtxReduceTool::new(db.clone())
            .execute(tool_ctx(), json!({ "drop": format!("{},999", tags[0]) }))
            .await;
        assert!(output.error.is_none());
        let text = output.content.as_str().unwrap();
        assert!(
            text.contains(&format!("Queued for drop: §{}§", tags[0])),
            "got: {text}"
        );
        assert!(text.contains("Unknown tag: §999§."), "got: {text}");
        // The drop row is queued, not applied.
        let state: String = {
            let conn = db.acquire().await.unwrap();
            let mut stmt = conn
                .prepare("SELECT state FROM mc_drop WHERE session_id = ?1 AND tag = ?2")
                .await
                .unwrap();
            stmt.query_row((SESSION, tags[0] as i64))
                .await
                .unwrap()
                .get(0)
                .unwrap()
        };
        assert_eq!(state, "queued");
    }

    #[tokio::test]
    async fn ctx_reduce_accepts_an_integer_array_as_a_tolerated_shape() {
        let db = db().await;
        let tags = tagged_session(&db).await;
        // The older array shape is still understood; the tags are reduced the
        // same way (deduplicated and sorted) before reporting.
        let output = CtxReduceTool::new(db)
            .execute(tool_ctx(), json!({ "drop": [tags[2], tags[0], tags[2]] }))
            .await;
        let text = output.content.as_str().unwrap();
        assert!(
            text.contains(&format!("Queued for drop: §{}§, §{}§", tags[0], tags[2])),
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn ctx_reduce_reports_already_applied_tags() {
        let db = db().await;
        let tags = tagged_session(&db).await;
        let tool = CtxReduceTool::new(db.clone());
        tool.execute(tool_ctx(), json!({ "drop": tags[1].to_string() }))
            .await;
        let queued = crate::mctx::tagger::peek_queued_drops(&db, SESSION)
            .await
            .unwrap();
        crate::mctx::tagger::mark_drops_applied(&db, SESSION, &[queued[0].0])
            .await
            .unwrap();
        let output = tool
            .execute(tool_ctx(), json!({ "drop": tags[1].to_string() }))
            .await;
        let text = output.content.as_str().unwrap();
        assert!(
            text.contains(&format!("Already dropped (no-op): §{}§.", tags[1])),
            "got: {text}"
        );
        assert!(!text.contains("Queued"), "got: {text}");
    }

    #[tokio::test]
    async fn ctx_reduce_reports_missing_and_invalid_drop() {
        let db = db().await;
        let tool = CtxReduceTool::new(db);
        // Missing, null or empty `drop` is the reference provided-error,
        // returned as content (not a tool failure).
        for input in [json!({}), json!({ "drop": "" }), json!({ "drop": null })] {
            let output = tool.execute(tool_ctx(), input.clone()).await;
            assert!(output.error.is_none(), "input {input}");
            assert_eq!(
                output.content.as_str().unwrap(),
                DROP_REQUIRED,
                "input {input}"
            );
        }
        // Present but unparsable → the reference invalid-range-syntax error.
        for input in [
            json!({ "drop": "abc" }),
            json!({ "drop": "5-3" }),
            json!({ "drop": 5 }),
        ] {
            let output = tool.execute(tool_ctx(), input.clone()).await;
            assert!(output.error.is_none(), "input {input}");
            assert!(
                output
                    .content
                    .as_str()
                    .unwrap()
                    .starts_with("Error: Invalid range syntax."),
                "input {input}"
            );
        }
    }

    // -- ctx_expand ----------------------------------------------------------

    #[tokio::test]
    async fn ctx_expand_reports_state_and_content() {
        let db = db().await;
        let tags = tagged_session(&db).await;
        let tool = CtxExpandTool::new(db.clone());

        // Live tag.
        let output = tool.execute(tool_ctx(), json!({ "tag": tags[0] })).await;
        assert!(output.error.is_none());
        let text = output.content.as_str().unwrap();
        assert!(
            text.starts_with(&format!("§{}§ [live] (11 chars)\nhello world", tags[0])),
            "got: {text}"
        );

        // Queue + peek/mark: the tag now reports [dropped], content intact.
        crate::mctx::tagger::queue_drop(&db, SESSION, tags[0], "r")
            .await
            .unwrap();
        let queued = crate::mctx::tagger::peek_queued_drops(&db, SESSION)
            .await
            .unwrap();
        crate::mctx::tagger::mark_drops_applied(&db, SESSION, &[queued[0].0])
            .await
            .unwrap();
        let output = tool.execute(tool_ctx(), json!({ "tag": tags[0] })).await;
        let text = output.content.as_str().unwrap();
        assert!(
            text.starts_with(&format!("§{}§ [dropped] (11 chars)\nhello world", tags[0])),
            "got: {text}"
        );

        // A queued (not yet applied) tag reports [drop queued].
        crate::mctx::tagger::queue_drop(&db, SESSION, tags[1], "r")
            .await
            .unwrap();
        let output = tool.execute(tool_ctx(), json!({ "tag": tags[1] })).await;
        let text = output.content.as_str().unwrap();
        assert!(
            text.starts_with(&format!(
                "§{}§ [drop queued] (17 chars)\nthinking out loud",
                tags[1]
            )),
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn ctx_expand_recovers_tool_state_and_unknown_tags() {
        let db = db().await;
        let tags = tagged_session(&db).await;
        let tool = CtxExpandTool::new(db);
        let output = tool.execute(tool_ctx(), json!({ "tag": tags[2] })).await;
        let text = output.content.as_str().unwrap();
        let expected_state = json!({
            "status": "completed",
            "input": { "path": "a.txt" },
            "content": [ { "type": "text", "text": "file body" } ],
        })
        .to_string();
        assert!(
            text.starts_with(&format!(
                "§{}§ [live] ({} chars)\n{expected_state}",
                tags[2],
                expected_state.chars().count()
            )),
            "got: {text}"
        );
        let output = tool.execute(tool_ctx(), json!({ "tag": 424242 })).await;
        let text = output.content.as_str().unwrap();
        assert!(
            text.contains("§424242§ unknown tag for this session."),
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn ctx_expand_accepts_copied_tag_handles() {
        let db = db().await;
        let tags = tagged_session(&db).await;
        let tool = CtxExpandTool::new(db);
        for input in [
            json!({ "tag": format!("§{}§", tags[0]) }),
            json!({ "tag": format!("[dropped §{}§]", tags[0]) }),
            json!({ "tag": format!("tag {}", tags[0]) }),
        ] {
            let output = tool.execute(tool_ctx(), input.clone()).await;
            let text = output.content.as_str().unwrap();
            assert!(
                text.starts_with(&format!("§{}§ [live] (11 chars)\nhello world", tags[0])),
                "input {input}: {text}"
            );
        }
    }

    #[tokio::test]
    async fn ctx_expand_returns_long_content_whole() {
        let db = db().await;
        let long = "x".repeat(5000);
        let messages = vec![json!({
            "id": "msg_u1", "type": "user", "time": { "created": 1 },
            "text": long,
        })];
        let text_payload = json!({ "text": long }).to_string();
        {
            let conn = db.acquire().await.unwrap();
            conn.execute(
                "INSERT INTO session_v2 (id, project_id, slug, directory, version, cost,
                    tokens_input, tokens_output, tokens_reasoning, tokens_cache_read,
                    tokens_cache_write, time_created, time_updated)
                 VALUES (?1, 'p', 'slug', '/tmp', 'v', 0, 0, 0, 0, 0, 0, 0, 0)",
                [SESSION],
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created,
                    time_updated, data)
                 VALUES ('msg_u1', ?1, 'user', 0, 0, 0, ?2)",
                (SESSION, text_payload.as_str()),
            )
            .await
            .unwrap();
        }
        let items = Tagger.assign(&db, SESSION, &messages).await.unwrap();
        let output = CtxExpandTool::new(db)
            .execute(tool_ctx(), json!({ "tag": items[0].tag }))
            .await;
        let text = output.content.as_str().unwrap();
        // No truncation: the full 5000-char body comes back.
        let expected = format!(
            "§{}§ [live] (5000 chars)\n{}",
            items[0].tag,
            "x".repeat(5000)
        );
        assert_eq!(text, expected);
    }

    #[tokio::test]
    async fn ctx_expand_reports_missing_and_ambiguous_tags() {
        let db = db().await;
        let tool = CtxExpandTool::new(db);
        for input in [json!({}), json!({ "tag": "" }), json!({ "tag": null })] {
            let output = tool.execute(tool_ctx(), input.clone()).await;
            assert!(output.error.is_none(), "input {input}");
            assert_eq!(
                output.content.as_str().unwrap(),
                "Error: 'tag' must be provided.",
                "input {input}"
            );
        }
        // A range that resolves to more than one tag is rejected.
        for input in [json!({ "tag": "3-5" }), json!({ "tag": "1,2" })] {
            let output = tool.execute(tool_ctx(), input.clone()).await;
            assert!(
                output
                    .content
                    .as_str()
                    .unwrap()
                    .starts_with("Error: 'tag' must resolve to exactly one tag"),
                "input {input}"
            );
        }
        let output = tool.execute(tool_ctx(), json!({ "tag": "abc" })).await;
        assert!(
            output
                .content
                .as_str()
                .unwrap()
                .starts_with("Error: Invalid tag."),
            "got: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn register_mctx_adds_exactly_the_five_ctx_tools() {
        let mut registry = crate::runner::ToolRegistry::with_builtins();
        // Not registered by with_builtins: a default-off config must keep
        // the advertised toolset byte-identical.
        assert!(registry.get("ctx_reduce").is_none());
        assert!(registry.get("ctx_expand").is_none());
        assert!(registry.get("ctx_memory").is_none());
        assert!(registry.get("ctx_note").is_none());
        assert!(registry.get("ctx_search").is_none());
        registry.register_mctx(db().await, None);
        assert!(registry.get("ctx_reduce").is_some());
        assert!(registry.get("ctx_expand").is_some());
        assert!(registry.get("ctx_memory").is_some());
        assert!(registry.get("ctx_note").is_some());
        assert!(registry.get("ctx_search").is_some());
        let names: Vec<String> = registry
            .definitions()
            .iter()
            .map(|def| def.function.name.clone())
            .collect();
        assert_eq!(names.len(), 17, "12 builtins + 5 ctx tools: {names:?}");
        for definition in registry.definitions() {
            assert!(!definition.function.description.is_empty());
        }
    }
}
