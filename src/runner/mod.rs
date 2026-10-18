//! Per-session execution actors (Task 6a).
//!
//! One actor task per active session, spawned lazily by [`Runner::dispatch`]
//! and parked (dropped) when the session goes idle — the same process-local
//! ownership model as upstream `packages/core/src/session/execution.ts`
//! (`SessionRunCoordinator`), collapsed to a plain tokio task:
//!
//! - **Busy period**: consecutive [`drain::drain`] passes covering every
//!   inbox item admitted while work was in flight. Exactly one
//!   `session.execution.started` opens the period and one terminal
//!   (`succeeded` / `failed` / `interrupted`) closes it, mirroring upstream's
//!   one-terminal-per-busy-period rule.
//! - **Interruption**: the endpoint cancels the busy period's
//!   [`CancellationToken`]; the drain aborts at the next await point and the
//!   actor publishes the terminal. Idle interruption is a no-op.
//! - **Parked handoff**: an idle session's mailbox receiver moves back into
//!   the registry entry; the next dispatch takes it out and spawns a fresh
//!   task. Send always happens before the take, and the parked task re-checks
//!   the channel under the slot mutex before parking, so no message can fall
//!   into the gap.
//!
//! The [`Tool`] trait + [`ToolRegistry`] here are the seam task 7 fills with
//! real tools; the registry ships empty and steps in 6a never execute tools.

pub use crate::cardinal::chat::Chat;

pub mod drain;
pub mod step;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, bail};
use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cardinal::wire::ToolDef;
use crate::config::Config;
use crate::db::Db;
use crate::event_bus::EventBus;
use crate::schema::event::{InterruptReason, SessionEventData};
use crate::schema::support::SessionError;

/// Messages the per-session actor mailbox accepts.
#[derive(Debug)]
pub enum ActorMsg {
    /// An inbox item was admitted; join (or open) the session's busy period.
    Enqueue {
        /// The `session_inbox` row id that was enqueued.
        inbox_id: String,
    },
    /// Abort the current busy period. The endpoint also cancels the period's
    /// token directly; this message covers the mailbox-absorb bookkeeping so
    /// a racing interrupt is never lost between absorb loops.
    Interrupt,
    /// Stop serving this session and drop the actor entirely.
    Shutdown,
}

/// Actor mailbox capacity. Prompt dispatch is one message per turn, so a
/// small bounded queue is ample; a full mailbox means a runaway client.
const MAILBOX_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// Tools (registry filled by task 7; exercised by tests)
// ---------------------------------------------------------------------------

/// Execution-scoped context handed to [`Tool::execute`].
pub struct ToolContext {
    /// Owning session.
    pub session_id: String,
    /// Canonical project directory the server runs in.
    pub directory: PathBuf,
    /// The assistant tool-call id this execution answers.
    pub call_id: String,
    /// Fires when the turn is interrupted; long tools should poll it.
    pub cancellation: CancellationToken,
}

/// A tool's terminal output (task 7 streams progress; 6a settles once).
pub struct ToolOutput {
    /// Content for the model (JSON text or structured value).
    pub content: Value,
    /// `Some` marks the execution as failed with this message.
    pub error: Option<String>,
}

/// A model-invocable unit of work. Task 7 registers real implementations;
/// the runner only needs the name/definition seam plus one async entry.
pub trait Tool: Send + Sync {
    /// Registry key; must equal `definition().function.name`.
    fn name(&self) -> &str;
    /// The OpenAI `tools` array entry advertised to the model.
    fn definition(&self) -> ToolDef;
    /// Run the tool with the parsed `arguments` object.
    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>>;
}

/// Name-keyed tool registry. Definitions iterate in name order so the wire
/// `tools` array is stable across steps (keeps the provider's cached prefix).
#[derive(Default)]
pub struct ToolRegistry {
    tools: std::collections::BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool; duplicate names are a programming error.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.name().to_owned();
        if self.tools.insert(name.clone(), tool).is_some() {
            bail!("tool `{name}` is already registered");
        }
        Ok(())
    }

    /// Look a tool up by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Whether no tools are registered (6a ships empty).
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The OpenAI `tools` array in name order.
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }
}

// ---------------------------------------------------------------------------
// Runner registry
// ---------------------------------------------------------------------------

/// Registry of per-session actors. `dispatch` lazily spawns; interrupt and
/// shutdown reach the live actor through its registry entry.
#[derive(Clone)]
pub struct Runner {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    /// Canonical project working directory (system prompt + tool roots).
    directory: PathBuf,
    db: Db,
    bus: EventBus,
    chat: Arc<dyn Chat>,
    tools: Arc<ToolRegistry>,
    sessions: DashMap<String, SessionHandle>,
}

