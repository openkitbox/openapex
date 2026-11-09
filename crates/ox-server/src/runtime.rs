//! Composition-scoped shutdown: close admissions, then await what was owned.
//!
//! One [`Runtime`] per composed server. It owns the two things every server
//! must be able to prove at exit: the *stop signal* that background producers
//! watch, and the *join set* of tasks the composition started. It holds no
//! session state — the runner, the shell domain and the database own their
//! own — and it never polls resource counters: [`Runtime::shutdown`] awaits
//! the tasks it adopted, so a `count == 0` observation is never mistaken for a
//! finished cleanup.
//!
//! Ordering is the contract:
//!
//! 1. close admissions (HTTP ingress, execution gate, service updates) and
//!    signal every producer to stop at its next safe boundary;
//! 2. join the runner (actors and their auxiliary work);
//! 3. stop and join the composition's shell domain (processes, pumps, sinks);
//! 4. join the maintenance/startup producers;
//! 5. shut every MCP hub this composition created down and join the transport
//!    cleanup they handed to [`Transports`];
//! 6. await the database's own drop-time rollback cleanup, and only then
//!    report success.
//!
//! [`Runtime::shutdown`] is idempotent: every caller waits for the same
//! supervisor and receives the same result, and a cancelled waiter never
//! takes the join set with it.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use ox_mcp::{Hub, Transports};

use crate::api::AppState;
use crate::supervision::Supervision;

/// Owned background work of one composition.
pub struct Runtime {
    stop: CancellationToken,
    closed: AtomicBool,
    /// Tasks started by the composition (maintenance, startup, catalogue).
    tasks: Mutex<TaskSet>,
    /// Every hub this composition created, including generations that were
    /// replaced or never installed, so a possibly-dropped hub's transports are
    /// still shut down through [`Transports`].
    hubs: Mutex<Vec<Arc<Hub>>>,
    transports: Arc<Transports>,
    /// Runs the supervision job once; every caller observes the same report.
    supervision: Supervision,
}

#[derive(Default)]
struct TaskSet {
    sealed: bool,
    handles: Vec<JoinHandle<()>>,
}

