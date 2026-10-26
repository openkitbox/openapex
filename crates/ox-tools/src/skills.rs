//! Skills: user-authored instruction files the model can load on demand.
//!
//! The discovery contract mirrors upstream opencode
//! (`packages/core/src/config/plugin/skill-scan.ts` @ v2.1.2): a recursive
//! walk to six directory levels that skips dot-prefixed entries and a fixed
//! set of dependency directories at every level. A directory with a
//! `SKILL.md` is a skill whose id is the directory's basename (a skill
//! directory is a leaf — its nested directories are not skills), and a root's
//! own `*.md` files are file skills whose id is the file stem. Frontmatter
//! carries the display `name` and `description`; everything after it is the
//! body the `skill` tool loads into the conversation.
//!
//! The model-facing catalog is stricter than discovery (upstream
//! `skill/instructions.ts:81-84`): a skill without a description, or with
//! auto-invocation off, is never listed in `<available_skills>` — but stays
//! loadable by explicit id, because the lookup the `skill` tool and user
//! attachments go through has no such filter (upstream `Skill.Service.get`,
//! `skill.ts:126-128`).
//!
//! The model-facing envelope has exactly one formatter, `render_content`
//! (upstream `Skill.toModelOutput`), reached through two entry points that
//! differ only in who discovers: `prepare` discovers and then renders one id
//! (what the `skill` tool uses), and `prepare_from` renders one id over an
//! already-discovered slice (what the prompt handler uses, so one scan answers
//! every requested id). `select_files` is their pure companion-file policy, so
//! neither entry point can format differently.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use serde::Serialize;

/// One discovered skill (upstream `Skill.Info`).
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub path: PathBuf,
    /// The markdown body after frontmatter — what the `skill` tool returns.
    pub content: String,
}

/// Frontmatter-only view of a skill: what the catalog needs. The body is
/// parsed and dropped in place — reading the file is inherent to parsing
/// frontmatter, but the per-step catalog walk must not retain body strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// How deep the scan walks below a root, upstream `MAX_DEPTH`
/// (`skill-scan.ts:7`): the root itself is depth 0, a child of a depth-5
/// directory is scanned at depth 6, and a depth-6 directory's children are
/// never enqueued (`item.depth < MAX_DEPTH`, `skill-scan.ts:38-43`) — so the
/// deepest skill directory is six levels below the root.
const MAX_DEPTH: u32 = 6;

/// The directory names upstream's scan skips at any depth
/// (`FSUtil.ignoredDirectories`, `fs-util.ts:281`, in upstream order).
const IGNORED_DIRECTORIES: [&str; 5] = ["node_modules", ".git", ".venv", "venv", "__pycache__"];

/// [`IGNORED_DIRECTORIES`] matching, shared by the discovery scan and the
/// companion-file walk so the two can never disagree on the names.
fn is_ignored_directory(name: &OsStr) -> bool {
    IGNORED_DIRECTORIES
        .iter()
        .any(|ignored| name == OsStr::new(ignored))
}

/// Whether the discovery scan skips a directory entry outright
/// (`skill-scan.ts:18`): dot-prefixed names — files as well as directories,
/// so a dot file is never a file skill either — and [`IGNORED_DIRECTORIES`],
/// at every level.
fn skipped_scan_entry(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.') || is_ignored_directory(name)
}

/// The directories skills are discovered under, in precedence order: the
/// project-local `.ox/skills`, then the global `~/.ox/skills`. ox-native only —
/// upstream's ecosystem roots (`.claude/skills`, `.agents/skills`) are not
/// consulted. Each root is walked recursively to [`MAX_DEPTH`] levels (a skill
/// directory does not contain skills).
fn roots(directory: &Path) -> Vec<PathBuf> {
    let mut roots = vec![directory.join(".ox").join("skills")];
    if let Some(home) = dirs_home() {
        roots.push(home.join(".ox").join("skills"));
    }
    roots
}

