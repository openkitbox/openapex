//! Transport cleanup ownership.
//!
//! A transport (`stdio` child + its pipes, an HTTP client's in-flight IO) can
//! outlive the hub that opened it: a handshake can fail after a child was
//! spawned, and a config reload retires a hub whose transports are still
//! registered. `Drop` cannot await, so every cleanup future a transport hands
//! off is registered here, where the composition owns it.
//!
//! [`Transports::seal_and_join`] is the single place those futures are awaited.
//! It runs after the runner, the shell domain and every maintenance producer
//! have been joined, so no transport can still be starting; a registration that
//! arrives during sealing is still awaited rather than dropped.
//!
//! # Sealing contract
//!
//! `adopt` and `seal_and_join` are serialized by one mutex, so a registration
//! either lands before the seal (the owner awaits it) or is refused after the
//! seal (the caller gets its handle back and awaits it). No window exists where
//! a task is accepted and then never awaited.
//!
//! ```text
//! adopt(t): lock, sealed? -> Err(t) | push(t), Ok
//! seal:     lock, sealed = true, take(pending)  ... repeat while non-empty
//! ```

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::task::JoinHandle;

/// Composition-scoped registry of transport cleanup tasks.
#[derive(Default)]
pub struct Transports {
    pending: Mutex<Vec<JoinHandle<()>>>,
    sealed: AtomicBool,
}

impl Transports {
    /// An empty owner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand a cleanup task to this owner.
    ///
    /// # Errors
    ///
    /// Returns the handle back once the owner is sealed: ownership cannot be
    /// transferred, so the caller must await it itself. Silently accepting it
    /// would leave the task unobserved by anyone.
    pub fn adopt(&self, cleanup: JoinHandle<()>) -> Result<(), JoinHandle<()>> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.sealed.load(Ordering::Acquire) {
            return Err(cleanup);
        }
        pending.push(cleanup);
        Ok(())
    }

    /// Seal and await every registered cleanup task. Idempotent: a repeat call
    /// joins whatever was registered since the previous one.
    ///
    /// # Errors
    ///
    /// Returns the first cleanup task that panicked (every other task is still
    /// awaited first, so a panic cannot leave a child running unobserved).
    pub async fn seal_and_join(&self) -> Result<(), String> {
        let mut failure = None;
        loop {
            let tasks = {
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                self.sealed.store(true, Ordering::Release);
                std::mem::take(&mut *pending)
            };
            if tasks.is_empty() {
                break;
            }
            for task in tasks {
                if let Err(error) = task.await {
                    failure.get_or_insert_with(|| format!("transport cleanup failed: {error}"));
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Whether shutdown already sealed this owner.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }
}
