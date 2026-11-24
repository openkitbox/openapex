//! Server-side automation composition.
//!
//! The scheduler's DB lease makes one process the holder at a time; every
//! instance still watches the config gate so a reload can disable automations
//! without a restart. All run dispatch goes through one `Scheduler`, which owns
//! the ticket guards and the lease renewal.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use futures_core::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use ox_automation::action::code::CodeExecutor;
use ox_automation::action::prompt::{
    PromptExecutor, PromptPort, SessionPromptRequest, SubagentRequest,
};
use ox_automation::action::shell::{ShellExecutor, ShellJob, ShellOutcome, ShellRequest};
use ox_automation::action::{
    ActionContext, ActionExecutor, ActionLimits, Authorizer, Grant, RunTicket,
};
use ox_automation::decision::next_due;
use ox_automation::lease::Lease;
use ox_automation::scheduler::{
    ClaimedRun, Clock, RunResult, Scheduler, SchedulerConfig, SystemClock,
};
use ox_automation::store::AutomationStore;
use ox_automation::types::{Action, Info, Status, Target, TimeInfo, Trigger};
use ox_core::config::Config;
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_engine::runner::{Live, Runner, ToolRegistry};
use ox_schema::event::SessionEventData;
use ox_schema::id::now_millis;
use ox_schema::inbox::{Delivery, Item, UNATTENDED_KEY, UNATTENDED_RULES_KEY, UserPayload};
use ox_schema::permission::{Effect, Rule};
use ox_schema::support::{LocationRef, Prompt as PromptPayload, Record, SessionMetadata};
use ox_tools::automation::AutomationService;

const LEASE_NAME: &str = "scheduler";
const MCP_EVENT_KIND: &str = "experimental.mcp.event";

/// A config gate consulted before every lease acquisition and every dispatch.
pub(crate) type Gate = Arc<dyn Fn() -> bool + Send + Sync>;

/// Wiring-time options for one scheduler instance. Test code shortens the
/// deadlines so takeover can be observed without waiting the production TTL.
pub(crate) struct SchedulerOptions {
    pub owner: String,
    pub config: SchedulerConfig,
    pub gate: Gate,
    pub limits: ActionLimits,
    pub authorizer: Option<Arc<dyn Authorizer>>,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        Self {
            owner: "ox".to_owned(),
            config: SchedulerConfig::new(),
            gate: Arc::new(|| true),
            limits: ActionLimits::default(),
            authorizer: None,
        }
    }
}

/// Spawn one scheduler. The handle is adopted by `Runtime`, so the composition
/// joins it before releasing the database.
pub(crate) fn spawn_scheduler(
    db: Db,
    executor: Arc<dyn ActionExecutor>,
    options: SchedulerOptions,
    stop: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (terminal_tx, terminal_rx) = mpsc::unbounded_channel();
        let watcher = tokio::spawn(watch_terminals(db.clone(), terminal_rx, stop.clone()));
        if let Err(error) = scheduler_loop(&db, executor, &options, &stop, terminal_tx).await {
            tracing::warn!(%error, "automation scheduler stopped");
        }
        stop.cancel();
        let _ = watcher.await;
    })
}

/// One terminal result observed by a tick, kept until the row's completion has
/// been settled by the scheduler.
#[derive(Debug, Clone)]
struct TerminalRun {
    id: String,
    run_count: u64,
}

