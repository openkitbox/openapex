//! System prompt assembly: the static harness layer ([`system_static`]) plus
//! the keyed instruction layer. [`gather`] observes this step's sources in
//! upstream combine order; [`render_initial`] freezes the first render into
//! system[1], and [`changed_text`]/[`removed_text`] render per-key deltas
//! against the runner's baseline (see `runner/instructions.rs`).

use std::path::Path;

use serde::Serialize;

/// The skills block embeds user-controlled text in an XML-ish envelope; a raw
/// `<` would let a skill description break the block out early.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
/// line's `OpenCode` → ox. `${OPENCODE_TOOL_GUIDANCE}` is rendered from the
/// registered tool set on every assembly (upstream
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
    /// The instruction file list (global first, then project), upstream
    /// `instruction-discovery.ts`.
    Files {
        files: Vec<InstructionFile>,
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

/// One instruction file in the `core/instructions` list (upstream
/// `InstructionDiscovery.File`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct InstructionFile {
    pub path: String,
    pub content: String,
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
pub(crate) fn gather(
    directory: &Path,
    home: &Path,
    mcp_instructions: Option<&str>,
) -> Vec<Observed> {
    let skills: Vec<SkillSummary> = crate::tool::skills::discover(directory)
        .iter()
        .map(|s| SkillSummary {
            id: s.id.clone(),
            name: s.name.clone(),
            description: s.description.clone(),
        })
        .collect();
    let agents = observed_instructions(directory, home);
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

/// The `core/instructions` file list: the global `<home>/AGENTS.md` first,
/// then the project's `<directory>/AGENTS.md`, deduped by path. A missing
/// file is normal; any other read failure makes the whole source
/// `Unavailable` (the model keeps what it already has), and an empty list is
/// `Removed`.
fn observed_instructions(directory: &Path, home: &Path) -> ObservedValue {
    let mut files = Vec::new();
    let mut unavailable = false;
    for path in [home.join("AGENTS.md"), directory.join("AGENTS.md")] {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let path = path.display().to_string();
                if !files.iter().any(|file: &InstructionFile| file.path == path) {
                    files.push(InstructionFile {
                        path,
                        content: content.trim_end().to_owned(),
                    });
                }
            }
            // Missing is normal: the file may not exist yet.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // A transient read failure says nothing about the source: keep
            // whatever the model already has (upstream `Unavailable`).
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "instruction file unavailable");
                unavailable = true;
            }
        }
    }
    if unavailable {
        ObservedValue::Unavailable
    } else if files.is_empty() {
        ObservedValue::Removed
    } else {
        ObservedValue::Present(SourceValue::Files { files })
    }
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
        SourceValue::Files { files } => render_instruction_files(files),
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
        (SourceValue::Files { files: after }, Some(SourceValue::Files { files: before })) => {
            instruction_files_update(before, after)
        }
        (SourceValue::Skills { skills: after }, Some(SourceValue::Skills { skills: before })) => {
            skills_update(before, after)
        }
        (SourceValue::Mcp { block }, Some(_)) => format!(
            "The available MCP server instructions have changed. This list supersedes the previous one.\n{block}"
        ),
        (v, _) => initial_text(v),
    }
}

/// Upstream per-source `removed` renderers keyed by source kind; `None` =
/// not deliverable (date/env are always present).
pub(crate) fn removed_text(key: &str) -> Option<String> {
    match key {
        KEY_MCP => Some("MCP server instructions are no longer available.".to_owned()),
        KEY_SKILLS => Some(
            "Skill guidance is no longer available. Do not use any previously listed skill."
                .to_owned(),
        ),
        KEY_INSTRUCTIONS => Some("Previously loaded instructions no longer apply.".to_owned()),
        _ => None,
    }
}

/// Upstream `instruction-discovery.ts:122-124`: one file renders as
/// `Instructions from: {path}\n{content}`, files joined by "\n\n".
fn render_file(file: &InstructionFile) -> String {
    format!("Instructions from: {}\n{}", file.path, file.content)
}

