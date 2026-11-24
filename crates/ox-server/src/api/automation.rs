//! Automation HTTP endpoints (`experimental.automation.*`).
//!
//! The caller's requested directory is the project scope and is echoed in
//! every envelope. A host that never sees an interactive permission prompt
//! must reject shell/code and event watches rather than granting them by
//! default; those are created through the tools in a local session.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Result, bail, ensure};
use ox_automation::cron::Cron;
use ox_automation::store::AutomationStore;
use ox_automation::types::{Action, Info, Status, Target, TimeInfo, Trigger};
use ox_db::Db;
use ox_schema::id;
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use super::compat::{directory_of, envelope};
use super::state;
use crate::project::resolve_project;

/// Mount the six upstream automation routes under `/api/experimental`.
pub(crate) fn mount(api: Router) -> Router {
    api.push(
        Router::with_path("experimental/automation")
            .get(list)
            .post(create)
            .push(
                Router::with_path("{id}")
                    .get(get)
                    .delete(delete)
                    .push(Router::with_path("pause").post(pause))
                    .push(Router::with_path("resume").post(resume)),
            ),
    )
}

/// The upstream `/automation` command template, ported verbatim from
/// `packages/core/src/plugin/command/automation.txt` (reference @ 4d5965762).
pub(crate) const AUTOMATION_COMMAND_PROMPT: &str = "Manage this project's automations.\n\n\
Request: $ARGUMENTS\n\n\
Use the `automation` tools: `tools.automation.create` to add one, and `list`, `pause`, `resume` or `delete` to manage them (find ids with `list`). Their descriptions explain triggers and actions. If the request is empty, list the automations with their name, trigger, status and next run.\n\n\
Reply in one line: what changed and when it runs.";

/// Public snapshot in upstream wire spelling. The domain type spells the
/// expiry `expiresAt`; upstream's `Automation.Info` exposes it as `expires`.
pub fn info_to_value(info: &Info) -> Value {
    let mut value = serde_json::to_value(info).unwrap_or_else(|_| json!({}));
    if let Some(map) = value.as_object_mut()
        && let Some(expires) = map.remove("expiresAt")
    {
        map.insert("expires".to_owned(), expires);
    }
    value
}

/// The tool host port reports the same disabled shape as HTTP.
#[must_use]
pub fn disabled_error() -> Value {
    json!({
        "_tag": "Automation.DisabledError",
        "message": "Automations are disabled. Set automations to true in config to enable them.",
    })
}

fn enabled(app: &crate::api::AppState) -> bool {
    app.services().config.automations
}

fn api_error(res: &mut Response, status: StatusCode, tag: &str, message: &str) {
    res.status_code(status);
    res.render(Json(json!({ "_tag": tag, "message": message })));
}

fn disabled(res: &mut Response) {
    res.status_code(StatusCode::FORBIDDEN);
    res.render(Json(disabled_error()));
}

fn invalid(res: &mut Response, message: &str) {
    api_error(res, StatusCode::BAD_REQUEST, "InvalidRequestError", message);
}

fn not_found(res: &mut Response, id: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "AutomationNotFoundError",
        "automationID": id,
        "message": format!("Automation not found: {id}"),
    })));
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    trigger: Trigger,
    action: Action,
    #[serde(default)]
    expires: Option<String>,
    #[serde(default, rename = "sessionID")]
    session_id: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionScope {
    project_id: String,
}

/// Only the owning project's sessions may be named as an automation owner.
async fn session_scope(db: &Db, session_id: &str) -> Result<Option<SessionScope>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT project_id FROM session_v2 WHERE id = ?1")
        .await?;
    Ok(ox_db::optional(&mut stmt, [session_id])
        .await?
        .map(|row| row.get::<String>(0))
        .transpose()?
        .map(|project_id| SessionScope { project_id }))
}

fn parse_duration(input: &str) -> Result<i64> {
    let text = input.trim();
    let split = text
        .find(|character: char| character.is_ascii_alphabetic())
        .ok_or_else(|| anyhow::anyhow!("Invalid duration: {input}"))?;
    let (number, unit) = text.split_at(split);
    let amount: f64 = number
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid duration: {input}"))?;
    let factor = match unit.trim().to_ascii_lowercase().as_str() {
        "ms" | "millisecond" | "milliseconds" => 1,
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000,
        "d" | "day" | "days" => 86_400_000,
        "w" | "week" | "weeks" => 604_800_000,
        _ => bail!("Invalid duration: {input}"),
    };
    ensure!(amount > 0.0, "Invalid duration: {input}");
    Ok((amount * factor as f64).round() as i64)
}

