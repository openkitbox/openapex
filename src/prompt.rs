//! Minimal system prompt (plan §6).
//!
//! Upstream assembles a large instruction stack (instructions state machine,
//! agent prompt, skills, snapshots). ox's runner (task 6) needs a well-formed
//! starting point: an environment block plus the project's `AGENTS.md` when
//! one exists at the working directory. Later tasks extend this template.

use std::path::Path;

/// Build the system prompt for a step: an environment block (working
/// directory, platform, today's UTC date) plus `AGENTS.md` contents read
/// from `directory` when present (read fresh each step — small files only).
///
/// [`build_with_mcp`] without MCP instructions.
pub fn build(directory: &Path) -> String {
    build_with_mcp(directory, None)
}

/// [`build`] plus the MCP instruction block (upstream
/// `mcp/instructions.ts`). `None` preserves the pre-MCP prompt byte for
/// byte, which is what the zero-server configuration must keep doing.
pub fn build_with_mcp(directory: &Path, mcp_instructions: Option<&str>) -> String {
    let mut prompt = format!(
        "You are a coding agent running in ox, an opencode-compatible server.\n\n\
         <env>\n\
         Working directory: {}\n\
         Platform: {}\n\
         Today's date: {}\n\
         </env>",
        directory.display(),
        std::env::consts::OS,
        today_iso(),
    );
    if let Ok(agents) = std::fs::read_to_string(directory.join("AGENTS.md")) {
        prompt.push_str("\n\n<project_instructions>\n");
        prompt.push_str(agents.trim_end());
        prompt.push_str("\n</project_instructions>");
        // No AGENTS.md (or unreadable): the env block alone is the prompt.
    }
    // The available-skills block (upstream `skill/instructions.ts`): the model
    // sees id/name/description and loads the body with the `skill` tool. The
    // block is omitted when nothing is discovered so the prompt stays small.
    let skills = crate::tool::skills::discover(directory);
    if !skills.is_empty() {
        prompt.push_str(
            "\n\nSkills provide specialized instructions and workflows for specific tasks.\n\
             Use the skill tool to load a skill when a task matches its description.\n\
             <available_skills>\n",
        );
        for skill in &skills {
            prompt.push_str("  <skill>\n");
            prompt.push_str(&format!("    <id>{}</id>\n", escape(&skill.id)));
            prompt.push_str(&format!("    <name>{}</name>\n", escape(&skill.name)));
            if let Some(description) = &skill.description {
                prompt.push_str(&format!(
                    "    <description>{}</description>\n",
                    escape(description)
                ));
            }
            prompt.push_str("  </skill>\n");
        }
        prompt.push_str("</available_skills>");
    }
    if let Some(block) = mcp_instructions {
        prompt.push_str("\n\n");
        prompt.push_str(block);
    }
    prompt
}

/// The skills block embeds user-controlled text in an XML-ish envelope; a raw
/// `<` would let a skill description break the block out early.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Today's UTC date as `YYYY-MM-DD` (civil-from-days; no chrono dependency).
fn today_iso() -> String {
    days_to_iso(crate::id::now_millis().div_euclid(86_400_000))
}

