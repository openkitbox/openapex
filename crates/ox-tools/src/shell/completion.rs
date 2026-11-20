//! Completion notifications for background shells (upstream
//! `packages/core/src/shell/result.ts` `notification`): a shell that was
//! handed back to the model as running reports its terminal state once, as a
//! `source=shell` synthetic inbox item in the session that started it, and
//! wakes that session through the runner.
//!
//! Split in two so the mapping is testable without a database: [`payload`] is
//! a pure function from a [`Completion`] to the wire payload, and
//! [`ShellCompletion`] is the edge that persists the item and dispatches the
//! wake-up.
//!
//! # Deviations from upstream (`shell/result.ts`, `tool/plugin/shell.ts`)
//!
//! Recorded here because this module owns the wire shape of the notification:
//!
//! - `signal` is omitted from the metadata: [`Completion::signal`] feeds the
//!   notice text only, and a stop shows as the `cancelled` state.
//! - An empty capture renders as `(no output)`; upstream renders an empty body.
//! - A truncated capture ends with a `[output truncated; full output is paged
//!   through GET /api/shell/{id}/output]` footer pointing at the paging
//!   endpoint; upstream points at its capture file.
//! - The timeout notice is the bare `Timed out before completion`; upstream
//!   additionally appends `Command exceeded timeout of N ms...` to the output
//!   (the one-shot tool result in `shell/mod.rs` does append it).
//! - The `running` tool result lacks `truncated: false` in its metadata.
//! - A nonzero exit is `completed` with an `Exited with code N` notice, as
//!   upstream `notifyWhenDone` does; only a run with neither exit code nor
//!   signal (a wait failure) is `error`, with the fixed text `Command
//!   failed` instead of upstream's `info.error`.
//! - `jobID` is not emitted: ox has one id per shell, the `shellID`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_schema::inbox::{Delivery, Item, SyntheticPayload};
use ox_schema::support::Record;
use serde_json::json;

use super::sessions::{Completion, CompletionDelivery, Status};

/// A completion sink that also learns which session the shell belongs to.
/// The tool binds the session id per background call, so the shell session table
/// stays unaware of sessions.
pub type SessionSink = Arc<dyn Fn(&str, Completion) -> CompletionDelivery + Send + Sync + 'static>;

/// Wakes a session for a newly enqueued inbox item. Implemented by the
/// runner; a trait here keeps the shell session table from depending on it.
pub trait ShellWaker: Send + Sync {
    fn try_admit(&self) -> Result<crate::contract::ExecutionAdmission, crate::contract::Closing>;
    fn dispatch<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

/// The completion sink of the `shell` tool. Owns no session state: it
/// persists through the event bus and wakes through [`ShellWaker`].
pub struct ShellCompletion {
    db: Db,
    bus: EventBus,
    jobs: Arc<crate::jobs::Jobs>,
    /// `None` until the runner attaches; a completion without a waker is
    /// dropped (fail closed) rather than persisted and left undelivered.
    waker: OnceLock<Arc<dyn ShellWaker>>,
    wakes_closed: std::sync::atomic::AtomicBool,
    deliveries: crate::contract::TaskOwner,
}

impl ShellCompletion {
    #[must_use]
    pub fn new(db: Db, bus: EventBus, jobs: Arc<crate::jobs::Jobs>) -> Self {
        Self {
            db,
            bus,
            jobs,
            waker: OnceLock::new(),
            wakes_closed: std::sync::atomic::AtomicBool::new(false),
            deliveries: crate::contract::TaskOwner::default(),
        }
    }