async fn scheduler_loop(
    db: &Db,
    executor: Arc<dyn ActionExecutor>,
    options: &SchedulerOptions,
    stop: &CancellationToken,
    terminals: mpsc::UnboundedSender<TerminalRun>,
) -> Result<()> {
    let clock = SystemClock;
    let ttl_ms = i64::try_from(options.config.ttl.as_millis())?;
    loop {
        if stop.is_cancelled() {
            return Ok(());
        }
        if !(options.gate)() {
            tokio::select! {
                () = stop.cancelled() => return Ok(()),
                () = sleep(options.config.poll) => {}
            }
            continue;
        }
        let Some(lease) =
            Lease::acquire(db, LEASE_NAME, &options.owner, clock.now_ms(), ttl_ms).await?
        else {
            tokio::select! {
                () = stop.cancelled() => return Ok(()),
                () = sleep(options.config.poll) => {}
            }
            continue;
        };

        let gate = Arc::clone(&options.gate);
        let cancelled = stop.clone();
        let limits = options.limits;
        let authorizer = options.authorizer.clone();
        let executor_for_handler = Arc::clone(&executor);
        let terminals_for_handler = terminals.clone();
        let handler = move |info: &Info| {
            let executor = Arc::clone(&executor_for_handler);
            let gate = Arc::clone(&gate);
            let cancelled = cancelled.clone();
            let terminals = terminals_for_handler.clone();
            let authorizer = authorizer.clone();
            let info = info.clone();
            async move {
                if cancelled.is_cancelled() || !gate() {
                    return Ok(RunResult::failed(Some(
                        "automations are disabled or shutting down".to_owned(),
                    )));
                }
                let run = ClaimedRun { info, late: false };
                let context = ActionContext::new(now_millis(), authorizer.as_deref(), &limits);
                let result = executor.execute(&run, context).await?;
                if result.terminal {
                    let _ = terminals.send(TerminalRun {
                        id: run.info.id.clone(),
                        run_count: run.info.run_count,
                    });
                }
                Ok(result)
            }
        };

        let store = AutomationStore::new(db);
        let scheduler = Scheduler::new(db, store, lease).with_config(options.config);
        let gate = Arc::clone(&options.gate);
        let cancelled = stop.clone();
        let poll = options.config.poll;
        let shutdown = async move {
            loop {
                if cancelled.is_cancelled() || !gate() {
                    return;
                }
                sleep(poll).await;
            }
        };
        // `serve` releases the lease only on its shutdown branch; an error exit
        // leaves it to expire at the TTL instead.
        scheduler.serve(clock, handler, shutdown).await?;
    }
}

/// A terminal action must be marked `Done` only after the scheduler has
/// settled its own completion, which owns `last_outcome` and the code state.
async fn watch_terminals(
    db: Db,
    mut terminals: mpsc::UnboundedReceiver<TerminalRun>,
    stop: CancellationToken,
) {
    let store = AutomationStore::new(&db);
    while let Some(terminal) = terminals.recv().await {
        loop {
            let settled = match store.get(&terminal.id).await {
                Ok(Some(info)) => {
                    info.run_count == terminal.run_count && info.last_outcome.is_some()
                }
                Ok(None) => false,
                Err(error) => {
                    tracing::warn!(%error, "failed to read a terminal automation");
                    false
                }
            };
            if settled {
                match store
                    .set_status(&terminal.id, Status::Done, now_millis(), terminal.run_count)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => tracing::debug!(
                        id = %terminal.id,
                        "terminal result was superseded before it could settle"
                    ),
                    Err(error) => tracing::warn!(%error, "failed to settle a terminal automation"),
                }
                break;
            }
            tokio::select! {
                () = stop.cancelled() => return,
                () = sleep(Duration::from_millis(10)) => {}
            }
        }
    }
}

/// Translate one published MCP event into a due tick for matching event
/// automations. The store's revision guard uses the observed run count, so
/// duplicate events and two processes seeing the same frame cannot run the
/// same row twice.
async fn fire_mcp_event(db: &Db, data: &Value) -> Result<usize> {
    let (Some(server), Some(event)) = (
        data.get("server").and_then(Value::as_str),
        data.get("name").and_then(Value::as_str),
    ) else {
        return Ok(0);
    };
    let store = AutomationStore::new(db);
    let now_ms = now_millis();
    let mut fired = 0;
    for project in store.active_projects().await? {
        for info in store.list(&project).await? {
            let matches = info.status == Status::Active
                && matches!(&info.trigger, Trigger::Event { server: s, event: e, .. } if s == server && e == event);
            if !matches {
                continue;
            }
            if store
                .queue_event_due(&info.id, now_ms, info.run_count)
                .await?
            {
                fired += 1;
            }
        }
    }
    Ok(fired)
}

