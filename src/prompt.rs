//! Minimal system prompt (plan §6).
//!
//! Upstream assembles a large instruction stack (instructions state machine,
//! agent prompt, skills, snapshots). ox's runner (task 6) needs a well-formed
//! starting point: an environment block plus the project's `AGENTS.md` when
//! one exists at the working directory. Later tasks extend this template.

use std::path::Path;

use serde::Serialize;

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

pub(crate) const KEY_MCP: &str = "core/mcp-guidance";
pub(crate) const KEY_SKILLS: &str = "core/skill-guidance";
pub(crate) const KEY_INSTRUCTIONS: &str = "core/instructions";
pub(crate) const KEY_DATE: &str = "core/date";
pub(crate) const KEY_ENVIRONMENT: &str = "core/environment";

/// One keyed source's value. Struct variants throughout: serde's internally
/// tagged representation (`tag = "kind"`) cannot serialize newtype variants
/// holding a `String`/`Vec`, and later steps hash this JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceValue {
    /// `<directory>/AGENTS.md` (upstream discovery file list; ox is single-file).
    File {
        path: String,
        content: String,
    },
    Skills {
        skills: Vec<SkillSummary>,
    },
    /// Pre-rendered `<mcp_instructions>` block from the MCP hub.
    Mcp {
        block: String,
    },
    Date {
        value: String,
    },
    Environment {
        value: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SkillSummary {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Three-state observation: upstream distinguishes "the source is gone"
/// (render a removal notice) from "the read failed transiently" (keep what
/// the model already has). Collapsing the two would tell the model its
/// instructions no longer apply whenever AGENTS.md is being atomically
/// replaced or is momentarily unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ObservedValue {
    Present(SourceValue),
    Removed,
    /// Read failed transiently: keep whatever the model already has.
    Unavailable,
}

pub(crate) struct Observed {
    pub key: &'static str,
    pub value: ObservedValue,
}

/// Gather this step's sources in upstream combine order
/// (`session/context.ts:155`): MCP, skills, AGENTS.md, date, environment.
pub(crate) fn gather(directory: &Path, mcp_instructions: Option<&str>) -> Vec<Observed> {
    let skills: Vec<SkillSummary> = crate::tool::skills::discover(directory)
        .iter()
        .map(|s| SkillSummary {
            id: s.id.clone(),
            name: s.name.clone(),
            description: s.description.clone(),
        })
        .collect();
    let agents_path = directory.join("AGENTS.md");
    let agents = match std::fs::read_to_string(&agents_path) {
        Ok(content) => ObservedValue::Present(SourceValue::File {
            path: agents_path.display().to_string(),
            content: content.trim_end().to_owned(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ObservedValue::Removed,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %agents_path.display(),
                "AGENTS.md unreadable; keeping the previously loaded instructions"
            );
            ObservedValue::Unavailable
        }
    };
    vec![
        Observed {
            key: KEY_MCP,
            value: match mcp_instructions {
                Some(block) => ObservedValue::Present(SourceValue::Mcp {
                    block: block.to_owned(),
                }),
                None => ObservedValue::Removed,
            },
        },
        Observed {
            key: KEY_SKILLS,
            value: if skills.is_empty() {
                ObservedValue::Removed
            } else {
                ObservedValue::Present(SourceValue::Skills { skills })
            },
        },
        Observed {
            key: KEY_INSTRUCTIONS,
            value: agents,
        },
        Observed {
            key: KEY_DATE,
            value: ObservedValue::Present(SourceValue::Date {
                value: date_string(),
            }),
        },
        Observed {
            key: KEY_ENVIRONMENT,
            value: ObservedValue::Present(SourceValue::Environment {
                value: env_block(directory),
            }),
        },
    ]
}

/// system[1] body: present sources in order, joined by "\n\n" (upstream
/// `instructions/index.ts:render`). `Removed` and `Unavailable` contribute
/// nothing; a removal notice is only ever a delta against a baseline.
pub(crate) fn render_initial(sources: &[Observed]) -> String {
    sources
        .iter()
        .filter_map(|s| match &s.value {
            ObservedValue::Present(value) => Some(initial_text(value)),
            ObservedValue::Removed | ObservedValue::Unavailable => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Upstream per-source `initial` renderers.
pub(crate) fn initial_text(value: &SourceValue) -> String {
    match value {
        SourceValue::Mcp { block } => block.clone(),
        SourceValue::Skills { skills } => render_skills(skills),
        SourceValue::File { path, content } => format!("Instructions from: {path}\n{content}"),
        SourceValue::Date { value } => format!("Today's date: {value}"),
        SourceValue::Environment { value } => format!(
            "Here is some useful information about the environment you are running in:\n{value}"
        ),
    }
}

/// Upstream per-source `changed` renderers; `previous: None` falls back to
/// the initial render (upstream `instructions/index.ts:111`).
pub(crate) fn changed_text(value: &SourceValue, previous: Option<&SourceValue>) -> String {
    match (value, previous) {
        (SourceValue::Date { value }, _) => format!("Today's date is now: {value}"),
        (SourceValue::Environment { value }, _) => {
            format!("The environment you are running in is now:\n{value}")
        }
        (
            SourceValue::File { path, content },
            Some(SourceValue::File {
                content: before, ..
            }),
        ) => instructions_changed_or_diff(path, before, content),
        (SourceValue::Skills { skills: after }, Some(SourceValue::Skills { skills: before })) => {
            skills_update(before, after)
        }
        (SourceValue::Mcp { block }, Some(_)) => format!(
            "The available MCP server instructions have changed. This list supersedes the previous one.\n{block}"
        ),
        (v, _) => initial_text(v),
    }
}

/// Upstream per-source `removed` renderers; `None` = not deliverable
/// (date/env are always present).
pub(crate) fn removed_text(previous: &SourceValue) -> Option<String> {
    match previous {
        SourceValue::Mcp { .. } => {
            Some("MCP server instructions are no longer available.".to_owned())
        }
        SourceValue::Skills { .. } => Some(
            "Skill guidance is no longer available. Do not use any previously listed skill."
                .to_owned(),
        ),
        SourceValue::File { .. } => {
            Some("Previously loaded instructions no longer apply.".to_owned())
        }
        SourceValue::Date { .. } | SourceValue::Environment { .. } => None,
    }
}

/// Upstream `instruction-discovery.ts:137-147`: the diff form wins when shorter.
fn instructions_changed_or_diff(path: &str, before: &str, after: &str) -> String {
    let patch =
        crate::tool::diff::file_diff(path, before, after, crate::tool::diff::DiffStatus::Modified)
            .patch;
    let hunks = match patch.find("@@") {
        Some(at) => patch[at..].trim_end().to_owned(),
        None => patch.trim_end().to_owned(),
    };
    let diff =
        format!("The instructions from {path} changed. Here's the diff:\n```diff\n{hunks}\n```");
    let replacement = format!("The instructions changed:\nInstructions from: {path}\n{after}");
    if diff.len() < replacement.len() {
        diff
    } else {
        replacement
    }
}

/// Upstream `skill/instructions.ts:render`, plus ox's escaping and optional
/// description (documented deviation: upstream assumes a description).
fn render_skills(skills: &[SkillSummary]) -> String {
    let mut out = String::from(
        "Skills provide specialized instructions and workflows for specific tasks.\n\
         Use the skill tool to load a skill when a task matches its description.\n\
         The user may also invoke a skill directly. When that happens, its instructions appear in the conversation as a <skill_content> block, the same shape the skill tool returns. A skill that is already present this way does not need to be invoked again.\n",
    );
    if skills.is_empty() {
        out.push_str("No skills are currently available.");
        return out;
    }
    out.push_str("<available_skills>\n");
    push_skill_entries(&mut out, skills);
    out.push_str("</available_skills>");
    out
}

fn push_skill_entries(out: &mut String, skills: &[SkillSummary]) {
    for skill in skills {
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <id>{}</id>\n", escape(&skill.id)));
        out.push_str(&format!("    <name>{}</name>\n", escape(&skill.name)));
        if let Some(description) = &skill.description {
            out.push_str(&format!(
                "    <description>{}</description>\n",
                escape(description)
            ));
        }
        out.push_str("  </skill>\n");
    }
}

/// Upstream `skill/instructions.ts:update` — per-id added/changed/removed,
/// full restatement only when no entry differs.
fn skills_update(before: &[SkillSummary], after: &[SkillSummary]) -> String {
    let added: Vec<&SkillSummary> = after
        .iter()
        .filter(|s| !before.iter().any(|b| b.id == s.id))
        .collect();
    let changed: Vec<&SkillSummary> = after
        .iter()
        .filter(|s| {
            before
                .iter()
                .any(|b| b.id == s.id && (b.name != s.name || b.description != s.description))
        })
        .collect();
    let removed: Vec<&str> = before
        .iter()
        .filter(|b| !after.iter().any(|s| s.id == b.id))
        .map(|b| b.id.as_str())
        .collect();
    if added.is_empty() && changed.is_empty() && removed.is_empty() {
        return format!(
            "The available skills have changed. This list supersedes the previous available skills list.\n{}",
            render_skills(after)
        );
    }
    let mut sections: Vec<String> = Vec::new();
    if !added.is_empty() {
        let mut s =
            String::from("New skills are available in addition to those previously listed:\n");
        push_skill_entries(
            &mut s,
            &added.iter().map(|s| (*s).clone()).collect::<Vec<_>>(),
        );
        sections.push(s.trim_end().to_owned());
    }
    if !changed.is_empty() {
        let mut s = String::from(
            "The following skills have changed. Each entry replaces the previously listed skill with the same ID:\n",
        );
        push_skill_entries(
            &mut s,
            &changed.iter().map(|s| (*s).clone()).collect::<Vec<_>>(),
        );
        sections.push(s.trim_end().to_owned());
    }
    if !removed.is_empty() {
        sections.push(format!(
            "The following skill IDs are no longer available and must not be used: {}.",
            removed.join(", ")
        ));
    }
    sections.join("\n")
}

/// Upstream env block (`instructions/builtins.ts:36-46`). ox has no workspace
/// concept: workspace root == working directory; `global.tmp` → `<ox_home>/tmp`.
fn env_block(directory: &Path) -> String {
    let tmp = crate::config::ox_home().join("tmp");
    format!(
        "<env>\n  Working directory: {}\n  Workspace root folder: {}\n  Is directory a git repo: {}\n  Platform: {}\n  Prefer {} over generic system temporary directories such as /tmp; it is pre-created and approved for external access.\n</env>",
        directory.display(),
        directory.display(),
        if directory.join(".git").exists() {
            "yes"
        } else {
            "no"
        },
        std::env::consts::OS,
        tmp.display(),
    )
}

/// JS `Date#toDateString` format ("Thu Oct 22 2026"), UTC.
fn date_string() -> String {
    date_string_from_days(crate::id::now_millis().div_euclid(86_400_000))
}

fn date_string_from_days(days: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let iso = days_to_iso(days); // "YYYY-MM-DD"
    let year = &iso[0..4];
    let month: usize = iso[5..7].parse().expect("days_to_iso month");
    let day = &iso[8..10];
    // 1970-01-01 (day 0) was a Thursday.
    let weekday = WEEKDAYS[((days + 4).rem_euclid(7)) as usize];
    format!("{weekday} {month} {day} {year}", month = MONTHS[month - 1])
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

    /// Serializes the tests that redirect HOME: cargo runs tests on parallel
    /// threads and `std::env::set_var` is process-global.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with HOME pointed at an empty tempdir so the host's global
    /// skill roots (`~/.config/opencode/skills`) cannot leak into discovery.
    fn with_isolated_home<R>(f: impl FnOnce(&Path) -> R) -> R {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        // SAFETY: serialized by HOME_LOCK; the other prompt tests that may
        // run concurrently do not assert on HOME-derived content, so a
        // momentarily redirected HOME cannot change their outcome.
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let result = f(home.path());
        // SAFETY: same serialization as above.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        result
    }

    #[test]
    fn gather_orders_sources_upstream_style() {
        with_isolated_home(|dir| {
            std::fs::write(dir.join("AGENTS.md"), "rule one\n").unwrap();
            let observed = gather(dir, Some("<mcp_instructions>\n</mcp_instructions>"));
            let keys: Vec<&str> = observed.iter().map(|s| s.key).collect();
            assert_eq!(
                keys,
                [
                    KEY_MCP,
                    KEY_SKILLS,
                    KEY_INSTRUCTIONS,
                    KEY_DATE,
                    KEY_ENVIRONMENT
                ]
            );
            assert!(matches!(
                observed[0].value,
                ObservedValue::Present(SourceValue::Mcp { .. })
            ));
            assert!(matches!(observed[1].value, ObservedValue::Removed));
            assert!(matches!(
                observed[2].value,
                ObservedValue::Present(SourceValue::File { .. })
            ));
        });
    }

    #[test]
    fn initial_render_joins_blocks_in_order() {
        with_isolated_home(|dir| {
            std::fs::write(dir.join("AGENTS.md"), "rule one\n").unwrap();
            let observed = gather(dir, None);
            let render = render_initial(&observed);
            let date_pos = render.find("Today's date: ").unwrap();
            let env_pos = render
                .find("Here is some useful information about the environment")
                .unwrap();
            let agents_pos = render.find("Instructions from: ").unwrap();
            assert!(agents_pos < date_pos && date_pos < env_pos);
            assert!(!render.contains("<mcp_instructions>"));
        });
    }

    #[test]
    fn changed_renderings_match_upstream() {
        let path = "/repo/AGENTS.md";
        // One-line change in a long file: upstream picks whichever rendering
        // is shorter, so the diff form only wins when the replacement form
        // would have to restate the whole file.
        let before_body: String = (1..=40).map(|i| format!("rule number {i}\n")).collect();
        let after_body = before_body.replacen("rule number 10\n", "rule number ten\n", 1);
        let before = SourceValue::File {
            path: path.into(),
            content: before_body,
        };
        let after = SourceValue::File {
            path: path.into(),
            content: after_body,
        };
        let text = changed_text(&after, Some(&before));
        assert!(
            text.starts_with("The instructions from /repo/AGENTS.md changed. Here's the diff:"),
            "{text}"
        );
        let big_after = SourceValue::File {
            path: path.into(),
            content: "x".repeat(5000),
        };
        let small_before = SourceValue::File {
            path: path.into(),
            content: "old rule".into(),
        };
        let text = changed_text(&big_after, Some(&small_before));
        assert!(
            text.starts_with("The instructions changed:\nInstructions from: /repo/AGENTS.md"),
            "{text}"
        );
        let date = SourceValue::Date {
            value: "Thu Oct 22 2026".into(),
        };
        assert_eq!(
            changed_text(&date, None),
            "Today's date is now: Thu Oct 22 2026"
        );
        let env = SourceValue::Environment {
            value: "<env>x</env>".into(),
        };
        assert_eq!(
            changed_text(&env, None),
            "The environment you are running in is now:\n<env>x</env>"
        );
        assert_eq!(
            removed_text(&before).as_deref(),
            Some("Previously loaded instructions no longer apply.")
        );
        assert!(removed_text(&date).is_none());
    }

    #[test]
    fn date_string_matches_js_todatestring_format() {
        // Anchor: day 20_748 is 2026-10-22, a Thursday. (The plan's 20_726
        // was mis-anchored — it is 2026-09-30.)
        assert_eq!(days_to_iso(20_748), "2026-10-22");
        assert_eq!(date_string_from_days(20_748), "Thu Oct 22 2026");
        assert_eq!(date_string_from_days(0), "Thu Jan 01 1970");
    }

    #[test]
    fn source_values_serialize_deterministically() {
        let values = [
            SourceValue::File {
                path: "/a".into(),
                content: "x".into(),
            },
            SourceValue::Skills {
                skills: vec![SkillSummary {
                    id: "i".into(),
                    name: "n".into(),
                    description: None,
                }],
            },
            SourceValue::Mcp {
                block: "<mcp_instructions>\n</mcp_instructions>".into(),
            },
            SourceValue::Date {
                value: "Thu Oct 22 2026".into(),
            },
            SourceValue::Environment {
                value: "<env></env>".into(),
            },
        ];
        for value in &values {
            let once = serde_json::to_string(value).expect("serializable");
            let twice = serde_json::to_string(value).expect("serializable");
            assert_eq!(once, twice, "deterministic");
        }
        assert_eq!(
            serde_json::to_string(&SourceValue::Date { value: "x".into() }).unwrap(),
            r#"{"kind":"date","value":"x"}"#
        );
    }

    #[test]
    fn unreadable_agents_md_is_unavailable_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: read_to_string fails with
        // IsADirectory, which is not NotFound.
        std::fs::create_dir(dir.path().join("AGENTS.md")).unwrap();
        let observed = gather(dir.path(), None);
        assert!(matches!(observed[2].value, ObservedValue::Unavailable));
        // Truly absent → Removed.
        std::fs::remove_dir(dir.path().join("AGENTS.md")).unwrap();
        let observed = gather(dir.path(), None);
        assert!(matches!(observed[2].value, ObservedValue::Removed));
    }

    #[test]
    fn render_initial_skips_non_present_sources() {
        let dir = tempfile::tempdir().unwrap();
        let observed = gather(dir.path(), None); // no AGENTS.md / skills / mcp
        let render = render_initial(&observed);
        assert!(!render.contains("Instructions from:"));
        assert!(render.contains("Today's date: "));
    }
}