fn render_instruction_files(files: &[InstructionFile]) -> String {
    files
        .iter()
        .map(render_file)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Upstream `instruction-discovery.ts:renderUpdate`: a per-path three-way
/// diff of the file list — removals, additions, then per-file content
/// changes — joined by "\n\n".
fn instruction_files_update(before: &[InstructionFile], after: &[InstructionFile]) -> String {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for file in before {
        if !after.iter().any(|next| next.path == file.path) {
            removed.push(format!(
                "The instructions from {} no longer apply.",
                file.path
            ));
        }
    }
    for file in after {
        match before.iter().find(|previous| previous.path == file.path) {
            None => added.push(format!(
                "New instructions apply from:\n{}",
                render_file(file)
            )),
            Some(previous) if previous.content != file.content => {
                changed.push(instruction_file_changed(previous, file));
            }
            Some(_) => {}
        }
    }
    [removed, added, changed]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Upstream `instruction-discovery.ts:137-147`: the diff form wins when shorter.
fn instruction_file_changed(previous: &InstructionFile, current: &InstructionFile) -> String {
    let patch = crate::tool::diff::file_diff(
        &current.path,
        &previous.content,
        &current.content,
        crate::tool::diff::DiffStatus::Modified,
    )
    .patch;
    let hunks = match patch.find("@@") {
        Some(at) => patch[at..].trim_end().to_owned(),
        None => patch.trim_end().to_owned(),
    };
    let diff = format!(
        "The instructions from {} changed. Here's the diff:\n```diff\n{hunks}\n```",
        current.path
    );
    let replacement = format!("The instructions changed:\n{}", render_file(current));
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
    fn days_to_iso_matches_known_epoch_anchors() {
        // Day 0 = 1970-01-01; leap-year and century boundaries stay exact.
        assert_eq!(days_to_iso(0), "1970-01-01");
        assert_eq!(days_to_iso(59), "1970-03-01");
        assert_eq!(days_to_iso(19_723), "2024-01-01");
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
            let home = tempfile::tempdir().unwrap();
            let observed = gather(
                dir,
                home.path(),
                Some("<mcp_instructions>\n</mcp_instructions>"),
            );
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
                ObservedValue::Present(SourceValue::Files { .. })
            ));
        });
    }

    #[test]
    fn gather_reads_global_then_project_instructions() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("AGENTS.md"), "global rule\n").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "project rule\n").unwrap();
        let observed = gather(dir.path(), home.path(), None);
        let Some(ObservedValue::Present(SourceValue::Files { files })) = observed
            .iter()
            .find(|s| s.key == KEY_INSTRUCTIONS)
            .map(|s| &s.value)
        else {
            panic!("instructions source must be present");
        };
        assert_eq!(files.len(), 2);
        assert!(files[0].path.ends_with("AGENTS.md") && files[0].content == "global rule");
        assert!(files[0].path.starts_with(home.path().to_str().unwrap()));
        assert!(files[1].path.starts_with(dir.path().to_str().unwrap()));
        let render = render_initial(&observed);
        let global_at = render.find("global rule").unwrap();
        let project_at = render.find("project rule").unwrap();
        assert!(global_at < project_at, "global instructions render first");
    }

    #[test]
    fn gather_dedupes_a_shared_instruction_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "only rule\n").unwrap();
        // home == directory：同一文件只出现一次
        let observed = gather(dir.path(), dir.path(), None);
        let Some(ObservedValue::Present(SourceValue::Files { files })) = observed
            .iter()
            .find(|s| s.key == KEY_INSTRUCTIONS)
            .map(|s| &s.value)
        else {
            panic!("present");
        };
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn instruction_file_set_updates_render_added_and_removed_files() {
        let global = InstructionFile {
            path: "/home/.ox/AGENTS.md".into(),
            content: "global rule".into(),
        };
        let project = InstructionFile {
            path: "/repo/AGENTS.md".into(),
            content: "project rule".into(),
        };
        // 全局文件中途出现（baseline 只有项目文件）
        let added = changed_text(
            &SourceValue::Files {
                files: vec![global.clone(), project.clone()],
            },
            Some(&SourceValue::Files {
                files: vec![project.clone()],
            }),
        );
        assert!(
            added.starts_with(
                "New instructions apply from:\nInstructions from: /home/.ox/AGENTS.md\nglobal rule"
            ),
            "{added}"
        );
        // 全局文件被删
        let removed = changed_text(
            &SourceValue::Files {
                files: vec![project.clone()],
            },
            Some(&SourceValue::Files {
                files: vec![global.clone(), project.clone()],
            }),
        );
        assert_eq!(
            removed,
            "The instructions from /home/.ox/AGENTS.md no longer apply."
        );
        // 单文件内容变化：diff 或替换形式（每文件独立）
        let changed = changed_text(
            &SourceValue::Files {
                files: vec![InstructionFile {
                    path: "/repo/AGENTS.md".into(),
                    content: "new project rule".into(),
                }],
            },
            Some(&SourceValue::Files {
                files: vec![project],
            }),
        );
        assert!(changed.contains("The instructions"), "{changed}");
        assert!(changed.contains("new project rule"), "{changed}");
    }

    #[test]
    fn unreadable_global_instructions_make_the_source_unavailable() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "project rule\n").unwrap();
        std::fs::create_dir(home.path().join("AGENTS.md")).unwrap(); // 目录 → IsADirectory
        let observed = gather(dir.path(), home.path(), None);
        let source = observed.iter().find(|s| s.key == KEY_INSTRUCTIONS).unwrap();
        assert!(matches!(source.value, ObservedValue::Unavailable));
    }

    #[test]
    fn initial_render_joins_blocks_in_order() {
        with_isolated_home(|dir| {
            std::fs::write(dir.join("AGENTS.md"), "rule one\n").unwrap();
            let home = tempfile::tempdir().unwrap();
            let observed = gather(dir, home.path(), None);
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
        let before = SourceValue::Files {
            files: vec![InstructionFile {
                path: path.into(),
                content: before_body,
            }],
        };
        let after = SourceValue::Files {
            files: vec![InstructionFile {
                path: path.into(),
                content: after_body,
            }],
        };
        let text = changed_text(&after, Some(&before));
        assert!(
            text.starts_with("The instructions from /repo/AGENTS.md changed. Here's the diff:"),
            "{text}"
        );
        let big_after = SourceValue::Files {
            files: vec![InstructionFile {
                path: path.into(),
                content: "x".repeat(5000),
            }],
        };
        let small_before = SourceValue::Files {
            files: vec![InstructionFile {
                path: path.into(),
                content: "old rule".into(),
            }],
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
            removed_text(KEY_INSTRUCTIONS).as_deref(),
            Some("Previously loaded instructions no longer apply.")
        );
        assert!(removed_text(KEY_DATE).is_none());
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
            SourceValue::Files {
                files: vec![InstructionFile {
                    path: "/a".into(),
                    content: "x".into(),
                }],
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
        let home = tempfile::tempdir().unwrap();
        // A directory where the file should be: read_to_string fails with
        // IsADirectory, which is not NotFound.
        std::fs::create_dir(dir.path().join("AGENTS.md")).unwrap();
        let observed = gather(dir.path(), home.path(), None);
        assert!(matches!(observed[2].value, ObservedValue::Unavailable));
        // Truly absent → Removed.
        std::fs::remove_dir(dir.path().join("AGENTS.md")).unwrap();
        let observed = gather(dir.path(), home.path(), None);
        assert!(matches!(observed[2].value, ObservedValue::Removed));
    }

    #[test]
    fn render_initial_skips_non_present_sources() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let observed = gather(dir.path(), home.path(), None); // no AGENTS.md / skills / mcp
        let render = render_initial(&observed);
        assert!(!render.contains("Instructions from:"));
        assert!(render.contains("Today's date: "));
    }
}
