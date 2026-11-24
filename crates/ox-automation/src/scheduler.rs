//! Tokio scheduling loop, run dispatch and completion guards.
//!
//! A tick reads due row ids, applies the pure [`due_decision`] to each row and
//! dispatches at most four actions concurrently. Every write is guarded by the
//! lease and by the run ticket issued by [`AutomationStore::claim`].

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::stream::{self, StreamExt};
use ox_db::Db;
use serde_json::Value;

use crate::decision::{RunOutcome, SettleInput, due_decision, next_due, settle};
use crate::lease::Lease;
use crate::store::{AutomationStore, Completion};
use crate::types::{Info, Outcome, Status, Trigger};

/// An action handed to the caller with the ticket that authorises its result.
#[derive(Debug)]
pub struct ClaimedRun {
    pub info: Info,
    pub late: bool,
}

/// What an executor reports back. `terminal` marks a one-shot `at` trigger.
#[derive(Debug)]
pub struct RunResult {
    pub success: bool,
    pub output: Option<String>,
    pub state: Option<Value>,
    pub terminal: bool,
}

impl RunResult {
    #[must_use]
    pub fn success(output: Option<String>) -> Self {
        Self {
            success: true,
            output,
            state: None,
            terminal: false,
        }
    }

    #[must_use]
    pub fn failed(output: Option<String>) -> Self {
        Self {
            success: false,
            output,
            state: None,
            terminal: false,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub lease_held: bool,
    pub fired: usize,
    pub missed: usize,
    pub expired: usize,
}

/// Injected clock: the decision path never calls `SystemTime::now` directly.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Wall-clock source used by the production loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        jiff::Timestamp::now().as_millisecond()
    }
}

/// Loop deadlines. The heartbeat must stay well below the lease TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub ttl: Duration,
    pub heartbeat: Duration,
    pub poll: Duration,
    pub concurrency: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ttl: Duration::from_secs(15),
            heartbeat: Duration::from_secs(5),
            poll: Duration::from_secs(1),
            concurrency: 4,
        }
    }
}

/// Scheduler bound to one database, one store and one held lease.
pub struct Scheduler<'a> {
    db: &'a Db,
    store: AutomationStore<'a>,
    lease: Lease<'a>,
    config: SchedulerConfig,
    enabled: bool,
}

impl<'a> Scheduler<'a> {
    #[must_use]
    pub const fn new(db: &'a Db, store: AutomationStore<'a>, lease: Lease<'a>) -> Self {
        Self {
            db,
            store,
            lease,
            config: SchedulerConfig::new(),
            enabled: true,
        }
    }

    #[must_use]
    pub const fn with_config(mut self, config: SchedulerConfig) -> Self {
        self.config = config;
        self
    }

    /// The config gate can be flipped on reload; disabled never dispatches.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Process every currently due row once. The clock is read at entry for
    /// the due decisions, again for each heartbeat and again for each
    /// completion: no settlement derives its instant from the tick start.
    pub async fn tick<C, F, Fut>(&self, clock: &C, handler: F) -> Result<TickReport>
    where
        C: Clock,
        F: Fn(&Info) -> Fut,
        Fut: Future<Output = Result<RunResult>> + 'static,
    {
        let now_ms = clock.now_ms();
        let mut report = TickReport {
            lease_held: self.lease.valid(now_ms),
            ..TickReport::default()
        };
        if !report.lease_held || !self.enabled {
            return Ok(report);
        }

        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, retry_at FROM automation
                 WHERE status = 'active' AND next_due_at IS NOT NULL AND next_due_at <= ?1
                 ORDER BY next_due_at, id LIMIT 64",
            )
            .await?;
        let mut cursor = stmt.query([now_ms]).await?;
        let mut candidates = Vec::new();
        while let Some(row) = cursor.next().await? {
            candidates.push((row.get::<String>(0)?, row.get::<Option<i64>>(1)?));
        }
        drop(cursor);
        drop(stmt);
        drop(conn);

        let mut claims = Vec::new();
        for (id, retry_at) in candidates {
            let Some(info) = self.store.get(&id).await? else {
                continue;
            };
            let decision = due_decision(&crate::decision::DueInput {
                now_ms,
                status: info.status,
                enabled: self.enabled,
                next_due_at: info.next_due_at,
                expires_at: info.expires_at,
                retry_at,
            });
            match decision {
                crate::decision::Decision::Skip => {}
                crate::decision::Decision::Expire => {
                    if self
                        .store
                        .set_status(&id, Status::Done, now_ms, info.run_count)
                        .await?
                    {
                        report.expired += 1;
                    }
                }
                crate::decision::Decision::Missed => {
                    let completion = Completion {
                        id: &id,
                        run_count: info.run_count,
                        status: Status::Active,
                        next_due_at: next_due(&info, now_ms)?,
                        retry_at: None,
                        outcome: Outcome::Skipped,
                        output: None,
                        state: None,
                        now_ms,
                    };
                    if self.store.finish(&completion).await? {
                        report.missed += 1;
                    }
                }
                crate::decision::Decision::Fire { late } => {
                    if self.store.claim(&id, now_ms, info.run_count).await? {
                        // `claim` issues a new ticket; settle must carry that
                        // number, not the revision observed before claiming.
                        if let Some(claimed) = self.store.get(&id).await? {
                            claims.push(ClaimedRun {
                                info: claimed,
                                late,
                            });
                        }
                    }
                }
            }
        }

