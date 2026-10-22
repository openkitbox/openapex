//! `session.stats` — upstream `packages/core/src/session/stats.ts`.
//!
//! `GET /api/experimental/session/stats?from&to&project&timezone&tools` walks
//! the same four sources upstream does, in 31-day windows so a long range does
//! not have to hold every row at once:
//!
//! 1. `session_message` rows of type user/assistant → sessions, subagents,
//!    prompts, steps, token totals, cost, per-day activity and per-model usage;
//! 2. `session_message` tool parts (`data.content[].state`) → tool reliability
//!    (`tools=none|summary|detail`);
//! 3. `session.usage.recorded` events whose `source` is `compaction` → the
//!    usage the compaction step itself spent;
//! 4. `session_v2.fork_session_id` — a fork re-uses its parent's history, so
//!    messages older than the fork's own `time_created` are not counted twice.
//!
//! Deviations from upstream, both deliberate:
//! - day bucketing uses `jiff`'s IANA database instead of ICU. The same zone
//!   names resolve; an unknown zone fails with upstream's 400
//!   `InvalidRequestError` rather than silently bucketing in UTC.
//! - `tools` and `project` are validated here instead of by the protocol
//!   schema layer (ox has no generated schema validators); the error shape is
//!   the same `InvalidRequestError`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result as AnyResult;
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Serialize;
use serde_json::json;

use ox_db::Db;
use ox_db::turso::{Row, Value as SqlValue};
use ox_schema::id::now_millis;
use ox_schema::support::{ModelRef, TokenCache, TokenUsage};

use super::session::invalid_request;
use super::state;

/// Upstream `Window`: aggregation is chunked so a multi-year range streams.
const WINDOW: i64 = 31 * 24 * 60 * 60 * 1_000;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolMode {
    None,
    Summary,
    Detail,
}

#[derive(Debug, Clone)]
pub(crate) struct Input {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub project: Option<String>,
    pub timezone: String,
    pub tools: ToolMode,
}

#[derive(Debug)]
pub(crate) enum StatsError {
    InvalidRange,
    InvalidTimezone(String),
    Db(anyhow::Error),
}

// ---------------------------------------------------------------------------
// Output (upstream `SessionStats.Info`, field-for-field)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Envelope {
    data: Stats,
}

#[derive(Debug, Serialize)]
pub(crate) struct Stats {
    range: Range,
    sessions: i64,
    subagents: i64,
    prompts: i64,
    steps: i64,
    tokens: TokenUsage,
    cost: f64,
    tools: Tools,
    #[serde(rename = "activeDays")]
    active_days: i64,
    streak: i64,
    activity: Vec<Activity>,
    models: Vec<ModelUsage>,
}

#[derive(Debug, Serialize)]
struct Range {
    from: i64,
    to: i64,
}

#[derive(Debug, Serialize)]
struct Activity {
    date: String,
    steps: i64,
}

#[derive(Debug, Serialize)]
struct ModelUsage {
    model: ModelRef,
    steps: i64,
    tokens: TokenUsage,
    cost: f64,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
struct ToolTotals {
    calls: i64,
    succeeded: i64,
    failed: i64,
    unfinished: i64,
}

#[derive(Debug, Serialize)]
struct ToolUsage {
    name: String,
    calls: i64,
    succeeded: i64,
    failed: i64,
    unfinished: i64,
    #[serde(rename = "durationP50", skip_serializing_if = "Option::is_none")]
    duration_p50: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum Tools {
    None,
    Summary {
        totals: ToolTotals,
    },
    Detail {
        totals: ToolTotals,
        usage: Vec<ToolUsage>,
    },
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.stats
pub(crate) async fn get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let input = match parse_query(req) {
        Ok(input) => input,
        Err(message) => {
            invalid_request(res, &message);
            return;
        }
    };
    match aggregate(&app.db, input).await {
        Ok(stats) => res.render(Json(Envelope { data: stats })),
        Err(StatsError::InvalidTimezone(timezone)) => {
            invalid_request(res, &format!("Invalid time zone: {timezone}"));
        }
        Err(StatsError::InvalidRange) => {
            invalid_request(res, "Stats range must end after it starts");
        }
        Err(StatsError::Db(error)) => {
            tracing::error!(error = %error, "session stats failed");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(json!({
                "_tag": "UnknownError",
                "message": "Unexpected server error. Check server logs for details.",
            })));
        }
    }
}

fn parse_query(req: &mut Request) -> Result<Input, String> {
    let from = number(req, "from")?;
    let to = number(req, "to")?;
    let project = req
        .query::<String>("project")
        .filter(|value| !value.is_empty());
    let timezone = req
        .query::<String>("timezone")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "UTC".to_owned());
    let tools = match req.query::<String>("tools").as_deref() {
        None | Some("") | Some("summary") => ToolMode::Summary,
        Some("none") => ToolMode::None,
        Some("detail") => ToolMode::Detail,
        Some(other) => return Err(format!("Invalid tools mode: {other}")),
    };
    Ok(Input {
        from,
        to,
        project,
        timezone,
        tools,
    })
}

