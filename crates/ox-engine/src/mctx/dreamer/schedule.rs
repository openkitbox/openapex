use std::collections::BTreeMap;

use anyhow::{Context as _, Result};

use ox_db::Db;

use super::task::{DreamTask, TaskFailure};

/// Consecutive failures of one `(project, task)` after which the task stops
/// being retried on every tick and waits for its next interval slot instead
/// (the reference's `MAX_TASK_RETRIES` in `dreamer/task-scheduler.ts`).
pub const MAX_DREAM_FAILURES: u32 = 3;

/// The longest a repeatedly failing `(project, task)` waits between attempts
/// (24 hours): the ceiling of the exponential wait in [`failure_backoff_millis`].
const MAX_DREAM_BACKOFF_MILLIS: i64 = 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// Schedule state (SQL)
// ---------------------------------------------------------------------------

/// The scheduler's persisted view of one `(project, task)`.
#[derive(Debug, Clone, Default)]
pub(super) struct DreamState {
    /// The start of the last successful run, or `None` if it never succeeded.
    pub(super) last_run_at: Option<i64>,
    /// The start of the last run of any outcome (the retry cap's clock).
    last_attempt_at: i64,
    /// `completed` / `skipped` / `failed` of the last run.
    pub(super) last_status: Option<String>,
    /// `<class>: <message>` of the last failure.
    pub(super) last_error: Option<String>,
    /// How many failures have happened in a row without a success or skip.
    pub(super) consecutive_failures: u32,
    /// The `curate` rotation cursor (an index into
    /// [`CATEGORIES`](crate::mctx::memory::CATEGORIES)).
    pub(super) rotation_cursor: u32,
    /// `key → fingerprint` of the input the last successful run saw, per key
    /// the task chooses (`curate`: the category). The dirty check: a task
    /// whose input fingerprints the same skips its completion.
    pub(super) fingerprints: BTreeMap<String, String>,
}

/// One task run's input fingerprint, recorded on success under `key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Fingerprint {
    pub(super) key: &'static str,
    pub(super) value: String,
}

impl DreamState {
    /// Whether `fingerprint` is what the last successful run under its key
    /// already saw.
    pub(super) fn is_unchanged(&self, fingerprint: &Fingerprint) -> bool {
        self.fingerprints.get(fingerprint.key) == Some(&fingerprint.value)
    }
}

/// Whether a task should run at `now`.
///
/// Due means `now - last_run_at >= interval`, with `last_run_at` the start of
/// the last *successful* run (so work landing during a run is not skipped).
/// A row that has failed [`MAX_DREAM_FAILURES`] times in a row is not
/// retried until [`failure_backoff_millis`] has elapsed *since the failed
/// attempt*: the first retries are hot (next tick), then the wait doubles per
/// further failure up to [`MAX_DREAM_BACKOFF_MILLIS`]. The scheduler ticks once
/// per interval, so a flat one-interval wait would never throttle anything.
pub(super) fn is_due(state: &DreamState, now: i64, interval_millis: i64) -> bool {
    if let Some(last_run_at) = state.last_run_at
        && now - last_run_at < interval_millis
    {
        return false;
    }
    if state.consecutive_failures >= MAX_DREAM_FAILURES
        && now - state.last_attempt_at
            < failure_backoff_millis(state.consecutive_failures, interval_millis)
    {
        return false;
    }
    true
}

/// How long a row with `consecutive_failures` failures in a row waits after its
/// last attempt: zero below [`MAX_DREAM_FAILURES`], then
/// `interval * 2^(failures - MAX_DREAM_FAILURES + 1)`, saturating and capped at
/// [`MAX_DREAM_BACKOFF_MILLIS`]. A non-positive interval yields no wait.
fn failure_backoff_millis(consecutive_failures: u32, interval_millis: i64) -> i64 {
    if consecutive_failures < MAX_DREAM_FAILURES {
        return 0;
    }
    let exponent = (consecutive_failures - MAX_DREAM_FAILURES + 1).min(20);
    interval_millis
        .max(0)
        .saturating_mul(1i64 << exponent)
        .min(MAX_DREAM_BACKOFF_MILLIS)
}

/// The state row for `(project, task)`, or defaults when it has none.
pub(super) async fn load_state(db: &Db, project: &str, task: DreamTask) -> Result<DreamState> {
    let context = format!("load dream state for {project}");
    let task = task.as_str();
    let state: Result<DreamState> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT last_run_at, last_attempt_at, last_status, last_error,
                        consecutive_failures, rotation_cursor, fingerprints
                   FROM mc_dream_state WHERE project = ?1 AND task = ?2",
            )
            .await?;
        let Some(row) = ox_db::optional(&mut stmt, (project, task)).await? else {
            return Ok(DreamState::default());
        };
        let fingerprints: Option<String> = row.get(6)?;
        Ok(DreamState {
            last_run_at: row.get::<Option<i64>>(0)?,
            last_attempt_at: row.get::<i64>(1)?,
            last_status: row.get::<Option<String>>(2)?,
            last_error: row.get::<Option<String>>(3)?,
            consecutive_failures: row.get::<i64>(4)?.max(0) as u32,
            rotation_cursor: row.get::<i64>(5)?.max(0) as u32,
            fingerprints: parse_fingerprints(fingerprints.as_deref()),
        })
    }
    .await;
    state.with_context(|| context)
}

