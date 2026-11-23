//! Process-local blocking registry plus durable snapshots for background jobs.
//!
//! `Jobs` owns the durable lifecycle boundary and the existing blocking registry;
//! shell and subagent execution retain process control and completion delivery.
//! Database writes are spawned after state locks are released, so synchronous
//! registry guards never cross an await.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use ox_db::Db;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::watch;

/// Durable job status exposed by the experimental job API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Completed,
    Error,
    Cancelled,
}

impl JobStatus {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "error" => Self::Error,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

impl JobStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Filters accepted by the experimental job list endpoint.
#[derive(Debug, Clone, Default)]
pub struct JobFilter<'a> {
    pub session_id: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub status: Option<JobStatus>,
    pub descendants: bool,
}

/// A durable job snapshot. Output is intentionally absent: shell captures stay
/// owned by `ShellSessions` and are not copied into the job table.
#[derive(Debug, Clone, PartialEq)]
pub struct JobSnapshot {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub title: String,
    pub status: JobStatus,
    pub started: i64,
    pub completed: Option<i64>,
    pub metadata: Value,
    /// Owning session directory used to stamp ephemeral lifecycle frames.
    pub directory: Option<String>,
    pub generation: i64,
}

impl JobSnapshot {
    #[must_use]
    pub fn event_directory(&self) -> Option<&str> {
        self.directory.as_deref().or_else(|| {
            self.metadata
                .get("__ox_job_directory")
                .and_then(Value::as_str)
        })
    }

    /// Convert a durable snapshot to the public `Job.Info` wire object.
    #[must_use]
    pub fn to_info(&self) -> Option<Value> {
        let mut info = serde_json::Map::new();
        info.insert("id".to_owned(), Value::String(self.id.clone()));
        info.insert(
            "sessionID".to_owned(),
            Value::String(self.session_id.clone()),
        );
        info.insert("kind".to_owned(), Value::String(self.kind.clone()));
        info.insert(
            "status".to_owned(),
            Value::String(self.status.as_str().to_owned()),
        );
        if !self.title.is_empty() {
            info.insert("title".to_owned(), Value::String(self.title.clone()));
        }
        let mut time = serde_json::Map::new();
        time.insert("started".to_owned(), Value::from(self.started));
        if let Some(completed) = self.completed {
            time.insert("completed".to_owned(), Value::from(completed));
        }
        info.insert("time".to_owned(), Value::Object(time));

        let metadata = self.metadata.as_object()?;
        match self.kind.as_str() {
            "shell" => {
                insert_metadata_string(&mut info, metadata, "shellID")?;
                insert_metadata_string(&mut info, metadata, "command")?;
            }
            "subagent" => {
                insert_metadata_string(&mut info, metadata, "childSessionID")?;
                insert_metadata_string(&mut info, metadata, "agent")?;
                insert_metadata_string(&mut info, metadata, "description")?;
            }
            _ => return None,
        }
        Some(Value::Object(info))
    }
}

fn metadata_with_directory(metadata: Option<Value>, directory: Option<&str>) -> Value {
    let mut metadata = metadata.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if let Some(directory) = directory
        && let Some(object) = metadata.as_object_mut()
    {
        object.insert(
            "__ox_job_directory".to_owned(),
            Value::String(directory.to_owned()),
        );
    }
    metadata
}

fn insert_metadata_string(
    info: &mut serde_json::Map<String, Value>,
    metadata: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<()> {
    info.insert(
        key.to_owned(),
        Value::String(metadata.get(key)?.as_str()?.to_owned()),
    );
    Some(())
}

/// Handle returned when a running row is registered. The generation prevents a
/// late completion from settling a newer reuse of the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobGeneration {
    pub generation: i64,
}

/// Snapshot of one job moved to the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub id: String,
    pub kind: &'static str,
    pub title: String,
}

