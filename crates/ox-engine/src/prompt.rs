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
///
/// `project_root` bounds the AGENTS.md ancestor walk (global, then the
/// session directory nearest → farthest up to the root); `None` degenerates
/// to the leaf directory alone.
pub(crate) fn gather(
    directory: &Path,
    home: &Path,
    project_root: Option<&Path>,
    mcp_instructions: Option<&str>,
) -> Vec<Observed> {
    let skills: Vec<SkillSummary> = ox_tools::skills::list(directory)
        .into_iter()
        .map(|s| SkillSummary {
            id: s.id,
            name: s.name,
            description: s.description,
        })
        .collect();
    let agents = observed_instructions(directory, home, project_root);
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
                // One zone lookup per step: the system zone can change while
                // the process runs, so it is never cached.
                value: local_date(ox_schema::id::now_millis(), &jiff::tz::TimeZone::system()),
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

/// Hard cap on the ancestor chain. Documented deviation from upstream:
/// upstream `instruction.ts` has no limit, but ox derives the project root
/// itself, so a stray `git init` in `/` or `$HOME` must not turn one session
/// into a walk of the whole filesystem. Over the cap the chain fails closed
/// to the leaf directory.
const MAX_INSTRUCTION_ANCESTORS: usize = 16;

/// Directories whose `AGENTS.md` contribute to `core/instructions`, nearest
/// first. `None` (no repo anchor) degenerates to the leaf directory, which is
/// today's behaviour. A `project_root` that is not an ancestor, or a chain
/// deeper than `MAX_INSTRUCTION_ANCESTORS`, also degenerates to the leaf:
/// a partial chain would inject half the rules into a frozen baseline with no
/// way for the author to notice (fail-closed).
///
/// Every entry is a prefix slice of `directory` — zero path allocation. The
/// caller canonicalizes; this function is pure path math.
fn instruction_directories<'a>(directory: &'a Path, project_root: Option<&Path>) -> Vec<&'a Path> {
    let Some(root) = project_root else {
        return vec![directory];
    };
    if directory == root {
        return vec![directory];
    }
    if !directory.starts_with(root) {
        return vec![directory];
    }
    let mut chain: Vec<&'a Path> = Vec::new();
    for ancestor in directory.ancestors() {
        chain.push(ancestor);
        if ancestor == root {
            return chain;
        }
        if chain.len() == MAX_INSTRUCTION_ANCESTORS {
            return vec![directory];
        }
    }
    // `starts_with` guarantees `root` is on the chain, so this is unreachable
    // for component-normalized paths; fail closed rather than return a partial
    // chain.
    vec![directory]
}

