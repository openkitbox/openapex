//! `skill`: load one discovered skill's model-facing envelope.
//!
//! The model learns which skills exist from the system prompt (the
//! `<available_skills>` block, rendered from [`crate::skills::list`]) and
//! loads one here, exactly the way upstream's `skill` tool does. The result
//! text is [`crate::skills::prepare`]'s `<skill_content>` envelope — body, base
//! directory and sampled companion files. There is exactly one format: this
//! tool formats nothing itself, so it can never drift from the prompt path.

use serde_json::Value;

use crate::contract::{Tool, ToolContext, ToolOutput};
use ox_schema::wire::ToolDef;

use super::{fail, object_param, ok_text, param};

pub struct SkillTool;

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "skill",
            "Load a skill's instructions and resource files into the conversation. Available \
             skills are listed in the system prompt's <available_skills> block with their \
             descriptions.\n\n\
             When to use: the task matches a listed skill's description — the skill carries \
             instructions and workflows for that kind of work. The id must match an available \
             skill or one the user explicitly referenced.\n\
             When NOT to use: a skill the conversation already shows in a <skill_content> block \
             (it is loaded — do not load it twice), or no listed skill matches.",
            object_param(
                vec![(
                    "id",
                    param(
                        "string",
                        "The skill id from <available_skills> (not its display name).",
                    ),
                )],
                &["id"],
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

/// The `id` argument, falling back to the legacy `name` key some sessions and
/// clients still send. `id` wins when both are present, but a blank `id`
/// (whitespace only) counts as absent, so the legacy `name` is used instead.
fn requested_id(input: &Value) -> Option<&str> {
    ["id", "name"].into_iter().find_map(|key| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
    })
}

fn run(ctx: &ToolContext, input: &Value) -> ToolOutput {
    let Some(id) = requested_id(input) else {
        return super::invalid_arguments(
            "skill",
            &[("id", "Missing string field `id`".to_owned())],
            input,
        );
    };
    let Some(prepared) = super::skills::prepare(&ctx.directory, id) else {
        let available: Vec<String> = super::skills::discover(&ctx.directory)
            .into_iter()
            .map(|skill| skill.id)
            .collect();
        return fail(format!(
            "Unknown skill `{id}`. Available skills: {}",
            if available.is_empty() {
                "(none)".to_owned()
            } else {
                available.join(", ")
            }
        ));
    };
    ok_text(prepared.text).with_metadata(serde_json::json!({
        "id": prepared.id,
        "name": prepared.name,
        "directory": prepared.directory.display().to_string(),
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

    /// A `review` directory skill with a frontmatter display name.
    fn write_review(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join(".ox/skills/review")).unwrap();
        std::fs::write(
            dir.join(".ox/skills/review/SKILL.md"),
            "---\nname: Review\ndescription: code review\n---\nLook at the diff.\n",
        )
        .unwrap();
    }

    #[test]
    fn the_definition_declares_one_required_id() {
        let definition = SkillTool.definition();
        let parameters = &definition.function.parameters;
        assert_eq!(parameters["properties"]["id"]["type"], "string");
        assert!(
            parameters["properties"]["id"]["description"]
                .as_str()
                .unwrap()
                .contains("<available_skills>")
        );
        assert_eq!(parameters["required"], serde_json::json!(["id"]));
        assert!(parameters["properties"].get("name").is_none());
    }

    #[tokio::test]
    async fn an_id_argument_loads_the_skill() {
        let dir = tempfile::tempdir().unwrap();
        write_review(dir.path());

        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "id": "review" }))
            .await;
        assert_eq!(output.error, None);
        let text = output.content.as_str().unwrap();
        assert!(
            text.starts_with("<skill_content name=\"Review\">"),
            "{text}"
        );
        assert!(text.contains("Look at the diff."), "{text}");
    }

    #[tokio::test]
    async fn a_legacy_name_argument_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        write_review(dir.path());

        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "name": "review" }))
            .await;
        assert_eq!(output.error, None);
        assert!(
            output
                .content
                .as_str()
                .unwrap()
                .starts_with("<skill_content name=\"Review\">")
        );
    }

    #[tokio::test]
    async fn an_id_wins_over_a_legacy_name_argument() {
        let dir = tempfile::tempdir().unwrap();
        write_review(dir.path());

        let output = SkillTool
            .execute(
                ctx(dir.path()),
                serde_json::json!({ "id": "review", "name": "nope" }),
            )
            .await;
        assert_eq!(output.error, None);
        assert!(
            output
                .content
                .as_str()
                .unwrap()
                .contains("Look at the diff.")
        );
    }

    #[tokio::test]
    async fn a_missing_id_is_an_argument_error() {
        let dir = tempfile::tempdir().unwrap();

        for input in [serde_json::json!({}), serde_json::json!({ "id": "  " })] {
            let output = SkillTool.execute(ctx(dir.path()), input.clone()).await;
            let error = output.error.as_deref().unwrap();
            assert!(error.contains("Missing string field `id`"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_blank_legacy_name_is_an_argument_error() {
        let dir = tempfile::tempdir().unwrap();

        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "name": "  " }))
            .await;
        let error = output.error.as_deref().unwrap();
        assert!(error.contains("Missing string field `id`"), "{error}");
    }

    #[tokio::test]
    async fn a_blank_id_falls_back_to_the_legacy_name() {
        let dir = tempfile::tempdir().unwrap();
        write_review(dir.path());

        let output = SkillTool
            .execute(
                ctx(dir.path()),
                serde_json::json!({ "id": "  ", "name": "review" }),
            )
            .await;
        assert_eq!(output.error, None);
        assert!(
            output
                .content
                .as_str()
                .unwrap()
                .starts_with("<skill_content name=\"Review\">")
        );
    }

    #[tokio::test]
    async fn the_output_is_the_prepare_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".ox/skills/review");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Review\n---\nLook at the diff.\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("scripts/b.sh"), "echo").unwrap();

        let prepared = super::super::skills::prepare(dir.path(), "review").unwrap();
        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "id": "review" }))
            .await;
        assert_eq!(output.error, None);
        let text = output.content.as_str().unwrap();
        assert_eq!(text, prepared.text);
        assert!(text.contains("Base directory for this skill:"), "{text}");
        assert!(
            text.contains(&format!(
                "<file>{}</file>",
                skill_dir.join("scripts/b.sh").display()
            )),
            "{text}"
        );
    }

    #[tokio::test]
    async fn the_metadata_carries_the_id_name_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_review(dir.path());

        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "id": "review" }))
            .await;
        let metadata = output.metadata.expect("metadata");
        assert_eq!(metadata["id"], "review");
        assert_eq!(metadata["name"], "Review");
        assert_eq!(
            metadata["directory"],
            dir.path().join(".ox/skills/review").display().to_string()
        );
        assert!(metadata.get("path").is_none());
    }

    #[tokio::test]
    async fn an_unknown_skill_lists_the_available_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ox/skills/review")).unwrap();
        std::fs::write(dir.path().join(".ox/skills/review/SKILL.md"), "body").unwrap();

        let output = SkillTool
            .execute(ctx(dir.path()), serde_json::json!({ "id": "nope" }))
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
            .execute(ctx(dir.path()), serde_json::json!({ "id": "anything" }))
            .await;
        let error = output.error.as_deref().unwrap();
        assert!(
            error.starts_with("Unknown skill `anything`. Available skills:"),
            "{error}"
        );
    }
}