/// A millisecond timestamp arriving as a query string. Upstream decodes it
/// through `NumberFromString`, so fractional and exponent forms are legal.
fn number(req: &mut Request, name: &str) -> Result<Option<i64>, String> {
    let Some(raw) = req.query::<String>(name) else {
        return Ok(None);
    };
    match raw.parse::<f64>() {
        Ok(value) if value.is_finite() => Ok(Some(value as i64)),
        _ => Err(format!("Invalid number: {raw}")),
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

struct Acc {
    sessions: BTreeSet<String>,
    subagents: BTreeSet<String>,
    prompts: i64,
    steps: i64,
    tokens: TokenUsage,
    cost: f64,
    activity: BTreeMap<String, i64>,
    models: BTreeMap<String, ModelUsage>,
}

impl Acc {
    fn empty() -> Self {
        Self {
            sessions: BTreeSet::new(),
            subagents: BTreeSet::new(),
            prompts: 0,
            steps: 0,
            tokens: empty_tokens(),
            cost: 0.0,
            activity: BTreeMap::new(),
            models: BTreeMap::new(),
        }
    }

    fn add_tokens(&mut self, tokens: &TokenUsage) {
        add_tokens(&mut self.tokens, tokens);
    }
}

pub(crate) async fn aggregate(db: &Db, input: Input) -> Result<Stats, StatsError> {
    let to = input.to.unwrap_or_else(now_millis);
    let timezone = jiff::tz::TimeZone::get(&input.timezone)
        .map_err(|_| StatsError::InvalidTimezone(input.timezone.clone()))?;
    let from = match input.from {
        Some(from) => {
            if from >= to {
                return Err(StatsError::InvalidRange);
            }
            from
        }
        None => earliest_message(db, input.project.as_deref(), to)
            .await
            .map_err(StatsError::Db)?
            .unwrap_or(to),
    };

    let mut acc = Acc::empty();
    let mut totals = ToolTotals::default();
    let mut tools: BTreeMap<String, (ToolTotals, Vec<f64>)> = BTreeMap::new();

    for window in windows(from, to) {
        for row in message_rows(db, window, input.project.as_deref())
            .await
            .map_err(StatsError::Db)?
        {
            fold_message(&mut acc, &timezone, &row);
        }
        match input.tools {
            ToolMode::None => {}
            ToolMode::Summary => {
                let row = tool_summary(db, window, input.project.as_deref())
                    .await
                    .map_err(StatsError::Db)?;
                add_tool_totals(&mut totals, row);
            }
            ToolMode::Detail => {
                for row in tool_rows(db, window, input.project.as_deref())
                    .await
                    .map_err(StatsError::Db)?
                {
                    add_tool_totals(&mut totals, row.status_totals());
                    let Some(name) = row.name.clone() else {
                        continue;
                    };
                    let entry = tools
                        .entry(name)
                        .or_insert((ToolTotals::default(), Vec::new()));
                    add_tool_totals(&mut entry.0, row.status_totals());
                    if let Some(duration) = row.duration {
                        entry.1.push(duration);
                    }
                }
            }
        }
    }

    // Compaction usage lives in the event log, not in a message row.
    let compaction = compaction_usage(db, (from, to), input.project.as_deref())
        .await
        .map_err(StatsError::Db)?;
    acc.add_tokens(&compaction.0);
    acc.cost += compaction.1;

    let days: Vec<(String, i64)> = acc.activity.into_iter().collect();
    let active_days = days.len() as i64;
    let streak = longest_streak(days.iter().map(|(date, _)| date.as_str()));

    let mut models: Vec<ModelUsage> = acc.models.into_values().collect();
    models.sort_by_key(|model| std::cmp::Reverse(tokens_total(&model.tokens)));

    let tools = match input.tools {
        ToolMode::None => Tools::None,
        ToolMode::Summary => Tools::Summary { totals },
        ToolMode::Detail => {
            let mut usage: Vec<ToolUsage> = tools
                .into_iter()
                .map(|(name, (totals, durations))| ToolUsage {
                    name,
                    calls: totals.calls,
                    succeeded: totals.succeeded,
                    failed: totals.failed,
                    unfinished: totals.unfinished,
                    duration_p50: median(durations),
                })
                .collect();
            usage.sort_by_key(|tool| std::cmp::Reverse(tool.calls));
            Tools::Detail { totals, usage }
        }
    };

    Ok(Stats {
        range: Range { from, to },
        sessions: acc.sessions.len() as i64,
        subagents: acc.subagents.len() as i64,
        prompts: acc.prompts,
        steps: acc.steps,
        tokens: acc.tokens,
        cost: acc.cost,
        tools,
        active_days,
        streak,
        activity: days
            .into_iter()
            .map(|(date, steps)| Activity { date, steps })
            .collect(),
        models,
    })
}

fn windows(from: i64, to: i64) -> Vec<(i64, i64)> {
    let count = ((to - from) + WINDOW - 1) / WINDOW;
    let count = count.max(1);
    (0..count)
        .map(|index| {
            let start = from + index * WINDOW;
            (start, to.min(start + WINDOW))
        })
        .collect()
}

/// One user/assistant message row with the usage fields upstream extracts.
struct MsgRow {
    session_id: String,
    parent_id: Option<String>,
    kind: String,
    created: i64,
    provider_id: Option<String>,
    model_id: Option<String>,
    variant: Option<String>,
    tokens: TokenUsage,
    cost: f64,
}

impl MsgRow {
    fn read(row: &Row) -> AnyResult<Self> {
        Ok(Self {
            session_id: text(&row.get_value(0)?).unwrap_or_default(),
            parent_id: text(&row.get_value(1)?),
            kind: text(&row.get_value(2)?).unwrap_or_default(),
            created: integer(&row.get_value(3)?),
            provider_id: text(&row.get_value(4)?),
            model_id: text(&row.get_value(5)?),
            variant: text(&row.get_value(6)?),
            tokens: TokenUsage {
                input: integer(&row.get_value(7)?),
                output: integer(&row.get_value(8)?),
                reasoning: integer(&row.get_value(9)?),
                cache: TokenCache {
                    read: integer(&row.get_value(10)?),
                    write: integer(&row.get_value(11)?),
                },
            },
            cost: real(&row.get_value(12)?),
        })
    }
}

fn fold_message(acc: &mut Acc, timezone: &jiff::tz::TimeZone, row: &MsgRow) {
    if row.parent_id.is_none() {
        acc.sessions.insert(row.session_id.clone());
    } else {
        acc.subagents.insert(row.session_id.clone());
    }
    if row.kind == "user" {
        if row.parent_id.is_none() {
            acc.prompts += 1;
        }
        return;
    }

    acc.steps += 1;
    acc.add_tokens(&row.tokens);
    acc.cost += row.cost;
    let day = date_key(timezone, row.created);
    *acc.activity.entry(day).or_insert(0) += 1;

    let (Some(provider_id), Some(model_id)) = (&row.provider_id, &row.model_id) else {
        return;
    };
    let key = format!(
        "{provider_id}/{model_id}#{}",
        row.variant.clone().unwrap_or_default()
    );
    let model = acc.models.entry(key).or_insert_with(|| ModelUsage {
        model: ModelRef {
            id: model_id.clone(),
            provider_id: provider_id.clone(),
            variant: row.variant.clone(),
        },
        steps: 0,
        tokens: empty_tokens(),
        cost: 0.0,
    });
    model.steps += 1;
    model.cost += row.cost;
    add_tokens(&mut model.tokens, &row.tokens);
}

async fn earliest_message(db: &Db, project: Option<&str>, to: i64) -> AnyResult<Option<i64>> {
    let mut sql = String::from(
        "SELECT min(message.time_created) \
         FROM session_message AS message \
         JOIN session_v2 AS session ON session.id = message.session_id \
         WHERE message.type IN ('user', 'assistant') \
           AND message.time_created < ?1 \
           AND (session.fork_session_id IS NULL OR message.time_created >= session.time_created)",
    );
    let mut params = vec![SqlValue::Integer(to)];
    if let Some(project) = project {
        sql.push_str(" AND session.project_id = ?2");
        params.push(SqlValue::Text(project.to_owned()));
    }
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let value = row.get_value(0)?;
    Ok(match value {
        SqlValue::Null => None,
        other => Some(integer(&other)),
    })
}

async fn message_rows(
    db: &Db,
    window: (i64, i64),
    project: Option<&str>,
) -> AnyResult<Vec<MsgRow>> {
    let mut sql = String::from(
        "SELECT \
           message.session_id, \
           session.parent_id, \
           message.type, \
           message.time_created, \
           json_extract(message.data, '$.model.providerID'), \
           json_extract(message.data, '$.model.id'), \
           json_extract(message.data, '$.model.variant'), \
           json_extract(message.data, '$.tokens.input'), \
           json_extract(message.data, '$.tokens.output'), \
           json_extract(message.data, '$.tokens.reasoning'), \
           json_extract(message.data, '$.tokens.cache.read'), \
           json_extract(message.data, '$.tokens.cache.write'), \
           json_extract(message.data, '$.cost') \
         FROM session_message AS message \
         JOIN session_v2 AS session ON session.id = message.session_id \
         WHERE message.type IN ('user', 'assistant') \
           AND message.time_created >= ?1 \
           AND message.time_created < ?2 \
           AND (session.fork_session_id IS NULL OR message.time_created >= session.time_created)",
    );
    let mut params = vec![SqlValue::Integer(window.0), SqlValue::Integer(window.1)];
    if let Some(project) = project {
        sql.push_str(" AND session.project_id = ?3");
        params.push(SqlValue::Text(project.to_owned()));
    }
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(MsgRow::read(&row)?);
    }
    Ok(out)
}

/// `tools=summary`: one row of counts, no per-tool breakdown.
async fn tool_summary(db: &Db, window: (i64, i64), project: Option<&str>) -> AnyResult<ToolTotals> {
    let mut sql = String::from(
        "SELECT \
           count(*), \
           count(*) FILTER (WHERE status = 'completed'), \
           count(*) FILTER (WHERE status = 'error'), \
           count(*) FILTER (WHERE status IS NULL OR status NOT IN ('completed', 'error')) \
         FROM ( \
           SELECT json_extract(content.value, '$.state.status') AS status \
           FROM session_message AS message \
           JOIN session_v2 AS session ON session.id = message.session_id, \
             json_each(message.data, '$.content') AS content \
           WHERE message.type = 'assistant' \
             AND message.time_created >= ?1 \
             AND message.time_created < ?2 \
             AND (session.fork_session_id IS NULL OR message.time_created >= session.time_created) \
             AND json_extract(content.value, '$.type') = 'tool'",
    );
    let mut params = vec![SqlValue::Integer(window.0), SqlValue::Integer(window.1)];
    if let Some(project) = project {
        sql.push_str(" AND session.project_id = ?3");
        params.push(SqlValue::Text(project.to_owned()));
    }
    sql.push(')');

    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let Some(row) = rows.next().await? else {
        return Ok(ToolTotals::default());
    };
    Ok(ToolTotals {
        calls: integer(&row.get_value(0)?),
        succeeded: integer(&row.get_value(1)?),
        failed: integer(&row.get_value(2)?),
        unfinished: integer(&row.get_value(3)?),
    })
}

struct ToolRow {
    name: Option<String>,
    status: Option<String>,
    duration: Option<f64>,
}

impl ToolRow {
    fn status_totals(&self) -> ToolTotals {
        let mut totals = ToolTotals {
            calls: 1,
            ..ToolTotals::default()
        };
        match self.status.as_deref() {
            Some("completed") => totals.succeeded = 1,
            Some("error") => totals.failed = 1,
            _ => totals.unfinished = 1,
        }
        totals
    }
}

/// `tools=detail`: one row per tool call, with the duration upstream medians.
async fn tool_rows(db: &Db, window: (i64, i64), project: Option<&str>) -> AnyResult<Vec<ToolRow>> {
    let mut sql = String::from(
        "SELECT \
           json_extract(content.value, '$.name'), \
           json_extract(content.value, '$.state.status'), \
           CASE \
             WHEN json_extract(content.value, '$.time.completed') IS NULL THEN NULL \
             ELSE json_extract(content.value, '$.time.completed') \
               - coalesce(json_extract(content.value, '$.time.ran'), json_extract(content.value, '$.time.created')) \
           END \
         FROM session_message AS message \
         JOIN session_v2 AS session ON session.id = message.session_id, \
           json_each(message.data, '$.content') AS content \
         WHERE message.type = 'assistant' \
           AND message.time_created >= ?1 \
           AND message.time_created < ?2 \
           AND (session.fork_session_id IS NULL OR message.time_created >= session.time_created) \
           AND json_extract(content.value, '$.type') = 'tool'",
    );
    let mut params = vec![SqlValue::Integer(window.0), SqlValue::Integer(window.1)];
    if let Some(project) = project {
        sql.push_str(" AND session.project_id = ?3");
        params.push(SqlValue::Text(project.to_owned()));
    }
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let duration = row.get_value(2)?;
        out.push(ToolRow {
            name: text(&row.get_value(0)?),
            status: text(&row.get_value(1)?),
            duration: match duration {
                SqlValue::Null => None,
                other => Some(real(&other)),
            },
        });
    }
    Ok(out)
}

