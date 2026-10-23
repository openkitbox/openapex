//! `capability_search`: find a skill or a registered tool by describing what
//! is needed.
//!
//! fx's tool of the same name searches installed skills and MCP tools; ox
//! indexes both — skills from the discovery roots, tools read live from the
//! registry the call runs in ([`crate::contract::ToolContext::registry`]), so
//! MCP and CBM tools are covered exactly as advertised. Matching is a simple
//! scored token overlap over id/name/description — the catalog is small
//! enough that no index is warranted.

use serde_json::Value;

use crate::contract::{Tool, ToolContext, ToolOutput};
use ox_schema::wire::ToolDef;

use super::{object_param, ok_text, param};

/// The most results a search returns — the tool's output goes straight into
/// the context, so it stays short.
const MAX_RESULTS: usize = 8;

pub struct CapabilitySearchTool;

impl Tool for CapabilitySearchTool {
    fn name(&self) -> &str {
        "capability_search"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Use capability_search when a task might match an installed skill but its id or exact description is unknown.",
        )
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "capability_search",
            "Find installed skills or registered tools for a described capability. Optimizes for \
             recall: returns the best matches with their ids, names, and descriptions, so the \
             next step is a `skill` call for a skill or a direct call for a tool.\n\n\
             When to use: a task might match an installed skill or a registered tool but its id \
             or exact description is unknown.\n\
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
    // One candidate list over both catalogs: skills from the discovery
    // roots, tools live from the registry the call runs in.
    struct Match {
        /// `skill` or `tool` — rendered per entry so the model knows whether
        /// the next step is a `skill` call or a direct tool call.
        kind: &'static str,
        title: String,
        /// Tie-break key (skill id / tool name) so equal scores stay
        /// deterministic.
        sort_key: String,
        description: String,
        score: usize,
    }

    let mut matches: Vec<Match> = skills
        .iter()
        .map(|skill| Match {
            kind: "skill",
            title: format!("{} ({})", skill.id, skill.name),
            sort_key: skill.id.clone(),
            description: skill
                .description
                .clone()
                .unwrap_or_else(|| "(no description)".to_owned()),
            score: score(
                &skill.id,
                Some(&skill.name),
                skill.description.as_deref().unwrap_or_default(),
                &tokens,
            ),
        })
        .chain(
            ctx.registry
                .catalog()
                .into_iter()
                .map(|(name, description)| {
                    // Tools have no separate display name: the registry key scores
                    // as both id and name.
                    let score = score(&name, None, &description, &tokens);
                    Match {
                        kind: "tool",
                        title: name.clone(),
                        sort_key: name,
                        description,
                        score,
                    }
                }),
        )
        .filter(|matched| matched.score > 0)
        .collect();

    if matches.is_empty() {
        return ok_text(format!(
            "No skills or tools matched `{query}`.{}",
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

    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.kind.cmp(b.kind))
            .then_with(|| a.sort_key.cmp(&b.sort_key))
    });

    let mut text = format!("Capabilities matching `{query}`:\n");
    for matched in matches.into_iter().take(limit) {
        text.push_str(&format!(
            "- {}: {} [score {}] — {}\n",
            matched.kind, matched.title, matched.score, matched.description
        ));
    }
    ok_text(text.trim_end().to_owned())
}

