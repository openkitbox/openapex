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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    use super::*;

    const GRACE: Duration = Duration::from_secs(10);
    const NS: Duration = Duration::from_nanos(1);

    #[test]
    fn decide_table() {
        let snap = |leases, turn_active, idle_for| Snapshot {
            leases,
            turn_active,
            idle_for,
        };
        let cases = [
            (snap(1, false, GRACE * 2), false),
            (snap(0, true, GRACE * 2), false),
            (snap(0, false, GRACE - NS), false),
            (snap(0, false, GRACE), true),
            (snap(0, false, GRACE + NS), true),
        ];
        for (s, exit) in cases {
            let want = if exit { Decision::Exit } else { Decision::Wait };
            assert_eq!(decide(&s, GRACE), want, "{s:?}");
        }
    }

    #[test]
    fn closes_after_grace_when_never_leased() {
        let t0 = Instant::now();
        let lc = Lifecycle::new(t0);
        assert!(!lc.try_close(t0 + GRACE - NS, false, GRACE));
        assert!(lc.try_close(t0 + GRACE, false, GRACE));
    }

    #[test]
    fn lease_blocks_close_and_drop_restarts_grace() {
        let t0 = Instant::now();
        let lc = Lifecycle::new(t0);
        let guard = lc.acquire(t0).unwrap();
        assert!(!lc.try_close(t0 + GRACE * 5, false, GRACE));
        drop(guard);
        let after = Instant::now();
        assert!(!lc.try_close(after + GRACE - Duration::from_millis(1), false, GRACE));
        assert!(lc.try_close(after + GRACE + Duration::from_secs(1), false, GRACE));
    }

    #[test]
    fn active_turn_blocks_close_and_restarts_grace() {
        let t0 = Instant::now();
        let lc = Lifecycle::new(t0);
        let busy_at = t0 + GRACE * 3;
        assert!(!lc.try_close(busy_at, true, GRACE));
        assert!(!lc.try_close(busy_at + GRACE - NS, false, GRACE));
        assert!(lc.try_close(busy_at + GRACE, false, GRACE));
    }

    #[test]
    fn acquire_fails_only_after_close() {
        let t0 = Instant::now();
        let lc = Lifecycle::new(t0);
        assert!(!lc.try_close(t0, false, GRACE));
        assert!(lc.acquire(t0).is_ok());
        let lc = Lifecycle::new(t0);
        assert!(lc.try_close(t0 + GRACE, false, GRACE));
        assert!(matches!(lc.acquire(t0 + GRACE), Err(Closing)));
    }

    #[test]
    fn touch_restarts_the_grace_window() {
        let t0 = Instant::now();
        let lc = Lifecycle::new(t0);
        let started = t0 + GRACE * 3;
        lc.touch(started);
        assert!(!lc.try_close(started + GRACE - NS, false, GRACE));
        assert!(lc.try_close(started + GRACE, false, GRACE));
    }

    #[test]
    fn counts_every_guard() {
        let t0 = Instant::now();
        let lc = Lifecycle::new(t0);
        let a = lc.acquire(t0).unwrap();
        let b = lc.acquire(t0).unwrap();
        assert_eq!(lc.lease_count(), 2);
        drop(a);
        assert_eq!(lc.lease_count(), 1);
        drop(b);
        assert_eq!(lc.lease_count(), 0);
    }

    #[test]
    fn close_never_races_with_acquire() {
        const WORKERS: usize = 4;
        const ROUNDS: usize = 2000;
        let t0 = Instant::now();
        for _ in 0..20 {
            let lc = Lifecycle::new(t0);
            let barrier = Arc::new(Barrier::new(WORKERS + 1));
            let workers: Vec<_> = (0..WORKERS)
                .map(|_| {
                    let (lc, barrier) = (Arc::clone(&lc), Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        barrier.wait();
                        let mut after_close = 0usize;
                        for _ in 0..ROUNDS {
                            match lc.acquire(Instant::now()) {
                                Ok(guard) => {
                                    // A held lease must keep the server open.
                                    assert!(!lc.try_close(
                                        Instant::now() + GRACE * 2,
                                        false,
                                        GRACE
                                    ));
                                    drop(guard);
                                }
                                Err(Closing) => after_close += 1,
                            }
                        }
                        after_close
                    })
                })
                .collect();
            barrier.wait();
            // Every poll is past the grace window, so only live leases veto it.
            let closed = loop {
                if lc.try_close(Instant::now() + GRACE * 2, false, GRACE) {
                    break true;
                }
                if workers.iter().all(|w| w.is_finished()) {
                    break false;
                }
            };
            if closed {
                assert_eq!(lc.lease_count(), 0);
                assert!(matches!(lc.acquire(Instant::now()), Err(Closing)));
            }
            for w in workers {
                w.join().unwrap();
            }
            if closed {
                assert!(matches!(lc.acquire(Instant::now()), Err(Closing)));
            }
        }
    }
}
