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
pub(crate) fn spawn_sweeper(app: AppState, days: u32) {
    if days == 0 {
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(error) = sweep_once(&app, ox_schema::id::now_millis(), days).await {
                tracing::warn!(error = %error, "session retention: sweep failed");
            }
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn row(id: &str, parent: Option<&str>, updated: i64) -> (String, Option<String>, i64) {
        (id.to_owned(), parent.map(str::to_owned), updated)
    }

    fn activity(rows: &[(String, Option<String>, i64)]) -> Vec<(&str, i64)> {
        let mut out: Vec<_> = tree_activities(rows)
            .into_iter()
            .map(|a| (a.id, a.last_activity_ms))
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn single_root_reports_its_own_update_time() {
        let rows = [row("a", None, 100)];
        assert_eq!(activity(&rows), vec![("a", 100)]);
    }

    #[test]
    fn recently_updated_child_keeps_the_tree_alive() {
        let rows = [row("a", None, 100), row("b", Some("a"), 900)];
        assert_eq!(activity(&rows), vec![("a", 900)]);
    }

    #[test]
    fn recently_updated_grandchild_keeps_the_tree_alive() {
        let rows = [
            row("a", None, 100),
            row("b", Some("a"), 200),
            row("c", Some("b"), 900),
        ];
        assert_eq!(activity(&rows), vec![("a", 900)]);
    }

    #[test]
    fn descendants_are_found_regardless_of_row_order() {
        let rows = [
            row("c", Some("b"), 900),
            row("b", Some("a"), 200),
            row("a", None, 100),
        ];
        assert_eq!(activity(&rows), vec![("a", 900)]);
    }

    #[test]
    fn older_descendants_do_not_lower_the_root_activity() {
        let rows = [row("a", None, 900), row("b", Some("a"), 100)];
        assert_eq!(activity(&rows), vec![("a", 900)]);
    }

    #[test]
    fn independent_roots_are_reported_separately() {
        let rows = [
            row("a", None, 100),
            row("b", None, 500),
            row("a1", Some("a"), 300),
        ];
        assert_eq!(activity(&rows), vec![("a", 300), ("b", 500)]);
    }

    #[test]
    fn parent_cycle_terminates_and_yields_no_root() {
        let rows = [
            row("a", Some("c"), 1),
            row("b", Some("a"), 2),
            row("c", Some("b"), 3),
            row("self", Some("self"), 4),
        ];
        assert!(tree_activities(&rows).is_empty());
    }

    #[test]
    fn cycle_next_to_a_healthy_tree_does_not_affect_it() {
        let rows = [
            row("root", None, 10),
            row("kid", Some("root"), 20),
            row("x", Some("y"), 999),
            row("y", Some("x"), 999),
        ];
        assert_eq!(activity(&rows), vec![("root", 20)]);
    }

    #[test]
    fn orphan_with_a_missing_parent_is_not_a_root() {
        let rows = [
            row("orphan", Some("gone"), 1),
            row("kid", Some("orphan"), 2),
        ];
        assert!(tree_activities(&rows).is_empty());
    }

    #[test]
    fn deep_chain_does_not_overflow_the_stack() {
        let n = 200_000;
        let mut rows = vec![row("0", None, 1)];
        for i in 1..n {
            rows.push(row(
                &i.to_string(),
                Some(&(i - 1).to_string()),
                i64::from(i),
            ));
        }
        assert_eq!(activity(&rows), vec![("0", i64::from(n - 1))]);
    }

    #[test]
    fn cutoff_is_none_when_disabled() {
        assert_eq!(retention_cutoff(1_000 * DAY, 0), None);
    }

    #[test]
    fn cutoff_subtracts_whole_days() {
        assert_eq!(retention_cutoff(100 * DAY, 30), Some(70 * DAY));
    }

    #[test]
    fn cutoff_clamps_days_to_ten_years() {
        let now = 10_000 * DAY;
        let ten_years = Some(now - 3_650 * DAY);
        assert_eq!(retention_cutoff(now, 3_650), ten_years);
        assert_eq!(retention_cutoff(now, 3_651), ten_years);
        assert_eq!(retention_cutoff(now, u32::MAX), ten_years);
    }

    #[test]
    fn cutoff_never_wraps_into_a_huge_value() {
        assert_eq!(
            retention_cutoff(i64::MAX, u32::MAX),
            Some(i64::MAX - 3_650 * DAY)
        );
        // Early clock: the window reaches before the epoch, so nothing expires.
        assert_eq!(retention_cutoff(0, 30), Some(-30 * DAY));
        assert_eq!(retention_cutoff(i64::MIN, u32::MAX), Some(i64::MIN));
        assert_eq!(retention_cutoff(5, u32::MAX), Some(5 - 3_650 * DAY));
    }

    fn act(id: &str, last_activity_ms: i64) -> TreeActivity<'_> {
        TreeActivity {
            id,
            last_activity_ms,
        }
    }

    #[test]
    fn activity_exactly_at_the_cutoff_is_kept() {
        let now = 100 * DAY;
        let cutoff = 70 * DAY;
        assert!(expired_trees(&[act("a", cutoff)], now, 30).is_empty());
    }

    #[test]
    fn activity_one_millisecond_before_the_cutoff_is_expired() {
        let now = 100 * DAY;
        let cutoff = 70 * DAY;
        assert_eq!(expired_trees(&[act("a", cutoff - 1)], now, 30), vec!["a"]);
    }

    #[test]
    fn disabled_retention_expires_nothing() {
        assert!(expired_trees(&[act("a", i64::MIN)], 100 * DAY, 0).is_empty());
    }

    #[test]
    fn oversized_days_behave_as_ten_years() {
        let now = 10_000 * DAY;
        let old = [act("old", now - 3_651 * DAY), act("new", now - 3_649 * DAY)];
        assert_eq!(expired_trees(&old, now, 3_651), vec!["old"]);
        assert_eq!(expired_trees(&old, now, u32::MAX), vec!["old"]);
    }

    #[test]
    fn early_clock_expires_nothing() {
        let all = [act("a", 0), act("b", 1)];
        assert!(expired_trees(&all, 0, u32::MAX).is_empty());
        assert!(expired_trees(&all, 5, 1).is_empty());
        assert!(expired_trees(&all, i64::MIN, 1).is_empty());
    }

    #[test]
    fn expired_trees_are_ordered_oldest_first_with_id_tie_break() {
        let now = 100 * DAY;
        let trees = [
            act("c", 10),
            act("b", 5),
            act("a", 10),
            act("fresh", 99 * DAY),
            act("z", 1),
        ];
        assert_eq!(expired_trees(&trees, now, 30), vec!["z", "b", "a", "c"]);
    }

    #[test]
    fn tree_activity_feeds_expiry_end_to_end() {
        let now = 100 * DAY;
        let rows = [
            row("stale", None, 1),
            row("stale-kid", Some("stale"), 2),
            row("alive", None, 1),
            row("alive-kid", Some("alive"), 95 * DAY),
        ];
        let activities = tree_activities(&rows);
        assert_eq!(expired_trees(&activities, now, 30), vec!["stale"]);
    }

    use crate::api::AppState;
    use crate::test_support::{rows_of, seed_session, test_state};
    use ox_schema::event::SessionEventData;
    use ox_schema::support::ForkBoundary;

    const NOW: i64 = 1_000 * DAY;

    /// Age control only: the projector stamps event time, so tests rewind it.
    async fn set_updated(app: &AppState, id: &str, ms: i64) {
        let conn = app.db.acquire().await.expect("conn");
        conn.execute(
            "UPDATE session_v2 SET time_updated = ?2 WHERE id = ?1",
            (id, ms),
        )
        .await
        .expect("set time_updated");
    }

    async fn count(app: &AppState, sql: &str) -> i64 {
        let conn = app.db.acquire().await.expect("conn");
        let mut stmt = conn.prepare(sql).await.expect("prepare");
        let mut rows = stmt.query(()).await.expect("query");
        rows.next()
            .await
            .expect("next")
            .expect("row")
            .get::<i64>(0)
            .expect("count")
    }

    async fn exists(app: &AppState, id: &str) -> bool {
        count(
            app,
            &format!("SELECT COUNT(*) FROM session_v2 WHERE id = '{id}'"),
        )
        .await
            == 1
    }

    #[tokio::test]
    async fn expired_tree_is_deleted_whole_and_recent_tree_is_kept() {
        let app = test_state().await;
        seed_session(&app, "old", None).await;
        seed_session(&app, "old_kid", Some("old")).await;
        seed_session(&app, "new", None).await;
        set_updated(&app, "old", NOW - 40 * DAY).await;
        set_updated(&app, "old_kid", NOW - 39 * DAY).await;
        set_updated(&app, "new", NOW - DAY).await;

        let report = sweep_once(&app, NOW, 30).await.expect("sweep");

        assert_eq!(
            report,
            SweepReport {
                candidate_trees: 1,
                deleted_trees: 1,
                failed_trees: 0
            }
        );
        for id in ["old", "old_kid"] {
            assert!(
                rows_of(&app.db, id).await.iter().all(|(_, n)| *n == 0),
                "{id} left rows"
            );
        }
        assert!(rows_of(&app.db, "new").await.iter().all(|(_, n)| *n > 0));
    }

    #[tokio::test]
    async fn recent_child_keeps_an_old_root_alive() {
        let app = test_state().await;
        seed_session(&app, "root", None).await;
        seed_session(&app, "kid", Some("root")).await;
        set_updated(&app, "root", NOW - 90 * DAY).await;
        set_updated(&app, "kid", NOW - DAY).await;

        let report = sweep_once(&app, NOW, 30).await.expect("sweep");

        assert_eq!(report, SweepReport::default());
        assert!(exists(&app, "root").await && exists(&app, "kid").await);
    }

    #[tokio::test]
    async fn expired_tree_with_inbox_or_pending_rows_is_kept() {
        let app = test_state().await;
        for id in ["inbox_root", "pending_kid_root", "free"] {
            seed_session(&app, id, None).await;
            set_updated(&app, id, NOW - 90 * DAY).await;
        }
        seed_session(&app, "pending_kid", Some("pending_kid_root")).await;
        set_updated(&app, "pending_kid", NOW - 90 * DAY).await;
        {
            let conn = app.db.acquire().await.expect("conn");
            conn.execute(
                "INSERT INTO session_inbox (id, session_id, type, payload, delivery, enqueued_seq, time_created)
                 VALUES ('i1', 'inbox_root', 'prompt', '{}', 'queued', 1, 1)",
                (),
            )
            .await
            .expect("inbox row");
            conn.execute(
                "INSERT INTO session_pending (id, session_id, type, data, delivery, admitted_seq, time_created)
                 VALUES ('p1', 'pending_kid', 'prompt', '{}', 'queued', 1, 1)",
                (),
            )
            .await
            .expect("pending row");
        }

        let report = sweep_once(&app, NOW, 30).await.expect("sweep");

        assert_eq!(report.deleted_trees, 1);
        assert!(exists(&app, "inbox_root").await);
        assert!(exists(&app, "pending_kid_root").await && exists(&app, "pending_kid").await);
        assert!(!exists(&app, "free").await);
    }

    #[tokio::test]
    async fn fork_survives_deletion_of_its_source() {
        let app = test_state().await;
        seed_session(&app, "source", None).await;
        let message_id: String = {
            let conn = app.db.acquire().await.expect("conn");
            let mut stmt = conn
                .prepare("SELECT id FROM session_message WHERE session_id = 'source'")
                .await
                .expect("prepare");
            let mut rows = stmt.query(()).await.expect("query");
            rows.next()
                .await
                .expect("next")
                .expect("row")
                .get(0)
                .expect("id")
        };
        app.bus
            .publish(SessionEventData::SessionForked {
                session_id: "fork".to_owned(),
                parent_id: "source".to_owned(),
                boundary: ForkBoundary::Through { message_id },
                instructions: None,
                instruction_entries: None,
            })
            .await
            .expect("fork");
        set_updated(&app, "source", NOW - 90 * DAY).await;
        let copied = count(
            &app,
            "SELECT COUNT(*) FROM session_message WHERE session_id = 'fork'",
        )
        .await;
        assert!(copied > 0, "fork must hold its own message copies");

        let report = sweep_once(&app, NOW, 30).await.expect("sweep");

        assert_eq!(report.deleted_trees, 1);
        assert!(!exists(&app, "source").await);
        assert!(exists(&app, "fork").await);
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM session_message WHERE session_id = 'fork'"
            )
            .await,
            copied
        );
    }

    #[tokio::test]
    async fn project_memory_survives_a_sweep() {
        let app = test_state().await;
        seed_session(&app, "old", None).await;
        set_updated(&app, "old", NOW - 90 * DAY).await;
        {
            let conn = app.db.acquire().await.expect("conn");
            conn.execute(
                "INSERT INTO mc_memory (id, project, content, category, created_at, updated_at)
                 VALUES ('m1', 'default', 'keep me', 'note', 1, 1)",
                (),
            )
            .await
            .expect("memory row");
        }

        let report = sweep_once(&app, NOW, 30).await.expect("sweep");

        assert_eq!(report.deleted_trees, 1);
        assert_eq!(count(&app, "SELECT COUNT(*) FROM mc_memory").await, 1);
    }

    #[tokio::test]
    async fn a_sweep_deletes_at_most_fifty_trees_oldest_first() {
        let app = test_state().await;
        for i in 0..51_i64 {
            let id = format!("t{i:02}");
            seed_session(&app, &id, None).await;
            // t00 is the newest of the expired trees, t50 the oldest.
            set_updated(&app, &id, NOW - 60 * DAY - i * 1_000).await;
        }

        let report = sweep_once(&app, NOW, 30).await.expect("sweep");

        assert_eq!(
            report,
            SweepReport {
                candidate_trees: 51,
                deleted_trees: 50,
                failed_trees: 0
            }
        );
        assert_eq!(count(&app, "SELECT COUNT(*) FROM session_v2").await, 1);
        assert!(exists(&app, "t00").await, "the newest expired tree waits");
    }

    #[tokio::test]
    async fn disabled_retention_deletes_nothing() {
        let app = test_state().await;
        seed_session(&app, "old", None).await;
        set_updated(&app, "old", 1).await;

        let report = sweep_once(&app, NOW, 0).await.expect("sweep");

        assert_eq!(report, SweepReport::default());
        assert!(exists(&app, "old").await);
    }

    fn busy(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn blocked(rows: &[(String, Option<String>, i64)], ids: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = roots_with_busy_sessions(rows, &busy(ids))
            .into_iter()
            .map(str::to_owned)
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn busy_grandchild_blocks_its_root() {
        let rows = [
            row("a", None, 1),
            row("b", Some("a"), 1),
            row("c", Some("b"), 1),
            row("other", None, 1),
        ];
        assert_eq!(blocked(&rows, &["c"]), vec!["a"]);
    }

    #[test]
    fn busy_id_missing_from_rows_blocks_nothing() {
        let rows = [row("a", None, 1), row("b", Some("a"), 1)];
        assert!(blocked(&rows, &["ghost"]).is_empty());
    }

    #[test]
    fn busy_session_in_a_parent_cycle_terminates_and_blocks_nothing() {
        let rows = [
            row("x", Some("y"), 1),
            row("y", Some("x"), 1),
            row("self", Some("self"), 1),
            row("root", None, 1),
        ];
        assert!(blocked(&rows, &["x", "self"]).is_empty());
    }

    #[test]
    fn busy_root_blocks_only_that_root() {
        let rows = [
            row("a", None, 1),
            row("a1", Some("a"), 1),
            row("b", None, 1),
            row("b1", Some("b"), 1),
        ];
        assert_eq!(blocked(&rows, &["a"]), vec!["a"]);
    }

    async fn time_updated(app: &AppState, id: &str) -> i64 {
        count(
            app,
            &format!("SELECT time_updated FROM session_v2 WHERE id = '{id}'"),
        )
        .await
    }

    fn model_ref() -> ox_schema::support::ModelRef {
        ox_schema::support::ModelRef {
            id: "m".to_owned(),
            provider_id: "p".to_owned(),
            variant: None,
        }
    }

    /// The feature rests on this: real events refresh `time_updated`, so a
    /// session that is being used never looks idle to the sweeper.
    #[tokio::test]
    async fn ordinary_activity_advances_time_updated() {
        let app = test_state().await;
        let long_ago = ox_schema::id::now_millis() - 90 * DAY;
        for id in ["prompted", "stepped", "executed", "stale"] {
            seed_session(&app, id, None).await;
            set_updated(&app, id, long_ago).await;
        }

        app.bus
            .publish(SessionEventData::InboxEnqueued {
                session_id: "prompted".to_owned(),
                inbox_id: "inbox1".to_owned(),
                item: ox_schema::inbox::Item::User {
                    payload: ox_schema::inbox::UserPayload {
                        prompt: ox_schema::support::Prompt {
                            text: "hi".to_owned(),
                            files: None,
                            agents: None,
                            skills: None,
                        },
                        metadata: None,
                    },
                    delivery: ox_schema::inbox::Delivery::Queue,
                },
            })
            .await
            .expect("enqueue");
        app.bus
            .publish(SessionEventData::StepStarted {
                session_id: "stepped".to_owned(),
                assistant_message_id: "msg_a".to_owned(),
                agent: "build".to_owned(),
                model: model_ref(),
                snapshot: None,
                started: ox_schema::id::now_millis(),
            })
            .await
            .expect("step started");
        app.bus
            .publish(SessionEventData::ExecutionStarted {
                session_id: "executed".to_owned(),
            })
            .await
            .expect("execution started");

        let now = ox_schema::id::now_millis();
        for id in ["prompted", "stepped", "executed"] {
            let updated = time_updated(&app, id).await;
            assert!(updated > long_ago, "{id}: time_updated not advanced");
            assert!(
                (now - updated).abs() < 10_000,
                "{id}: time_updated {updated} is not near now {now}"
            );
        }

        // The prompt left a queued inbox row, which would keep its tree on its
        // own; clear it so the sweep below is decided by `time_updated` alone.
        app.bus
            .publish(SessionEventData::InboxCancelled {
                session_id: "prompted".to_owned(),
                inbox_id: "inbox1".to_owned(),
            })
            .await
            .expect("cancel");
        assert_eq!(
            count(&app, "SELECT COUNT(*) FROM session_inbox").await
                + count(&app, "SELECT COUNT(*) FROM session_pending").await,
            0,
            "cancelling must clear the queued prompt"
        );

        let report = sweep_once(&app, now, 30).await.expect("sweep");

        assert_eq!(
            report,
            SweepReport {
                candidate_trees: 1,
                deleted_trees: 1,
                failed_trees: 0
            }
        );
        assert!(!exists(&app, "stale").await, "control tree must expire");
        for id in ["prompted", "stepped", "executed"] {
            assert!(exists(&app, id).await, "{id} was deleted despite activity");
        }
    }

    #[tokio::test]
    async fn a_tree_that_fails_to_delete_does_not_stop_the_others() {
        let app = test_state().await;
        for (id, age) in [("ses_fail", 90), ("ses_ok1", 80), ("ses_ok2", 70)] {
            seed_session(&app, id, None).await;
            set_updated(&app, id, NOW - age * DAY).await;
        }
        {
            let conn = app.db.acquire().await.expect("conn");
            conn.execute(
                "CREATE TRIGGER block_delete BEFORE DELETE ON session_v2
                 WHEN OLD.id = 'ses_fail'
                 BEGIN SELECT RAISE(ABORT, 'boom'); END",
                (),
            )
            .await
            .expect("trigger");
        }

        let report = sweep_once(&app, NOW, 30).await.expect("sweep");

        assert_eq!(
            report,
            SweepReport {
                candidate_trees: 3,
                deleted_trees: 2,
                failed_trees: 1
            }
        );
        assert!(exists(&app, "ses_fail").await);
        assert!(!exists(&app, "ses_ok1").await && !exists(&app, "ses_ok2").await);
    }
}
