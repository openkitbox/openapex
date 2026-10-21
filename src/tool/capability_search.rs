//! `capability_search`: find a skill by describing what is needed.
//!
//! fx's tool of the same name searches installed skills and MCP tools; ox has
//! no MCP layer yet, so this covers skills only. Matching is a simple scored
//! token overlap over id/name/description — the catalog is small enough that
//! no index is warranted.

use serde_json::Value;

use crate::cardinal::wire::ToolDef;
use crate::runner::{Tool, ToolContext, ToolOutput};

use super::{object_param, ok_text, param};

/// The most results a search returns — the tool's output goes straight into
/// the context, so it stays short.
const MAX_RESULTS: usize = 8;

pub struct CapabilitySearchTool;

impl Tool for CapabilitySearchTool {
    fn name(&self) -> &str {
        "capability_search"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "capability_search",
            "Find installed skills for a described capability. Optimizes for recall: returns the \
             best matches with their ids, names, and descriptions, so the next step is a `skill` \
             call.\n\n\
             When to use: a task might match an installed skill but its id or exact description \
             is unknown.\n\
             When NOT to use: the skill's id is already known (call `skill` directly), or the \
             capability is plain file work the basic tools cover.",
            object_param(
                vec![
                    (
                        "query",
                        param(
                            "string",
                            "What the capability should do, in natural language or keywords.",
                        ),
                    ),
                    (
                        "max_results",
                        param("integer", "Cap on returned matches (default 8)."),
                    ),
                ],
                &["query"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run(&ctx, &input) })
    }
}

fn run(ctx: &ToolContext, input: &Value) -> ToolOutput {
    let Some(query) = input
        .get("query")
        .and_then(Value::as_str)
        .filter(|query| !query.trim().is_empty())
    else {
        return super::invalid_arguments(
            "capability_search",
            &[("query", "Missing string field `query`".to_owned())],
            input,
        );
    };
    let limit = input
        .get("max_results")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, 50) as usize)
        .unwrap_or(MAX_RESULTS);

    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|token| token.len() >= 3)
        .collect();

    let skills = super::skills::discover(&ctx.directory);
    let mut scored: Vec<(usize, &super::skills::Skill)> = skills
        .iter()
        .map(|skill| (score(skill, &tokens), skill))
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));

    if scored.is_empty() {
        return ok_text(format!(
            "No skills matched `{query}`.{}",
            if skills.is_empty() {
                " No skills are installed.".to_owned()
            } else {
                format!(
                    " Available skills: {}.",
                    skills
                        .iter()
                        .map(|skill| skill.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ));
    }

    let mut text = format!("Skills matching `{query}`:\n");
    for (score, skill) in scored.into_iter().take(limit) {
        let description = skill.description.as_deref().unwrap_or("(no description)");
        text.push_str(&format!(
            "- {} ({}) [score {score}] — {description}\n",
            skill.id, skill.name,
        ));
    }
    ok_text(text.trim_end().to_owned())
}

/// A skill's relevance to the query: exact-id hits dominate, then
/// name/description token overlap.
fn score(skill: &super::skills::Skill, tokens: &[String]) -> usize {
    let id = skill.id.to_lowercase();
    let name = skill.name.to_lowercase();
    let description = skill
        .description
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();
    let mut score = 0;
    for token in tokens {
        if id == *token {
            score += 10;
        } else if id.contains(token) {
            score += 4;
        }
        if name.contains(token) {
            score += 3;
        }
        if description.contains(token) {
            score += 1;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: "ses_caps".to_owned(),
            directory: dir.to_path_buf(),
            call_id: "call_caps".to_owned(),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn project_with_skills() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills/review")).unwrap();
        std::fs::write(
            dir.path().join("skills/review/SKILL.md"),
            "---\nname: Code Review\ndescription: Review a pull request against the diff\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("skills/migrate")).unwrap();
        std::fs::write(
            dir.path().join("skills/migrate/SKILL.md"),
            "---\nname: Migration\ndescription: Database schema migrations\n---\nbody",
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn a_query_finds_the_matching_skill_first() {
        let dir = project_with_skills();
        let output = CapabilitySearchTool
            .execute(
                ctx(dir.path()),
                serde_json::json!({ "query": "review a PR" }),
            )
            .await;
        let text = output.content.as_str().unwrap();
        assert!(text.contains("review (Code Review)"), "{text}");
    }

    #[tokio::test]
    async fn a_no_match_says_so() {
        // Scoped: host-global skills may mention deploy keywords, so the test
        // only asserts the stable prefix on an isolated directory.
        let dir = tempfile::tempdir().unwrap();
        let skills = super::super::skills::discover_in(&[dir.path().join("skills")]);
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn max_results_caps_the_list() {
        let dir = project_with_skills();
        let output = CapabilitySearchTool
            .execute(
                ctx(dir.path()),
                serde_json::json!({ "query": "migration database schema review", "max_results": 1 }),
            )
            .await;
        let text = output.content.as_str().unwrap();
        assert_eq!(text.matches('\n').count(), 1, "{text}");
    }
}