/// Everything an actor task shares with the registry.
#[derive(Clone)]
struct SessionHandle {
    tx: mpsc::Sender<ActorMsg>,
    /// `Some` while no task owns the mailbox receiver (session parked).
    parked: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<ActorMsg>>>>,
    /// Cancellation of the busy period in flight, if any.
    cancel: Arc<Mutex<Option<CancellationToken>>>,
}

/// Lock a std mutex, recovering from poisoning (a panic in one actor must
/// not wedge the registry; the guard data is always consistent enough).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Runner {
    /// A runner over the given bus/db/config rooted at `directory`, with the
    /// given chat transport and (initially empty or test-populated) tools.
    pub fn new(
        config: Config,
        directory: PathBuf,
        db: Db,
        bus: EventBus,
        chat: Arc<dyn Chat>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                directory,
                db,
                bus,
                chat,
                tools,
                sessions: DashMap::new(),
            }),
        }
    }

    /// Whether a busy period is currently in flight (process-local view).
    pub fn is_active(&self, session_id: &str) -> bool {
        self.inner
            .sessions
            .get(session_id)
            .is_some_and(|handle| lock(&handle.cancel).is_some())
    }

    /// Wake the session's actor for a drain: spawn one if parked, otherwise
    /// join the live task's mailbox. Safe to call repeatedly; the actor
    /// coalesces enqueues into one busy period.
    pub async fn dispatch(&self, session_id: &str, inbox_id: &str) -> Result<()> {
        let handle = self.handle(session_id);
        // Send BEFORE taking the parked receiver: a task parking concurrently
        // re-checks the channel under the slot lock, so a message that beat
        // the send is picked up by exactly one side.
        let _ = handle
            .tx
            .send(ActorMsg::Enqueue {
                inbox_id: inbox_id.to_owned(),
            })
            .await;
        self.spawn_if_parked(session_id, handle).await;
        Ok(())
    }

    /// Cancel the session's in-flight busy period. Returns whether one was
    /// running (idle interruption is a no-op, matching upstream).
    pub fn interrupt(&self, session_id: &str) -> bool {
        let Some(handle) = self.inner.sessions.get(session_id) else {
            return false;
        };
        let Some(token) = lock(&handle.cancel).take() else {
            return false;
        };
        token.cancel();
        // Wake the actor's absorb loop so the terminal settles promptly even
        // if the drain is parked between awaits.
        let _ = handle.tx.try_send(ActorMsg::Interrupt);
        true
    }

    /// Ask every live actor to stop. Dispatching afterwards starts a fresh
    /// actor (used by process-shutdown paths; task 9 wires signals).
    pub fn shutdown(&self) {
        for entry in self.inner.sessions.iter() {
            let _ = entry.value().tx.try_send(ActorMsg::Shutdown);
        }
    }

    /// Registry entry for `session_id`, creating (parked) when absent.
    fn handle(&self, session_id: &str) -> SessionHandle {
        self.inner
            .sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
                SessionHandle {
                    tx,
                    parked: Arc::new(tokio::sync::Mutex::new(Some(rx))),
                    cancel: Arc::new(Mutex::new(None)),
                }
            })
            .clone()
    }

    /// Take the parked receiver and hand it to a freshly spawned actor task.
    /// The parked task stores the receiver under the same mutex, so take and
    /// park are mutually exclusive: at most one live task per session.
    async fn spawn_if_parked(&self, session_id: &str, handle: SessionHandle) {
        let Some(rx) = handle.parked.lock().await.take() else {
            return;
        };
        let task = ActorTask {
            inner: self.inner.clone(),
            session_id: session_id.to_owned(),
            handle,
            rx: Some(rx),
        };
        tokio::spawn(task.run());
    }
}

// ---------------------------------------------------------------------------
// Actor task
// ---------------------------------------------------------------------------

/// One live actor. Owns the mailbox receiver while running; parks it back on
/// idle and exits (zero tasks for an idle session).
struct ActorTask {
    inner: Arc<Inner>,
    session_id: String,
    handle: SessionHandle,
    /// `Some` until parked; taken at task start.
    rx: Option<mpsc::Receiver<ActorMsg>>,
}

