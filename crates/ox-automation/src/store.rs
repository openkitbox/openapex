//! SQLite mapping for automations.
//!
//! Mutations use distinct guards: run completions require both the issued
//! `run_count` and `status='active'`; status transitions check the observed
//! `run_count`; snapshot saves check both. A row observed as `done` is
//! terminal, and `save` cannot write `done`: only `finish` completes a run.

use anyhow::{Result, ensure};
use ox_db::Db;
use ox_db::turso::Row;
use serde_json::Value;

use crate::types::{Action, Info, Outcome, Status, TimeInfo, Trigger};

/// Borrowed handle to the automation tables.
#[derive(Clone, Copy)]
pub struct AutomationStore<'a> {
    db: &'a Db,
}

/// Outcome written when a run finishes. `run_count` is the ticket issued by
/// [`AutomationStore::claim`]; a stale ticket writes nothing.
pub struct Completion<'a> {
    pub id: &'a str,
    pub run_count: u64,
    pub status: Status,
    pub next_due_at: Option<i64>,
    pub retry_at: Option<i64>,
    pub outcome: Outcome,
    pub output: Option<String>,
    pub state: Option<Value>,
    pub now_ms: i64,
}

const COLUMNS: &str = "id, project_id, session_id, directory, name, trigger, tz, action, \
     status, next_due_at, expires_at, run_count, last_run_at, last_outcome, last_output, \
     retry_at, state, time_created, time_updated";