/// Usage spent by compaction, recorded as `session.usage.recorded` events with
/// `source: "compaction"` (upstream sums these on top of the message rows).
async fn compaction_usage(
    db: &Db,
    range: (i64, i64),
    project: Option<&str>,
) -> AnyResult<(TokenUsage, f64)> {
    let mut sql = String::from(
        "SELECT \
           json_extract(data, '$.tokens.input'), \
           json_extract(data, '$.tokens.output'), \
           json_extract(data, '$.tokens.reasoning'), \
           json_extract(data, '$.tokens.cache.read'), \
           json_extract(data, '$.tokens.cache.write'), \
           json_extract(data, '$.cost') \
         FROM event \
         WHERE type = 'session.usage.recorded@v1' \
           AND json_extract(data, '$.source') = 'compaction' \
           AND created >= ?1 \
           AND created < ?2",
    );
    let mut params = vec![SqlValue::Integer(range.0), SqlValue::Integer(range.1)];
    if let Some(project) = project {
        sql.push_str(" AND aggregate_id IN (SELECT id FROM session_v2 WHERE project_id = ?3)");
        params.push(SqlValue::Text(project.to_owned()));
    }
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let mut tokens = empty_tokens();
    let mut cost = 0.0;
    while let Some(row) = rows.next().await? {
        let row_tokens = TokenUsage {
            input: integer(&row.get_value(0)?),
            output: integer(&row.get_value(1)?),
            reasoning: integer(&row.get_value(2)?),
            cache: TokenCache {
                read: integer(&row.get_value(3)?),
                write: integer(&row.get_value(4)?),
            },
        };
        add_tokens(&mut tokens, &row_tokens);
        cost += real(&row.get_value(5)?);
    }
    Ok((tokens, cost))
}