impl ActorTask {
    async fn run(mut self) {
        let Some(mut rx) = self.rx.take() else {
            return;
        };
        loop {
            // Absorb the backlog; the first Enqueue opens a busy period.
            let mut token: Option<CancellationToken> = None;
            loop {
                match rx.try_recv() {
                    Ok(ActorMsg::Enqueue { .. }) if token.is_none() => {
                        let period = CancellationToken::new();
                        *lock(&self.handle.cancel) = Some(period.clone());
                        // Upstream: the started hook publishes exactly once per
                        // busy period, before the first drain.
                        if let Err(error) = self
                            .inner
                            .bus
                            .publish(SessionEventData::ExecutionStarted {
                                session_id: self.session_id.clone(),
                            })
                            .await
                        {
                            tracing::error!(
                                session = %self.session_id,
                                error = %error,
                                "execution.started failed; releasing busy period"
                            );
                            *lock(&self.handle.cancel) = None;
                            break;
                        }
                        token = Some(period);
                    }
                    Ok(ActorMsg::Interrupt) => {
                        // The endpoint cancelled the token; this covers the
                        // absorb-window race where the slot was already taken.
                        if let Some(period) = &token {
                            period.cancel();
                        }
                    }
                    Ok(ActorMsg::Shutdown) => {
                        self.handle_shutdown();
                        return;
                    }
                    Ok(_) => {}
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            let Some(period) = token else {
                if !self.park(&mut rx).await {
                    return;
                }
                continue;
            };

            // Busy period: consecutive drains until the inbox is quiet. Every
            // coalesced drain shares one terminal publication.
            let ctx = drain::DrainCtx {
                session_id: self.session_id.clone(),
                directory: self.inner.directory.clone(),
                config: self.inner.config.clone(),
                db: self.inner.db.clone(),
                bus: self.inner.bus.clone(),
                chat: self.inner.chat.clone(),
                tools: self.inner.tools.clone(),
            };
            let mut scope = drain::Promotable::Input;
            loop {
                if period.is_cancelled() {
                    self.terminal_interrupted().await;
                    break;
                }
                match drain::drain(&ctx, &period, scope).await {
                    drain::DrainOutcome::Interrupted => {
                        self.terminal_interrupted().await;
                        break;
                    }
                    drain::DrainOutcome::Failed(error) => {
                        if let Err(publish_error) = self
                            .inner
                            .bus
                            .publish(SessionEventData::ExecutionFailed {
                                session_id: self.session_id.clone(),
                                error,
                            })
                            .await
                        {
                            tracing::error!(
                                session = %self.session_id,
                                error = %publish_error,
                                "execution.failed failed to publish"
                            );
                        }
                        break;
                    }
                    drain::DrainOutcome::Complete => {
                        // Doorbell: work admitted during the drain keeps the
                        // busy period open instead of publishing a terminal.
                        let mut more = false;
                        loop {
                            match rx.try_recv() {
                                Ok(ActorMsg::Enqueue { .. }) => more = true,
                                Ok(ActorMsg::Interrupt) => period.cancel(),
                                Ok(ActorMsg::Shutdown) => {
                                    self.handle_shutdown();
                                    return;
                                }
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => return,
                            }
                        }
                        if period.is_cancelled() {
                            self.terminal_interrupted().await;
                            break;
                        }
                        if !more {
                            if let Err(error) = self
                                .inner
                                .bus
                                .publish(SessionEventData::ExecutionSucceeded {
                                    session_id: self.session_id.clone(),
                                })
                                .await
                            {
                                tracing::error!(
                                    session = %self.session_id,
                                    error = %error,
                                    "execution.succeeded failed to publish"
                                );
                            }
                            break;
                        }
                        // Fresh work: the next pass starts at an input boundary.
                        scope = drain::Promotable::Input;
                    }
                }
            }
            *lock(&self.handle.cancel) = None;
            // Loop: absorb anything that arrived while the terminal settled.
        }
    }

    fn handle_shutdown(&self) {
        // The actor is done for good; drop the registry entry so a later
        // dispatch builds a fresh handle instead of messaging a dead mailbox.
        self.inner.sessions.remove(&self.session_id);
    }

    async fn terminal_interrupted(&self) {
        if let Err(error) = self
            .inner
            .bus
            .publish(SessionEventData::ExecutionInterrupted {
                session_id: self.session_id.clone(),
                reason: InterruptReason::User,
            })
            .await
        {
            tracing::error!(
                session = %self.session_id,
                error = %error,
                "execution.interrupted failed to publish"
            );
        }
    }

    /// Hand the receiver back to the registry and exit (the session is idle:
    /// zero tasks). Returns true to re-enter the absorb loop when a message
    /// was already waiting.
    async fn park(&self, rx: &mut mpsc::Receiver<ActorMsg>) -> bool {
        let mut slot = self.handle.parked.lock().await;
        if !rx.is_empty() {
            return true;
        }
        // Any dispatch past this point finds the slot occupied and spawns a
        // fresh task, so the handoff loses nothing.
        let parked = std::mem::replace(rx, mpsc::channel(1).1);
        *slot = Some(parked);
        false
    }
}

/// Session errors surfaced by the drain loop carry the `unknown` type (the
/// upstream `toSessionError` fallback for non-schema errors).
pub(crate) fn unknown_error(message: impl Into<String>) -> SessionError {
    SessionError {
        kind: "unknown".to_owned(),
        message: message.into(),
        status: None,
        response: None,
    }
}