/// The stored fingerprint map; an absent or unreadable column is empty (the
/// only cost of a bad value is one extra run).
fn parse_fingerprints(raw: Option<&str>) -> BTreeMap<String, String> {
    raw.and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default()
}

/// Record a successful run. `last_run_at` is stamped with `started_at` — the
/// run's start, not its end (the reference's `advanceAfterRun`): anything that
/// landed while the task ran is newer than this cutoff, so the gate sees it
/// next slot instead of never.
///
/// `fingerprint`, when the task recorded one, is merged into the row's map
/// under its key. The map is merged here from the state the run loaded —
/// the `(project, task)` in-flight guard makes that state the latest.
pub(super) async fn settle_success(
    db: &Db,
    project: &str,
    task: DreamTask,
    started_at: i64,
    cursor: Option<u32>,
    fingerprint: Option<(&DreamState, &Fingerprint)>,
) -> Result<()> {
    let context = format!("record dream success for {project}");
    let cursor = cursor.map(i64::from);
    let fingerprints = fingerprint
        .map(|(state, fingerprint)| merged_fingerprints(state, fingerprint))
        .transpose()?;
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO mc_dream_state
                 (project, task, last_run_at, last_attempt_at, last_status, last_error,
                  consecutive_failures, rotation_cursor, fingerprints)
             VALUES (?1, ?2, ?3, ?3, 'completed', NULL, 0, COALESCE(?4, 0), ?5)
             ON CONFLICT (project, task) DO UPDATE SET
                 last_run_at = excluded.last_run_at,
                 last_attempt_at = excluded.last_attempt_at,
                 last_status = 'completed',
                 last_error = NULL,
                 consecutive_failures = 0,
                 rotation_cursor = COALESCE(?4, mc_dream_state.rotation_cursor),
                 fingerprints = COALESCE(?5, mc_dream_state.fingerprints)",
        (
            project,
            task.as_str(),
            started_at,
            cursor,
            fingerprints.as_deref(),
        ),
    )
    .await
    .with_context(|| context)?;
    Ok(())
}

/// The state's map with `fingerprint` written under its key, as the column's
/// JSON.
fn merged_fingerprints(state: &DreamState, fingerprint: &Fingerprint) -> Result<String> {
    let mut map: BTreeMap<&str, &str> = state
        .fingerprints
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    map.insert(fingerprint.key, &fingerprint.value);
    serde_json::to_string(&map).context("serialize dream fingerprints")
}

/// Record a due task that found no work. `last_run_at` deliberately does NOT
/// move — a skip is not a successful pass over the pool — and `last_error`
/// stays NULL because nothing failed; the gate's reason is logged, not stored
/// (the reference's `advanceAfterRun(…, "skipped", null)`). `cursor` lets a
/// rotating task step past a slot it found unchanged, so the rotation does
/// not pin on it.
pub(super) async fn settle_skip(
    db: &Db,
    project: &str,
    task: DreamTask,
    now: i64,
    cursor: Option<u32>,
) -> Result<()> {
    let context = format!("record dream skip for {project}");
    let cursor = cursor.map(i64::from);
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO mc_dream_state
                 (project, task, last_attempt_at, last_status, last_error,
                  consecutive_failures, rotation_cursor)
             VALUES (?1, ?2, ?3, 'skipped', NULL, 0, COALESCE(?4, 0))
             ON CONFLICT (project, task) DO UPDATE SET
                 last_attempt_at = excluded.last_attempt_at,
                 last_status = 'skipped',
                 last_error = NULL,
                 consecutive_failures = 0,
                 rotation_cursor = COALESCE(?4, mc_dream_state.rotation_cursor)",
        (project, task.as_str(), now, cursor),
    )
    .await
    .with_context(|| context)?;
    Ok(())
}

/// Record a failed run: the attempt is stamped, the failure count grows, and
/// `last_run_at` stays where the last success left it.
pub(super) async fn settle_failure(
    db: &Db,
    project: &str,
    task: DreamTask,
    now: i64,
    failure: &TaskFailure,
) -> Result<()> {
    let context = format!("record dream failure for {project}");
    let error = failure.recorded();
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO mc_dream_state
                 (project, task, last_attempt_at, last_status, last_error,
                  consecutive_failures)
             VALUES (?1, ?2, ?3, 'failed', ?4, 1)
             ON CONFLICT (project, task) DO UPDATE SET
                 last_attempt_at = excluded.last_attempt_at,
                 last_status = 'failed',
                 last_error = excluded.last_error,
                 consecutive_failures = mc_dream_state.consecutive_failures + 1",
        (project, task.as_str(), now, error.as_str()),
    )
    .await
    .with_context(|| context)?;
    Ok(())
}