/// Upstream `parseTime`: relative `in …`, an offset-bearing RFC 3339 instant,
/// or a civil datetime interpreted in the automation's zone.
fn parse_time(input: &str, zone: &str, now: i64) -> Result<i64> {
    if let Some(relative) = input.trim().strip_prefix("in ") {
        return Ok(now
            .checked_add(parse_duration(relative)?)
            .unwrap_or(i64::MAX));
    }
    if let Ok(timestamp) = jiff::Timestamp::from_str(input.trim()) {
        return Ok(timestamp.as_millisecond());
    }
    let civil = jiff::civil::DateTime::from_str(input.trim())?;
    let zoned = civil.to_zoned_datetime(jiff::tz::TimeZone::get(zone)?)?;
    let timestamp = zoned.timestamp();
    Ok(timestamp.as_millisecond())
}

fn first_due(trigger: &Trigger, action: &Action, zone: &str, now: i64) -> Result<Option<i64>> {
    match trigger {
        Trigger::At { at, .. } => Ok(Some(parse_time(at, zone, now)?)),
        Trigger::Every { every, .. } => {
            let interval = parse_duration(every)?;
            // Upstream's minimum interval: prompts are human-scale, code and
            // shell actions may tick every 15 seconds.
            let minimum = if matches!(action, Action::Prompt { .. }) {
                60_000
            } else {
                15_000
            };
            ensure!(
                interval >= minimum,
                "trigger.every is below the {minimum}ms minimum"
            );
            Ok(Some(now.saturating_add(interval)))
        }
        Trigger::Cron { cron, .. } => Cron::parse(cron)?.next_after(now, zone),
        Trigger::Event { .. } => Ok(None),
    }
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Prompt { .. } => "prompt",
        Action::Code { .. } => "code",
        Action::Shell { .. } => "shell",
    }
}

/// Validate and build the row without touching storage. Every rejection here
/// is fail-closed: no owner, no unattended action; no zone, no wall-clock
/// trigger; no cross-project session.
async fn build_info(
    db: &Db,
    body: CreateBody,
    directory: &str,
    project_id: &str,
    zone: &str,
    now: i64,
) -> Result<Info> {
    ensure!(!body.name.trim().is_empty(), "name must not be empty");
    body.trigger.validate(Some(zone))?;
    body.action.validate()?;
    if matches!(body.trigger, Trigger::Event { .. }) {
        bail!("Create an event automation from a tool call in a local session");
    }

    let owner = match body.session_id.as_deref() {
        Some(id) => {
            let Some(scope) = session_scope(db, id).await? else {
                bail!("Session not found: {id}");
            };
            ensure!(
                scope.project_id == project_id,
                "Session {id} is in another project"
            );
            Some(id.to_owned())
        }
        None => None,
    };
    match &body.action {
        Action::Prompt { to, .. } => match to {
            Some(Target::Session { session }) => {
                let Some(scope) = session_scope(db, session).await? else {
                    bail!("Session not found: {session}");
                };
                ensure!(
                    scope.project_id == project_id,
                    "Session {session} is in another project"
                );
            }
            Some(Target::Subagent { .. }) | None if owner.is_none() => {
                bail!("A prompt action needs `to` when there is no session")
            }
            Some(Target::Subagent { .. }) | None => {}
        },
        Action::Code { .. } | Action::Shell { .. } if owner.is_none() => {
            bail!("A {} action needs a session", action_kind(&body.action))
        }
        Action::Code { .. } | Action::Shell { .. } => {
            bail!(
                "Unattended {} actions need an interactive permission grant",
                action_kind(&body.action)
            )
        }
    }

    let next_due_at = first_due(&body.trigger, &body.action, zone, now)?;
    let expires_at = body
        .expires
        .as_deref()
        .map(|expires| parse_time(expires, zone, now))
        .transpose()?;
    ensure!(
        expires_at.is_none_or(|expiry| next_due_at.unwrap_or(now) <= expiry),
        "The automation expires before it first runs"
    );
    Ok(Info {
        id: id::id("aut"),
        name: body.name,
        project_id: project_id.to_owned(),
        session_id: owner,
        directory: directory.to_owned(),
        trigger: body.trigger,
        tz: zone.to_owned(),
        action: body.action,
        expires_at,
        status: Status::Active,
        next_due_at,
        run_count: 0,
        last_run_at: None,
        last_outcome: None,
        last_output: None,
        state: None,
        time: TimeInfo {
            created: now,
            updated: now,
        },
    })
}

#[handler] // operation_id: experimental.automation.create
pub(crate) async fn create(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else { return };
    if !enabled(app) {
        disabled(res);
        return;
    }
    let Ok(body) = req.parse_json::<CreateBody>().await else {
        invalid(res, "Invalid request body");
        return;
    };
    let directory = directory_of(req);
    let Ok(project) = resolve_project(&app.db, Path::new(&directory)).await else {
        invalid(res, "Directory does not exist");
        return;
    };
    let services = app.services();
    let zone = body
        .trigger
        .tz()
        .or(services.config.timezone.as_deref())
        .unwrap_or("UTC")
        .to_owned();
    let now = id::now_millis();
    let info = match build_info(&app.db, body, &directory, &project.id, &zone, now).await {
        Ok(info) => info,
        Err(error) => {
            invalid(res, &error.to_string());
            return;
        }
    };
    if let Err(error) = AutomationStore::new(&app.db).insert(&info).await {
        api_error(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "InternalServerError",
            &error.to_string(),
        );
        return;
    }
    res.render(Json(envelope(&directory, info_to_value(&info))));
}