    pub fn close_wakes(&self) {
        self.wakes_closed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub async fn seal_and_join(&self) -> anyhow::Result<()> {
        self.close_wakes();
        self.deliveries.seal_and_join().await
    }

    /// The waker connecting completions to the runner. Registering twice keeps
    /// the first binding (startup happens once).
    pub fn attach(&self, waker: Arc<dyn ShellWaker>) {
        let _ = self.waker.set(waker);
    }

    /// The sink returns a future through persistence and wake-up. ShellSession
    /// owns its execution and retains completion-gate debt until it returns;
    /// the process drive loop never waits on the database.
    pub fn sink(self: &Arc<Self>) -> SessionSink {
        let this = Arc::clone(self);
        Arc::new(move |session_id: &str, completion: Completion| {
            let this = Arc::clone(&this);
            let session_id = session_id.to_owned();
            let shell_id = completion.id.clone();
            let payload = payload(&completion);
            let generation =
                this.jobs
                    .ensure_running(&shell_id, &session_id, "shell", &completion.command);
            let (reply, result) = tokio::sync::oneshot::channel();
            let owner = this.deliveries.clone();
            if this.wakes_closed.load(std::sync::atomic::Ordering::Acquire) {
                this.jobs.complete(
                    &shell_id,
                    generation.generation,
                    crate::jobs::JobStatus::Error,
                    Some("completion delivery is closed"),
                );
                return super::sessions::CompletionDelivery {
                    future: Box::pin(async {}),
                    on_registration_failure: Box::new(|| {}),
                };
            }
            let jobs = Arc::clone(&this.jobs);
            let task_jobs = Arc::clone(&jobs);
            let task_completion = Arc::clone(&this);
            let task_shell_id = shell_id.clone();
            let task_generation = generation;
            let registered = owner.spawn(async move {
                let (status, error) = completion_job_state(&payload);
                let delivered = task_completion
                    .enqueue_and_wake(&session_id, &task_shell_id, payload)
                    .await;
                if delivered {
                    task_jobs.complete(&task_shell_id, task_generation.generation, status, error);
                } else {
                    task_jobs.complete(
                        &task_shell_id,
                        task_generation.generation,
                        crate::jobs::JobStatus::Error,
                        Some("completion delivery failed"),
                    );
                }
                let _ = reply.send(());
            });
            let registration_error = registered.as_ref().err().map(ToString::to_string);
            if let Some(error) = registration_error {
                tracing::error!(%error, "shell completion registration closed");
                this.jobs.complete(
                    &shell_id,
                    generation.generation,
                    crate::jobs::JobStatus::Error,
                    Some("completion task registration failed"),
                );
            }
            let registration_jobs = Arc::clone(&this.jobs);
            let registration_shell_id = shell_id.clone();
            CompletionDelivery {
                future: Box::pin(async move {
                    if registered.is_err() {
                        return;
                    }
                    if result.await.is_err() {
                        tracing::error!("shell completion task failed");
                        jobs.complete(
                            &shell_id,
                            generation.generation,
                            crate::jobs::JobStatus::Error,
                            Some("completion task failed"),
                        );
                    }
                }),
                on_registration_failure: Box::new(move || {
                    registration_jobs.complete(
                        &registration_shell_id,
                        generation.generation,
                        crate::jobs::JobStatus::Error,
                        Some("completion task registration failed"),
                    );
                }),
            }
        })
    }

    async fn enqueue_and_wake(
        &self,
        session_id: &str,
        shell_id: &str,
        payload: SyntheticPayload,
    ) -> bool {
        let Some(waker) = self.waker.get() else {
            tracing::warn!(parent = %session_id, shell = %shell_id, "shell completion waker not attached; dropping completion");
            return false;
        };
        match parent_exists(&self.db, session_id).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(parent = %session_id, shell = %shell_id, "parent session gone; dropping shell completion");
                return false;
            }
            Err(error) => {
                tracing::warn!(parent = %session_id, shell = %shell_id, error = %error, "unable to check parent session; dropping shell completion");
                return false;
            }
        }
        let inbox_id = ox_schema::id::id("msg");
        let enqueued = SessionEventData::InboxEnqueued {
            session_id: session_id.to_owned(),
            inbox_id: inbox_id.clone(),
            item: Item::Synthetic {
                payload,
                delivery: Delivery::Steer,
            },
        };
        if let Err(error) = self.bus.publish(enqueued).await {
            tracing::warn!(parent = %session_id, shell = %shell_id, error = %error, "unable to enqueue shell completion");
            return false;
        }
        if self.wakes_closed.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        if let Err(error) = waker.dispatch(session_id, &inbox_id).await {
            tracing::warn!(parent = %session_id, shell = %shell_id, error = %error, "unable to wake parent for shell completion");
            return false;
        }
        true
    }
}