/// The user's home directory (`$HOME`, then `$USERPROFILE` on Windows).
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Discover every skill visible from `directory`. Unreadable roots and files
/// are skipped, never fatal — a broken skill dir must not break the toolset.
pub fn discover(directory: &Path) -> Vec<Skill> {
    discover_in(&roots(directory))
}

/// The catalog view of every skill visible from `directory`: id, name and
/// description only. The bodies are never retained, so a step's catalog walk
/// costs frontmatter parsing rather than one body allocation per skill.
pub fn list(directory: &Path) -> Vec<SkillMeta> {
    list_in(&roots(directory))
}

/// Discovery over an explicit root set: tests scope themselves to a tempdir
/// so the host's global skill roots never leak in.
pub(crate) fn discover_in(roots: &[PathBuf]) -> Vec<Skill> {
    dedupe_and_sort(
        skill_candidates(roots)
            .into_iter()
            .filter_map(|(file, id)| load(&file, id))
            .collect(),
        |skill| skill.id.as_str(),
    )
}

/// [`discover_in`] restricted to frontmatter metadata, filtered to what the
/// catalog may show: upstream `skill/instructions.ts:81-84` skips skills
/// whose `description` is unset or whose `autoinvoke` is `false`. Loading is
/// untouched — [`discover_in`] and [`prepare`] still see every skill, so an
/// explicit id keeps working (upstream `Skill.Service.get` has no filter,
/// `skill.ts:126-128`).
pub(crate) fn list_in(roots: &[PathBuf]) -> Vec<SkillMeta> {
    let listed = dedupe_and_sort(
        skill_candidates(roots)
            .into_iter()
            .filter_map(|(file, id)| meta(&file, id))
            .collect(),
        |(meta, _)| meta.id.as_str(),
    );
    // After the dedupe, not before: the filter must judge the skill a step
    // would actually load — the first root's entry, not a later root's
    // better-documented twin of the same id.
    listed
        .into_iter()
        .filter(|(meta, autoinvoke)| *autoinvoke && meta.description.is_some())
        .map(|(meta, _)| meta)
        .collect()
}

/// Every candidate skill file under `roots` as `(file, id)`, in discovery
/// order — upstream `SkillScan.scan` (`skill-scan.ts:11-46`) as a
/// breadth-first walk:
///
/// - entries whose name starts with `.` or is in [`IGNORED_DIRECTORIES`] are
///   skipped at every level, files as well as directories
///   (`skill-scan.ts:18`);
/// - a directory containing `SKILL.md` is a skill and a leaf — the walk does
///   not descend into it (`skill-scan.ts:29-33`). Its id is the directory's
///   basename, so `skills/group/foo/SKILL.md` is `foo` (upstream
///   `skill-file.ts:47`, the `basename(dirname(filepath))` branch; a
///   `SKILL.md` in a root itself names the skill after that root);
/// - only a root's own `*.md` files are file skills, id = file stem
///   (`skill-scan.ts:34-37`, `skill-file.ts:44-46`);
/// - recursion stops below [`MAX_DEPTH`] (`skill-scan.ts:38-43`).
///
/// Symlinks are classified by what they point at (`Path::is_dir` follows
/// them, like upstream's `stat`, `skill-scan.ts:21-26`); a broken link is
/// neither. No dedupe happens here — a file that fails to parse must not
/// shadow the same id from a later root.
fn skill_candidates(roots: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut candidates = Vec::new();
    for root in roots {
        // A root without a basename (`/`) cannot name a root-level
        // `SKILL.md`; nested skills inside it would still be sound, but no
        // real skill root looks like that — skip it.
        if root.file_name().and_then(OsStr::to_str).is_none() {
            continue;
        }
        let mut queue = VecDeque::from([(root.to_path_buf(), 0u32)]);
        while let Some((directory, depth)) = queue.pop_front() {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                // A missing directory is normal — most projects have no
                // skill dirs anywhere along the walk.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    // Skipping keeps "unreadable is never fatal", but silence
                    // here makes an unreadable directory surface later as a
                    // bogus 404.
                    tracing::warn!(
                        path = %directory.display(),
                        %error,
                        "skill directory unreadable; skipping"
                    );
                    continue;
                }
            };
            let mut directories = Vec::new();
            let mut markdown = Vec::new();
            let mut skill: Option<PathBuf> = None;
            for entry in entries.flatten() {
                let name = entry.file_name();
                if skipped_scan_entry(&name) {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    directories.push(path);
                } else if name == *OsStr::new("SKILL.md") {
                    skill = Some(path);
                } else if depth == 0 && path.extension().and_then(OsStr::to_str) == Some("md") {
                    let Some(id) = path.file_stem().and_then(OsStr::to_str).map(str::to_owned)
                    else {
                        continue;
                    };
                    markdown.push((path, id));
                }
            }
            if let Some(file) = skill {
                // Upstream id = basename of the containing directory
                // (`skill-file.ts:47`); every enqueued directory has a file
                // name, the fallback just keeps non-addressable paths honest.
                let Some(dir_id) = directory.file_name().and_then(OsStr::to_str) else {
                    continue;
                };
                candidates.push((file, dir_id.to_owned()));
                continue;
            }
            if depth == 0 {
                candidates.append(&mut markdown);
            }
            if depth < MAX_DEPTH {
                for path in directories {
                    queue.push_back((path, depth + 1));
                }
            }
        }
    }
    candidates
}

