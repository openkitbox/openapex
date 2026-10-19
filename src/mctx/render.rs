//! Deterministic project-memory pinboard render (the system-prompt tail).
//!
//! The pinboard is the read surface of `ctx_memory`: every active memory of
//! the project, grouped under its category, as `#id: content` lines. It is
//! deterministic by construction — a fixed category order and ascending ids —
//! so the rendered prompt is byte-stable between memory writes: the
//! provider's cached prefix only busts when a memory actually changes.
//!
//! An empty store renders an empty string (the caller appends nothing, so a
//! project with no memories gets a prompt byte-identical to the
//! integration-off one).
//!
//! Task 5 replaces this seam with the full decay/compartment render
//! (`m[0]`/`m[1]`); the pinboard's shape and determinism are its contract.

use anyhow::Result;

use crate::db::Db;
use crate::mctx::memory::{CATEGORIES, active_memories};

/// One `#id: …` line's worth of content: runs of whitespace collapse to a
/// single space. The pinboard's shape is a contract (`#id: content` per
/// line), while `ctx_memory get` still shows the stored text verbatim.
fn one_line(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render the project-memory pinboard for `project`, or an empty string when
/// the project has no active memories.
pub(crate) async fn render_memory_pinboard(db: &Db, project: &str) -> Result<String> {
    let rows = active_memories(db, project).await?;
    if rows.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::from("<project-memory>\n");
    for category in CATEGORIES {
        let mut rows_in = rows.iter().filter(|row| row.category == category);
        let Some(first) = rows_in.next() else {
            continue;
        };
        out.push_str(&format!("<{category}>\n"));
        out.push_str(&format!("#{}: {}\n", first.id, one_line(&first.content)));
        for row in rows_in {
            out.push_str(&format!("#{}: {}\n", row.id, one_line(&row.content)));
        }
        out.push_str(&format!("</{category}>\n"));
    }
    out.push_str("</project-memory>");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mctx::memory::{archive_memories, write_memory};

    const PROJECT: &str = "/proj/a";

    async fn db() -> Db {
        Db::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn empty_for_a_project_without_memories() {
        let db = db().await;
        assert_eq!(render_memory_pinboard(&db, PROJECT).await.unwrap(), "");
        // A memory of another project does not leak in.
        write_memory(&db, "/proj/b", "elsewhere", "NAMING")
            .await
            .unwrap();
        assert_eq!(render_memory_pinboard(&db, PROJECT).await.unwrap(), "");
    }

    #[tokio::test]
    async fn snapshot_groups_by_fixed_category_order_with_ascending_ids() {
        let db = db().await;
        // Written scrambled; rendered in the fixed category order, ids
        // ascending inside a section.
        write_memory(&db, PROJECT, "n1", "NAMING").await.unwrap();
        write_memory(&db, PROJECT, "r1", "PROJECT_RULES")
            .await
            .unwrap();
        write_memory(&db, PROJECT, "a1", "ARCHITECTURE")
            .await
            .unwrap();
        write_memory(&db, PROJECT, "n2", "NAMING").await.unwrap();

        let rendered = render_memory_pinboard(&db, PROJECT).await.unwrap();
        assert_eq!(
            rendered,
            "<project-memory>\n\
             <PROJECT_RULES>\n\
             #2: r1\n\
             </PROJECT_RULES>\n\
             <ARCHITECTURE>\n\
             #3: a1\n\
             </ARCHITECTURE>\n\
             <NAMING>\n\
             #1: n1\n\
             #4: n2\n\
             </NAMING>\n\
             </project-memory>"
        );
        // Empty categories are skipped entirely.
        assert!(!rendered.contains("CONSTRAINTS"));
        assert!(!rendered.contains("CONFIG_VALUES"));
    }

    #[tokio::test]
    async fn a_multi_line_fact_still_renders_on_one_line() {
        let db = db().await;
        write_memory(
            &db,
            PROJECT,
            "first line\n\n  second   line  ",
            "ARCHITECTURE",
        )
        .await
        .unwrap();
        assert_eq!(
            render_memory_pinboard(&db, PROJECT).await.unwrap(),
            "<project-memory>\n<ARCHITECTURE>\n#1: first line second line\n</ARCHITECTURE>\n</project-memory>"
        );
    }

    #[tokio::test]
    async fn archived_memories_drop_out_and_the_rest_stay_stable() {
        let db = db().await;
        write_memory(&db, PROJECT, "keep", "ARCHITECTURE")
            .await
            .unwrap();
        write_memory(&db, PROJECT, "drop", "ARCHITECTURE")
            .await
            .unwrap();
        let before = render_memory_pinboard(&db, PROJECT).await.unwrap();

        archive_memories(&db, PROJECT, &["2".to_owned()], Some("wrong"))
            .await
            .unwrap();
        let after = render_memory_pinboard(&db, PROJECT).await.unwrap();
        assert_eq!(
            after,
            "<project-memory>\n<ARCHITECTURE>\n#1: keep\n</ARCHITECTURE>\n</project-memory>"
        );
        assert_ne!(before, after, "an archive must change the rendered bytes");

        // Deterministic: two reads of the same state are byte-equal.
        assert_eq!(after, render_memory_pinboard(&db, PROJECT).await.unwrap());
    }
}
