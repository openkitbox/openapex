//! Client lease counting and the idle-exit decision.
//!
//! Owns one thing: whether the server may exit because no client is left.
//! It does not listen, serve HTTP or read the clock on its own; callers pass
//! `Instant`s in, so [`decide`] and [`Lifecycle::try_close`] are
//! deterministic. The one exception is [`LeaseGuard`]'s `Drop`, which has no
//! caller to pass a time.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Inputs to [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub leases: usize,
    pub turn_active: bool,
    pub idle_for: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Wait,
    Exit,
}

/// Exit exactly when no client holds a lease, no turn is running, and the
/// server has been idle for at least `grace`.
pub fn decide(s: &Snapshot, grace: Duration) -> Decision {
    if s.leases == 0 && !s.turn_active && s.idle_for >= grace {
        Decision::Exit
    } else {
        Decision::Wait
    }
}

/// Returned by [`Lifecycle::acquire`] once the server has committed to exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closing;

impl fmt::Display for Closing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("server is shutting down")
    }
}

impl std::error::Error for Closing {}

struct Inner {
    leases: usize,
    closing: bool,
    last_busy: Instant,
}

/// Lease counter. Once [`try_close`](Self::try_close) returns `true` no new
/// lease is granted, so a client either holds a lease the closer saw or is
/// told to start a fresh server.
pub struct Lifecycle {
    inner: Mutex<Inner>,
}

impl Lifecycle {
    pub fn new(now: Instant) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                leases: 0,
                closing: false,
                last_busy: now,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn acquire(self: &Arc<Self>, now: Instant) -> Result<LeaseGuard, Closing> {
        let mut inner = self.lock();
        if inner.closing {
            return Err(Closing);
        }
        inner.leases += 1;
        inner.last_busy = now;
        drop(inner);
        Ok(LeaseGuard {
            lifecycle: Arc::clone(self),
        })
    }

    /// Restart the idle clock at `now`, e.g. when the server starts serving.
    pub fn touch(&self, now: Instant) {
        self.lock().last_busy = now;
    }

    pub fn lease_count(&self) -> usize {
        self.lock().leases
    }

    /// Decide and, on `Exit`, commit under one lock. A live lease or active
    /// turn refreshes `last_busy` and vetoes the close.
    pub fn try_close(&self, now: Instant, turn_active: bool, grace: Duration) -> bool {
        let mut inner = self.lock();
        if inner.leases > 0 || turn_active {
            inner.last_busy = now;
            return false;
        }
        let snapshot = Snapshot {
            leases: inner.leases,
            turn_active,
            idle_for: now.saturating_duration_since(inner.last_busy),
        };
        if decide(&snapshot, grace) == Decision::Exit {
            inner.closing = true;
            return true;
        }
        false
    }
}

/// Holds one lease; releasing it restarts the idle clock.
pub struct LeaseGuard {
    lifecycle: Arc<Lifecycle>,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let mut inner = self.lifecycle.lock();
        inner.leases = inner.leases.saturating_sub(1);
        // No caller supplies a time on drop, so this is the one place the
        // clock is read directly.
        inner.last_busy = Instant::now();
    }
}