/// First occurrence wins on id collision (ordered discovery, upstream's
/// `Source.equals` dedup), then the ids sort ascending. Shared by both views
/// so `discover` and `list` can never disagree on which skill a step sees.
fn dedupe_and_sort<T>(items: Vec<T>, id: impl Fn(&T) -> &str) -> Vec<T> {
    let mut unique: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        if !unique.iter().any(|seen| id(seen) == id(&item)) {
            unique.push(item);
        }
    }
    unique.sort_by(|a, b| id(a).cmp(id(b)));
    unique
}

/// Read the skill file and parse its frontmatter, keeping the body.
fn load(file: &Path, id: String) -> Option<Skill> {
    let raw = std::fs::read_to_string(file).ok()?;
    let parsed = parse_skill(&raw, &id);
    Some(Skill {
        id,
        name: parsed.name,
        description: parsed.description,
        path: file.to_path_buf(),
        content: parsed.body.to_owned(),
    })
}

/// Read the skill file and keep the frontmatter only; the body is dropped
/// with `raw` and never allocated on its own. Returns the metadata plus the
/// parsed `autoinvoke` — the catalog filter needs both, and [`SkillMeta`]
/// stays a pure id/name/description view.
fn meta(file: &Path, id: String) -> Option<(SkillMeta, bool)> {
    let raw = std::fs::read_to_string(file).ok()?;
    let parsed = parse_skill(&raw, &id);
    Some((
        SkillMeta {
            id,
            name: parsed.name,
            description: parsed.description,
        },
        parsed.autoinvoke,
    ))
}

/// One skill file's frontmatter fields and the borrowed body after it.
struct ParsedSkill<'a> {
    name: String,
    description: Option<String>,
    /// Whether the model may invoke the skill on its own (upstream
    /// `autoinvoke`, `skill-file.ts:48-50`): the `autoinvoke` key (ox's
    /// spelling), the upstream-canonical `opencode/autoinvoke` (flat, or
    /// nested under `metadata:` — the line parser matches the trimmed key
    /// either way), else the inverse of a truthy `disable-model-invocation`
    /// (the Claude compatibility spelling). Defaults to true. Only the
    /// catalog honors it (`skill/instructions.ts:81-84`).
    autoinvoke: bool,
    body: &'a str,
}