#[handler] // operation_id: experimental.automation.list
pub(crate) async fn list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else { return };
    if !enabled(app) {
        disabled(res);
        return;
    }
    let directory = directory_of(req);
    let Ok(project) = resolve_project(&app.db, Path::new(&directory)).await else {
        invalid(res, "Directory does not exist");
        return;
    };
    let mut items = match AutomationStore::new(&app.db).list(&project.id).await {
        Ok(items) => items,
        Err(error) => {
            api_error(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalServerError",
                &error.to_string(),
            );
            return;
        }
    };
    items.sort_by(|left, right| left.id.cmp(&right.id));
    let data = json!({ "now": id::now_millis(), "automations": items.iter().map(info_to_value).collect::<Vec<_>>() });
    res.render(Json(envelope(&directory, data)));
}

/// Load an id only when it belongs to the requested directory's project.
async fn scoped_info(
    app: &crate::api::AppState,
    id: &str,
    directory: &str,
) -> Result<Option<Info>> {
    let store = AutomationStore::new(&app.db);
    let Some(info) = store.get(id).await? else {
        return Ok(None);
    };
    let project = resolve_project(&app.db, Path::new(directory)).await?;
    if info.project_id != project.id {
        return Ok(None);
    }
    Ok(Some(info))
}

#[handler] // operation_id: experimental.automation.get
pub(crate) async fn get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else { return };
    if !enabled(app) {
        disabled(res);
        return;
    }
    let directory = directory_of(req);
    let Some(id) = req.param::<String>("id") else {
        invalid(res, "Missing automation id");
        return;
    };
    match scoped_info(app, &id, &directory).await {
        Ok(Some(info)) => res.render(Json(envelope(&directory, info_to_value(&info)))),
        Ok(None) => not_found(res, &id),
        Err(error) => {
            api_error(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalServerError",
                &error.to_string(),
            );
        }
    }
}

async fn set_status(req: &mut Request, depot: &mut Depot, res: &mut Response, status: Status) {
    let Some(app) = state(depot, res) else { return };
    if !enabled(app) {
        disabled(res);
        return;
    }
    let directory = directory_of(req);
    let Some(id) = req.param::<String>("id") else {
        invalid(res, "Missing automation id");
        return;
    };
    let info = match scoped_info(app, &id, &directory).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            not_found(res, &id);
            return;
        }
        Err(error) => {
            api_error(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalServerError",
                &error.to_string(),
            );
            return;
        }
    };
    let store = AutomationStore::new(&app.db);
    let changed = match store
        .set_status(&id, status, id::now_millis(), info.run_count)
        .await
    {
        Ok(changed) => changed,
        Err(error) => {
            api_error(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalServerError",
                &error.to_string(),
            );
            return;
        }
    };
    if !changed {
        not_found(res, &id);
        return;
    }
    match store.get(&id).await {
        Ok(Some(info)) => res.render(Json(envelope(&directory, info_to_value(&info)))),
        Ok(None) => not_found(res, &id),
        Err(error) => {
            api_error(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalServerError",
                &error.to_string(),
            );
        }
    }
}

#[handler] // operation_id: experimental.automation.pause
pub(crate) async fn pause(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    set_status(req, depot, res, Status::Paused).await;
}

#[handler] // operation_id: experimental.automation.resume
pub(crate) async fn resume(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    set_status(req, depot, res, Status::Active).await;
}

#[handler] // operation_id: experimental.automation.delete
pub(crate) async fn delete(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else { return };
    if !enabled(app) {
        disabled(res);
        return;
    }
    let directory = directory_of(req);
    let Some(id) = req.param::<String>("id") else {
        invalid(res, "Missing automation id");
        return;
    };
    match scoped_info(app, &id, &directory).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            not_found(res, &id);
            return;
        }
        Err(error) => {
            api_error(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalServerError",
                &error.to_string(),
            );
            return;
        }
    }
    let connection = match app.db.acquire().await {
        Ok(connection) => connection,
        Err(error) => {
            api_error(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalServerError",
                &error.to_string(),
            );
            return;
        }
    };
    match connection
        .execute("DELETE FROM automation WHERE id = ?1", (id.as_str(),))
        .await
    {
        Ok(changed) if changed > 0 => {
            res.status_code(StatusCode::NO_CONTENT);
        }
        Ok(_) => not_found(res, &id),
        Err(error) => api_error(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "InternalServerError",
            &error.to_string(),
        ),
    }
}
