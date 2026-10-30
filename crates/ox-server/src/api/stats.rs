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
                estimated: None,
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
            estimated: None,
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
        estimated: None,
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