/// Bridge the event bus into the automation table. The scheduler remains the
/// only dispatcher: this task never runs an action itself.
pub(crate) fn spawn_event_bridge(db: Db, bus: EventBus, stop: CancellationToken) -> JoinHandle<()> {
    // Subscribe before spawning: a broadcast published immediately after this
    // call must not be lost to a subscriber that has not been installed yet.
    let mut events = bus.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                event = events.recv() => match event {
                    Ok(envelope) if envelope.kind == MCP_EVENT_KIND => {
                        if let Err(error) = fire_mcp_event(&db, &envelope.data).await {
                            tracing::warn!(%error, "automation event trigger failed");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "automation event bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    })
}

/// The production gate: a config reload can disable automations, and closing
/// admissions is always final.
pub(crate) fn production_gate(state: &crate::api::AppState) -> Gate {
    let state = state.clone();
    Arc::new(move || state.services().config.automations && !state.runtime.is_closing())
}

// ---------------------------------------------------------------------------
// Creation-time grants and the tool service
// ---------------------------------------------------------------------------

/// What the creating tool call approved for one automation. There is no
/// interactive approval during an unattended run, so an automation created in
/// another process (or before a restart) has no grants and fails closed.
#[derive(Default)]
struct Grants {
    shell: bool,
    execute: bool,
    tools: bool,
    sessions: HashSet<String>,
    subagents: HashSet<String>,
}

#[derive(Default)]
pub(crate) struct GrantBook {
    grants: Mutex<HashMap<String, Grants>>,
}

impl GrantBook {
    fn install(&self, id: &str, grants: Grants) {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_owned(), grants);
    }

    fn forget(&self, id: &str) {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }
}

impl Authorizer for GrantBook {
    fn allows(&self, ticket: &RunTicket<'_>, grant: Grant<'_>) -> bool {
        let book = self
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(grants) = book.get(ticket.id) else {
            return false;
        };
        match grant {
            Grant::Shell => grants.shell,
            Grant::Execute => grants.execute,
            Grant::Tool(_) => grants.tools && grants.execute,
            Grant::Subagent(name) => grants.subagents.contains(name),
            Grant::Session(name) => grants.sessions.contains(name),
        }
    }
}

fn grants_for(action: &Action, owner: &str) -> Grants {
    let mut grants = Grants::default();
    match action {
        Action::Shell { .. } => grants.shell = true,
        Action::Code { .. } => {
            grants.execute = true;
            grants.tools = true;
        }
        Action::Prompt { to, .. } => match to {
            Some(Target::Session { session }) => {
                grants.sessions.insert(session.clone());
            }
            Some(Target::Subagent { subagent }) => {
                grants.subagents.insert(subagent.clone());
            }
            None => {
                grants.sessions.insert(owner.to_owned());
            }
        },
    }
    grants
}

/// Translate the same creation-time grants into the rule set the permission
/// gate consults for an unattended turn. A grant that was never approved
/// produces no allow rule, so the gate denies the call instead of asking.
fn unattended_rules_for(action: &Action, owner: &str) -> Vec<Rule> {
    let allow = |action: &str, resource: &str| Rule {
        action: action.to_owned(),
        resource: resource.to_owned(),
        effect: Effect::Allow,
    };
    match action {
        Action::Shell { command } => vec![allow("shell", command)],
        Action::Code { .. } => match ox_tools::agent::AUTOMATION_TOOLS {
            ox_tools::agent::Tools::Only(names) => {
                names.iter().map(|name| allow("unknown", name)).collect()
            }
            _ => Vec::new(),
        },
        Action::Prompt { to, .. } => match to {
            Some(Target::Subagent { subagent }) => vec![allow("subagent", subagent)],
            Some(Target::Session { session }) => vec![allow("session", session)],
            None => vec![allow("session", owner)],
        },
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    name: String,
    trigger: Trigger,
    action: Action,
    #[serde(default)]
    expires: Option<String>,
}

/// Host half of the five automation tools. The caller's session determines
/// the project; a row from another project is never returned or changed.
pub(crate) struct AutomationHost {
    db: Db,
    grants: Arc<GrantBook>,
    live: Arc<OnceLock<Arc<Live>>>,
}

impl AutomationHost {
    #[must_use]
    pub(crate) fn new(db: Db, grants: Arc<GrantBook>) -> (Arc<Self>, Arc<OnceLock<Arc<Live>>>) {
        let live = Arc::new(OnceLock::new());
        let host = Arc::new(Self {
            db,
            grants,
            live: Arc::clone(&live),
        });
        (host, live)
    }

    fn enabled(&self) -> bool {
        self.live
            .get()
            .is_some_and(|live| live.current().config.automations)
    }

    async fn session_project(&self, session_id: &str) -> Result<(String, String)> {
        let conn = self.db.acquire().await?;
        let mut statement = conn
            .prepare("SELECT project_id, directory FROM session_v2 WHERE id = ?1")
            .await?;
        let row = ox_db::optional(&mut statement, (session_id,)).await?;
        let Some(row) = row else {
            bail!("session {session_id} does not exist");
        };
        Ok((row.get(0)?, row.get(1)?))
    }

    async fn scoped_info(&self, session_id: &str, id: &str) -> Result<Option<Info>> {
        let (project_id, _) = self.session_project(session_id).await?;
        let Some(info) = AutomationStore::new(&self.db).get(id).await? else {
            return Ok(None);
        };
        Ok((info.project_id == project_id).then_some(info))
    }

    async fn build_info(&self, session_id: &str, input: &Value) -> Result<Info> {
        let input: CreateInput = serde_json::from_value(input.clone())?;
        input.trigger.validate(None)?;
        input.action.validate()?;
        let (project_id, directory) = self.session_project(session_id).await?;
        let config = self
            .live
            .get()
            .map_or_else(Config::default, |live| (*live.current().config).clone());
        let zone = input
            .trigger
            .tz()
            .or(config.timezone.as_deref())
            .unwrap_or("UTC")
            .to_owned();
        let now = now_millis();
        let expires_at = input
            .expires
            .as_deref()
            .map(|expires| parse_expiry(expires, now))
            .transpose()?;
        let mut info = Info {
            id: ox_schema::id::id("aut"),
            name: input.name,
            project_id,
            session_id: Some(session_id.to_owned()),
            directory,
            trigger: input.trigger,
            tz: zone,
            action: input.action,
            expires_at,
            status: Status::Active,
            next_due_at: None,
            run_count: 0,
            last_run_at: None,
            last_outcome: None,
            last_output: None,
            state: None,
            time: TimeInfo {
                created: now,
                updated: now,
            },
        };
        info.next_due_at = next_due(&info, now)?;
        ensure!(
            expires_at.is_none_or(|expiry| info.next_due_at.unwrap_or(now) <= expiry),
            "the automation expires before it first runs"
        );
        Ok(info)
    }
}

/// A relative duration (`30m`, `2h`, `1d`) or an absolute RFC 3339 timestamp.
fn parse_expiry(value: &str, now_ms: i64) -> Result<i64> {
    let raw = value.trim();
    let (amount, unit_ms) = match raw.chars().last() {
        Some('m') => (&raw[..raw.len() - 1], 60_000),
        Some('h') => (&raw[..raw.len() - 1], 3_600_000),
        Some('d') => (&raw[..raw.len() - 1], 86_400_000),
        _ => {
            let stamp: jiff::Timestamp = raw.parse()?;
            return Ok(stamp.as_millisecond());
        }
    };
    let count: i64 = amount.parse()?;
    ensure!(count > 0, "expiry must be in the future");
    Ok(now_ms.saturating_add(count.saturating_mul(unit_ms)))
}

impl AutomationService for AutomationHost {
    fn create<'a>(
        &'a self,
        session_id: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            ensure!(self.enabled(), "automations are disabled in config");
            let info = self.build_info(session_id, input).await?;
            AutomationStore::new(&self.db).insert(&info).await?;
            self.grants.install(
                &info.id,
                grants_for(&info.action, info.session_id.as_deref().unwrap_or("")),
            );
            Ok(info_value(&info))
        })
    }

    fn list<'a>(
        &'a self,
        session_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            ensure!(self.enabled(), "automations are disabled in config");
            let (project_id, _) = self.session_project(session_id).await?;
            let rows = AutomationStore::new(&self.db).list(&project_id).await?;
            let items: Vec<Value> = rows.iter().map(info_value).collect();
            Ok(serde_json::json!({
                "now": now_millis(),
                "automations": items,
            }))
        })
    }

    fn pause<'a>(
        &'a self,
        session_id: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        let status = Status::Paused;
        Box::pin(async move { self.set_status(session_id, id, status).await })
    }

    fn resume<'a>(
        &'a self,
        session_id: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        let status = Status::Active;
        Box::pin(async move { self.set_status(session_id, id, status).await })
    }

    fn delete<'a>(
        &'a self,
        session_id: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            ensure!(self.enabled(), "automations are disabled in config");
            ensure!(
                self.scoped_info(session_id, id).await?.is_some(),
                "automation {id} does not exist in this project"
            );
            let conn = self.db.acquire().await?;
            let changed = conn
                .execute("DELETE FROM automation WHERE id = ?1", (id,))
                .await?;
            drop(conn);
            ensure!(changed > 0, "automation {id} could not be removed");
            self.grants.forget(id);
            Ok(serde_json::json!({ "id": id, "removed": true }))
        })
    }
}

