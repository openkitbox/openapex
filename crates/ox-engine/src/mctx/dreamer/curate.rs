use anyhow::Result;

use crate::mctx::memory::{self, CATEGORIES};
use ox_db::Db;

use super::manifest::{
    ApplyReport, archive_regex, attr_of, complete_manifest_body, directive_shaped_project_rule,
    keep_batch_id, merge_regex,
};

/// The curate task's prefixed completion prompt, adapting
/// `dreamer/task-prompts.ts::CURATE_SYSTEM_PROMPT` to a manifest instead of
/// `ctx_memory` tool calls.
pub(super) const CURATE_SYSTEM_PROMPT: &str = "You are a memory-pool curator for the magic-context system. You keep a project's cross-session memory store lean and well-formed.

## Rules
1. **Assume the pool is accurate.** A separate verify task checks memories against code. You handle QUALITY only — duplicates, wording, low-value entries — never correctness, and you do NOT read the codebase.
2. **Never mint new facts.** Merging and tightening only, never adding knowledge.
3. **Present-tense operational language.** \"X uses Y\", not \"X was changed to use Y\".
4. **One fact per memory.**
5. **Be conservative with archives.** Archive a memory only when another memory in the SAME category already preserves its information. A bare \"redundant\" verdict loses knowledge and is refused.
6. Every id in one merge must be in the category shown; cross-category merges are refused.

Output ONE XML manifest at the very end and NOTHING else — no narration, no per-memory commentary, no reasoning:
<curate>
<merge ids=\"1,2\" category=\"ARCHITECTURE\">consolidated canonical text</merge>
<archive id=\"3\" reason=\"fully restated by #1\"/>
</curate>

Rules:
- Each id you mention must appear exactly once, and must come from the list below.
- <merge> needs at least two ids and the consolidated text; <archive> needs a reason.
- A run that changes nothing is a valid answer: emit an empty <curate></curate>.";

// ---------------------------------------------------------------------------
// curate
// ---------------------------------------------------------------------------

/// The category one curate run covers, with the memories in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CurateScope {
    pub(super) category: &'static str,
    pub(super) memories: Vec<memory::StoredMemory>,
}

/// Pick the category for this run: scan the five canonical categories from the
/// persisted cursor, wrapping, and take the first one holding at least two
/// memories. Returns `None` when no category has a pair to work with — the
/// cheap gate that keeps an idle project free of model calls.
pub(super) fn curate_scope(cursor: u32, memories: &[memory::StoredMemory]) -> Option<CurateScope> {
    let len = CATEGORIES.len() as u32;
    let start = cursor % len;
    for offset in 0..len {
        let category = CATEGORIES[((start + offset) % len) as usize];
        let scoped: Vec<memory::StoredMemory> = memories
            .iter()
            .filter(|memory| memory.category == category)
            .cloned()
            .collect();
        if scoped.len() >= 2 {
            return Some(CurateScope {
                category,
                memories: scoped,
            });
        }
    }
    None
}

/// The `curate` user prompt: the scoped category's snapshot. The model mutates
/// nothing directly; it answers with a manifest the host applies.
pub(super) fn render_curate_prompt(
    project: &str,
    category: &str,
    memories: &[memory::StoredMemory],
) -> String {
    let mut parts = vec![format!(
        "## Project: {project}\n\nThis run covers the `{category}` category only. The other categories run in later windows.\n\nThe memories below are assumed ACCURATE (a separate verify task keeps them true). Remove duplicates, tighten wording, consolidate redundancy, and archive only what another memory in this category already restates. The memories:\n"
    )];
    for memory in memories {
        parts.push(format!("#{}: {}", memory.id, memory.content));
    }
    parts.join("\n")
}

/// The parsed `<curate>` manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CurateManifest {
    merges: Vec<CurateMerge>,
    archives: Vec<CurateArchive>,
}

/// One `<merge>`: the ids to collapse, the consolidated text, and the category
/// the model claims they share.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CurateMerge {
    ids: Vec<String>,
    category: Option<String>,
    content: String,
}

/// One `<archive>`: the id and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CurateArchive {
    id: String,
    reason: String,
}