/// Shared registry of jobs that foreground tool calls are waiting on.
const MAX_RETAINED_TERMINAL_JOBS: usize = 4096;
const MAX_SESSION_PARENTS: usize = 4096;
const MAX_RETAINED_DATABASE_JOBS: usize = MAX_RETAINED_TERMINAL_JOBS;

pub struct Jobs {
    state: Mutex<State>,
    durable: Mutex<Option<Db>>,
    persistence: Mutex<Option<Persistence>>,
    persistence_error: Arc<Mutex<Option<String>>>,
    observer: Mutex<Option<JobObserver>>,
    persistence_owner: crate::contract::TaskOwner,
}

pub type JobObserver = Arc<dyn Fn(&JobSnapshot) + Send + Sync + 'static>;

struct Persistence {
    sender: mpsc::UnboundedSender<PersistCommand>,
}

enum PersistCommand {
    Upsert(Box<JobSnapshot>),
    Barrier(tokio::sync::oneshot::Sender<anyhow::Result<()>>),
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            durable: Mutex::new(None),
            persistence: Mutex::new(None),
            persistence_error: Arc::new(Mutex::new(None)),
            observer: Mutex::new(None),
            persistence_owner: crate::contract::TaskOwner::default(),
        }
    }
}

#[derive(Default)]
struct State {
    next_seq: u64,
    entries: HashMap<String, Entry>,
    durable: HashMap<String, JobSnapshot>,
    /// Session parent links used by the synchronous job-list snapshot filter.
    session_parents: HashMap<String, String>,
    cancel: HashMap<String, Arc<dyn Fn() -> bool + Send + Sync>>,
}

struct Entry {
    /// Registration order, so `background_all` reports jobs in creation order.
    seq: u64,
    kind: &'static str,
    title: String,
    /// Blocked session id → number of live guards for it.
    blocking: HashMap<String, usize>,
    backgrounded: watch::Sender<bool>,
}

impl Jobs {
    /// Attach the synchronous observer used for live job lifecycle frames.
    pub fn set_observer(&self, observer: Option<JobObserver>) {
        *self.observer.lock().unwrap_or_else(|p| p.into_inner()) = observer;
    }

