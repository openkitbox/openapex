//! Compartment records: what the historian has folded out of the live
//! transcript, plus the coverage end the request assembly uses as its floor.
//!
//! One row per compartment in `mc_compartments` (`id, session_id,
//! coverage_end, importance, tiers JSON, search_text, created_at`). `tiers`
//! carries every field the decay renderer consumes; the historian's
//! `<facts>`/`<events>` side channels are NOT compartment fields (the
//! reference's own compartment table has none) — they are emitted once per
//! run and live on the producing `mc_historian_run` row.
//!
//! `search_text` is the derived full-text column: the heading and paraphrase
//! tiers joined by spaces, exactly as `MIGRATION_V7` backfills it (an FTS
//! index is declared over a column, so the text has to be materialized).
//! `tiers` stays the single source of truth — every insert derives the column
//! from the in-memory [`Tiers`], and no reader renders from `search_text`.
//!
//! Chronological order is insertion order (`created_at`, tie-broken by `id`):
//! the historian only ever appends, and `coverage_end` is text in the schema
//! so it must not be used for ordering.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::mctx::decay_render::DecayRenderCompartment;
use ox_db::Db;

/// The paraphrase tiers and heading fields of one compartment — the `tiers`
/// JSON column. `p1..p4` are the paraphrase ladder; `p1` absent or empty
/// marks a flat-content (`legacy`) row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tiers {
    /// Heading title (untrusted: historian-authored).
    #[serde(default)]
    pub title: String,
    /// First covered message ordinal.
    #[serde(default)]
    pub start_message: i64,
    /// Last covered message ordinal.
    #[serde(default)]
    pub end_message: i64,
    /// Start date (YYYY-MM-DD) of the covered span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    /// End date (YYYY-MM-DD) of the covered span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    /// Paraphrase tier P1 (most verbose).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p1: Option<String>,
    /// Paraphrase tier P2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2: Option<String>,
    /// Paraphrase tier P3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p3: Option<String>,
    /// Paraphrase tier P4 (most compressed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p4: Option<String>,
}

/// One stored compartment.
#[derive(Debug, Clone, PartialEq)]
pub struct Compartment {
    /// Row id (`mc_`-style opaque id).
    pub id: String,
    /// Owning session.
    pub session_id: String,
    /// Last message ordinal this compartment covers, as text in the column.
    pub coverage_end: String,
    /// 1..100 importance (decay rate).
    pub importance: f64,
    /// Heading + paraphrase tiers.
    pub tiers: Tiers,
    /// Insertion time (ms); chronological order.
    pub created_at: i64,
}

impl Compartment {
    /// The coverage end as a number, or `None` when the column is malformed.
    #[must_use]
    pub fn end_ordinal(&self) -> Option<i64> {
        self.coverage_end.trim().parse::<i64>().ok()
    }

    /// Projection into the historian prompt's reference shape (the
    /// `<session_references>` block: coverage range plus every paraphrase
    /// tier, no row id and no dates).
    #[must_use]
    pub fn reference_input(&self) -> crate::mctx::historian::ReferenceCompartment<'_> {
        crate::mctx::historian::ReferenceCompartment {
            start_message: self.tiers.start_message,
            end_message: self.tiers.end_message,
            title: &self.tiers.title,
            content: "",
            p1: self.tiers.p1.as_deref(),
            p2: self.tiers.p2.as_deref(),
            p3: self.tiers.p3.as_deref(),
            p4: self.tiers.p4.as_deref(),
            importance: Some(self.importance.round().clamp(1.0, 100.0) as i32),
            // ox's compartment rows carry no episode type: the reference uses
            // it only for its own prompt labels.
            episode_type: None,
        }
    }

    /// Projection into the decay renderer's input shape.
    #[must_use]
    pub fn render_input(&self) -> DecayRenderCompartment<'_> {
        DecayRenderCompartment {
            start_message: self.tiers.start_message,
            end_message: self.tiers.end_message,
            title: &self.tiers.title,
            content: "",
            start_date: self.tiers.start_date.as_deref(),
            end_date: self.tiers.end_date.as_deref(),
            p1: self.tiers.p1.as_deref(),
            p2: self.tiers.p2.as_deref(),
            p3: self.tiers.p3.as_deref(),
            p4: self.tiers.p4.as_deref(),
            importance: Some(self.importance.round().clamp(1.0, 100.0) as i32),
            legacy: None,
        }
    }
}