impl AutomationHost {
    async fn set_status(&self, session_id: &str, id: &str, status: Status) -> Result<Value> {
        ensure!(self.enabled(), "automations are disabled in config");
        let info = self
            .scoped_info(session_id, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("automation {id} does not exist in this project"))?;
        let next = Info { status, ..info };
        AutomationStore::new(&self.db)
            .save(&next, info.status)
            .await?;
        Ok(info_value(&next))
    }
}

/// The domain type spells the expiry `expiresAt`; tools use upstream's
/// `expires` spelling, matching the HTTP automation handlers.
fn info_value(info: &Info) -> Value {
    let mut value = serde_json::to_value(info).unwrap_or(Value::Null);
    if let Some(map) = value.as_object_mut()
        && let Some(expires) = map.remove("expiresAt")
    {
        map.insert("expires".to_owned(), expires);
    }
    value
}

// ---------------------------------------------------------------------------
// Server ports for the action executors
// ---------------------------------------------------------------------------

struct ServerPrompt {
    db: Db,
    bus: EventBus,
    runner: Runner,
}

impl ServerPrompt {
    #[must_use]
    fn new(state: &crate::api::AppState) -> Self {
        Self {
            db: state.db.clone(),
            bus: state.bus.clone(),
            runner: state.runner.clone(),
        }
    }