    /// Attach the database used for durable snapshots. This does not perform I/O.
    pub fn attach_db(&self, db: Db) {
        *self.durable.lock().unwrap_or_else(|p| p.into_inner()) = Some(db.clone());
        let mut persistence = self.persistence.lock().unwrap_or_else(|p| p.into_inner());
        if persistence.is_some() {
            return;
        }
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let error_slot = self.persistence_error.clone();
        let registered = self.persistence_owner.spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    PersistCommand::Upsert(snapshot) => {
                        if let Err(error) = persist_snapshot(&db, &snapshot).await {
                            *error_slot.lock().unwrap_or_else(|p| p.into_inner()) =
                                Some(error.to_string());
                        }
                    }
                    PersistCommand::Barrier(reply) => {
                        let result = error_slot
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .as_ref()
                            .map_or(Ok(()), |error| Err(anyhow::anyhow!(error.clone())));
                        let _ = reply.send(result);
                    }
                }
            }
        });
        if registered.is_ok() {
            *persistence = Some(Persistence { sender });
        } else {
            *self
                .persistence_error
                .lock()
                .unwrap_or_else(|p| p.into_inner()) =
                Some("job persistence task registration failed".to_owned());
        }
    }

    /// Record a session's persisted parent link for descendant filtering.
    pub fn record_session_parent(&self, session_id: &str, parent_id: Option<&str>) {
        let mut state = self.lock();
        if let Some(parent_id) = parent_id {
            state
                .session_parents
                .insert(session_id.to_owned(), parent_id.to_owned());
            trim_session_parents(&mut state.session_parents);
        } else {
            state.session_parents.remove(session_id);
        }
    }

    /// Load persisted session parent links used by descendant filtering.
    pub async fn load_session_parents(&self, db: &Db) -> anyhow::Result<()> {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT id, parent_id FROM session_v2 WHERE parent_id IS NOT NULL")
            .await?;
        let mut rows = stmt.query(()).await?;
        let mut parents = Vec::new();
        while let Some(row) = rows.next().await? {
            parents.push((row.get::<String>(0)?, row.get::<String>(1)?));
        }
        let mut state = self.lock();
        state.session_parents.extend(parents);
        trim_session_parents(&mut state.session_parents);
        Ok(())
    }

    /// Register a running job in the durable snapshot and return its generation.
    pub fn record_running(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        title: &str,
        metadata: Option<Value>,
    ) -> JobGeneration {
        self.record_running_at(id, session_id, kind, title, metadata, None)
    }

    pub fn record_running_at(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        title: &str,
        metadata: Option<Value>,
        directory: Option<String>,
    ) -> JobGeneration {
        let mut state = self.lock();
        if let Some(job) = state.durable.get(id)
            && job.status != JobStatus::Running
            && job.metadata.get("execution_owned") == Some(&Value::Bool(true))
            && job.session_id == session_id
            && job.kind == kind
            && job.title == title
        {
            return JobGeneration {
                generation: job.generation,
            };
        }
        let generation = state.durable.get(id).map_or(0, |job| job.generation + 1);
        let snapshot = JobSnapshot {
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            kind: kind.to_owned(),
            title: title.to_owned(),
            status: JobStatus::Running,
            started: chrono_millis(),
            completed: None,
            metadata: metadata_with_directory(metadata, directory.as_deref()),
            directory,
            generation,
        };
        state.durable.insert(id.to_owned(), snapshot.clone());
        trim_durable_jobs(&mut state.durable);
        drop(state);
        self.persist_upsert(&snapshot);
        self.observe(&snapshot);
        JobGeneration { generation }
    }

    /// Register a running job without replacing a terminal row created by an
    /// execution callback that won the handoff race.
    pub fn register_running(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        title: &str,
        metadata: Option<Value>,
    ) -> JobGeneration {
        self.register_running_at(id, session_id, kind, title, metadata, None)
    }

    pub fn register_running_at(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        title: &str,
        metadata: Option<Value>,
        directory: Option<String>,
    ) -> JobGeneration {
        let mut state = self.lock();
        if let Some(job) = state.durable.get(id) {
            return JobGeneration {
                generation: job.generation,
            };
        }
        let generation = 0;
        let snapshot = JobSnapshot {
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            kind: kind.to_owned(),
            title: title.to_owned(),
            status: JobStatus::Running,
            started: chrono_millis(),
            completed: None,
            metadata: metadata_with_directory(metadata, directory.as_deref()),
            directory,
            generation,
        };
        state.durable.insert(id.to_owned(), snapshot.clone());
        trim_durable_jobs(&mut state.durable);
        drop(state);
        self.persist_upsert(&snapshot);
        self.observe(&snapshot);
        JobGeneration { generation }
    }

    /// Ensure a completion callback has a durable row before settling it. This
    /// closes the process handoff race where a shell exits before its caller
    /// registers the row.
    pub fn ensure_running(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        title: &str,
        metadata: Option<Value>,
    ) -> JobGeneration {
        self.ensure_running_at(id, session_id, kind, title, metadata, None)
    }

    pub fn ensure_running_at(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        title: &str,
        metadata: Option<Value>,
        directory: Option<String>,
    ) -> JobGeneration {
        let mut state = self.lock();
        if let Some(job) = state.durable.get(id) {
            return JobGeneration {
                generation: job.generation,
            };
        }
        let generation = 0;
        let snapshot = JobSnapshot {
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            kind: kind.to_owned(),
            title: title.to_owned(),
            status: JobStatus::Running,
            started: chrono_millis(),
            completed: None,
            metadata: metadata_with_directory(metadata, directory.as_deref()),
            directory,
            generation,
        };
        state.durable.insert(id.to_owned(), snapshot.clone());
        trim_durable_jobs(&mut state.durable);
        drop(state);
        self.persist_upsert(&snapshot);
        self.observe(&snapshot);
        JobGeneration { generation }
    }

    /// Settle a row only when its generation is still current.
    pub fn complete(&self, id: &str, generation: i64, status: JobStatus, error: Option<&str>) {
        let mut state = self.lock();
        let Some(job) = state.durable.get_mut(id) else {
            return;
        };
        if job.generation != generation || job.status != JobStatus::Running {
            return;
        }
        job.status = status;
        job.completed = Some(chrono_millis());
        if let Some(error) = error
            && let Some(object) = job.metadata.as_object_mut()
        {
            object.insert("error".to_owned(), Value::String(error.to_owned()));
        }
        let snapshot = job.clone();
        state.cancel.remove(id);
        drop(state);
        self.persist_upsert(&snapshot);
        self.observe(&snapshot);
    }

    /// Settle the current generation, used by completion sinks that only carry
    /// the job id across the asynchronous process boundary.
    pub fn complete_current(&self, id: &str, status: JobStatus, error: Option<&str>) {
        let generation = self
            .lock()
            .durable
            .get(id)
            .filter(|job| job.status == JobStatus::Running)
            .map(|job| job.generation);
        if let Some(generation) = generation {
            self.complete(id, generation, status, error);
        }
    }

    /// Stop durable persistence after its queued snapshots have drained.
    ///
    /// Dropping the sender first lets the owned worker consume every queued
    /// command before [`TaskOwner::seal_and_join`] observes its completion.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.persistence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.persistence_owner.seal_and_join().await
    }

    /// Flush queued durable writes and refresh the process-local mirror from
    /// the database. API reads use this boundary so an in-memory snapshot can
    /// never outrun (or contradict) the authoritative job table.
    pub async fn durable_snapshot(
        &self,
        filter: JobFilter<'_>,
    ) -> anyhow::Result<Vec<JobSnapshot>> {
        let db = self
            .durable
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| anyhow::anyhow!("durable job database is not attached"))?;
        let persistence = self
            .persistence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|p| p.sender.clone());
        if let Some(sender) = persistence {
            let (reply, result) = tokio::sync::oneshot::channel();
            sender
                .send(PersistCommand::Barrier(reply))
                .map_err(|_| anyhow::anyhow!("job persistence task stopped"))?;
            result
                .await
                .map_err(|_| anyhow::anyhow!("job persistence barrier stopped"))??;
        }
        let loaded = load_rows(&db).await?;
        let mut state = self.lock();
        state
            .durable
            .extend(loaded.into_iter().map(|job| (job.id.clone(), job)));
        trim_durable_jobs(&mut state.durable);
        if let Some(error) = self
            .persistence_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Err(anyhow::anyhow!(error));
        }
        Ok(filter_snapshots(&state, filter))
    }

    /// Attach the process-specific cancellation action without exposing its
    /// execution map to HTTP callers.
    pub fn register_cancel(&self, id: &str, cancel: Arc<dyn Fn() -> bool + Send + Sync>) {
        self.lock().cancel.insert(id.to_owned(), cancel);
    }

    /// Request cancellation of a currently running job. The callback is called
    /// after releasing the registry lock; terminal settlement remains owned by
    /// the shell/subagent lifecycle.
    pub fn cancel(&self, id: &str) -> bool {
        let action = {
            let state = self.lock();
            if state
                .durable
                .get(id)
                .is_none_or(|job| job.status != JobStatus::Running)
            {
                return false;
            }
            state.cancel.get(id).cloned()
        };
        action.is_some_and(|action| action())
    }

    /// Return a bounded, process-local snapshot. Durable recovery is loaded by
    /// `load`; this method never exposes private execution maps or output.
    pub fn snapshot(&self, filter: JobFilter<'_>) -> Vec<JobSnapshot> {
        let state = self.lock();
        filter_snapshots(&state, filter)
    }

    /// Load durable rows on startup. Running rows remain queryable but no
    /// process is invented to execute them.
    pub async fn load(&self, db: &Db) -> anyhow::Result<()> {
        let loaded = load_rows(db).await?;
        let mut state = self.lock();
        state
            .durable
            .extend(loaded.into_iter().map(|job| (job.id.clone(), job)));
        trim_durable_jobs(&mut state.durable);
        Ok(())
    }

    fn persist_upsert(&self, snapshot: &JobSnapshot) {
        let sender = self
            .persistence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|p| p.sender.clone());
        let Some(sender) = sender else {
            return;
        };
        if sender
            .send(PersistCommand::Upsert(Box::new(snapshot.clone())))
            .is_err()
        {
            *self
                .persistence_error
                .lock()
                .unwrap_or_else(|p| p.into_inner()) =
                Some("job persistence task stopped".to_owned());
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Publish the durable rows recovered during startup after an observer is installed.
    pub fn observe_loaded(&self) {
        let snapshots: Vec<JobSnapshot> = self.lock().durable.values().cloned().collect();
        for snapshot in snapshots {
            self.observe(&snapshot);
        }
    }

    fn observe(&self, snapshot: &JobSnapshot) {
        let observer = self
            .observer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(observer) = observer {
            observer(snapshot);
        }
    }

    /// Register `session_id` as blocked on job `id`. Creates the entry when
    /// missing; an existing entry that is not backgrounded gains one more
    /// blocking reference for that session. Dropping the guard releases it.
    pub fn block(
        self: &Arc<Self>,
        id: &str,
        kind: &'static str,
        title: &str,
        session_id: &str,
    ) -> BlockGuard {
        let mut state = self.lock();
        if !state.entries.contains_key(id) {
            let seq = state.next_seq;
            state.next_seq += 1;
            state.entries.insert(
                id.to_owned(),
                Entry {
                    seq,
                    kind,
                    title: title.to_owned(),
                    blocking: HashMap::new(),
                    backgrounded: watch::Sender::new(false),
                },
            );
        }
        let entry = state
            .entries
            .get_mut(id)
            .expect("entry was inserted or found under the same lock");
        // A guard on an already-backgrounded entry holds no blocking
        // reference: its wait resolves at once and its drop releases nothing.
        let counted = !*entry.backgrounded.borrow();
        if counted {
            *entry.blocking.entry(session_id.to_owned()).or_insert(0) += 1;
        }
        let seq = entry.seq;
        let backgrounded = entry.backgrounded.subscribe();
        drop(state);
        BlockGuard {
            jobs: Arc::clone(self),
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            seq,
            counted,
            backgrounded,
        }
    }

    /// Upstream `backgroundAll`: every entry blocking `session_id` that is not
    /// yet backgrounded becomes backgrounded (its blocking map is cleared) and
    /// its waiters are woken. Returns those jobs in registration order; a
    /// second call returns nothing.
    pub fn background_all(&self, session_id: &str) -> Vec<JobInfo> {
        let mut state = self.lock();
        let mut moved: Vec<(u64, JobInfo)> = Vec::new();
        for (id, entry) in &mut state.entries {
            if *entry.backgrounded.borrow() || !entry.blocking.contains_key(session_id) {
                continue;
            }
            entry.blocking.clear();
            entry.backgrounded.send_replace(true);
            moved.push((
                entry.seq,
                JobInfo {
                    id: id.clone(),
                    kind: entry.kind,
                    title: entry.title.clone(),
                },
            ));
        }
        moved.sort_by_key(|(seq, _)| *seq);
        moved.into_iter().map(|(_, info)| info).collect()
    }

    /// Release one guard's registration. Only the entry generation the guard
    /// was made on is touched, so a stale guard never releases a newer entry
    /// that reuses the same id.
    fn release(&self, guard: &BlockGuard) {
        let mut state = self.lock();
        let Some(entry) = state.entries.get_mut(&guard.id) else {
            return;
        };
        if entry.seq != guard.seq {
            return;
        }
        if guard.counted
            && let Some(count) = entry.blocking.get_mut(&guard.session_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.blocking.remove(&guard.session_id);
            }
        }
        if entry.blocking.is_empty() {
            state.entries.remove(&guard.id);
        }
    }
}

