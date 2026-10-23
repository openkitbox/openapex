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
