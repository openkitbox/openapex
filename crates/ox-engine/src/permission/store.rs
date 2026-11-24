//! Saved permission approvals, backed by V25's `permission_saved` table.
//!
//! This is the only authority for "always" approvals: there is no in-memory
//! cache, so an HTTP DELETE takes effect on the very next tool call. Only
//! `allow` rows exist — a permanent refusal belongs in the configuration's
//! rule set and short-circuits before saved rules are consulted.

use anyhow::Result;
use ox_db::Db;
use ox_db::turso::Row;
use ox_schema::permission::{Effect, Rule, SavedInfo, SavedTime, new_saved_id};

/// Why a saved row could not be edited. The HTTP layer maps `NotFound` to 404
/// and `Conflict` to 409; a storage failure stays a 500.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("saved permission not found: {id}")]
    NotFound { id: String },
    #[error("saved permission already exists: {id}")]
    Conflict {
        id: String,
        action: String,
        resource: String,
    },
    #[error("saved permission update failed: {0}")]
    Failed(String),
}

impl From<anyhow::Error> for UpdateError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

impl From<ox_db::turso::Error> for UpdateError {
    fn from(error: ox_db::turso::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

#[derive(Clone)]
pub struct SavedStore {
    db: Db,
}

impl SavedStore {
    #[must_use]
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }

    /// List a project's approvals, newest first.
    pub async fn list(&self, project_id: &str) -> Result<Vec<SavedInfo>> {
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project_id, action, resource, time_created, time_updated
                 FROM permission_saved WHERE project_id = ?1
                 ORDER BY time_updated DESC, id DESC",
            )
            .await?;
        let mut rows = stmt.query((project_id,)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(info_of(&row)?);
        }
        Ok(out)
    }

    /// The rules the engine evaluates as the ask-only fallback.
    pub(crate) async fn rules(&self, project_id: &str) -> Result<Vec<Rule>> {
        Ok(self
            .list(project_id)
            .await?
            .into_iter()
            .map(|info| Rule {
                action: info.action,
                resource: info.resource,
                effect: Effect::Allow,
            })
            .collect())
    }

    /// Insert one allow per resource. Repeated approvals refresh the timestamp
    /// instead of creating a second row (the table's UNIQUE constraint).
    pub async fn save(&self, project_id: &str, action: &str, resources: &[String]) -> Result<()> {
        if resources.is_empty() {
            return Ok(());
        }
        let tx = self.db.begin().await?;
        for resource in resources {
            let now = ox_schema::id::now_millis();
            tx.execute(
                "INSERT INTO permission_saved
                 (id, project_id, action, resource, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT (project_id, action, resource)
                 DO UPDATE SET time_updated = excluded.time_updated",
                (new_saved_id(), project_id, action, resource.as_str(), now),
            )
            .await?;
        }
        tx.commit().await
    }

    /// Revoke one approval. A missing id is a no-op: clients can DELETE again
    /// after a concurrent delete and see the same settled state.
    pub async fn delete(&self, project_id: &str, id: &str) -> Result<()> {
        let tx = self.db.begin().await?;
        tx.execute(
            "DELETE FROM permission_saved WHERE project_id = ?1 AND id = ?2",
            (project_id, id),
        )
        .await?;
        tx.commit().await
    }

    /// Change a row's matcher, preserving its identity and creation time. The
    /// project predicate keeps another project's row untouched even when its
    /// id is named explicitly; a duplicate `(project, action, resource)` is a
    /// conflict rather than a silent overwrite.
    pub async fn update(
        &self,
        project_id: &str,
        id: &str,
        action: Option<&str>,
        resource: Option<&str>,
    ) -> std::result::Result<SavedInfo, UpdateError> {
        let current = self
            .get(project_id, id)
            .await?
            .ok_or_else(|| UpdateError::NotFound { id: id.to_owned() })?;
        let action = action.unwrap_or(current.action.as_str());
        let resource = resource.unwrap_or(current.resource.as_str());
        let now = ox_schema::id::now_millis();
        let tx = self.db.begin().await?;
        if let Err(error) = tx
            .execute(
                "UPDATE permission_saved SET action = ?3, resource = ?4, time_updated = ?5
                 WHERE project_id = ?1 AND id = ?2",
                (project_id, id, action, resource, now),
            )
            .await
        {
            let mapped = classify_update(error, id, action, resource);
            let _ = tx.rollback().await;
            return Err(mapped);
        }
        let mut stmt = tx
            .prepare(
                "SELECT id, project_id, action, resource, time_created, time_updated
                 FROM permission_saved WHERE project_id = ?1 AND id = ?2",
            )
            .await?;
        let row = ox_db::optional(&mut stmt, (project_id, id)).await?;
        let updated = row.as_ref().map(info_of).transpose()?;
        tx.commit().await?;
        updated.ok_or_else(|| UpdateError::NotFound { id: id.to_owned() })
    }

    pub async fn get(&self, project_id: &str, id: &str) -> Result<Option<SavedInfo>> {
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project_id, action, resource, time_created, time_updated
                 FROM permission_saved WHERE project_id = ?1 AND id = ?2",
            )
            .await?;
        let row = ox_db::optional(&mut stmt, (project_id, id)).await?;
        row.as_ref().map(info_of).transpose()
    }
}

fn info_of(row: &Row) -> Result<SavedInfo> {
    Ok(SavedInfo {
        id: row.get(0)?,
        project_id: row.get(1)?,
        action: row.get(2)?,
        resource: row.get(3)?,
        time: SavedTime {
            created: row.get(4)?,
            updated: row.get(5)?,
        },
    })
}

fn classify_update(
    error: ox_db::turso::Error,
    id: &str,
    action: &str,
    resource: &str,
) -> UpdateError {
    if matches!(error, ox_db::turso::Error::Constraint(_)) {
        UpdateError::Conflict {
            id: id.to_owned(),
            action: action.to_owned(),
            resource: resource.to_owned(),
        }
    } else {
        UpdateError::Failed(error.to_string())
    }
}