// ---------------------------------------------------------------------------
// Small helpers (pure)
// ---------------------------------------------------------------------------

fn text(value: &SqlValue) -> Option<String> {
    match value {
        SqlValue::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn integer(value: &SqlValue) -> i64 {
    match value {
        SqlValue::Integer(number) => *number,
        SqlValue::Real(number) => *number as i64,
        _ => 0,
    }
}

fn real(value: &SqlValue) -> f64 {
    match value {
        SqlValue::Integer(number) => *number as f64,
        SqlValue::Real(number) => *number,
        _ => 0.0,
    }
}

fn empty_tokens() -> TokenUsage {
    TokenUsage {
        input: 0,
        output: 0,
        reasoning: 0,
        cache: TokenCache { read: 0, write: 0 },
    }
}

fn add_tokens(target: &mut TokenUsage, source: &TokenUsage) {
    target.input += source.input;
    target.output += source.output;
    target.reasoning += source.reasoning;
    target.cache.read += source.cache.read;
    target.cache.write += source.cache.write;
}

fn add_tool_totals(target: &mut ToolTotals, source: ToolTotals) {
    target.calls += source.calls;
    target.succeeded += source.succeeded;
    target.failed += source.failed;
    target.unfinished += source.unfinished;
}

/// Upstream `TokenUsage.total`.
fn tokens_total(tokens: &TokenUsage) -> i64 {
    tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write
}

/// `YYYY-MM-DD` in the requested zone (upstream `makeDateKey`).
fn date_key(timezone: &jiff::tz::TimeZone, millis: i64) -> String {
    let timestamp = jiff::Timestamp::from_millisecond(millis).unwrap_or_default();
    let zoned = timestamp.to_zoned(timezone.clone());
    format!(
        "{:04}-{:02}-{:02}",
        zoned.year(),
        zoned.month(),
        zoned.day()
    )
}

/// Longest run of consecutive active days (upstream `longestStreak`).
fn longest_streak<'a>(days: impl Iterator<Item = &'a str>) -> i64 {
    let mut previous: Option<i64> = None;
    let mut current = 0;
    let mut longest = 0;
    for day in days {
        let ordinal = day_ordinal(day);
        current = match previous {
            Some(previous) if ordinal - previous == 1 => current + 1,
            _ => 1,
        };
        longest = longest.max(current);
        previous = Some(ordinal);
    }
    longest
}

/// Days since the Unix epoch for a `YYYY-MM-DD` key (proleptic Gregorian).
fn day_ordinal(value: &str) -> i64 {
    let mut parts = value.split('-');
    let year: i64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(1970);
    let month: i64 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(1);
    let day: i64 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(1);
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn median(values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 2023-11-14T22:13:20Z — a fixed clock so day keys are stable.
    const BASE: i64 = 1_700_000_000_000;
    const DAY: i64 = 24 * 60 * 60 * 1_000;

    async fn seed_session(db: &Db, id: &str, parent: Option<&str>, created: i64) {
        exec(
            db,
            "INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, version, time_created, time_updated) \
             VALUES (?1, 'proj', ?2, ?1, '/tmp', '1', ?3, ?3)",
            vec![
                SqlValue::Text(id.to_owned()),
                match parent {
                    Some(parent) => SqlValue::Text(parent.to_owned()),
                    None => SqlValue::Null,
                },
                SqlValue::Integer(created),
            ],
        )
        .await;
    }

    async fn seed_fork(db: &Db, id: &str, fork_of: &str, created: i64) {
        exec(
            db,
            "INSERT INTO session_v2 (id, project_id, fork_session_id, slug, directory, version, time_created, time_updated) \
             VALUES (?1, 'proj', ?2, ?1, '/tmp', '1', ?3, ?3)",
            vec![
                SqlValue::Text(id.to_owned()),
                SqlValue::Text(fork_of.to_owned()),
                SqlValue::Integer(created),
            ],
        )
        .await;
    }

    async fn seed_message(
        db: &Db,
        id: &str,
        session: &str,
        kind: &str,
        created: i64,
        data: serde_json::Value,
    ) {
        exec(
            db,
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, (SELECT coalesce(max(seq), 0) + 1 FROM session_message WHERE session_id = ?2), ?4, ?4, ?5)",
            vec![
                SqlValue::Text(id.to_owned()),
                SqlValue::Text(session.to_owned()),
                SqlValue::Text(kind.to_owned()),
                SqlValue::Integer(created),
                SqlValue::Text(data.to_string()),
            ],
        )
        .await;
    }

    async fn seed_usage_event(
        db: &Db,
        id: &str,
        session: &str,
        created: i64,
        tokens: i64,
        cost: f64,
    ) {
        exec(
            db,
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, 1) \
             ON CONFLICT(aggregate_id) DO UPDATE SET seq = seq + 1",
            vec![SqlValue::Text(session.to_owned())],
        )
        .await;
        let data = json!({
            "sessionID": session,
            "source": "compaction",
            "cost": cost,
            "tokens": { "input": tokens, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
        });
        exec(
            db,
            "INSERT INTO event (id, aggregate_id, seq, created, type, data) \
             VALUES (?1, ?2, (SELECT seq FROM event_sequence WHERE aggregate_id = ?2), ?3, 'session.usage.recorded@v1', ?4)",
            vec![
                SqlValue::Text(id.to_owned()),
                SqlValue::Text(session.to_owned()),
                SqlValue::Integer(created),
                SqlValue::Text(data.to_string()),
            ],
        )
        .await;
    }

    async fn exec(db: &Db, sql: &str, params: Vec<SqlValue>) {
        let conn = db.acquire().await.unwrap();
        conn.execute(sql, params).await.unwrap();
    }

    fn assistant(tokens: i64, cost: f64, content: serde_json::Value) -> serde_json::Value {
        json!({
            "id": "msg",
            "time": { "created": BASE },
            "agent": "build",
            "model": { "id": "k3-256k", "providerID": "cardinal" },
            "content": content,
            "cost": cost,
            "tokens": {
                "input": tokens,
                "output": 0,
                "reasoning": 0,
                "cache": { "read": 0, "write": 0 }
            }
        })
    }

    fn input(from: i64, to: i64) -> Input {
        Input {
            from: Some(from),
            to: Some(to),
            project: None,
            timezone: "UTC".to_owned(),
            tools: ToolMode::None,
        }
    }

    #[tokio::test]
    async fn counts_sessions_prompts_steps_and_tokens() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, "root", None, BASE).await;
        seed_session(&db, "child", Some("root"), BASE).await;
        seed_message(
            &db,
            "u1",
            "root",
            "user",
            BASE,
            json!({ "id": "u1", "text": "hi" }),
        )
        .await;
        seed_message(
            &db,
            "a1",
            "root",
            "assistant",
            BASE + 1_000,
            assistant(100, 0.5, json!([])),
        )
        .await;
        seed_message(
            &db,
            "a2",
            "root",
            "assistant",
            BASE + DAY,
            assistant(200, 1.5, json!([])),
        )
        .await;
        seed_message(
            &db,
            "a3",
            "child",
            "assistant",
            BASE + DAY + 5,
            assistant(50, 0.25, json!([])),
        )
        .await;

        let stats = aggregate(&db, input(BASE, BASE + 10 * DAY)).await.unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.subagents, 1);
        assert_eq!(stats.prompts, 1);
        assert_eq!(stats.steps, 3);
        assert_eq!(stats.tokens.input, 350);
        assert!((stats.cost - 2.25).abs() < 1e-9);
        assert_eq!(stats.active_days, 2);
        // 2023-11-14 and 2023-11-15 are consecutive, so the streak is 2.
        assert_eq!(stats.streak, 2);
        assert_eq!(stats.activity.len(), 2);
        assert_eq!(stats.activity[0].steps, 1);
        assert_eq!(stats.activity[1].steps, 2);
        assert_eq!(stats.models.len(), 1);
        assert_eq!(stats.models[0].steps, 3);
        assert_eq!(stats.models[0].tokens.input, 350);
    }

    #[tokio::test]
    async fn streak_counts_consecutive_days() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, "root", None, BASE).await;
        for day in [0, 1, 2, 5] {
            seed_message(
                &db,
                &format!("a{day}"),
                "root",
                "assistant",
                BASE + day * DAY,
                assistant(1, 0.0, json!([])),
            )
            .await;
        }
        let stats = aggregate(&db, input(BASE, BASE + 10 * DAY)).await.unwrap();
        assert_eq!(stats.active_days, 4);
        assert_eq!(stats.streak, 3);
    }

    #[tokio::test]
    async fn activity_buckets_in_the_requested_timezone() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, "root", None, BASE).await;
        // 2023-11-14T22:13:20Z is 2023-11-15 06:13 in Shanghai.
        seed_message(
            &db,
            "a1",
            "root",
            "assistant",
            BASE,
            assistant(1, 0.0, json!([])),
        )
        .await;

        let utc = aggregate(&db, input(BASE, BASE + DAY)).await.unwrap();
        assert_eq!(utc.activity[0].date, "2023-11-14");

        let mut shanghai = input(BASE, BASE + DAY);
        shanghai.timezone = "Asia/Shanghai".to_owned();
        let local = aggregate(&db, shanghai).await.unwrap();
        assert_eq!(local.activity[0].date, "2023-11-15");
    }

    #[tokio::test]
    async fn fork_history_before_the_fork_is_not_counted_twice() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, "root", None, BASE).await;
        seed_message(
            &db,
            "a1",
            "root",
            "assistant",
            BASE,
            assistant(10, 0.0, json!([])),
        )
        .await;
        // The fork starts at BASE + DAY and carries a copy of the row above,
        // which must not be counted a second time.
        seed_fork(&db, "fork", "root", BASE + DAY).await;
        seed_message(
            &db,
            "a1-copy",
            "fork",
            "assistant",
            BASE,
            assistant(10, 0.0, json!([])),
        )
        .await;
        seed_message(
            &db,
            "a2",
            "fork",
            "assistant",
            BASE + DAY + 1,
            assistant(20, 0.0, json!([])),
        )
        .await;

        let stats = aggregate(&db, input(BASE, BASE + 10 * DAY)).await.unwrap();
        assert_eq!(stats.steps, 2);
        assert_eq!(stats.tokens.input, 30);
    }

    #[tokio::test]
    async fn tool_summary_and_detail_agree() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, "root", None, BASE).await;
        let content = json!([
            { "type": "tool", "id": "t1", "name": "bash",
              "state": { "status": "completed", "input": {}, "content": [] },
              "time": { "created": 1_000, "ran": 1_100, "completed": 1_300 } },
            { "type": "tool", "id": "t2", "name": "bash",
              "state": { "status": "error", "input": {}, "error": { "name": "ToolError", "message": "boom" } },
              "time": { "created": 2_000 } },
            { "type": "tool", "id": "t3", "name": "read",
              "state": { "status": "running", "input": {}, "metadata": {} },
              "time": { "created": 3_000 } }
        ]);
        seed_message(
            &db,
            "a1",
            "root",
            "assistant",
            BASE,
            assistant(1, 0.0, content),
        )
        .await;

        let mut summary = input(BASE, BASE + DAY);
        summary.tools = ToolMode::Summary;
        let stats = aggregate(&db, summary).await.unwrap();
        let Tools::Summary { totals } = stats.tools else {
            panic!("expected summary");
        };
        assert_eq!(totals.calls, 3);
        assert_eq!(totals.succeeded, 1);
        assert_eq!(totals.failed, 1);
        assert_eq!(totals.unfinished, 1);

        let mut detail = input(BASE, BASE + DAY);
        detail.tools = ToolMode::Detail;
        let stats = aggregate(&db, detail).await.unwrap();
        let Tools::Detail { totals, usage } = stats.tools else {
            panic!("expected detail");
        };
        assert_eq!(totals.calls, 3);
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].name, "bash");
        assert_eq!(usage[0].calls, 2);
        assert_eq!(usage[0].duration_p50, Some(200.0));
        assert_eq!(usage[1].name, "read");
        assert_eq!(usage[1].duration_p50, None);
    }

    #[tokio::test]
    async fn compaction_usage_is_added_to_the_totals() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, "root", None, BASE).await;
        seed_message(
            &db,
            "a1",
            "root",
            "assistant",
            BASE,
            assistant(10, 0.5, json!([])),
        )
        .await;
        seed_usage_event(&db, "e1", "root", BASE + 1, 1_000, 2.0).await;

        let stats = aggregate(&db, input(BASE, BASE + DAY)).await.unwrap();
        assert_eq!(stats.tokens.input, 1_010);
        assert!((stats.cost - 2.5).abs() < 1e-9);
        // Compaction spend is usage, not activity.
        assert_eq!(stats.steps, 1);
    }

    #[tokio::test]
    async fn rejects_inverted_ranges_and_unknown_timezones() {
        let db = Db::open_in_memory().await.unwrap();
        let inverted = aggregate(&db, input(BASE + 1, BASE)).await;
        assert!(matches!(inverted, Err(StatsError::InvalidRange)));

        let mut unknown = input(BASE, BASE + DAY);
        unknown.timezone = "Mars/Olympus".to_owned();
        assert!(matches!(
            aggregate(&db, unknown).await,
            Err(StatsError::InvalidTimezone(zone)) if zone == "Mars/Olympus"
        ));
    }

    #[tokio::test]
    async fn missing_from_falls_back_to_the_first_message() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, "root", None, BASE).await;
        seed_message(
            &db,
            "a1",
            "root",
            "assistant",
            BASE,
            assistant(5, 0.0, json!([])),
        )
        .await;

        let mut open = input(BASE, BASE + DAY);
        open.from = None;
        let stats = aggregate(&db, open).await.unwrap();
        assert_eq!(stats.range.from, BASE);
        assert_eq!(stats.steps, 1);
    }

    #[test]
    fn day_ordinals_are_consecutive() {
        assert_eq!(day_ordinal("1970-01-01"), 0);
        assert_eq!(day_ordinal("1970-01-02") - day_ordinal("1970-01-01"), 1);
        assert_eq!(day_ordinal("2023-11-15") - day_ordinal("2023-11-14"), 1);
        assert_eq!(day_ordinal("2024-03-01") - day_ordinal("2024-02-29"), 1);
    }
}