    async fn automation_project(&self, ticket: &RunTicket<'_>) -> Result<Info> {
        AutomationStore::new(&self.db)
            .get(ticket.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("automation {} no longer exists", ticket.id))
    }

    async fn session_row(
        &self,
        session_id: &str,
    ) -> Result<(String, String, Option<String>, Option<String>)> {
        let conn = self.db.acquire().await?;
        let mut statement = conn
            .prepare(
                "SELECT project_id, directory, workspace_id, path FROM session_v2 WHERE id = ?1",
            )
            .await?;
        let row = ox_db::optional(&mut statement, (session_id,)).await?;
        let Some(row) = row else {
            bail!("session {session_id} does not exist");
        };
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    }

    async fn enqueue(
        &self,
        session_id: &str,
        message_id: &str,
        text: &str,
        unattended_rules: Option<&[Rule]>,
    ) -> Result<()> {
        let metadata = unattended_rules.map(|rules| {
            let mut metadata = Record::default();
            metadata.insert(UNATTENDED_KEY.to_owned(), json!(true));
            metadata.insert(UNATTENDED_RULES_KEY.to_owned(), json!(rules));
            metadata
        });
        self.bus
            .publish(SessionEventData::InboxEnqueued {
                session_id: session_id.to_owned(),
                inbox_id: message_id.to_owned(),
                item: Item::User {
                    payload: UserPayload {
                        prompt: PromptPayload {
                            text: text.to_owned(),
                            files: None,
                            agents: None,
                            skills: None,
                        },
                        metadata: Some(metadata.unwrap_or_default()),
                    },
                    delivery: Delivery::Steer,
                },
            })
            .await?;
        self.runner.dispatch(session_id, message_id).await
    }
}

impl PromptPort for ServerPrompt {
    fn prompt_session<'a>(&'a self, req: SessionPromptRequest<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let automation = self.automation_project(&req.ticket).await?;
            let (project_id, _, _, _) = self.session_row(req.session_id).await?;
            ensure!(
                project_id == automation.project_id,
                "target session {} left the automation's project",
                req.session_id
            );
            let rules = unattended_rules_for(&automation.action, req.session_id);
            let rules = req.unattended.then_some(rules.as_slice());
            self.enqueue(req.session_id, &req.message_id, req.text, rules)
                .await
        })
    }

    fn start_subagent<'a>(&'a self, req: SubagentRequest<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let automation = self.automation_project(&req.ticket).await?;
            let agent = ox_tools::agent::get(req.agent)
                .ok_or_else(|| anyhow::anyhow!("unknown subagent {}", req.agent))?;
            ensure!(
                agent.mode == ox_tools::agent::Mode::Subagent,
                "agent {} cannot run as a subagent",
                req.agent
            );
            let (project_id, directory, workspace_id, subpath) =
                self.session_row(req.parent_id).await?;
            ensure!(
                project_id == automation.project_id,
                "the parent session left the automation's project"
            );
            self.bus
                .publish(SessionEventData::SessionCreated {
                    session_id: req.child_id.clone(),
                    project_id,
                    location: LocationRef {
                        directory,
                        workspace_id,
                    },
                    subpath,
                    parent_id: Some(req.parent_id.to_owned()),
                    slug: ox_schema::slug::create(),
                    title: Some(format!("automation {}", req.agent)),
                    agent: Some(req.agent.to_owned()),
                    model: None,
                    metadata: Some(SessionMetadata::default()),
                    permissions: None,
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                })
                .await?;
            let rules = unattended_rules_for(&automation.action, req.parent_id);
            let rules = req.unattended.then_some(rules.as_slice());
            self.enqueue(&req.child_id, &req.message_id, req.text, rules)
                .await
        })
    }
}

struct ServerShell {
    shells: Arc<ox_tools::shell::sessions::ShellSessions>,
}