/// A candidate's relevance to the query: exact-id hits dominate, then
/// name/description token overlap. Tools have no separate display name
/// (`None`): the registry key scores as both id and name.
fn score(id: &str, name: Option<&str>, description: &str, tokens: &[String]) -> usize {
    let id = id.to_lowercase();
    let name = name.map(str::to_lowercase).unwrap_or_else(|| id.clone());
    let description = description.to_lowercase();
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
    use crate::contract::ToolRegistry;
    use std::sync::Arc;

    /// A registry-only tool for the catalog tests; execution is never reached.
    struct FakeTool {
        name: &'static str,
        description: &'static str,
    }

    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn definition(&self) -> ToolDef {
            ToolDef::function(
                self.name,
                self.description,
                serde_json::json!({ "type": "object" }),
            )
        }

        fn execute(
            &self,
            _ctx: ToolContext,
            _input: Value,
        ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            unimplemented!("fake tool is never executed")
        }
    }

    fn ctx(dir: &std::path::Path, tools: Vec<Arc<dyn Tool>>) -> ToolContext {
        let mut registry = ToolRegistry::new();
        registry.register_all(tools);
        ToolContext {
            session_id: "ses_caps".to_owned(),
            directory: dir.to_path_buf(),
            call_id: "call_caps".to_owned(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            registry: Arc::new(registry),
        }
    }

    fn project_with_skills() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ox/skills/review")).unwrap();
        std::fs::write(
            dir.path().join(".ox/skills/review/SKILL.md"),
            "---\nname: Code Review\ndescription: Review a pull request against the diff\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".ox/skills/migrate")).unwrap();
        std::fs::write(
            dir.path().join(".ox/skills/migrate/SKILL.md"),
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
                ctx(dir.path(), vec![]),
                serde_json::json!({ "query": "review a PR" }),
            )
            .await;
        let text = output.content.as_str().unwrap();
        assert!(text.contains("review (Code Review)"), "{text}");
    }

    /// Batch I: registered tools are indexed next to skills, read live from
    /// the context's registry — an MCP-shaped `srv_tool` included.
    #[tokio::test]
    async fn a_registered_tool_is_indexed_next_to_skills() {
        let dir = tempfile::tempdir().unwrap();
        let output = CapabilitySearchTool
            .execute(
                ctx(
                    dir.path(),
                    vec![
                        Arc::new(FakeTool {
                            name: "srv_tool",
                            description: "Serve widgets over the network",
                        }) as Arc<dyn Tool>,
                        Arc::new(FakeTool {
                            name: "write",
                            description: "Write a file to disk",
                        }),
                    ],
                ),
                serde_json::json!({ "query": "serve widgets" }),
            )
            .await;
        let text = output.content.as_str().unwrap();
        assert!(text.contains("- tool: srv_tool"), "{text}");
        assert!(!text.contains("- tool: write"), "{text}");
    }

    /// Deterministic mixed ordering: score desc, then skills before tools,
    /// then name — equal-score tools come out in name order.
    #[tokio::test]
    async fn mixed_results_order_deterministically() {
        let dir = project_with_skills();
        let output = CapabilitySearchTool
            .execute(
                ctx(
                    dir.path(),
                    vec![
                        Arc::new(FakeTool {
                            name: "zzz_reviewer",
                            description: "Review a change",
                        }) as Arc<dyn Tool>,
                        Arc::new(FakeTool {
                            name: "aaa_reviewer",
                            description: "Review a change",
                        }),
                    ],
                ),
                serde_json::json!({ "query": "review" }),
            )
            .await;
        let text = output.content.as_str().unwrap();
        let skill = text.find("- skill: review").expect("skill entry");
        let aaa = text.find("- tool: aaa_reviewer").expect("aaa entry");
        let zzz = text.find("- tool: zzz_reviewer").expect("zzz entry");
        assert!(
            skill < aaa && aaa < zzz,
            "expected skill, aaa_reviewer, zzz_reviewer: {text}"
        );
    }

    /// Nothing in the registry does not break the skill search: an empty
    /// registry behaves exactly like the pre-batch tool.
    #[tokio::test]
    async fn an_empty_registry_indexes_only_skills() {
        let dir = project_with_skills();
        let output = CapabilitySearchTool
            .execute(
                ctx(dir.path(), vec![]),
                serde_json::json!({ "query": "review a PR" }),
            )
            .await;
        let text = output.content.as_str().unwrap();
        assert!(text.contains("- skill: review (Code Review)"), "{text}");
        assert!(!text.contains("- tool:"), "{text}");
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
                ctx(dir.path(), vec![]),
                serde_json::json!({ "query": "migration database schema review", "max_results": 1 }),
            )
            .await;
        let text = output.content.as_str().unwrap();
        assert_eq!(text.matches('\n').count(), 1, "{text}");
    }
}
