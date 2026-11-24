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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::time::Duration;

    use ox_db::Db;

    use super::{Clock, RunResult, Scheduler, SchedulerConfig};
    use crate::cron::Cron;
    use crate::lease::Lease;
    use crate::store::AutomationStore;
    use crate::types::{Action, Info, Outcome, Status, TimeInfo, Trigger};

    fn info(id: &str, next_due_at: Option<i64>, status: Status) -> Info {
        Info {
            id: id.to_owned(),
            name: "nightly".to_owned(),
            project_id: "proj_1".to_owned(),
            session_id: None,
            directory: "/work".to_owned(),
            trigger: Trigger::Cron {
                cron: "0 9 * * *".to_owned(),
                tz: None,
            },
            tz: "UTC".to_owned(),
            action: Action::Shell {
                command: "true".to_owned(),
            },
            expires_at: None,
            status,
            next_due_at,
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

    /// Wall-clock stand-in whose milliseconds follow tokio's (paused) clock,
    /// so one `advance` drives both the heartbeat timer and the injected clock.
    #[derive(Debug, Clone, Copy)]
    struct VirtualClock {
        epoch: tokio::time::Instant,
        base_ms: i64,
    }

    impl VirtualClock {
        fn new(base_ms: i64) -> Self {
            Self {
                epoch: tokio::time::Instant::now(),
                base_ms,
            }
        }
    }

    impl Clock for VirtualClock {
        fn now_ms(&self) -> i64 {
            let elapsed = i64::try_from(self.epoch.elapsed().as_millis())
                .expect("test durations fit into i64");
            self.base_ms.saturating_add(elapsed)
        }
    }

    /// Clock with explicitly stepped instants for the non-timer tests.
    struct ManualClock {
        now_ms: AtomicI64,
    }

    impl ManualClock {
        fn new(now_ms: i64) -> Self {
            Self {
                now_ms: AtomicI64::new(now_ms),
            }
        }

        fn set(&self, now_ms: i64) {
            self.now_ms.store(now_ms, Ordering::Relaxed);
        }
    }

    impl Clock for ManualClock {
        fn now_ms(&self) -> i64 {
            self.now_ms.load(Ordering::Relaxed)
        }
    }

    async fn lease_expiry(db: &Db) -> anyhow::Result<i64> {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT expires_at FROM automation_lease WHERE name = 'scheduler'")
            .await?;
        let row = ox_db::optional(&mut stmt, ())
            .await?
            .expect("scheduler lease row");
        Ok(row.get(0)?)
    }

    struct CancelOnDrop(Arc<AtomicBool>);

    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn short_config() -> SchedulerConfig {
        SchedulerConfig {
            ttl: Duration::from_millis(150),
            heartbeat: Duration::from_millis(50),
            ..SchedulerConfig::new()
        }
    }

    #[tokio::test]
    async fn retry_backoff_blocks_execution_until_due() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        store
            .insert(&info("retrying", Some(1_000), Status::Active))
            .await?;
        let lease = Lease::acquire(&db, "scheduler", "host_a", 0, 60_000)
            .await?
            .expect("lease");
        let scheduler = Scheduler::new(&db, store, lease);
        let clock = ManualClock::new(2_000);

        let conn = db.acquire().await?;
        conn.execute(
            "UPDATE automation SET retry_at = 20_000 WHERE id = 'retrying'",
            (),
        )
        .await?;
        drop(conn);

        let report = scheduler
            .tick(&clock, |_| async { Ok(RunResult::success(None)) })
            .await?;
        assert_eq!(report.fired, 0, "a run in retry backoff must not execute");
        assert_eq!(store.get("retrying").await?.expect("row").run_count, 0);

        let conn = db.acquire().await?;
        conn.execute(
            "UPDATE automation SET retry_at = 1_000 WHERE id = 'retrying'",
            (),
        )
        .await?;
        drop(conn);
        let report = scheduler
            .tick(&clock, |_| async { Ok(RunResult::success(None)) })
            .await?;
        assert_eq!(report.fired, 1, "once backoff expires the run executes");
        Ok(())
    }

    #[tokio::test]
    async fn stale_completion_guard() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        store
            .insert(&info("aut_1", Some(1_000), Status::Active))
            .await?;
        let lease = Lease::acquire(&db, "scheduler", "host_a", 0, 60_000)
            .await?
            .expect("scheduler acquires the lease");
        let scheduler = Scheduler::new(&db, store, lease);
        let clock = ManualClock::new(2_600);

        assert!(store.claim("aut_1", 2_000, 0).await?, "first ticket");
        assert!(store.claim("aut_1", 2_500, 1).await?, "second ticket");
        let mut fresh = store.get("aut_1").await?.expect("claimed");
        assert_eq!(fresh.run_count, 2);

        let stale = super::ClaimedRun {
            info: {
                let mut row = fresh.clone();
                row.run_count = 1;
                row
            },
            late: false,
        };
        assert!(
            !scheduler
                .complete(&clock, &stale, RunResult::success(Some("stale".to_owned())))
                .await?,
            "a completion from run 1 must not settle run 2"
        );
        let row = store.get("aut_1").await?.expect("row");
        assert_eq!(
            row.run_count, 2,
            "the stale callback did not advance the run counter"
        );
        assert_eq!(
            row.last_outcome, None,
            "the stale callback wrote no outcome"
        );

        assert!(
            scheduler
                .complete(
                    &clock,
                    &super::ClaimedRun {
                        info: fresh.clone(),
                        late: true
                    },
                    RunResult::success(Some("fresh".to_owned()))
                )
                .await?,
            "the current ticket settles its run"
        );
        let row = store.get("aut_1").await?.expect("row");
        assert_eq!(row.last_outcome, Some(Outcome::Fired));
        assert_eq!(row.last_output.as_deref(), Some("fresh"));

        clock.set(2_800);
        assert!(
            !scheduler
                .complete(&clock, &stale, RunResult::failed(None))
                .await?,
            "a late retry of the stale callback is still rejected"
        );
        assert_eq!(
            store
                .get("aut_1")
                .await?
                .expect("row")
                .last_output
                .as_deref(),
            Some("fresh"),
            "the stale failure did not overwrite the newer output"
        );

        assert!(
            store.set_status("aut_1", Status::Paused, 2_900, 2).await?,
            "pause the row after a ticket was issued"
        );
        fresh.status = Status::Done;
        clock.set(3_000);
        assert!(
            !scheduler
                .complete(
                    &clock,
                    &super::ClaimedRun {
                        info: fresh,
                        late: false
                    },
                    RunResult::success(None)
                )
                .await?,
            "a callback carrying a new status cannot overwrite a paused row"
        );
        assert_eq!(
            store.get("aut_1").await?.expect("row").status,
            Status::Paused
        );
        Ok(())
    }

    #[tokio::test]
    async fn scheduler_tick_claims_only_due_rows_and_stops_when_lease_is_lost() -> anyhow::Result<()>
    {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        store
            .insert(&info("due", Some(1_000), Status::Active))
            .await?;
        store
            .insert(&info("future", Some(5_000), Status::Active))
            .await?;
        store
            .insert(&info("paused", Some(1_000), Status::Paused))
            .await?;
        let lease = Lease::acquire(&db, "scheduler", "host_a", 0, 15_000)
            .await?
            .expect("lease");
        let scheduler = Scheduler::new(&db, store, lease);
        let clock = ManualClock::new(2_000);

        let report = scheduler
            .tick(&clock, |_| async {
                Ok(RunResult::success(Some("done".to_owned())))
            })
            .await?;
        assert!(report.lease_held);
        assert_eq!(report.fired, 1, "only the due active row fires");
        assert_eq!(report.missed, 0);
        assert_eq!(report.expired, 0);
        assert_eq!(
            store.get("due").await?.expect("due").last_outcome,
            Some(Outcome::Fired)
        );
        assert_eq!(store.get("future").await?.expect("future").run_count, 0);
        assert_eq!(store.get("paused").await?.expect("paused").run_count, 0);

        let expected_next = Cron::parse("0 9 * * *")?
            .next_after(2_000, "UTC")?
            .expect("daily cron has a next occurrence");
        assert_eq!(
            store.get("due").await?.expect("due").next_due_at,
            Some(expected_next)
        );

        clock.set(30_000);
        let lost = scheduler
            .tick(&clock, |_| async { Ok(RunResult::success(None)) })
            .await?;
        assert!(
            !lost.lease_held,
            "an expired lease is reported, never executed on"
        );
        assert_eq!(lost.fired, 0);
        assert_eq!(store.get("future").await?.expect("future").run_count, 0);
        Ok(())
    }

    /// A handler running past the heartbeat window must see its lease renewed,
    /// and its completion must be computed from a fresh clock reading.
    #[tokio::test]
    async fn heartbeat_renews_during_handlers_and_completion_uses_fresh_clock() -> anyhow::Result<()>
    {
        tokio::time::pause();
        let clock = VirtualClock::new(0);
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        let mut due = info("long", Some(0), Status::Active);
        due.trigger = Trigger::Every {
            every: "1000".to_owned(),
            tz: None,
        };
        store.insert(&due).await?;
        let lease = Lease::acquire(&db, "scheduler", "host_a", 0, 150)
            .await?
            .expect("lease");
        assert_eq!(lease_expiry(&db).await?, 150);
        let scheduler = Scheduler::new(&db, store, lease).with_config(short_config());

        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let release_rx = std::sync::Mutex::new(Some(release_rx));
        let tick = scheduler.tick(&clock, move |_| {
            let started_tx = started_tx.clone();
            let mut release = release_rx
                .lock()
                .expect("handler receiver mutex")
                .take()
                .expect("one handler runs");
            async move {
                let _ = started_tx.send(());
                release.recv().await;
                Ok(RunResult::success(None))
            }
        });

        let driver = async {
            started_rx.recv().await.expect("handler starts");
            assert_eq!(clock.now_ms(), 0, "handler starts before the heartbeat");
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
            assert_eq!(clock.now_ms(), 100);
            let renewed = lease_expiry(&db).await.expect("lease row");
            assert!(
                renewed > 150,
                "lease must be renewed while the handler runs, got {renewed}"
            );
            let _ = release_tx.send(());
        };
        let (report, ()) = tokio::join!(tick, driver);
        let report = report?;
        assert_eq!(report.fired, 1);
        let row = store.get("long").await?.expect("row");
        assert_eq!(
            row.next_due_at,
            Some(1_100),
            "completion uses the fresh clock, not the tick-start instant"
        );
        Ok(())
    }

    /// If the SQL CAS rejects renewal because a successor owns the lease, the
    /// tick must stop, cancel the in-flight handler and write no completion.
    #[tokio::test]
    async fn stolen_lease_cancels_handler_and_blocks_completion() -> anyhow::Result<()> {
        tokio::time::pause();
        let clock = VirtualClock::new(0);
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        store
            .insert(&info("stolen", Some(0), Status::Active))
            .await?;
        let lease = Lease::acquire(&db, "scheduler", "host_a", 0, 150)
            .await?
            .expect("lease");
        let scheduler = Scheduler::new(&db, store, lease).with_config(short_config());

        let cancelled = Arc::new(AtomicBool::new(false));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let release_rx = std::sync::Mutex::new(Some(release_rx));
        let cancel = cancelled.clone();
        let tick = scheduler.tick(&clock, move |_| {
            let started_tx = started_tx.clone();
            let mut release = release_rx
                .lock()
                .expect("handler receiver mutex")
                .take()
                .expect("one handler runs");
            let flag = CancelOnDrop(cancel.clone());
            async move {
                let _guard = flag;
                let _ = started_tx.send(());
                release.recv().await;
                Ok(RunResult::success(None))
            }
        });

        let driver = async {
            started_rx.recv().await.expect("handler starts");
            let conn = db.acquire().await.expect("database connection");
            conn.execute(
                "UPDATE automation_lease SET owner = 'host_b', token = 'stolen'
                 WHERE name = 'scheduler'",
                (),
            )
            .await
            .expect("successor steals the lease");
            drop(conn);
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
            let _ = release_tx.send(());
        };
        let (result, ()) = tokio::join!(tick, driver);
        assert!(result.is_err(), "a stolen lease must abort the tick");
        assert!(
            cancelled.load(Ordering::SeqCst),
            "outstanding handler work is cancelled"
        );
        let row = store.get("stolen").await?.expect("row");
        assert_eq!(row.last_outcome, None, "no completion after lease loss");
        assert_eq!(row.run_count, 1, "no extra claim after lease loss");
        assert_eq!(row.next_due_at, Some(0), "the due time is untouched");
        Ok(())
    }
}