impl ShellJob for ServerShell {
    fn run<'a>(&'a self, req: ShellRequest<'a>) -> BoxFuture<'a, Result<ShellOutcome>> {
        Box::pin(async move {
            let session = self.shells.spawn(ox_tools::shell::sessions::SpawnRequest {
                command: req.command.to_owned(),
                cwd: PathBuf::from(req.cwd),
                timeout_ms: Some(i64::try_from(req.timeout.as_millis())?.max(1).try_into()?),
                completion: None,
                foreground: false,
                owner: None,
                metadata: Record::default(),
            })?;
            while !session.settled() {
                session.wait_change(Duration::from_millis(50)).await;
            }
            let exit = session.exit();
            Ok(ShellOutcome {
                exit_code: exit.code,
                output: session.tail(),
                timed_out: matches!(
                    exit.status,
                    Some(ox_schema::support::ShellStatus::Timeout)
                        | Some(ox_schema::support::ShellStatus::Killed)
                ),
            })
        })
    }
}

/// Bridge a scanned tool path to the live tool registry. `ToolInvoker` is
/// synchronous because it runs inside the JS blocking task; the async tool
/// contract is driven to completion on the composition's runtime handle.
struct RegistryInvoker {
    tools: Arc<ToolRegistry>,
    directory: PathBuf,
}

impl ox_codemode::ToolInvoker for RegistryInvoker {
    fn invoke(&self, path: &str, args: &str) -> Result<String> {
        let handle = tokio::runtime::Handle::try_current()?;
        let path = path.to_owned();
        let arguments: Value = serde_json::from_str(args)?;
        handle.block_on(async move {
            let tools = Arc::clone(&self.tools);
            let tool = tools
                .get(&path)
                .ok_or_else(|| anyhow::anyhow!("tool {path} is not registered"))?;
            let output = tool
                .execute(
                    ox_tools::contract::ToolContext {
                        session_id: "automation".to_owned(),
                        directory: self.directory.clone(),
                        assistant_message_id: format!("automation_{path}"),
                        call_id: format!("automation_{path}"),
                        cancellation: CancellationToken::new(),
                        registry: tools,
                    },
                    arguments,
                )
                .await;
            if let Some(error) = output.error {
                bail!("{error}");
            }
            Ok(output.content.to_string())
        })
    }
}

struct ServerActions {
    actions: ox_automation::action::Actions<ServerPrompt, ServerShell>,
    db: Db,
    grants: Arc<GrantBook>,
}

impl ServerActions {
    #[must_use]
    fn new(state: &crate::api::AppState, grants: Arc<GrantBook>) -> Self {
        let prompt = Arc::new(PromptExecutor::new(Arc::new(ServerPrompt::new(state))));
        let shell = Arc::new(ShellExecutor::new(Arc::new(ServerShell {
            shells: Arc::clone(&state.services().tools.shells),
        })));
        // `Tools::Only` is the complete unattended toolset: a script must not
        // reach the primary session's tools through the live registry.
        let allowed: &[&str] = match ox_tools::agent::AUTOMATION_TOOLS {
            ox_tools::agent::Tools::Only(names) => names,
            _ => &[],
        };
        let base = Arc::clone(&state.services().tools);
        let names: Vec<String> = base
            .definitions()
            .into_iter()
            .map(|definition| definition.function.name)
            .collect();
        let dropped: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|name| !allowed.contains(name))
            .collect();
        let unattended = base.without(&dropped);
        let invoker = Arc::new(RegistryInvoker {
            tools: Arc::new(unattended),
            directory: state.directory.clone(),
        });
        let code = Arc::new(CodeExecutor::new(
            Arc::clone(&prompt),
            invoker,
            ox_codemode::Limits::default(),
        ));
        Self {
            actions: ox_automation::action::Actions {
                prompt,
                shell,
                code,
            },
            db: state.db.clone(),
            grants,
        }
    }
}

impl ActionExecutor for ServerActions {
    fn execute<'a>(
        &'a self,
        run: &'a ClaimedRun,
        ctx: ActionContext<'a>,
    ) -> BoxFuture<'a, Result<RunResult>> {
        Box::pin(async move {
            // Grants are creation-time facts about the stored action. Deriving
            // them from the row keeps an automation authorized after a restart,
            // without widening an action that was never approved at creation.
            if let Some(info) = AutomationStore::new(&self.db).get(&run.info.id).await? {
                self.grants.install(
                    &info.id,
                    grants_for(&info.action, info.session_id.as_deref().unwrap_or("")),
                );
            }
            self.actions.execute(run, ctx).await
        })
    }
}