fn chrono_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

async fn load_rows(db: &Db) -> anyhow::Result<Vec<JobSnapshot>> {
    let conn = db.acquire().await?;
    let mut loaded = load_running_rows(&conn).await?;
    loaded.extend(load_terminal_rows(&conn).await?);
    Ok(loaded)
}

async fn load_running_rows(conn: &ox_db::turso::Connection) -> anyhow::Result<Vec<JobSnapshot>> {
    let mut stmt = conn
        .prepare("SELECT id, session_id, kind, title, status, started, completed, metadata, generation FROM job WHERE status = 'running' ORDER BY started")
        .await?;
    let mut rows = stmt.query(()).await?;
    decode_rows(&mut rows).await
}

async fn load_terminal_rows(conn: &ox_db::turso::Connection) -> anyhow::Result<Vec<JobSnapshot>> {
    let mut stmt = conn
        .prepare("SELECT id, session_id, kind, title, status, started, completed, metadata, generation FROM job WHERE status <> 'running' ORDER BY started DESC LIMIT ?1")
        .await?;
    let mut rows = stmt.query((MAX_RETAINED_TERMINAL_JOBS as i64,)).await?;
    decode_rows(&mut rows).await
}

async fn decode_rows(rows: &mut ox_db::turso::Rows) -> anyhow::Result<Vec<JobSnapshot>> {
    let mut loaded = Vec::new();
    while let Some(row) = rows.next().await? {
        let status = JobStatus::parse(&row.get::<String>(4)?).unwrap_or(JobStatus::Running);
        loaded.push(JobSnapshot {
            id: row.get(0)?,
            session_id: row.get(1)?,
            kind: row.get(2)?,
            title: row.get(3)?,
            status,
            started: row.get(5)?,
            completed: row.get(6)?,
            metadata: serde_json::from_str(&row.get::<String>(7)?)
                .unwrap_or(Value::Object(serde_json::Map::new())),
            directory: None,
            generation: row.get(8)?,
        });
    }
    Ok(loaded)
}

