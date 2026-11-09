//! One-shot, cancellation-independent supervision.
//!
//! A shutdown supervisor must outlive the caller that started it: two callers
//! may wait for the same cleanup, and cancelling one of them must not cancel
//! the work the other is waiting for. The first caller runs the job as an
//! owned task; every caller — the first one included — observes that single
//! report, so a cancelled waiter only stops waiting.
//!
//! ```text
//! run(job):  lock, started? -> wait(receiver)
//!                     no    -> spawn(job -> send report), wait(receiver)
//! ```
//!
//! The sender lives inside the job task, so a task that dies without reporting
//! closes the channel: waiters then fail closed instead of parking forever.

use std::future::Future;

use tokio::sync::Mutex;
use tokio::sync::watch;

/// The single report of a supervision job.
type Report = Result<(), String>;

/// Runs one job at most once and hands its report to every caller.
#[derive(Default)]
pub struct Supervision {
    /// `None` until the first caller starts the job.
    started: Mutex<Option<watch::Receiver<Option<Report>>>>,
}

impl Supervision {
    /// A supervision that has not started any job yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `job` unless it already runs, and await its report.
    ///
    /// A job passed by a later caller is dropped unrun: the first job is the
    /// one this supervisor owns. Cancelling the caller never cancels the job.
    pub async fn run<Job>(&self, job: Job) -> Report
    where
        Job: Future<Output = Report> + Send + 'static,
    {
        let mut waiting = {
            let mut started = self.started.lock().await;
            match started.as_ref() {
                Some(receiver) => receiver.clone(),
                None => {
                    let (sender, receiver) = watch::channel(None);
                    tokio::spawn(async move {
                        let report = job.await;
                        let _ = sender.send(Some(report));
                    });
                    *started = Some(receiver.clone());
                    receiver
                }
            }
        };
        loop {
            if let Some(report) = (*waiting.borrow_and_update()).clone() {
                return report;
            }
            if waiting.changed().await.is_err() {
                return Err("supervised job ended without reporting".to_owned());
            }
        }
    }
}
