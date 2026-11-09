//! Session retention: which session trees are old enough to delete.
//!
//! This module owns "which trees, and when": the selection rule as pure
//! functions over `(id, parent_id, time_updated)` rows, and a thin edge
//! ([`sweep_once`], [`spawn_sweeper`]) that feeds it from the database. How a
//! tree is deleted belongs to `api::session::delete_session_tree`; what the
//! deletion leaves consistent belongs to the `SessionDeleted` projection.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;

use crate::api::AppState;

const DAY_MS: i64 = 86_400_000;

/// Trees deleted per sweep; the rest wait for the next one, oldest first.
const MAX_TREES_PER_SWEEP: usize = 50;

/// Same cadence as `event_bus::PRUNE_INTERVAL` (private to ox-core).
const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Upper bound on `retention_days`, matching `event_bus::retention_cutoff`:
/// a nonsensical value must not overflow into a cutoff that deletes everything.
const MAX_RETENTION_DAYS: u32 = 3_650;

/// A root session and the newest `time_updated` found anywhere in its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeActivity<'a> {
    pub(crate) id: &'a str,
    pub(crate) last_activity_ms: i64,
}

/// One [`TreeActivity`] per root (`parent_id` is `None`): the maximum of the
/// root's and every descendant's `time_updated`, at any depth.
///
/// A parent that is not among `rows` makes its subtree an orphan: it is
/// neither a root nor reported, so it is never selected for deletion. A parent
/// cycle is unreachable from any root and so is never reported either; the
/// traversal also tracks visited rows, so malformed input cannot loop.
pub(crate) fn tree_activities(rows: &[(String, Option<String>, i64)]) -> Vec<TreeActivity<'_>> {
    let index: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, (id, _, _))| (id.as_str(), i))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    for (i, (_, parent, _)) in rows.iter().enumerate() {
        if let Some(&p) = parent.as_deref().and_then(|p| index.get(p)) {
            children[p].push(i);
        }
    }

    let mut visited = vec![false; rows.len()];
    let mut stack = Vec::new();
    let mut out = Vec::new();
    for (root, (id, parent, updated)) in rows.iter().enumerate() {
        if parent.is_some() || visited[root] {
            continue;
        }
        let mut last = *updated;
        visited[root] = true;
        stack.push(root);
        while let Some(node) = stack.pop() {
            for &child in &children[node] {
                if !visited[child] {
                    visited[child] = true;
                    last = last.max(rows[child].2);
                    stack.push(child);
                }
            }
        }
        out.push(TreeActivity {
            id,
            last_activity_ms: last,
        });
    }
    out
}

/// Epoch-millisecond cutoff below which a tree's last activity is expired.
/// `days == 0` disables retention. Days are clamped to a decade and the
/// subtraction saturates, so no input can wrap the cutoff.
pub(crate) fn retention_cutoff(now_ms: i64, days: u32) -> Option<i64> {
    if days == 0 {
        return None;
    }
    let window = i64::from(days.min(MAX_RETENTION_DAYS)) * DAY_MS;
    Some(now_ms.saturating_sub(window))
}

/// Ids of trees whose last activity is strictly older than the cutoff, oldest
/// first with ties broken by id, so a capped sweep always takes the oldest.
pub(crate) fn expired_trees<'a>(
    activities: &[TreeActivity<'a>],
    now_ms: i64,
    days: u32,
) -> Vec<&'a str> {
    let Some(cutoff) = retention_cutoff(now_ms, days) else {
        return Vec::new();
    };
    let mut expired: Vec<TreeActivity<'a>> = activities
        .iter()
        .filter(|a| a.last_activity_ms < cutoff)
        .copied()
        .collect();
    expired.sort_unstable_by(|a, b| {
        a.last_activity_ms
            .cmp(&b.last_activity_ms)
            .then_with(|| a.id.cmp(b.id))
    });
    expired.into_iter().map(|a| a.id).collect()
}

/// What one [`sweep_once`] found and did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepReport {
    /// Expired trees eligible for deletion, before the per-sweep cap.
    pub(crate) candidate_trees: usize,
    pub(crate) deleted_trees: usize,
    pub(crate) failed_trees: usize,
}