fn filter_snapshots(state: &State, filter: JobFilter<'_>) -> Vec<JobSnapshot> {
    let mut snapshots: Vec<JobSnapshot> = state
        .durable
        .values()
        .filter(|job| job.kind == "shell" || job.kind == "subagent")
        .filter(|job| filter.kind.is_none_or(|kind| job.kind == kind))
        .filter(|job| filter.status.is_none_or(|status| job.status == status))
        .filter(|job| {
            filter.session_id.is_none_or(|session| {
                job.session_id == session
                    || filter.descendants
                        && is_descendant(&state.session_parents, &job.session_id, session)
            })
        })
        .cloned()
        .collect();
    snapshots.sort_by_key(|job| job.started);
    snapshots
}

async fn persist_snapshot(db: &Db, snapshot: &JobSnapshot) -> anyhow::Result<()> {
    let conn = db.acquire().await?;
    let metadata = snapshot.metadata.to_string();
    let params = (
        snapshot.id.as_str(),
        snapshot.session_id.as_str(),
        snapshot.kind.as_str(),
        snapshot.title.as_str(),
        snapshot.status.as_str(),
        snapshot.started,
        snapshot.completed,
        metadata.as_str(),
        snapshot.generation,
    );
    conn.execute("INSERT INTO job (id, session_id, kind, title, status, started, completed, metadata, generation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id, kind=excluded.kind, title=excluded.title, status=excluded.status, started=excluded.started, completed=excluded.completed, metadata=excluded.metadata, generation=excluded.generation WHERE excluded.generation > job.generation OR (excluded.generation = job.generation AND job.status = 'running' AND excluded.status <> 'running')", params).await?;
    conn.execute(
        "DELETE FROM job WHERE status <> 'running' AND id NOT IN (SELECT id FROM job WHERE status <> 'running' ORDER BY started DESC LIMIT ?1)",
        (MAX_RETAINED_DATABASE_JOBS as i64,),
    )
    .await?;
    Ok(())
}

