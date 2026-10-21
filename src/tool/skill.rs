//! `skill`: load one discovered skill's body into the conversation.
//!
//! The model learns which skills exist from the system prompt (the
//! `<available_skills>` block, rendered from [`crate::tool::skills::discover`])
//! and loads one here, exactly the way upstream's `skill` tool does — the
//! body comes back as tool result text wrapped in `<skill_content>`.

use serde_json::Value;

use crate::cardinal::wire::ToolDef;
use crate::runner::{Tool, ToolContext, ToolOutput};

use super::{fail, object_param, ok_text, param};

pub struct SkillTool;

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "skill",
            "Load a skill's instructions into the conversation. Available skills are listed in \
             the system prompt's <available_skills> block with their descriptions.\n\n\
             When to use: the task matches a listed skill's description — the skill carries \
             instructions and workflows for that kind of work.\n\
             When NOT to use: a skill the conversation already shows in a <skill_content> block \
             (it is loaded — do not load it twice), or no listed skill matches.",
            object_param(
                vec![(
                    "name",
                    param(
                        "string",
                        "The skill's id from <available_skills> (not its display name).",
                    ),
                )],
                &["name"],
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
    let Some(name) = input
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
    else {
        return super::invalid_arguments(
            "skill",
            &[("name", "Missing string field `name`".to_owned())],
            input,
        );
    };
    let skills = super::skills::discover(&ctx.directory);
    let skills: Vec<&super::skills::Skill> = skills.iter().collect();
    let Some(skill) = skills.iter().find(|skill| skill.id == name) else {
        let available: Vec<&str> = skills.iter().map(|skill| skill.id.as_str()).collect();
        return fail(format!(
            "Unknown skill `{name}`. Available skills: {}",
            if available.is_empty() {
                "(none)".to_owned()
            } else {
                available.join(", ")
            }
        ));
    };
    ok_text(format!(
        "<skill_content id=\"{}\" name=\"{}\">\n{}\n</skill_content>",
        skill.id, skill.name, skill.content
    ))
    .with_metadata(serde_json::json!({
        "id": skill.id,
        "name": skill.name,
        "path": skill.path,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: "ses_skill".to_owned(),
            directory: dir.to_path_buf(),
            call_id: "call_skill".to_owned(),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn a_known_skill_loads_its_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills/review")).unwrap();
        std::fs::write(
            dir.path().join("skills/review/SKILL.md"),
            "---\nname: Review\ndescription: code review\n---\nLook at the diff.\n",
        )
        .unwrap();

        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "name": "review" }))
            .await;
        assert_eq!(output.error, None);
        let text = output.content.as_str().unwrap();
        assert!(
            text.contains("<skill_content id=\"review\" name=\"Review\">"),
            "{text}"
        );
        assert!(text.contains("Look at the diff."), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_skill_lists_the_available_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills/review")).unwrap();
        std::fs::write(dir.path().join("skills/review/SKILL.md"), "body").unwrap();

        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "name": "nope" }))
            .await;
        assert!(output.error.is_some());
        assert!(
            output
                .error
                .as_deref()
                .unwrap()
                .contains("Unknown skill `nope`")
        );
        assert!(output.error.as_deref().unwrap().contains("review"));
    }

    #[tokio::test]
    async fn an_empty_root_discovers_nothing() {
        // Scoped to the tempdir: the host's global skill roots would leak in.
        let dir = tempfile::tempdir().unwrap();
        let skills = super::super::skills::discover_in(&[dir.path().join("skills")]);
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn an_unknown_skill_on_a_host_with_no_skills_says_none() {
        // The full tool path with a directory whose skill roots are empty —
        // on hosts with global skills installed the message lists them
        // instead of `(none)`, so only the stable prefix is asserted.
        let dir = tempfile::tempdir().unwrap();
        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "name": "anything" }))
            .await;
        let error = output.error.as_deref().unwrap();
        assert!(
            error.starts_with("Unknown skill `anything`. Available skills:"),
            "{error}"
        );
    }
}