/// All background tasks the composition owns for automations. Both are
/// adopted by `Runtime` at startup and join on the same stop signal.
pub(crate) fn spawn_tasks(
    state: &crate::api::AppState,
    grants: Arc<GrantBook>,
) -> Vec<JoinHandle<()>> {
    let stop = state.runtime.stop().clone();
    let scheduler = spawn_scheduler(
        state.db.clone(),
        Arc::new(ServerActions::new(state, Arc::clone(&grants))),
        SchedulerOptions {
            owner: format!("ox-{}-{}", std::process::id(), ox_schema::id::id("own")),
            gate: production_gate(state),
            ..SchedulerOptions::default()
        },
        stop.clone(),
    );
    vec![
        scheduler,
        spawn_event_bridge(state.db.clone(), state.bus.clone(), stop),
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use anyhow::Result;
    use futures_core::future::BoxFuture;
    use tokio_util::sync::CancellationToken;

    use ox_automation::action::{ActionContext, ActionExecutor};
    use ox_automation::scheduler::{ClaimedRun, RunResult, SchedulerConfig};
    use ox_automation::store::AutomationStore;
    use ox_automation::types::{Action, Info, Status, TimeInfo, Trigger};
    use ox_db::Db;
    use ox_schema::id::now_millis;

    use super::{SchedulerOptions, spawn_scheduler};

    fn config() -> SchedulerConfig {
        SchedulerConfig {
            ttl: Duration::from_millis(400),
            heartbeat: Duration::from_millis(80),
            poll: Duration::from_millis(10),
            concurrency: 4,
        }
    }

    /// `due_after_ms` is relative to now: an absolute 1970 timestamp would be
    /// classified as missed by the real clock the production loop uses.
    fn info(id: &str, due_after_ms: Option<i64>) -> Info {
        Info {
            id: id.to_owned(),
            name: format!("test {id}"),
            project_id: "proj_1".to_owned(),
            session_id: None,
            directory: "/work".to_owned(),
            trigger: Trigger::Every {
                every: "1h".to_owned(),
                tz: None,
            },
            tz: "UTC".to_owned(),
            action: Action::Shell {
                command: "true".to_owned(),
            },
            expires_at: None,
            status: Status::Active,
            next_due_at: due_after_ms.map(|after| now_millis().saturating_add(after)),
            run_count: 0,
            last_run_at: None,
            last_outcome: None,
            last_output: None,
            state: None,
            time: TimeInfo {
                created: 1_000,
                updated: 1_000,
            },
        }
    }

    async fn wait_for<F, Fut>(what: &str, done: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if done().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn lease_expiry(db: &Db) -> Option<i64> {
        let conn = db.acquire().await.ok()?;
        let mut statement = conn
            .prepare("SELECT expires_at FROM automation_lease WHERE name = 'scheduler'")
            .await
            .ok()?;
        let row = ox_db::optional(&mut statement, [] as [String; 0])
            .await
            .ok()??;
        row.get::<i64>(0).ok()
    }

    #[test]
    fn expiry_parses_relative_and_absolute() {
        let now = 1_700_000_000_000;
        assert_eq!(
            super::parse_expiry("2h", now).expect("relative expiry"),
            now + 7_200_000
        );
        assert_eq!(
            super::parse_expiry("2023-11-14T22:13:20Z", now).expect("absolute expiry"),
            1_700_000_000_000
        );
        assert!(super::parse_expiry("0h", now).is_err());
    }

    struct RecordingExecutor {
        calls: AtomicUsize,
        terminal: bool,
        started: Arc<AtomicBool>,
        release: Arc<tokio::sync::Notify>,
    }

    impl RecordingExecutor {
        fn new(terminal: bool) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                terminal,
                started: Arc::new(AtomicBool::new(false)),
                release: Arc::new(tokio::sync::Notify::new()),
            }
        }

        fn count(&self) -> usize {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl ActionExecutor for RecordingExecutor {
        fn execute<'a>(
            &'a self,
            _run: &'a ClaimedRun,
            _ctx: ActionContext<'a>,
        ) -> BoxFuture<'a, Result<RunResult>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::AcqRel);
                self.started.store(true, Ordering::Release);
                self.release.notified().await;
                let mut result = RunResult::success(Some("done".to_owned()));
                result.terminal = self.terminal;
                Ok(result)
            })
        }
    }

    #[tokio::test]
    async fn automation_multi_instance_lease() {
        let db = Db::open_in_memory().await.expect("db");
        let store = AutomationStore::new(&db);
        store
            .insert(&info("aut_first", Some(1)))
            .await
            .expect("insert first");
        store
            .insert(&info("aut_second", Some(86_400_000)))
            .await
            .expect("insert second");

        let first = Arc::new(RecordingExecutor::new(false));
        let second = Arc::new(RecordingExecutor::new(false));
        let stop_a = CancellationToken::new();
        let task_a = spawn_scheduler(
            db.clone(),
            first.clone(),
            SchedulerOptions {
                owner: "instance-a".to_owned(),
                config: config(),
                ..SchedulerOptions::default()
            },
            stop_a.clone(),
        );
        let calls = first.clone();
        wait_for("instance A to run the due automation", || async {
            calls.count() == 1
        })
        .await;
        let initial_expiry = lease_expiry(&db).await.expect("holder lease");

        let stop_b = CancellationToken::new();
        let task_b = spawn_scheduler(
            db.clone(),
            second.clone(),
            SchedulerOptions {
                owner: "instance-b".to_owned(),
                config: config(),
                ..SchedulerOptions::default()
            },
            stop_b.clone(),
        );
        first.release.notify_one();
        let holder_db = db.clone();
        wait_for("the holder to renew while the follower polls", || async {
            lease_expiry(&holder_db)
                .await
                .is_some_and(|expiry| expiry > initial_expiry)
        })
        .await;
        assert_eq!(second.count(), 0, "a non-holder never dispatches");

        stop_a.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task_a)
            .await
            .expect("holder exits promptly");
        let second_info = info("aut_second", Some(1));
        store
            .save(&second_info, Status::Active)
            .await
            .expect("make the second automation due");
        let calls = second.clone();
        wait_for("instance B to take over", || async { calls.count() == 1 }).await;
        second.release.notify_one();
        stop_b.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task_b)
            .await
            .expect("successor exits promptly");
        assert_eq!(first.count(), 1);
        assert_eq!(second.count(), 1);
    }

    #[tokio::test]
    async fn automation_code_done_terminal() {
        let db = Db::open_in_memory().await.expect("db");
        let store = AutomationStore::new(&db);
        store
            .insert(&info("aut_terminal", Some(1)))
            .await
            .expect("insert");
        let executor = Arc::new(RecordingExecutor::new(true));
        let stop = CancellationToken::new();
        let task = spawn_scheduler(
            db.clone(),
            executor.clone(),
            SchedulerOptions {
                config: config(),
                ..SchedulerOptions::default()
            },
            stop.clone(),
        );
        executor.release.notify_one();
        wait_for("terminal result to mark the row Done", || async {
            store
                .get("aut_terminal")
                .await
                .ok()
                .flatten()
                .is_some_and(|row| row.status == Status::Done)
        })
        .await;
        stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }

    #[tokio::test]
    async fn automation_shutdown() {
        let db = Db::open_in_memory().await.expect("db");
        let store = AutomationStore::new(&db);
        store
            .insert(&info("aut_running", Some(1)))
            .await
            .expect("insert");
        let executor = Arc::new(RecordingExecutor::new(false));
        let stop = CancellationToken::new();
        let task = spawn_scheduler(
            db.clone(),
            executor.clone(),
            SchedulerOptions {
                config: config(),
                ..SchedulerOptions::default()
            },
            stop.clone(),
        );
        let started = Arc::clone(&executor.started);
        wait_for("the action to start", || async {
            started.load(Ordering::Acquire)
        })
        .await;
        stop.cancel();
        assert_eq!(executor.count(), 1, "shutdown never starts a second run");
        store
            .insert(&info("aut_after_close", Some(1)))
            .await
            .expect("a due row arriving during shutdown");
        executor.release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the running action can finish and the task can exit");
        assert_eq!(executor.count(), 1);
    }

    #[tokio::test]
    async fn automation_event_trigger() {
        let db = Db::open_in_memory().await.expect("db");
        let bus = ox_core::event_bus::EventBus::new(db.clone());
        let store = AutomationStore::new(&db);
        let mut event_info = info("aut_event", None);
        event_info.trigger = Trigger::Event {
            server: "git".to_owned(),
            event: "push".to_owned(),
            args: None,
        };
        store
            .insert(&event_info)
            .await
            .expect("insert event automation");

        let executor = Arc::new(RecordingExecutor::new(false));
        let stop = CancellationToken::new();
        let scheduler = spawn_scheduler(
            db.clone(),
            executor.clone(),
            SchedulerOptions {
                config: config(),
                ..SchedulerOptions::default()
            },
            stop.clone(),
        );
        executor.release.notify_one();
        let bridge = super::spawn_event_bridge(db.clone(), bus.clone(), stop.clone());
        bus.broadcast_ephemeral(
            "experimental.mcp.event",
            serde_json::json!({
                "server": "git",
                "name": "push",
                "eventId": "evt_1",
                "data": {},
            }),
            None,
        );
        wait_for("an MCP event to fire its automation", || async {
            store
                .get("aut_event")
                .await
                .ok()
                .flatten()
                .is_some_and(|row| row.run_count == 1)
        })
        .await;
        stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), scheduler).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), bridge).await;
    }
}