fn trim_durable_jobs(jobs: &mut HashMap<String, JobSnapshot>) {
    while jobs.len() > MAX_RETAINED_TERMINAL_JOBS {
        let Some(id) = jobs
            .iter()
            .filter(|(_, job)| job.status != JobStatus::Running)
            .min_by_key(|(_, job)| job.completed.unwrap_or(job.started))
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        jobs.remove(&id);
    }
}

fn trim_session_parents(parents: &mut HashMap<String, String>) {
    while parents.len() > MAX_SESSION_PARENTS {
        let Some(id) = parents.keys().next().cloned() else {
            break;
        };
        parents.remove(&id);
    }
}
fn is_descendant(parents: &HashMap<String, String>, session_id: &str, ancestor: &str) -> bool {
    let mut current = session_id.to_owned();
    let mut visited = std::collections::HashSet::new();
    while let Some(parent) = parents.get(&current) {
        if !visited.insert(current.clone()) {
            return false;
        }
        if parent == ancestor {
            return true;
        }
        current.clone_from(parent);
    }
    false
}

/// A session's registration as blocked on one job; dropping it releases the
/// registration.
#[must_use = "dropping the guard releases the blocking registration at once"]
pub struct BlockGuard {
    jobs: Arc<Jobs>,
    id: String,
    session_id: String,
    /// The entry generation this guard belongs to.
    seq: u64,
    /// Whether this guard holds a blocking reference on that entry.
    counted: bool,
    backgrounded: watch::Receiver<bool>,
}