/// Every session as `(id, parent_id, time_updated)`; nothing else is read.
async fn load_session_rows(app: &AppState) -> Result<Vec<(String, Option<String>, i64)>> {
    let conn = app.db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT id, parent_id, time_updated FROM session_v2")
        .await?;
    let mut rows = stmt.query(()).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push((row.get(0)?, row.get(1)?, row.get(2)?));
    }
    Ok(out)
}

/// Ids of sessions with a queued inbox item or a pending admission.
async fn load_busy_sessions(app: &AppState) -> Result<HashSet<String>> {
    let conn = app.db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT session_id FROM session_inbox UNION SELECT session_id FROM session_pending",
        )
        .await?;
    let mut rows = stmt.query(()).await?;
    let mut out = HashSet::new();
    while let Some(row) = rows.next().await? {
        out.insert(row.get::<String>(0)?);
    }
    Ok(out)
}

/// Roots whose tree contains a busy session, found by walking each busy
/// session up to its root. The step bound keeps a parent cycle from looping.
fn roots_with_busy_sessions<'a>(
    rows: &'a [(String, Option<String>, i64)],
    busy: &HashSet<String>,
) -> HashSet<&'a str> {
    let parents: HashMap<&str, Option<&str>> = rows
        .iter()
        .map(|(id, parent, _)| (id.as_str(), parent.as_deref()))
        .collect();
    let mut blocked = HashSet::new();
    for id in busy {
        let mut node = id.as_str();
        for _ in 0..=rows.len() {
            // The map key, not `node`, carries the `'a` borrow of `rows`.
            match parents.get_key_value(node) {
                Some((_, Some(parent))) => node = parent,
                Some((&root, None)) => {
                    blocked.insert(root);
                    break;
                }
                None => break,
            }
        }
    }
    blocked
}

/// One retention pass: delete up to [`MAX_TREES_PER_SWEEP`] expired trees,
/// oldest first. A tree with queued work anywhere in it is kept, and one that
/// fails to delete is logged and skipped so the rest still run.
pub(crate) async fn sweep_once(app: &AppState, now_ms: i64, days: u32) -> Result<SweepReport> {
    let rows = load_session_rows(app).await?;
    let activities = tree_activities(&rows);
    let mut expired = expired_trees(&activities, now_ms, days);
    if expired.is_empty() {
        return Ok(SweepReport::default());
    }

    let busy = load_busy_sessions(app).await?;
    if !busy.is_empty() {
        let blocked = roots_with_busy_sessions(&rows, &busy);
        expired.retain(|root| !blocked.contains(root));
    }

    let mut report = SweepReport {
        candidate_trees: expired.len(),
        ..SweepReport::default()
    };
    for root in expired.into_iter().take(MAX_TREES_PER_SWEEP) {
        match crate::api::session::delete_session_tree(app, root).await {
            Ok(()) => report.deleted_trees += 1,
            Err(error) => {
                report.failed_trees += 1;
                tracing::warn!(session = root, error = %error, "session retention: deleting tree failed");
            }
        }
    }
    if report.deleted_trees > 0 {
        tracing::info!(
            candidates = report.candidate_trees,
            deleted = report.deleted_trees,
            failed = report.failed_trees,
            days,
            "session retention: deleted expired session trees"
        );
    }
    Ok(report)
}

/// Sweep once now and then every [`SWEEP_INTERVAL`]. `days == 0` disables
/// retention. Best-effort: a failed sweep is logged and retried next tick.
///
/// Returns the sweeper's join handle (`None` when retention is off) so the
/// composition can await its last deletion pass; `stop` is observed at the
/// sleep boundary, so an in-progress tree deletion is never dropped.
pub(crate) fn spawn_sweeper(
    app: AppState,
    days: u32,
    stop: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if days == 0 {
        return None;
    }
    Some(tokio::spawn(async move {
        loop {
            if let Err(error) = sweep_once(&app, ox_schema::id::now_millis(), days).await {
                tracing::warn!(error = %error, "session retention: sweep failed");
            }
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(SWEEP_INTERVAL) => {}
            }
        }
    }))
}