/// Parse a `<curate>` manifest with the same fail-closed rules as `verify`.
pub(super) fn parse_curate_manifest(
    text: &str,
    scope: &[memory::StoredMemory],
) -> Result<CurateManifest, String> {
    let body = complete_manifest_body(text, "curate")?;
    let mut manifest = CurateManifest::default();
    for captures in merge_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let raw_ids = attr_of(attrs, "ids").ok_or("curate manifest <merge> is missing ids")?;
        let mut ids: Vec<String> = Vec::new();
        for id in raw_ids.split(',') {
            let id = id.trim().to_owned();
            if id.is_empty() {
                continue;
            }
            // A duplicated id inside one merge is the reference's
            // `ctx_memory` dedupe: nothing may be merged into itself.
            if ids.contains(&id) {
                return Err(format!("curate manifest <merge> lists id {id} twice"));
            }
            ids.push(id);
        }
        if ids.len() < 2 {
            return Err("curate manifest <merge> needs at least two ids".to_owned());
        }
        manifest.merges.push(CurateMerge {
            ids,
            category: attr_of(attrs, "category"),
            content: captures.get(2).map_or("", |m| m.as_str()).trim().to_owned(),
        });
    }
    for captures in archive_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("curate manifest <archive> is missing an id")?;
        manifest.archives.push(CurateArchive {
            id,
            reason: attr_of(attrs, "reason").unwrap_or_default(),
        });
    }

    let scope_ids: Vec<&str> = scope.iter().map(|memory| memory.id.as_str()).collect();
    // A merge is all-or-nothing: one id outside the scope drops the whole
    // merge (the reference refuses the whole mutation, and applying half a
    // merge would archive nothing while rewriting the survivor from a group
    // that never existed). Archives are independent single-id verdicts.
    let mut known: Vec<String> = Vec::new();
    let mut unknown = 0usize;
    let mut merges: Vec<CurateMerge> = Vec::new();
    for merge in std::mem::take(&mut manifest.merges) {
        let outside = merge
            .ids
            .iter()
            .filter(|id| !scope_ids.contains(&id.as_str()))
            .count();
        if outside > 0 {
            unknown += outside;
            continue;
        }
        if let Some(duplicate) = merge.ids.iter().find(|id| known.contains(id)) {
            return Err(format!(
                "curate manifest lists id {duplicate} in more than one element"
            ));
        }
        known.extend(merge.ids.iter().cloned());
        merges.push(merge);
    }
    manifest.merges = merges;
    let mut error = None;
    manifest.archives.retain(|archive| {
        keep_batch_id(
            &archive.id,
            &scope_ids,
            &mut known,
            &mut unknown,
            &mut error,
        )
    });
    if let Some(error) = error {
        return Err(error);
    }
    if unknown > 0 {
        tracing::warn!(
            unknown,
            "curate manifest named memories outside the scoped category; dropped"
        );
    }
    Ok(manifest)
}

/// Apply a parsed `<curate>` manifest. Only one thing can be "wrong" enough to
/// refuse here: a merge or archive whose ids (or whose declared category) fall
/// outside the run's category. The reference refuses that in
/// `getCurateCategoryScopeRefusal` before any write; ox refuses it here, per
/// verdict, so one bad entry cannot discard the good ones.
pub(super) async fn apply_curate_manifest(
    db: &Db,
    project: &str,
    scope: &CurateScope,
    manifest: &CurateManifest,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    for merge in &manifest.merges {
        if merge
            .category
            .as_deref()
            .is_some_and(|c| c != scope.category)
        {
            tracing::warn!(
                scope = scope.category,
                requested = merge.category.as_deref().unwrap_or(""),
                "curate cross-category merge refusal"
            );
            report.refused += 1;
            continue;
        }
        let in_scope = merge
            .ids
            .iter()
            .all(|id| scope.memories.iter().any(|memory| &memory.id == id));
        if !in_scope {
            tracing::warn!(
                scope = scope.category,
                "curate merge outside the scoped category refusal"
            );
            report.refused += 1;
            continue;
        }
        let content = merge.content.trim();
        if content.is_empty() || content.chars().count() > 20_000 {
            tracing::warn!("curate merge with an unsafe content length refusal");
            report.refused += 1;
            continue;
        }
        // A merge rewrites the survivor's text, so it carries the same risk as
        // an update: consolidating a directive-shaped rule into prose would
        // launder a directive into a description.
        if merge.ids.iter().any(|id| {
            scope.memories.iter().any(|memory| {
                &memory.id == id && directive_shaped_project_rule(&memory.category, &memory.content)
            })
        }) {
            tracing::warn!(
                scope = scope.category,
                reason = "directive-shaped-project-rule",
                "curate safety refusal"
            );
            report.refused += 1;
            continue;
        }
        if memory::merge_memories(db, project, &merge.ids, content, None)
            .await?
            .is_some()
        {
            report.merged += 1;
        }
    }
    for archive in &manifest.archives {
        let Some(memory) = scope.memories.iter().find(|memory| memory.id == archive.id) else {
            // An archive outside the scope is the same refusal as a merge
            // outside it; the id was already dropped by the parser when it was
            // unknown, so reaching here means the caller passed a wider scope.
            tracing::warn!(
                scope = scope.category,
                "curate archive outside the scoped category refusal"
            );
            report.refused += 1;
            continue;
        };
        if directive_shaped_project_rule(&memory.category, &memory.content) {
            tracing::warn!(
                memory_id = %archive.id,
                reason = "directive-shaped-project-rule",
                "curate safety refusal"
            );
            report.refused += 1;
            continue;
        }
        let reason = if archive.reason.trim().is_empty() {
            "curate: redundant"
        } else {
            archive.reason.trim()
        };
        if !memory::archive_memories(db, project, std::slice::from_ref(&archive.id), Some(reason))
            .await?
            .is_empty()
        {
            report.archived += 1;
        }
    }
    Ok(report)
}