impl BlockGuard {
    /// Resolves once the job is backgrounded, including when that happened
    /// before this call. Never resolves for a job that was not backgrounded.
    pub async fn backgrounded(&self) {
        let mut receiver = self.backgrounded.clone();
        // `wait_for` checks the current value before waiting, so an earlier
        // backgrounding is never missed. A closed channel resolves only if the
        // last value says backgrounded; otherwise fail closed and keep waiting.
        if receiver
            .wait_for(|backgrounded| *backgrounded)
            .await
            .is_err()
            && !*receiver.borrow()
        {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for BlockGuard {
    fn drop(&mut self) {
        self.jobs.release(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_snapshot_filters_supported_jobs_and_descendants() {
        let jobs = Jobs::default();
        jobs.record_running("shell-1", "root", "shell", "echo hi", None);
        jobs.record_session_parent("child", Some("root"));
        jobs.record_running(
            "child-1",
            "child",
            "subagent",
            "review",
            Some(serde_json::json!({ "parentSessionID": "root" })),
        );
        jobs.record_running("video-1", "root", "video", "not supported", None);

        let jobs = jobs.snapshot(JobFilter {
            session_id: Some("root"),
            descendants: true,
            ..JobFilter::default()
        });

        assert_eq!(jobs.len(), 2, "only shell and subagent jobs are exposed");
        assert!(jobs.iter().any(|job| job.id == "shell-1"));
        assert!(jobs.iter().any(|job| job.id == "child-1"));
    }

    #[test]
    fn descendants_filter_transitive_session_hierarchy() {
        let jobs = Jobs::default();
        jobs.record_session_parent("child", Some("root"));
        jobs.record_session_parent("grand", Some("child"));
        jobs.record_running(
            "root-child",
            "child",
            "subagent",
            "child",
            Some(serde_json::json!({
                "parentSessionID": "root",
            })),
        );
        jobs.record_running(
            "grandchild",
            "grand",
            "subagent",
            "grandchild",
            Some(serde_json::json!({
                "parentSessionID": "child",
            })),
        );
        jobs.record_running("grand-shell", "grand", "shell", "echo nested", None);
        jobs.record_running("unrelated", "other", "shell", "echo unrelated", None);

        let snapshots = jobs.snapshot(JobFilter {
            session_id: Some("root"),
            descendants: true,
            ..JobFilter::default()
        });

        let mut ids: Vec<&str> = snapshots.iter().map(|job| job.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["grand-shell", "grandchild", "root-child"]);
    }

    #[test]
    fn job_info_mapper_emits_exact_shell_wire_shape() {
        let snapshot = JobSnapshot {
            id: "job-shell".to_owned(),
            session_id: "session-1".to_owned(),
            kind: "shell".to_owned(),
            title: "printf hi".to_owned(),
            status: JobStatus::Completed,
            started: 10,
            completed: Some(20),
            metadata: serde_json::json!({
                "shellID": "shell-1",
                "command": "printf hi",
                "error": "must not leak",
            }),
            directory: None,
            generation: 0,
        };

        assert_eq!(
            snapshot.to_info(),
            Some(serde_json::json!({
                "id": "job-shell",
                "sessionID": "session-1",
                "kind": "shell",
                "status": "completed",
                "title": "printf hi",
                "time": { "started": 10, "completed": 20 },
                "shellID": "shell-1",
                "command": "printf hi",
            }))
        );
    }

    #[test]
    fn job_info_mapper_emits_exact_subagent_wire_shape() {
        let snapshot = JobSnapshot {
            id: "job-child".to_owned(),
            session_id: "parent".to_owned(),
            kind: "subagent".to_owned(),
            title: "review".to_owned(),
            status: JobStatus::Running,
            started: 30,
            completed: None,
            metadata: serde_json::json!({
                "parentSessionID": "parent",
                "childSessionID": "child",
                "agent": "review",
                "description": "review",
            }),
            directory: None,
            generation: 0,
        };

        assert_eq!(
            snapshot.to_info(),
            Some(serde_json::json!({
                "id": "job-child",
                "sessionID": "parent",
                "kind": "subagent",
                "status": "running",
                "title": "review",
                "time": { "started": 30 },
                "childSessionID": "child",
                "agent": "review",
                "description": "review",
            }))
        );
    }

    #[test]
    fn job_info_mapper_rejects_invalid_legacy_records() {
        let snapshot = JobSnapshot {
            id: "legacy".to_owned(),
            session_id: "session".to_owned(),
            kind: "shell".to_owned(),
            title: "old".to_owned(),
            status: JobStatus::Completed,
            started: 1,
            completed: Some(2),
            metadata: serde_json::json!({ "name": "old", "time": { "created": 1 } }),
            directory: None,
            generation: 0,
        };

        assert_eq!(snapshot.to_info(), None);
    }

    #[test]
    fn observer_receives_only_current_running_and_terminal_transitions() {
        let jobs = Jobs::default();
        let observed = Arc::new(Mutex::new(Vec::<(String, JobStatus, i64)>::new()));
        let sink = Arc::clone(&observed);
        jobs.set_observer(Some(Arc::new(move |snapshot| {
            sink.lock().expect("observer lock").push((
                snapshot.id.clone(),
                snapshot.status,
                snapshot.generation,
            ));
        })));

        let first = jobs.record_running("same", "session", "shell", "first", None);
        jobs.complete("same", first.generation, JobStatus::Completed, None);
        let second = jobs.record_running("same", "session", "shell", "second", None);
        jobs.complete("same", first.generation, JobStatus::Error, Some("stale"));
        jobs.complete("same", second.generation, JobStatus::Error, Some("current"));
        jobs.complete(
            "same",
            second.generation,
            JobStatus::Error,
            Some("duplicate"),
        );

        assert_eq!(
            *observed.lock().expect("observer lock"),
            vec![
                ("same".to_owned(), JobStatus::Running, first.generation),
                ("same".to_owned(), JobStatus::Completed, first.generation),
                ("same".to_owned(), JobStatus::Running, second.generation),
                ("same".to_owned(), JobStatus::Error, second.generation),
            ]
        );
    }
}
