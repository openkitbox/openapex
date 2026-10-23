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

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Load a skill from the `<available_skills>` listing with the skill tool before relying on its workflow.",
        )
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
    let Some(prepared) = super::skills::prepare(&ctx.directory, id, &ox_core::config::user_home())
    else {
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