const COLUMNS: &str = "id, session_id, coverage_end, importance, tiers, created_at";

fn from_row(row: &ox_db::turso::Row) -> Result<Compartment> {
    let tiers: String = row.get(4)?;
    Ok(Compartment {
        id: row.get(0)?,
        session_id: row.get(1)?,
        coverage_end: row.get(2)?,
        importance: row.get(3)?,
        tiers: serde_json::from_str(&tiers).unwrap_or_else(|error| {
            tracing::warn!(error = %error, "malformed mc_compartments.tiers; treating as empty");
            Tiers::default()
        }),
        created_at: row.get(5)?,
    })
}

/// The derived search text: `trim(COALESCE(title,'') || ' ' || COALESCE(p1,'')
/// || … )`, exactly the expression `MIGRATION_V7` backfills. Built from the
/// in-memory tiers so the JSON stays the only source of truth; `trim_matches`
/// matches SQLite's `trim`, which strips spaces only.
fn search_text(tiers: &Tiers) -> String {
    format!(
        "{} {} {} {} {}",
        tiers.title,
        tiers.p1.as_deref().unwrap_or(""),
        tiers.p2.as_deref().unwrap_or(""),
        tiers.p3.as_deref().unwrap_or(""),
        tiers.p4.as_deref().unwrap_or(""),
    )
    .trim_matches(' ')
    .to_owned()
}

/// Load every compartment of a session, chronologically (oldest first).
pub async fn load(db: &Db, session_id: &str) -> Result<Vec<Compartment>> {
    let context = || format!("load mc_compartments for session {session_id}");
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM mc_compartments
             WHERE session_id = ?1
             ORDER BY created_at, id"
        ))
        .await
        .with_context(context)?;
    let mut rows = stmt.query([session_id]).await.with_context(context)?;
    let mut compartments = Vec::new();
    while let Some(row) = rows.next().await.with_context(context)? {
        compartments.push(from_row(&row)?);
    }
    Ok(compartments)
}

/// The highest covered ordinal across `compartments` (the request-assembly
/// floor), or `None` when nothing is covered.
pub fn coverage_end(compartments: &[Compartment]) -> Option<i64> {
    compartments
        .iter()
        .filter_map(Compartment::end_ordinal)
        .max()
}

/// A compartment about to be stored.
#[derive(Debug, Clone)]
pub struct NewCompartment<'a> {
    /// Row id (`mc_`-style opaque id).
    pub id: &'a str,
    /// Owning session.
    pub session_id: &'a str,
    /// 1..100 importance (decay rate).
    pub importance: f64,
    /// Heading + paraphrase tiers.
    pub tiers: &'a Tiers,
}

/// Append one compartment.
pub async fn insert(db: &Db, compartment: &NewCompartment<'_>) -> Result<()> {
    insert_all(db, std::slice::from_ref(compartment)).await
}

/// Append several compartments atomically, in the order given.
///
/// One transaction, because coverage is derived from these rows: a publish that
/// failed halfway would advance the floor past content that was never
/// summarized.
pub async fn insert_all(db: &Db, compartments: &[NewCompartment<'_>]) -> Result<()> {
    if compartments.is_empty() {
        return Ok(());
    }
    // Serialize (and derive `search_text`) before opening the transaction: a
    // `serde` failure must not leave a started transaction to roll back, and
    // the bind values are then borrowed straight from these owners.
    let now = ox_schema::id::now_millis();
    let tiers = compartments
        .iter()
        .map(|compartment| {
            serde_json::to_string(compartment.tiers).context("serialize compartment tiers")
        })
        .collect::<Result<Vec<_>>>()?;
    let session_id = compartments[0].session_id;
    let count = compartments.len();
    let written = async {
        let tx = db.begin().await?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO mc_compartments
                         (id, session_id, coverage_end, importance, tiers, search_text, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .await?;
            for (compartment, tiers) in compartments.iter().zip(&tiers) {
                let coverage_end = compartment.tiers.end_message.to_string();
                let search_text = search_text(compartment.tiers);
                stmt.execute((
                    compartment.id,
                    compartment.session_id,
                    coverage_end.as_str(),
                    compartment.importance,
                    tiers.as_str(),
                    search_text.as_str(),
                    now,
                ))
                .await?;
            }
        }
        tx.commit().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    written
        .with_context(|| format!("insert {count} mc_compartments rows for session {session_id}"))?;
    Ok(())
}