/// The `core/instructions` file list: the global `<home>/AGENTS.md` first,
/// then the session directory's ancestor chain (`instruction_directories`,
/// nearest → farthest), deduped by path string keeping the first
/// occurrence. A missing file is normal; any other read failure makes the
/// whole source `Unavailable` (the model keeps what it already has), and an
/// empty list is `Removed`.
///
/// This function reads files but never canonicalizes: the paths it is given
/// must already be canonical (the anchor resolution does that) or equal
/// strings will not dedupe.
fn observed_instructions(
    directory: &Path,
    home: &Path,
    project_root: Option<&Path>,
) -> ObservedValue {
    let mut candidates = vec![home.join("AGENTS.md")];
    candidates.extend(
        instruction_directories(directory, project_root)
            .into_iter()
            .map(|dir| dir.join("AGENTS.md")),
    );
    let mut files = Vec::new();
    let mut unavailable = false;
    for path in candidates {
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
    let patch = ox_tools::diff::file_diff(
        &current.path,
        &previous.content,
        &current.content,
        ox_tools::diff::DiffStatus::Modified,
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
        // Unreachable from `gather`, which classifies an empty list as
        // `Removed`; upstream renders the same dead branch.
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
    let tmp = ox_core::config::ox_home().join("tmp");
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

/// The local calendar date at `timestamp_ms` in `zone`, ISO `YYYY-MM-DD`
/// (upstream renders `new Date().toDateString()`-equivalent local dates).
///
/// The wire value was already an ISO date string; what changed is the basis
/// (UTC calendar day → local calendar day), so the first post-deploy request
/// per session emits a one-time `core/date` delta — a documented re-baseline,
/// not a bug.
fn local_date(timestamp_ms: i64, zone: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_millisecond(timestamp_ms)
        .expect("timestamp out of range")
        .to_zoned(zone.clone())
        .date()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// skill roots (`~/.ox/skills`) cannot leak into discovery.
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
                None,
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
        let observed = gather(dir.path(), home.path(), None, None);
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
        let observed = gather(dir.path(), dir.path(), None, None);
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
    fn instruction_directories_walks_leaf_to_project_root() {
        let directory = Path::new("/root/a/b");
        let root = Path::new("/root");
        let chain = instruction_directories(directory, Some(root));
        assert_eq!(
            chain,
            vec![Path::new("/root/a/b"), Path::new("/root/a"), root]
        );
    }

    #[test]
    fn instruction_directories_of_the_project_root_is_the_root_alone() {
        let directory = Path::new("/root/a/b");
        let chain = instruction_directories(directory, Some(directory));
        assert_eq!(chain, vec![directory]);
    }

    #[test]
    fn instruction_directories_with_a_non_ancestor_root_reads_the_leaf_only() {
        let directory = Path::new("/x/y");
        let chain = instruction_directories(directory, Some(Path::new("/tmp/other")));
        assert_eq!(chain, vec![directory]);
    }

    #[test]
    fn instruction_directories_caps_the_chain_at_sixteen_levels() {
        // `levels` path components below the root: the chain is `levels + 1`.
        let nested = |levels: usize| {
            let mut path = String::from("/root");
            for level in 0..levels {
                path.push_str(&format!("/l{level}"));
            }
            path
        };
        let root = Path::new("/root");

        let at_cap = nested(15);
        let chain = instruction_directories(Path::new(&at_cap), Some(root));
        assert_eq!(chain.len(), 16);
        assert_eq!(chain.first(), Some(&Path::new(at_cap.as_str())));
        assert_eq!(chain.last(), Some(&root));

        let over_cap = nested(24);
        assert_eq!(
            instruction_directories(Path::new(&over_cap), Some(root)),
            vec![Path::new(over_cap.as_str())]
        );
    }

    #[test]
    fn instruction_directories_are_borrowed_prefixes_in_ancestor_order() {
        // 15 components below the root: exactly `MAX_INSTRUCTION_ANCESTORS`.
        let directory = Path::new("/root/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o");
        let chain = instruction_directories(directory, Some(Path::new("/root")));
        // Equal to the input's own ancestor chain: no intermediate path is
        // constructed, the entries are slices of `directory`.
        let expected: Vec<&Path> = directory.ancestors().take(16).collect();
        assert_eq!(chain, expected);
        assert!(chain.iter().all(|entry| directory.starts_with(entry)));
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
        let observed = gather(dir.path(), home.path(), None, None);
        let source = observed.iter().find(|s| s.key == KEY_INSTRUCTIONS).unwrap();
        assert!(matches!(source.value, ObservedValue::Unavailable));
    }

    #[test]
    fn initial_render_joins_blocks_in_order() {
        with_isolated_home(|dir| {
            std::fs::write(dir.join("AGENTS.md"), "rule one\n").unwrap();
            let home = tempfile::tempdir().unwrap();
            let observed = gather(dir, home.path(), None, None);
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
    fn local_date_uses_the_zone_calendar_day() {
        // 2026-10-22T18:00:00Z is already 2026-10-23 in UTC+8.
        assert_eq!(
            local_date(
                1_792_692_000_000,
                &jiff::tz::TimeZone::get("Asia/Shanghai").unwrap()
            ),
            "2026-10-23"
        );
        // 2026-10-22T02:00:00Z is still 2026-10-22 there.
        assert_eq!(
            local_date(
                1_792_634_400_000,
                &jiff::tz::TimeZone::get("Asia/Shanghai").unwrap()
            ),
            "2026-10-22"
        );
    }

    #[test]
    fn local_date_can_differ_from_the_utc_date() {
        // The same instant is a different calendar day in UTC, which is what
        // the old epoch-day value reported unconditionally.
        let instant = 1_792_692_000_000; // 2026-10-22T18:00:00Z
        assert_eq!(local_date(instant, &jiff::tz::TimeZone::UTC), "2026-10-22");
        assert_eq!(
            local_date(instant, &jiff::tz::TimeZone::get("Asia/Shanghai").unwrap()),
            "2026-10-23"
        );
    }

    #[test]
    fn local_date_respects_dst_transitions() {
        // 2026-11-01T04:00Z is midnight EDT in New York (fall-back is at 06:00Z
        // this year): 11-01 locally, but still 10-31 in a fixed UTC-5 zone.
        // This pins that the zone, not a fixed offset, decides the date.
        let instant = 1_793_505_600_000; // 2026-11-01T04:00:00Z
        assert_eq!(
            local_date(
                instant,
                &jiff::tz::TimeZone::get("America/New_York").unwrap()
            ),
            "2026-11-01"
        );
        assert_eq!(
            local_date(instant, &jiff::tz::TimeZone::get("Etc/GMT+5").unwrap()),
            "2026-10-31"
        );
    }

    #[test]
    fn the_date_source_renders_the_iso_local_date() {
        let zone = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap();
        let value = local_date(1_792_692_000_000, &zone);
        assert_eq!(
            initial_text(&SourceValue::Date {
                value: value.clone()
            }),
            "Today's date: 2026-10-23"
        );
        assert_eq!(
            changed_text(&SourceValue::Date { value }, None),
            "Today's date is now: 2026-10-23"
        );
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
        let observed = gather(dir.path(), home.path(), None, None);
        assert!(matches!(observed[2].value, ObservedValue::Unavailable));
        // Truly absent → Removed.
        std::fs::remove_dir(dir.path().join("AGENTS.md")).unwrap();
        let observed = gather(dir.path(), home.path(), None, None);
        assert!(matches!(observed[2].value, ObservedValue::Removed));
    }

    #[test]
    fn render_initial_skips_non_present_sources() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let observed = gather(dir.path(), home.path(), None, None); // no AGENTS.md / skills / mcp
        let render = render_initial(&observed);
        assert!(!render.contains("Instructions from:"));
        assert!(render.contains("Today's date: "));
    }

    /// Paths of the `core/instructions` file list, in injection order.
    fn instruction_files(observed: &[Observed]) -> Vec<String> {
        let Some(ObservedValue::Present(SourceValue::Files { files })) = observed
            .iter()
            .find(|s| s.key == KEY_INSTRUCTIONS)
            .map(|s| &s.value)
        else {
            panic!("instructions source must be present");
        };
        files.iter().map(|f| f.path.clone()).collect()
    }

    /// `root/a/b` under a tempdir, each with a distinct AGENTS.md.
    fn nested_dirs(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let a = root.join("a");
        let b = a.join("b");
        std::fs::create_dir_all(&b).unwrap();
        (a, b)
    }

    #[test]
    fn gather_walks_the_full_ancestor_chain_global_first() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (a, b) = nested_dirs(root.path());
        std::fs::write(home.path().join("AGENTS.md"), "global\n").unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root rule\n").unwrap();
        std::fs::write(a.join("AGENTS.md"), "middle rule\n").unwrap();
        std::fs::write(b.join("AGENTS.md"), "leaf rule\n").unwrap();
        let observed = gather(&b, home.path(), Some(root.path()), None);
        let expected: Vec<String> = [
            home.path().join("AGENTS.md"),
            b.join("AGENTS.md"),
            a.join("AGENTS.md"),
            root.path().join("AGENTS.md"),
        ]
        .iter()
        .map(|p| p.display().to_string())
        .collect();
        assert_eq!(instruction_files(&observed), expected);
    }

    #[test]
    fn gather_skips_a_missing_middle_layer_without_reordering() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (_a, b) = nested_dirs(root.path());
        std::fs::write(home.path().join("AGENTS.md"), "global\n").unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root rule\n").unwrap();
        std::fs::write(b.join("AGENTS.md"), "leaf rule\n").unwrap();
        // `a` has no AGENTS.md: the gap is skipped, order is preserved.
        let observed = gather(&b, home.path(), Some(root.path()), None);
        let expected: Vec<String> = [
            home.path().join("AGENTS.md"),
            b.join("AGENTS.md"),
            root.path().join("AGENTS.md"),
        ]
        .iter()
        .map(|p| p.display().to_string())
        .collect();
        assert_eq!(instruction_files(&observed), expected);
    }

    #[test]
    fn an_unreadable_ancestor_makes_the_whole_source_unavailable() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (a, b) = nested_dirs(root.path());
        std::fs::write(b.join("AGENTS.md"), "leaf rule\n").unwrap();
        let ancestor = a.join("AGENTS.md");
        std::fs::write(&ancestor, "middle rule\n").unwrap();
        std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads through any file mode; skip rather than flake.
        if std::fs::read_to_string(&ancestor).is_ok() {
            std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        let observed = gather(&b, home.path(), Some(root.path()), None);
        std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o644)).unwrap();
        let source = observed.iter().find(|s| s.key == KEY_INSTRUCTIONS).unwrap();
        assert!(matches!(source.value, ObservedValue::Unavailable));
    }

    #[test]
    fn a_none_project_root_keeps_the_leaf_only_shape() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let b = root.path().join("deep").join("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(home.path().join("AGENTS.md"), "global\n").unwrap();
        std::fs::write(b.join("AGENTS.md"), "leaf rule\n").unwrap();
        // D5 invariant: `None` is byte-identical to today's two-file shape.
        let observed = gather(&b, home.path(), None, None);
        let expected: Vec<String> = [home.path().join("AGENTS.md"), b.join("AGENTS.md")]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(instruction_files(&observed), expected);
    }

    #[test]
    fn the_global_file_dedupes_against_the_ancestor_chain() {
        // home == project root: the farthest chain entry repeats the global
        // file, and the first occurrence wins.
        let root = tempfile::tempdir().unwrap();
        let (a, b) = nested_dirs(root.path());
        std::fs::write(root.path().join("AGENTS.md"), "global and root\n").unwrap();
        std::fs::write(a.join("AGENTS.md"), "middle rule\n").unwrap();
        std::fs::write(b.join("AGENTS.md"), "leaf rule\n").unwrap();
        let observed = gather(&b, root.path(), Some(root.path()), None);
        let expected: Vec<String> = [
            root.path().join("AGENTS.md"),
            b.join("AGENTS.md"),
            a.join("AGENTS.md"),
        ]
        .iter()
        .map(|p| p.display().to_string())
        .collect();
        assert_eq!(instruction_files(&observed), expected);
    }

    #[test]
    fn a_non_ancestor_project_root_reads_the_leaf_only() {
        let home = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let b = dir.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(home.path().join("AGENTS.md"), "global\n").unwrap();
        std::fs::write(unrelated.path().join("AGENTS.md"), "elsewhere\n").unwrap();
        std::fs::write(b.join("AGENTS.md"), "leaf rule\n").unwrap();
        let observed = gather(&b, home.path(), Some(unrelated.path()), None);
        let expected: Vec<String> = [home.path().join("AGENTS.md"), b.join("AGENTS.md")]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(instruction_files(&observed), expected);
    }
}