        if claims.is_empty() {
            return Ok(report);
        }

        let mut aborts = Vec::with_capacity(claims.len());
        let mut futures = Vec::with_capacity(claims.len());
        for run in claims {
            let future = handler(&run.info);
            let (future, abort) = futures_util::future::abortable(future);
            aborts.push(abort);
            futures.push(async move { (run, future.await) });
        }
        let mut running = stream::iter(futures).buffer_unordered(self.config.concurrency);
        let heartbeat = tokio::time::sleep(self.config.heartbeat);
        tokio::pin!(heartbeat);
        loop {
            tokio::select! {
                next = running.next() => {
                    let Some((run, outcome)) = next else { break; };
                    // An aborted handler produced no result; never settle it.
                    let Ok(result) = outcome else { continue; };
                    if self.complete(clock, &run, result?).await? {
                        report.fired += 1;
                    }
                }
                _ = &mut heartbeat => {
                    if !self.heartbeat(clock).await? {
                        for abort in &aborts {
                            abort.abort();
                        }
                        bail!("scheduler lease lost while handlers were running");
                    }
                    heartbeat.as_mut().reset(
                        tokio::time::Instant::now() + self.config.heartbeat,
                    );
                }
            }
        }
        Ok(report)
    }

    /// Renew using a freshly read clock. A rejected CAS leaves the local lease
    /// invalid, so callers must stop acting on it.
    async fn heartbeat<C: Clock>(&self, clock: &C) -> Result<bool> {
        self.lease
            .renew(clock.now_ms(), i64::try_from(self.config.ttl.as_millis())?)
            .await
    }

    /// Settle a claimed run. Both the lease and the issued ticket are checked:
    /// a late callback from an older run, or from a lost lease, writes nothing.
    pub async fn complete<C: Clock>(
        &self,
        clock: &C,
        run: &ClaimedRun,
        result: RunResult,
    ) -> Result<bool> {
        let now_ms = clock.now_ms();
        if !self.lease.valid(now_ms) {
            return Ok(false);
        }
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT failures FROM automation WHERE id = ?1")
            .await?;
        let failures = match ox_db::optional(&mut stmt, [run.info.id.as_str()]).await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        drop(stmt);
        drop(conn);
        let failures = u32::try_from(failures.max(0))?;

        let outcome = if result.success {
            RunOutcome::Succeeded
        } else {
            RunOutcome::Failed
        };
        let settled = settle(&SettleInput {
            now_ms,
            outcome,
            failures,
            next_due_at: if result.success {
                next_due(&run.info, now_ms)?
            } else {
                run.info.next_due_at
            },
            expires_at: run.info.expires_at,
        });
        let one_shot = result.success && matches!(run.info.trigger, Trigger::At { .. });
        let completion = Completion {
            id: &run.info.id,
            run_count: run.info.run_count,
            status: if one_shot {
                Status::Done
            } else {
                settled.status
            },
            next_due_at: settled.next_due_at,
            retry_at: settled.retry_at,
            outcome: if one_shot {
                Outcome::Fired
            } else {
                settled.outcome
            },
            output: result.output,
            state: result.state,
            now_ms,
        };
        self.store.finish(&completion).await
    }

    /// Production loop: renew the lease while idle, tick, then wait for the
    /// next poll or shutdown. A failed renewal returns immediately; no action
    /// runs after the lease is lost. Ticks renew concurrently with handlers.
    pub async fn serve<C, F, Fut>(
        self,
        clock: C,
        handler: F,
        shutdown: impl Future<Output = ()>,
    ) -> Result<()>
    where
        C: Clock,
        F: Fn(&Info) -> Fut,
        Fut: Future<Output = Result<RunResult>> + 'static,
    {
        let mut shutdown = std::pin::pin!(shutdown);
        let mut last_renew = i64::MIN;
        loop {
            let now = clock.now_ms();
            if !self.lease.valid(now) {
                bail!("scheduler lease expired before renewal");
            }
            if now.saturating_sub(last_renew) >= i64::try_from(self.config.heartbeat.as_millis())? {
                if !self.heartbeat(&clock).await? {
                    bail!("scheduler lease lost during heartbeat");
                }
                last_renew = now;
            }
            tokio::select! {
                _ = &mut shutdown => {
                    self.lease.release().await?;
                    return Ok(());
                }
                _ = tokio::time::sleep(self.config.poll) => {}
            }
            self.tick(&clock, &handler).await?;
        }
    }
}