/// The one frontmatter parser: both views go through it, so `list` can never
/// report a name or description `discover` would not.
fn parse_skill<'a>(raw: &'a str, id: &str) -> ParsedSkill<'a> {
    let (frontmatter, body) = split_frontmatter(raw);
    let name = frontmatter
        .and_then(|front| value(front, "name"))
        .unwrap_or_else(|| id.to_owned());
    let description = frontmatter.and_then(|front| value(front, "description"));
    let autoinvoke = frontmatter
        .map(|front| {
            boolean_value(front, "autoinvoke")
                .or_else(|| boolean_value(front, "opencode/autoinvoke"))
                .unwrap_or_else(|| {
                    !boolean_value(front, "disable-model-invocation").unwrap_or(false)
                })
        })
        .unwrap_or(true);
    ParsedSkill {
        name,
        description,
        autoinvoke,
        body,
    }
}

/// Split `---\nname: …\n---\n` frontmatter from the markdown body. Returns
/// `None` and the whole file as body when there is none — a plain markdown
/// file is a valid skill.
fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let Some(trimmed) = raw.strip_prefix("---") else {
        return (None, raw);
    };
    let trimmed = trimmed.trim_start_matches(['\r', '\n']);
    match trimmed.find("\n---") {
        Some(end) => (
            Some(&trimmed[..end]),
            trimmed[end + 4..].trim_start_matches(['\r', '\n']),
        ),
        None => (None, raw),
    }
}

/// One `key: value` entry of frontmatter. A deliberate subset — skills use
/// `name`, `description` and the auto-invoke keys, and a YAML parser
/// dependency is not justified. A line without a colon says nothing about
/// any key; it must not stop the scan (upstream parses real YAML, where such
/// lines are ordinary).
///
/// Beside single-line scalars, YAML block scalars are supported: a header such
/// as `description: >-` followed by indented continuation lines is the common
/// way to write a long description, and reading the header as the literal
/// value `">-"` would silently blank the skill's catalog entry. The folded
/// style joins lines with spaces, the literal style keeps line breaks, and the
/// `-`/default/`+` chomping indicators control the trailing newline. Lines
/// belonging to another key's block are skipped while scanning, so a
/// `name:`-looking line inside a description is never mistaken for the real
/// key. Explicit indentation indicators (`>2`) and header comments are outside
/// the subset and read as plain scalars.
fn value(frontmatter: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let Some((head, tail)) = line.split_once(':') else {
            index += 1;
            continue;
        };
        let text = tail.trim();
        if let Some((folded, chomp)) = block_style(text) {
            let end = index + 1 + block_span(&lines[index + 1..], indentation(line));
            if head.trim() == key {
                return render_block(&lines[index + 1..end], folded, chomp);
            }
            index = end;
            continue;
        }
        if head.trim() == key {
            let text = text.trim_matches('"').trim_matches('\'');
            if !text.is_empty() {
                return Some(text.to_owned());
            }
        }
        index += 1;
    }
    None
}

/// Leading spaces, in bytes. Block scalar indentation is spaces by contract;
/// a tab-indented continuation simply fails the deeper-than-the-key test.
fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// A block scalar header: `>` (folded) or `|` (literal), optionally followed
/// by a chomping indicator (`-` strip, `+` keep; absent means clip). Returns
/// `(folded, chomp)` — `chomp` is `' '` for clip.
fn block_style(header: &str) -> Option<(bool, char)> {
    let mut chars = header.chars();
    let folded = match chars.next() {
        Some('>') => true,
        Some('|') => false,
        _ => return None,
    };
    let chomp = match (chars.next(), chars.next()) {
        (None, None) => ' ',
        (Some(chomp @ ('-' | '+')), None) => chomp,
        _ => return None,
    };
    Some((folded, chomp))
}

/// How many lines after a block scalar header belong to the scalar: blank
/// lines and lines indented deeper than the key. The first non-blank line at
/// or below the key's indentation starts the next entry.
fn block_span(lines: &[&str], key_indent: usize) -> usize {
    lines
        .iter()
        .position(|line| !line.trim().is_empty() && indentation(line) <= key_indent)
        .unwrap_or(lines.len())
}