impl<'a> AutomationStore<'a> {
    /// Borrow the database handle; the store never owns or clones it.
    #[must_use]
    pub const fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// Create a row. An existing id is an error rather than an overwrite.
    pub async fn insert(&self, info: &Info) -> Result<()> {
        let trigger = serde_json::to_string(&info.trigger)?;
        let action = serde_json::to_string(&info.action)?;
        let state = info.state.as_ref().map(serde_json::to_string).transpose()?;
        let tx = self.db.begin().await?;
        tx.execute(
            "INSERT INTO automation
                (id, project_id, session_id, directory, name, trigger, tz, action, status,
                 next_due_at, expires_at, run_count, last_run_at, last_outcome, last_output,
                 state, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18)",
            ox_db::turso::params![
                info.id.as_str(),
                info.project_id.as_str(),
                info.session_id.as_deref(),
                info.directory.as_str(),
                info.name.as_str(),
                trigger.as_str(),
                info.tz.as_str(),
                action.as_str(),
                info.status.as_str(),
                info.next_due_at,
                info.expires_at,
                i64::try_from(info.run_count)?,
                info.last_run_at,
                info.last_outcome.map(Outcome::as_str),
                info.last_output.as_deref(),
                state.as_deref(),
                info.time.created,
                info.time.updated,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Read one snapshot by id.
    pub async fn get(&self, id: &str) -> Result<Option<Info>> {
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare(&format!("SELECT {COLUMNS} FROM automation WHERE id = ?1"))
            .await?;
        match ox_db::optional(&mut stmt, [id]).await? {
            Some(row) => Ok(Some(row_to_info(&row)?)),
            None => Ok(None),
        }
    }

    /// All automations owned by one project, oldest first.
    pub async fn list(&self, project_id: &str) -> Result<Vec<Info>> {
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM automation WHERE project_id = ?1 ORDER BY time_created, id"
            ))
            .await?;
        let mut cursor = stmt.query([project_id]).await?;
        let mut infos = Vec::new();
        while let Some(row) = cursor.next().await? {
            infos.push(row_to_info(&row)?);
        }
        Ok(infos)
    }

    /// Project ids that own at least one active automation.
    pub async fn active_projects(&self) -> Result<Vec<String>> {
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT DISTINCT project_id FROM automation WHERE status = 'active'")
            .await?;
        let mut cursor = stmt.query([] as [String; 0]).await?;
        let mut projects = Vec::new();
        while let Some(row) = cursor.next().await? {
            projects.push(row.get::<String>(0)?);
        }
        Ok(projects)
    }

    /// Persist a snapshot only if its revision and observed status are still
    /// current. `expected_status` is the state the caller read; a late writer
    /// that observed `active` cannot rewrite a row now paused by someone else.
    /// A `done` row or a `done` snapshot is rejected: `finish` owns completion.
    pub async fn save(&self, info: &Info, expected_status: Status) -> Result<bool> {
        if expected_status == Status::Done || info.status == Status::Done {
            return Ok(false);
        }
        let trigger = serde_json::to_string(&info.trigger)?;
        let action = serde_json::to_string(&info.action)?;
        let state = info.state.as_ref().map(serde_json::to_string).transpose()?;
        let tx = self.db.begin().await?;
        let changed = tx
            .execute(
                "UPDATE automation SET name = ?1, trigger = ?2, tz = ?3, action = ?4,
                    status = ?5, next_due_at = ?6, expires_at = ?7, last_outcome = ?8,
                    last_output = ?9, state = ?10, time_updated = ?11
                 WHERE id = ?12 AND run_count = ?13 AND status = ?14",
                (
                    info.name.as_str(),
                    trigger.as_str(),
                    info.tz.as_str(),
                    action.as_str(),
                    info.status.as_str(),
                    info.next_due_at,
                    info.expires_at,
                    info.last_outcome.map(Outcome::as_str),
                    info.last_output.as_deref(),
                    state.as_deref(),
                    info.time.updated,
                    info.id.as_str(),
                    i64::try_from(info.run_count)?,
                    expected_status.as_str(),
                ),
            )
            .await?;
        tx.commit().await?;
        Ok(changed == 1)
    }

    /// Atomically issue one run to a caller holding `expected_run_count`.
    pub async fn claim(&self, id: &str, now_ms: i64, expected_run_count: u64) -> Result<bool> {
        let tx = self.db.begin().await?;
        let changed = tx
            .execute(
                "UPDATE automation SET run_count = run_count + 1, last_run_at = ?1,
                    last_outcome = NULL, last_output = NULL, retry_at = NULL
                 WHERE id = ?2 AND status = 'active' AND run_count = ?3
                   AND (next_due_at IS NULL OR next_due_at <= ?1)
                   AND (retry_at IS NULL OR retry_at <= ?1)",
                (now_ms, id, i64::try_from(expected_run_count)?),
            )
            .await?;
        tx.commit().await?;
        Ok(changed == 1)
    }

    /// Move an event automation's due time while the caller still holds the
    /// observed revision. Returns `false` for a missing, inactive, or advanced row.
    pub async fn queue_event_due(
        &self,
        id: &str,
        now_ms: i64,
        expected_run_count: u64,
    ) -> Result<bool> {
        let tx = self.db.begin().await?;
        let changed = tx
            .execute(
                "UPDATE automation SET next_due_at = ?1
                 WHERE id = ?2 AND status = 'active' AND run_count = ?3",
                (now_ms, id, i64::try_from(expected_run_count)?),
            )
            .await?;
        tx.commit().await?;
        Ok(changed == 1)
    }

    /// Settle a claimed run. Returns `false` when the row is gone, paused by
    /// another writer, or its run number has advanced since the claim.
    pub async fn finish(&self, completion: &Completion<'_>) -> Result<bool> {
        let state = completion
            .state
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let failed = i64::from(completion.outcome == Outcome::Failed);
        let tx = self.db.begin().await?;
        let changed = tx
            .execute(
                "UPDATE automation SET status = ?1, next_due_at = ?2, retry_at = ?3,
                    last_outcome = ?4, last_output = ?5, state = ?6, time_updated = ?7,
                    failures = failures + ?8
                 WHERE id = ?9 AND run_count = ?10 AND status = 'active'",
                (
                    completion.status.as_str(),
                    completion.next_due_at,
                    completion.retry_at,
                    completion.outcome.as_str(),
                    completion.output.as_deref(),
                    state.as_deref(),
                    completion.now_ms,
                    failed,
                    completion.id,
                    i64::try_from(completion.run_count)?,
                ),
            )
            .await?;
        tx.commit().await?;
        Ok(changed == 1)
    }

    /// Pause or resume a row at a known revision. `done` is terminal.
    pub async fn set_status(
        &self,
        id: &str,
        status: Status,
        now_ms: i64,
        expected_run_count: u64,
    ) -> Result<bool> {
        let tx = self.db.begin().await?;
        let changed = tx
            .execute(
                "UPDATE automation SET status = ?1, time_updated = ?2
                 WHERE id = ?3 AND run_count = ?4 AND status <> 'done'",
                (
                    status.as_str(),
                    now_ms,
                    id,
                    i64::try_from(expected_run_count)?,
                ),
            )
            .await?;
        tx.commit().await?;
        Ok(changed == 1)
    }
}

fn row_to_info(row: &Row) -> Result<Info> {
    // Positions follow COLUMNS exactly: id, project, session, directory, name,
    // trigger, tz, action, status, next_due, expires, runs, last_run,
    // last_outcome, last_output, retry_at, state, created, updated.
    let status: String = row.get(8)?;
    let outcome: Option<String> = row.get(13)?;
    let raw_trigger: String = row.get(5)?;
    let raw_action: String = row.get(7)?;
    let raw_state: Option<String> = row.get(16)?;
    let run_count = row.get::<i64>(11)?;
    ensure!(run_count >= 0, "negative automation run_count: {run_count}");
    let status =
        Status::parse(&status).ok_or_else(|| anyhow::anyhow!("unknown status: {status}"))?;
    let outcome = match outcome {
        None => None,
        Some(value) => Some(
            Outcome::parse(&value).ok_or_else(|| anyhow::anyhow!("unknown outcome: {value}"))?,
        ),
    };
    Ok(Info {
        id: row.get(0)?,
        name: row.get(4)?,
        project_id: row.get(1)?,
        session_id: row.get(2)?,
        directory: row.get(3)?,
        trigger: serde_json::from_str::<Trigger>(&raw_trigger)?,
        tz: row.get(6)?,
        action: serde_json::from_str::<Action>(&raw_action)?,
        expires_at: row.get(10)?,
        status,
        next_due_at: row.get(9)?,
        run_count: u64::try_from(run_count)?,
        last_run_at: row.get(12)?,
        last_outcome: outcome,
        last_output: row.get(14)?,
        state: raw_state.as_deref().map(serde_json::from_str).transpose()?,
        time: TimeInfo {
            created: row.get(17)?,
            updated: row.get(18)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use ox_db::Db;

    use super::AutomationStore;
    use crate::types::{Action, Info, Status, TimeInfo, Trigger};

    fn sample_info(id: &str, now: i64) -> Info {
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
            status: Status::Active,
            next_due_at: Some(now),
            run_count: 0,
            last_run_at: None,
            last_outcome: None,
            last_output: None,
            state: None,
            time: TimeInfo {
                created: now,
                updated: now,
            },
        }
    }

    #[tokio::test]
    async fn automation_store_cas_guards_stale_updates() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        let info = sample_info("aut_1", 1_000);
        store.insert(&info).await?;

        assert_eq!(store.get("aut_1").await?.expect("inserted").name, "nightly");
        assert!(
            store.claim("aut_1", 2_000, 0).await?,
            "fresh claim must succeed"
        );
        assert!(
            !store.claim("aut_1", 2_001, 0).await?,
            "stale run count must not claim again"
        );

        let mut claimed = store.get("aut_1").await?.expect("claimed");
        assert_eq!(claimed.run_count, 1);
        assert_eq!(claimed.last_run_at, Some(2_000));
        claimed.next_due_at = Some(3_000);
        assert!(
            store.save(&claimed, Status::Active).await?,
            "current revision must save"
        );
        assert!(
            !store
                .save(&sample_info("aut_1", 1_000), Status::Active)
                .await?,
            "stale revision must not overwrite"
        );

        assert!(
            store.set_status("aut_1", Status::Paused, 2_600, 1).await?,
            "current revision must be pausable"
        );
        assert!(
            !store.set_status("aut_1", Status::Active, 2_700, 0).await?,
            "stale completion must not resume a paused row"
        );
        assert_eq!(
            store.get("aut_1").await?.expect("paused").status,
            Status::Paused
        );
        assert!(
            !store.save(&claimed, Status::Active).await?,
            "a writer that observed active must not rewrite a paused row"
        );
        assert_eq!(store.list("proj_1").await?.len(), 1);
        assert!(store.list("other").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn automation_store_save_cannot_resurrect_done() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        store.insert(&sample_info("aut_3", 1_000)).await?;
        assert!(store.claim("aut_3", 2_000, 0).await?);

        let completion = super::Completion {
            id: "aut_3",
            run_count: 1,
            status: Status::Done,
            next_due_at: None,
            retry_at: None,
            outcome: crate::types::Outcome::Fired,
            output: None,
            state: None,
            now_ms: 2_100,
        };
        assert!(
            store.finish(&completion).await?,
            "fresh completion must reach done"
        );

        let done = store.get("aut_3").await?.expect("done row");
        assert_eq!(done.status, Status::Done);
        assert!(
            !store.save(&done, Status::Done).await?,
            "save must not rewrite a done row, even with its own snapshot"
        );

        let mut active = done.clone();
        active.status = Status::Active;
        assert!(
            !store.save(&active, Status::Done).await?,
            "save must not flip a done row back to active"
        );
        assert_eq!(
            store.get("aut_3").await?.expect("still done").status,
            Status::Done,
            "done is terminal across every writer"
        );
        Ok(())
    }

    #[tokio::test]
    async fn automation_store_finish_records_outcome() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        store.insert(&sample_info("aut_2", 1_000)).await?;
        assert!(store.claim("aut_2", 2_000, 0).await?);

        let completion = super::Completion {
            id: "aut_2",
            run_count: 1,
            status: Status::Active,
            next_due_at: Some(3_000),
            retry_at: None,
            outcome: crate::types::Outcome::Fired,
            output: Some("ok".to_owned()),
            state: None,
            now_ms: 2_100,
        };
        assert!(
            store.finish(&completion).await?,
            "fresh completion must land"
        );
        let finished = store.get("aut_2").await?.expect("finished");
        assert_eq!(finished.last_outcome, Some(crate::types::Outcome::Fired));
        assert_eq!(finished.last_output.as_deref(), Some("ok"));
        assert_eq!(finished.next_due_at, Some(3_000));

        let stale = super::Completion {
            run_count: 0,
            outcome: crate::types::Outcome::Failed,
            ..completion
        };
        assert!(
            !store.finish(&stale).await?,
            "stale completion must not overwrite the new status"
        );
        Ok(())
    }

    #[tokio::test]
    async fn automation_store_active_projects_excludes_inactive() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);

        let mut paused = sample_info("aut_paused", 1_000);
        paused.project_id = "proj_paused".to_owned();
        paused.status = Status::Paused;
        let mut done = sample_info("aut_done", 1_000);
        done.project_id = "proj_done".to_owned();
        done.status = Status::Done;
        let mut second_active = sample_info("aut_active_2", 1_000);
        second_active.name = "another".to_owned();

        store.insert(&sample_info("aut_active_1", 1_000)).await?;
        store.insert(&second_active).await?;
        store.insert(&paused).await?;
        store.insert(&done).await?;

        let mut projects = store.active_projects().await?;
        projects.sort();
        assert_eq!(
            projects,
            vec!["proj_1"],
            "only active projects are listed once"
        );
        Ok(())
    }