impl Runtime {
    /// A fresh composition scope: open admissions, nothing adopted.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: CancellationToken::new(),
            closed: AtomicBool::new(false),
            tasks: Mutex::new(TaskSet::default()),
            hubs: Mutex::new(Vec::new()),
            transports: Arc::new(Transports::new()),
            supervision: Supervision::new(),
        })
    }

    /// The stop signal maintenance producers watch. Cancelled once by
    /// [`Runtime::shutdown`]; watching it makes a producer's join finite.
    #[must_use]
    pub fn stop(&self) -> &CancellationToken {
        &self.stop
    }

    /// Whether admissions are already closed. HTTP ingress reads this.
    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// The transport owner shared by every hub of this composition.
    #[must_use]
    pub fn transports(&self) -> &Arc<Transports> {
        &self.transports
    }

    /// Create one hub whose transport cleanup belongs to this composition.
    #[must_use]
    pub fn new_hub(self: &Arc<Self>) -> Arc<Hub> {
        let hub = Arc::new(Hub::with_transports(Arc::clone(&self.transports)));
        self.hubs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(Arc::clone(&hub));
        hub
    }

    /// Adopt a spawned producer. Adoption must happen while the task is being
    /// spawned, before anything can observe shutdown.
    pub fn adopt(&self, handle: JoinHandle<()>) -> Result<(), JoinHandle<()>> {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if tasks.sealed {
            return Err(handle);
        }
        tasks.handles.push(handle);
        Ok(())
    }

    /// Spawn `work` under this composition's owner and await its value.
    ///
    /// The task is adopted before the caller can be cancelled, so a detached
    /// HTTP request cannot orphan a mutation or blocking operation. If the
    /// shutdown supervisor sealed the owner first, this call awaits the task
    /// itself rather than losing its handle.
    pub async fn own<Value, Work>(&self, work: Work) -> anyhow::Result<Value>
    where
        Value: Send + 'static,
        Work: Future<Output = Value> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let value = work.await;
            let _ = sender.send(value);
        });
        self.finish_owned(task, receiver).await
    }

    /// Spawn blocking `work` under this composition's owner.
    pub async fn own_blocking<Value, Work>(&self, work: Work) -> anyhow::Result<Value>
    where
        Value: Send + 'static,
        Work: FnOnce() -> Value + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let value = work();
            let _ = sender.send(value);
        });
        self.finish_owned(task, receiver).await
    }

    async fn finish_owned<Value>(
        &self,
        task: JoinHandle<()>,
        receiver: oneshot::Receiver<Value>,
    ) -> anyhow::Result<Value> {
        if let Err(task) = self.adopt(task) {
            task.await
                .map_err(|error| anyhow::anyhow!("owned task failed: {error}"))?;
        }
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("owned task ended without returning its result"))
    }

    /// Close every admission in `state` without waiting: HTTP ingress rejects
    /// new requests, the execution gate refuses new work, service updates are
    /// sealed, and every producer watching [`Runtime::stop`] is signalled.
    /// Close admissions and signal the listener to stop, without waiting for
    /// owned resources. The serving edge calls this before graceful listener
    /// shutdown; [`Runtime::shutdown`] repeats it idempotently before joining.
    pub fn close_admissions(&self, state: &AppState) {
        self.closed.store(true, Ordering::Release);
        state.live.close_updates();
        state.runner.close();
        self.stop.cancel();
    }

    fn close(&self, state: &AppState) {
        self.close_admissions(state);
    }

    /// Close, then await everything this composition owns. Idempotent and
    /// repeatable; the join set is executed by the first caller and every
    /// later caller waits for that result.
    ///
    /// # Errors
    ///
    /// Returns the first cleanup failure (a failed runner join, shell stop, or
    /// database rollback). A failure never short-circuits the remaining
    /// steps: every owned resource is still awaited before reporting.
    pub async fn shutdown(self: &Arc<Self>, state: &AppState) -> anyhow::Result<()> {
        self.close(state);
        let runtime = Arc::clone(self);
        let state = state.clone();
        self.supervision
            .run(async move {
                runtime
                    .supervise(&state)
                    .await
                    .map_err(|error| format!("{error:#}"))
            })
            .await
            .map_err(anyhow::Error::msg)
    }

    async fn supervise(&self, state: &AppState) -> anyhow::Result<()> {
        let mut failure: Option<String> = None;
        let mut record = |error: anyhow::Error| {
            if failure.is_none() {
                failure = Some(format!("{error:#}"));
            }
        };
        // 2–4. Runner actors, shell processes/sinks, and maintenance producers
        // are independent after admissions close. Join them in parallel so a
        // slow actor cannot delay shell/process teardown or producer cleanup.
        let tasks = {
            let mut owner = self
                .tasks
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            owner.sealed = true;
            std::mem::take(&mut owner.handles)
        };
        let runner_join = state.runner.join();
        let shells = state.shells();
        let shell_join = shells.stop_all_and_join();
        let maintenance_join = async move {
            let mut failure = None;
            for task in tasks {
                if let Err(error) = task.await {
                    failure.get_or_insert_with(|| {
                        anyhow::anyhow!("background task did not finish: {error}")
                    });
                }
            }
            failure.map_or(Ok(()), Err)
        };
        let (runner_result, shell_result, maintenance_result) =
            tokio::join!(runner_join, shell_join, maintenance_join);
        if let Err(error) = runner_result {
            record(error);
        }
        if let Err(error) = shell_result {
            record(error);
        }
        if let Err(error) = maintenance_result {
            record(error);
        }
        // 5. Every hub this composition created: current, retired and never
        // installed alike. Shutting them down awaits each transport's IO.
        let hubs: Vec<Arc<Hub>> = std::mem::take(
            &mut *self
                .hubs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        for hub in hubs {
            if let Err(error) = hub.shutdown().await {
                record(anyhow::anyhow!(error));
            }
        }
        if let Err(error) = self.transports.seal_and_join().await {
            record(anyhow::anyhow!(error));
        }
        // 6. Drop-time rollback cleanup belongs to the database, and only now
        // is every user of it gone.
        if let Err(error) = state.db.join_cleanup().await {
            record(error);
        }
        match failure {
            Some(error) => Err(anyhow::anyhow!(error)),
            None => Ok(()),
        }
    }
}

impl Default for Runtime {
    fn default() -> Self {
        // `Runtime` is shared as an `Arc`; this exists so `AppState`'s builder
        // can use `unwrap_or_default` in tests that inject their own.
        Self {
            stop: CancellationToken::new(),
            closed: AtomicBool::new(false),
            tasks: Mutex::new(TaskSet::default()),
            hubs: Mutex::new(Vec::new()),
            transports: Arc::new(Transports::new()),
            supervision: Supervision::new(),
        }
    }
}