/// Convert the synthetic state into the durable job terminal state.
fn completion_job_state(
    payload: &SyntheticPayload,
) -> (crate::jobs::JobStatus, Option<&'static str>) {
    match payload
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("state"))
        .and_then(serde_json::Value::as_str)
    {
        Some("completed") => (crate::jobs::JobStatus::Completed, None),
        Some("cancelled") => (crate::jobs::JobStatus::Cancelled, None),
        _ => (crate::jobs::JobStatus::Error, Some("completion failed")),
    }
}

/// Whether a session row exists. A completion for a deleted parent is dropped.
async fn parent_exists(db: &Db, session_id: &str) -> Result<bool, String> {
    let conn = db.acquire().await.map_err(|error| error.to_string())?;
    let mut stmt = conn
        .prepare("SELECT 1 FROM session_v2 WHERE id = ?1")
        .await
        .map_err(|error| error.to_string())?;
    let row = ox_db::optional(&mut stmt, (session_id,))
        .await
        .map_err(|error| error.to_string())?;
    Ok(row.is_some())
}

/// The synthetic payload for a settled shell. Pure: no I/O, no clock.
///
/// State mapping follows [`classify`]. Metadata carries `source`, `shellID`,
/// `state`, `truncated`, and `exit` / `timeout` when they apply.
#[must_use]
pub fn payload(completion: &Completion) -> SyntheticPayload {
    let (state, notice) = classify(completion);
    // The tail is already bounded by the session; apply the model-facing line
    // cap here, the same cap a one-shot result gets.
    let (output, lines_truncated, _, _) = super::truncate_output(&completion.output);
    let output = output.trim_end_matches('\n');
    let truncated = completion.truncated || lines_truncated;

    let mut parts: Vec<String> = Vec::new();
    parts.push(if output.is_empty() {
        "(no output)".to_owned()
    } else {
        output.to_owned()
    });
    if let Some(notice) = notice {
        parts.push(notice);
    }
    if truncated {
        parts.push(format!(
            "[output truncated; full output is paged through GET /api/shell/{}/output]",
            completion.id
        ));
    }
    let body = parts.join("\n\n");
    let text = format!(
        "<shell id=\"{id}\" state=\"{state}\" command=\"{command}\">\n{body}\n</shell>",
        id = completion.id,
        command = completion.command,
    );

    let mut metadata = Record::new();
    metadata.insert("source".to_owned(), json!("shell"));
    metadata.insert("shellID".to_owned(), json!(completion.id));
    metadata.insert("state".to_owned(), json!(state));
    metadata.insert("truncated".to_owned(), json!(truncated));
    if let Some(code) = completion.exit {
        metadata.insert("exit".to_owned(), json!(code));
    }
    if completion.status == Status::Timeout {
        metadata.insert("timeout".to_owned(), json!(true));
    }

    SyntheticPayload {
        text,
        description: Some(completion.command.clone()),
        metadata: Some(metadata),
    }
}

/// `(state, notice)` for a settled run, mirroring upstream `notifyWhenDone`:
/// the state is the job's, not the process's, so a command that ran and exited
/// nonzero is still `completed` and says so in the notice. Only a run with no
/// exit code at all (a wait failure) is `error`.
fn classify(completion: &Completion) -> (&'static str, Option<String>) {
    match completion.status {
        Status::Killed => ("cancelled", Some("Cancelled".to_owned())),
        Status::Timeout => ("completed", Some("Timed out before completion".to_owned())),
        Status::Running | Status::Exited => {
            if completion.exit.is_none() && completion.signal.is_none() {
                ("error", Some("Command failed".to_owned()))
            } else {
                (
                    "completed",
                    super::exit_notice_parts(completion.exit, completion.signal),
                )
            }
        }
    }
}