    #[tokio::test]
    async fn automation_store_queue_event_due_guards_status_and_revision() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        store.insert(&sample_info("aut_due", 1_000)).await?;
        assert!(
            store.claim("aut_due", 1_500, 0).await?,
            "advance the run count so an older ticket is stale"
        );

        assert!(
            !store.queue_event_due("aut_due", 2_000, 0).await?,
            "a stale run count must not move the due time"
        );
        assert_eq!(
            store.get("aut_due").await?.expect("unchanged").next_due_at,
            Some(1_000)
        );
        assert!(
            store.queue_event_due("aut_due", 2_000, 1).await?,
            "a fresh event must make an active row due"
        );
        assert_eq!(
            store.get("aut_due").await?.expect("updated").next_due_at,
            Some(2_000)
        );
        assert!(
            store.claim("aut_due", 2_100, 1).await?,
            "issue a newer ticket before checking the old one again"
        );
        assert!(
            !store.queue_event_due("aut_due", 2_200, 1).await?,
            "the superseded ticket must not move the due time"
        );
        assert_eq!(
            store.get("aut_due").await?.expect("still due").next_due_at,
            Some(2_000)
        );

        let mut paused = sample_info("aut_paused_due", 1_000);
        paused.status = Status::Paused;
        let mut done = sample_info("aut_done_due", 1_000);
        done.status = Status::Done;
        store.insert(&paused).await?;
        store.insert(&done).await?;

        assert!(
            !store.queue_event_due("aut_paused_due", 2_000, 0).await?,
            "a paused row must not become due"
        );
        assert!(
            !store.queue_event_due("aut_done_due", 2_000, 0).await?,
            "a done row must not become due"
        );
        assert_eq!(
            store
                .get("aut_paused_due")
                .await?
                .expect("still paused")
                .next_due_at,
            Some(1_000)
        );
        assert_eq!(
            store
                .get("aut_done_due")
                .await?
                .expect("still done")
                .next_due_at,
            Some(1_000)
        );
        Ok(())
    }
}