/// Render a block scalar's content lines. The content indentation is that of
/// the first non-blank line, per YAML; later lines are stripped by that many
/// columns. Folding joins adjacent non-blank lines with one space and turns
/// each blank line into a newline, so paragraphs survive a `>` scalar.
fn render_block(lines: &[&str], folded: bool, chomp: char) -> Option<String> {
    let content_indent = lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| indentation(line))?;
    let mut out = String::new();
    for line in lines {
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let text = line.get(content_indent..).unwrap_or_else(|| line.trim());
        if folded && !out.is_empty() && !out.ends_with('\n') {
            out.push(' ');
        }
        out.push_str(text);
        if !folded {
            out.push('\n');
        }
    }
    // A folded scalar's last line still ends at its line break; chomping
    // below decides how much of the trailing newline run survives.
    if folded && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if chomp != '+' {
        out.truncate(out.trim_end_matches('\n').len());
        if chomp == ' ' && !out.is_empty() {
            // Clip: exactly one trailing newline.
            out.push('\n');
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// A frontmatter boolean, upstream `booleanValue` (`skill-file.ts:28-36`):
/// `true/false/1/0/yes/no/on/off`, case-insensitive; anything else — or an
/// absent key — is unset.
fn boolean_value(frontmatter: &str, key: &str) -> Option<bool> {
    match value(frontmatter, key)?.to_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Render the model-facing envelope for one skill (upstream
/// `Skill.toModelOutput`). `directory` is the base directory relative paths
/// resolve against — [`prepare_from`] passes the same value it stores in
/// [`PreparedSkill::directory`], so the envelope and the struct cannot drift.
/// `files` are already selected absolute paths and are emitted in the given
/// order; the `<skill_files>` block is always present, even when empty. This is
/// the only skill format the model ever sees. `home` is the user's home
/// directory: the base-directory line renders a leading `home` as `~`
/// ([`ox_core::home_paths::redact_home`], batch G), because it is injected
/// text; the companion file list stays absolute.
///
/// The body is untrusted (a checked-in `SKILL.md`), so harness-looking
/// `<system-reminder>` framing is escaped before it enters the envelope
/// ([`ox_core::reminder_tags::neutralize_reminder_tags`], batch D): system[0]
/// tells the model those blocks are harness-authored. Tag-free bodies render
/// byte-identically.
pub fn render_content(skill: &Skill, directory: &Path, files: &[PathBuf], home: &Path) -> String {
    let directory = ox_core::home_paths::redact_home(directory, home);
    let mut lines = vec![
        format!("<skill_content name=\"{}\">", skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        ox_core::reminder_tags::neutralize_reminder_tags(skill.content.trim()).into_owned(),
        String::new(),
        format!("Base directory for this skill: {directory}"),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory."
            .to_owned(),
        "Note: file list is sampled.".to_owned(),
        String::new(),
        "<skill_files>".to_owned(),
    ];
    for file in files {
        lines.push(format!("<file>{}</file>", file.display()));
    }
    lines.push("</skill_files>".to_owned());
    lines.push("</skill_content>".to_owned());
    lines.join("\n")
}

/// Whether `skill` is a directory skill (`<dir>/SKILL.md`). Upstream `prepare`
/// gates companion files on exactly this basename, so a file skill
/// (`<root>/notes.md`) never lists its siblings.
fn is_directory_skill(skill: &Skill) -> bool {
    skill.path.file_name() == Some(OsStr::new("SKILL.md"))
}

/// The pure policy half of upstream `prepare`'s file handling: no companions
/// for a file skill, drop the `SKILL.md` itself, sort ascending, keep at most
/// ten. Scanning is [`companion_files`]' job, so this stays testable without a
/// filesystem.
pub(crate) fn select_files(skill: &Skill, mut files: Vec<PathBuf>) -> Vec<PathBuf> {
    if !is_directory_skill(skill) {
        return Vec::new();
    }
    files.retain(|file| file.file_name() != Some(OsStr::new("SKILL.md")));
    // Upstream `.toSorted()` sorts path strings in JS UTF-16 code-unit order;
    // UTF-8 byte order is equivalent for ASCII and differs only for astral vs
    // U+E000..U+FFFF code points.
    files.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    files.truncate(10);
    files
}

/// The directory relative paths in a skill resolve against (upstream
/// `path.dirname(skill.path)`): the file's parent, or `.` when the path carries
/// no directory at all (a bare `notes.md`). One derivation so the envelope's
/// `Base directory for this skill:` line and [`PreparedSkill::directory`] can
/// never disagree.
fn base_directory(skill: &Skill) -> &Path {
    match skill.path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// The I/O half of upstream `prepare`: walk a directory skill's directory for
/// its companions (upstream `fs.scan("**/*", {absolute: true, include: "file",
/// dot: true, ignore: FSUtil.ignoreGlobs})`) and apply [`select_files`]. A file
/// skill walks nothing.
///
/// Upstream's `ignore` list is only [`IGNORED_DIRECTORIES`], so every
/// gitignore-style filter is off: the walker's defaults would drop companions
/// named by the enclosing repo's `.gitignore` (or the user's global excludes),
/// making the model-visible list machine-dependent. An unreadable or vanished
/// directory yields an empty file list rather than an error — this module's
/// "unreadable is never fatal" contract, where upstream's `fs.scan` would
/// propagate the failure.
pub fn companion_files(skill: &Skill) -> Vec<PathBuf> {
    if !is_directory_skill(skill) {
        return Vec::new();
    }
    let directory = base_directory(skill);
    let files: Vec<PathBuf> = WalkBuilder::new(directory)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .filter_entry(|entry| {
            // Only directories are pruned, so a companion *file* that happens to
            // share one of these names survives (upstream's globs only exclude
            // the directories and their contents).
            entry.depth() == 0
                || !(entry
                    .file_type()
                    .is_some_and(|file_type| file_type.is_dir())
                    && is_ignored_directory(entry.file_name()))
        })
        .build()
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        })
        .map(|entry| entry.path().to_path_buf())
        .collect();
    select_files(skill, files)
}

/// One skill ready to hand to the model: its identity, the directory relative
/// paths resolve against, and the rendered `<skill_content>` envelope.
#[derive(Debug)]
pub struct PreparedSkill {
    pub id: String,
    pub name: String,
    pub directory: PathBuf,
    pub text: String,
}

/// Load `id` from an already-discovered skill set and render it.
///
/// This is the pure half of [`prepare`]: deciding which skill `id` names and
/// how it renders is separable from discovery, so a caller that has already
/// discovered — the prompt path resolves several ids per admission — pays for
/// one scan, and tests can decide without a filesystem. The only I/O left is
/// [`companion_files`], which is per-skill and unavoidable. `home` is threaded
/// through to [`render_content`]'s base-directory redaction.
pub fn prepare_from(skills: &[Skill], id: &str, home: &Path) -> Option<PreparedSkill> {
    let skill = skills.iter().find(|skill| skill.id == id)?;
    let directory = base_directory(skill).to_path_buf();
    let text = render_content(skill, &directory, &companion_files(skill), home);
    Some(PreparedSkill {
        id: skill.id.clone(),
        name: skill.name.clone(),
        directory,
        text,
    })
}

/// Discover the skills under `directory` and load `id`, redacting `home`
/// ([`ox_core::config::user_home`]) in the envelope's base-directory line.
///
/// This is the formatter entry point the `skill` tool uses; the prompt handler
/// pairs [`discover`] with [`prepare_from`] to answer several ids per scan.
/// Both end in [`render_content`], the only producer of the envelope, so the
/// two can never drift.
pub fn prepare(directory: &Path, id: &str, home: &Path) -> Option<PreparedSkill> {
    prepare_from(&discover(directory), id, home)
}