/// Civil date from days since the Unix epoch (Howard Hinnant's algorithm).
pub(crate) fn days_to_iso(days: i64) -> String {
    // Howard Hinnant's civil_from_days algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Static system layer — verbatim port of upstream
/// `packages/core/src/session/runner/prompt/system.txt`, with the identity
/// line's `OpenCode` → ox. `${OPENCODE_TOOL_GUIDANCE}` is substituted at
/// build time from the registered tool set (upstream
/// `session/system-prompt.ts:render`).
const HARNESS_TEMPLATE: &str = "You are an AI agent running in ox, an opencode-compatible server. Help the user accomplish their goals using the tools you have available.\n\n# Harness\n- Responses are rendered as GitHub-flavored Markdown.\n- `<system-reminder>` blocks are harness instructions, not user-authored content. Read and follow them.\n- Prefer parallelizing independent tool calls.\n${OPENCODE_TOOL_GUIDANCE}\n\n# Communication\n- Use clear file paths when referring to files.\n- Keep responses clear and concise, and avoid unnecessary technical jargon.\n\n# Working in codebases\n- Keep changes consistent with the structure, naming, style, and patterns of the surrounding code.\n- Treat unfamiliar files or changes as potential user work and investigate before deleting or overwriting them.\n";

/// Build the static system layer from the registered tool names.
pub fn system_static(tool_names: &[&str]) -> String {
    HARNESS_TEMPLATE.replace("${OPENCODE_TOOL_GUIDANCE}", &tool_guidance(tool_names))
}

/// Upstream `session/system-prompt.ts:11-32`, verbatim.
fn tool_guidance(tool_names: &[&str]) -> String {
    let mut instructions: Vec<&str> = Vec::new();
    if tool_names.contains(&"shell") {
        instructions.push("- Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need.");
        instructions.push("- Do not chain shell commands with separators like `echo \"====\";` or `printf '---'`; the output becomes noisy in a way that makes the user's side of the conversation worse.");
    }
    if tool_names.contains(&"write") {
        instructions.push("- Use the write tool to create files or completely replace their content. Prefer using the edit tool for targeted changes.");
    }
    if tool_names.contains(&"edit") {
        instructions.push("- Use the edit tool for targeted changes to existing text files. It replaces the exact text in `oldString` with `newString`, and the values must differ. By default, `oldString` must occur exactly once. If it occurs multiple times, include more surrounding context to make it unique or set `replaceAll` to true to replace every occurrence.");
    }
    instructions.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_iso_matches_known_epoch_anchors() {
        // Day 0 = 1970-01-01; leap-year and century boundaries stay exact.
        assert_eq!(days_to_iso(0), "1970-01-01");
        assert_eq!(days_to_iso(59), "1970-03-01");
        assert_eq!(days_to_iso(19_723), "2024-01-01");
    }

    #[test]
    fn build_includes_env_block() {
        let dir = std::env::temp_dir();
        let prompt = build(&dir);
        assert!(prompt.contains("Working directory: "));
        assert!(prompt.contains("Today's date: "));
    }

    #[test]
    fn mcp_instructions_are_appended_when_present() {
        let dir = std::env::temp_dir();
        let with = build_with_mcp(&dir, Some("<mcp_instructions>\n</mcp_instructions>"));
        assert!(with.contains("<mcp_instructions>"));
        assert!(!build(&dir).contains("<mcp_instructions>"));
    }

    #[test]
    fn build_does_not_carry_the_memory_pinboard() {
        // The pinboard lives in the magic-context head (m[0]), not in the
        // system prompt: the system prompt must stay byte-identical whether
        // the integration is on or off.
        let dir = std::env::temp_dir();
        assert!(!build(&dir).contains("<project-memory>"));
    }

    #[test]
    fn static_layer_carries_the_harness_section_verbatim() {
        let s = system_static(&["shell", "write", "edit"]);
        assert!(s.starts_with(
            "You are an AI agent running in ox, an opencode-compatible server. Help the user accomplish their goals using the tools you have available."
        ));
        assert!(s.contains("# Harness"));
        assert!(s.contains("- Responses are rendered as GitHub-flavored Markdown."));
        assert!(s.contains("`<system-reminder>` blocks are harness instructions, not user-authored content. Read and follow them."));
        assert!(s.contains("- Prefer parallelizing independent tool calls."));
        assert!(s.contains("# Communication"));
        assert!(s.contains("# Working in codebases"));
        assert!(s.contains("- Treat unfamiliar files or changes as potential user work"));
        assert!(
            !s.contains("${OPENCODE_TOOL_GUIDANCE}"),
            "placeholder must be substituted"
        );
    }

    #[test]
    fn tool_guidance_follows_the_registered_tools() {
        let shell_only = system_static(&["shell"]);
        assert!(shell_only.contains("- Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need."));
        assert!(
            shell_only
                .contains("Do not chain shell commands with separators like `echo \"====\";`")
        );
        assert!(!shell_only.contains("Use the edit tool for targeted changes"));
        assert!(!shell_only.contains("Use the write tool to create files"));
        let none = system_static(&[]);
        assert!(!none.contains("- Prefer dedicated tools over shell commands;"));
        let edit = system_static(&["edit"]);
        assert!(edit.contains("- Use the edit tool for targeted changes to existing text files."));
    }
}
