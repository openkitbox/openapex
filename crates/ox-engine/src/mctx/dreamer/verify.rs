use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use regex::Regex;

use crate::mctx::memory;
use ox_db::Db;

use super::manifest::{
    ApplyReport, archive_regex, attr_of, complete_manifest_body, directive_shaped_project_rule,
    keep_batch_id, skip_regex, update_regex, verified_regex,
};

/// Most memories one `verify` completion carries (the reference's
/// `VERIFY_BATCH_SIZE` in `dreamer/verify.ts`, chosen there for prompt-token
/// cost; ox keeps the same number so a batch stays a readable prompt).
pub(super) const VERIFY_MEMORY_LIMIT: usize = 20;

/// Characters of each referenced file the host shows the verifier. The
/// reference reads short line ranges through a tool; ox has no tools, so it
/// prints an excerpt instead. File heads carry the declarations (paths,
/// signatures, constants) that most claims are about.
const VERIFY_EXCERPT_CHARS: usize = 4_000;

/// Total excerpt budget for one `verify` prompt, across every cited file. Not
/// a model limit — a cost guard, so one task never assembles an arbitrarily
/// large prompt from a memory that cites many files.
const VERIFY_EXCERPT_TOTAL_CHARS: usize = 40_000;

/// The task's own prefixed completion prompt (see the port map). Adapts
/// `dreamer/verify-prompt.ts::VERIFY_SYSTEM_PROMPT` to the tool-free host.
pub(super) const VERIFY_SYSTEM_PROMPT: &str = "You are a memory verifier for the magic-context system. You verify project memories against the CURRENT code.

The host has already read the files each memory names and printed short excerpts below; you have no tools and must not ask for more. Judge each memory on that evidence alone.

Decide ONE of four outcomes per memory:
- VERIFIED — still accurate. Keep it as-is.
- UPDATE — a CODE FACT is still true but a file-falsifiable DETAIL drifted (a renamed symbol, a moved file, a changed number or name). Provide the corrected content in terse present tense (\"X uses Y\", not \"X was changed to Y\"). Only update for genuine drift, not style. If a legitimate update intentionally consolidates the old claim into much shorter content, set consolidation=\"true\" explicitly.
- ARCHIVE — a CODE FACT is positively falsified: the code clearly contradicts it, or the API/symbol/path it describes no longer exists.
- SKIP — the claim is not a code fact, or code cannot decide it. Keep it unchanged and do not claim verification.

UPDATE and ARCHIVE are ONLY for claims a repository file can falsify. Behavioral directives (when to act, how to work, who decides, tool-usage discipline) can only be VERIFIED or SKIPPED — a named path inside a directive does not make the directive a code fact, and a file's failure to corroborate a behavioral rule is never grounds to update or archive it. A memory whose referenced file is simply gone is NOT automatically falsified: if the claim is behavioral, or the excerpt cannot decide it, mark it VERIFIED or SKIPPED and leave it alone.

BE CONSERVATIVE ABOUT ARCHIVING. Wrong archival of a TRUE memory is the worst possible outcome — far worse than leaving a slightly-stale memory. If you are unsure, or the excerpt cannot decide it: mark it VERIFIED or SKIPPED, never archived. Archive ONLY with positive evidence the code contradicts a code fact.

Every memory below must appear in your manifest: each input memory id appears exactly once, in exactly one element.

Output ONE XML manifest at the very end and NOTHING else — no narration, no per-memory commentary, no reasoning:
<verify>
<verified id=\"N\"/>
<update id=\"M\">corrected present-tense content</update>
<update id=\"C\" consolidation=\"true\">intentionally consolidated content</update>
<archive id=\"K\" reason=\"specific evidence the code contradicts it\"/>
<skip id=\"S\" reason=\"behavioral directive; code cannot decide it\"/>
</verify>

Rules:
- Every input memory id MUST appear exactly once, in exactly one of verified/update/archive/skip. Ids must come from the list below.
- Default to VERIFIED. skip when code cannot decide the claim. update and archive are the exceptions, not the norm.";

// ---------------------------------------------------------------------------
// verify: the host-side gate
// ---------------------------------------------------------------------------

/// One memory that needs a re-check (a cited file is newer, or missing for a
/// never-verified memory), with every path its text named and what the host
/// found there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidate {
    id: String,
    category: String,
    content: String,
    /// When the memory was last written; the verifier sees it so it can judge
    /// "the file is newer than the claim".
    updated_at: i64,
    paths: Vec<PathState>,
}

/// What the filesystem says about one referenced path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathState {
    /// The path as resolved for this project (absolute), or `None` when the
    /// written path leaves the project — those are never read.
    path: Option<PathBuf>,
    /// The path as the memory wrote it, for the prompt.
    written: String,
    /// False when the file does not exist (the reference's "was deleted"), or
    /// when the path is outside the project. A missing path only makes a
    /// never-verified memory a candidate (see [`needs_check`]).
    exists: bool,
    /// The file's mtime in epoch milliseconds, when readable.
    mtime: Option<i64>,
    /// `mtime > verified_at`: the file changed after the memory was last
    /// checked against it (a never-verified memory has `verified_at = 0`, so
    /// every existing file counts as newer and it is checked once).
    newer: bool,
}

/// The verify gate: `Some` when this memory must be re-checked, `None` when it
/// is free to leave alone. A memory that names no path is never a candidate
/// (the reference's "file-independent" exclusion); [`needs_check`] decides the
/// rest from what the filesystem said.
pub(super) fn candidate(project_dir: &str, memory: &memory::StoredMemory) -> Option<Candidate> {
    let paths: Vec<PathState> = referenced_paths(&memory.content)
        .into_iter()
        .map(|written| inspect(project_dir, &written, memory.verified_at))
        .collect();
    if !needs_check(&paths, memory.verified_at) {
        return None;
    }
    Some(Candidate {
        id: memory.id.clone(),
        category: memory.category.clone(),
        content: memory.content.clone(),
        updated_at: memory.updated_at,
        paths,
    })
}

/// Whether the inspected paths make a memory worth a verify completion.
///
/// A stamped memory (`verified_at > 0`) has already been judged against its
/// missing file, so re-sending it can never change the outcome, and the
/// deletion time is not recorded. Deviation: a file deleted AFTER stamping is
/// not re-checked until the memory is rewritten (which resets `verified_at` to
/// 0 in memory.rs) or another cited file changes.
fn needs_check(paths: &[PathState], verified_at: i64) -> bool {
    paths
        .iter()
        .any(|state| (state.exists && state.newer) || (!state.exists && verified_at == 0))
}

/// Resolve one written path against the project directory and read the
/// filesystem's answer. A path that cannot be stat'ed counts as missing — the
/// safe direction, because the model is then asked and may say "still fine".
fn inspect(project_dir: &str, written: &str, verified_at: i64) -> PathState {
    let path = resolve_path(project_dir, written);
    let (exists, mtime) = match path.as_deref().map(std::fs::metadata) {
        Some(Ok(meta)) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_millis() as i64);
            (true, mtime)
        }
        _ => (false, None),
    };
    PathState {
        exists: path.is_some() && exists,
        path,
        written: written.to_owned(),
        mtime,
        // Strictly newer: a file whose mtime equals the watermark is not a
        // change. The watermark is the run's START, so anything written during
        // the verification stays newer and is picked up next time.
        newer: mtime.is_some_and(|mtime| mtime > verified_at),
    }
}

/// The absolute path a memory's written path refers to, if it stays inside the
/// project.
///
/// Confined to the project on purpose: the verify prompt prints file contents
/// to a model, and a memory is model-writable, so `~/.ssh/id_rsa.pub`, a
/// `../../etc/passwd` climb, or a symlink pointing out of the tree must not
/// turn the verify task into a file-disclosure primitive. `None` means "outside
/// the project — never read".
fn resolve_path(project_dir: &str, written: &str) -> Option<PathBuf> {
    if written.starts_with('~') {
        return None;
    }
    let root = normalize_lexically(Path::new(project_dir))?;
    let joined = normalize_lexically(&root.join(written))?;
    if !joined.starts_with(&root) {
        return None;
    }
    // A symlink inside the tree can still point out of it, so the real path
    // decides. A path that does not exist has no real path; it stays a
    // "missing" state, which never reads anything.
    match std::fs::canonicalize(&joined) {
        Ok(real) => {
            let real_root = std::fs::canonicalize(&root).unwrap_or(root);
            real.starts_with(&real_root).then_some(real)
        }
        Err(_) => Some(joined),
    }
}

/// Resolve `.` and `..` without touching the filesystem. `None` when `..`
/// climbs above the root, which no valid project path does.
fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// The file paths a memory's text names.
///
/// The reference learns these with `map-memories` (a model call per memory);
/// the MVP reads what the text already says. Two shapes are recognized: a
/// slash path that ends in an extension (`src/api.rs`, `~/.ox/ox.toml`,
/// `/abs/path`) and a bare filename with a known extension (`Cargo.toml`).
/// Both then lose a `:line[:col]` suffix and trailing punctuation. A false
/// positive costs one extra memory in one batch, never a write, so the
/// recognizer errs toward finding paths.
fn referenced_paths(content: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // A clone drops its lazy-DFA cache with this call (see
    // `historian_validate.rs`'s regex section).
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?:~|\.\.?|[\w._\-]+)?/[\w._\-/@]+\.[A-Za-z0-9]{1,8}|\b[\w._\-]+\.(?:rs|toml|lock|md|json|yaml|yml|ts|tsx|js|jsx|sql|go|py|sh|cfg|ini|txt|proto|graphql)",
        )
        .expect("memory path regex")
    })
    .clone();
    let mut seen: Vec<String> = Vec::new();
    for found in re.find_iter(content) {
        let path = found.as_str().trim_end_matches(|c: char| {
            // Trailing punctuation of prose, not part of an extension-bearing
            // path. `:` also ends a `path:line` reference.
            matches!(
                c,
                '.' | ',' | ';' | ':' | ')' | ']' | '}' | '"' | '\'' | '`'
            )
        });
        // `./src/x.rs` and `../src/x.rs` are spelled relative to the project
        // root's parent; the leading dot segments are noise for the store.
        let path = path.strip_prefix("./").unwrap_or(path);
        if path.is_empty() || seen.iter().any(|known| known == path) {
            continue;
        }
        seen.push(path.to_owned());
    }
    seen
}

/// Read at most `chars` characters' worth of bytes (UTF-8 is at most 4 bytes
/// per character) and report whether the file had more. One byte past the
/// bound is read to tell "exactly the bound" from "more".
fn read_head(path: &Path, chars: usize) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::Read as _;
    let cap = chars.saturating_mul(4);
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    let cut = bytes.len() > cap;
    bytes.truncate(cap);
    Ok((bytes, cut))
}

/// The `verify` user prompt: the candidate memories with their path states,
/// then the excerpts of the files that exist.
pub(super) fn render_verify_prompt(project: &str, batch: &[Candidate]) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "## Project: {project}\n\nThe memories below cite files that are missing or newer than the memory. The host read each existing file and printed an excerpt.\n\n<memories>"
    ));
    for candidate in batch {
        parts.push(format!(
            "<memory id=\"{}\" category=\"{}\" updated_at=\"{}\">",
            candidate.id, candidate.category, candidate.updated_at
        ));
        for state in &candidate.paths {
            let verdict = if !state.exists {
                "missing"
            } else if state.newer {
                "newer"
            } else {
                "unchanged"
            };
            parts.push(format!(
                "  <path state=\"{verdict}\">{}</path>",
                state.written
            ));
        }
        parts.push(candidate.content.clone());
        parts.push("</memory>".to_owned());
    }
    parts.push("</memories>".to_owned());

    // One excerpt per unique file, in first-seen order, inside the total
    // budget: a memory citing an unchanged sibling file must not pay for it
    // twice.
    let mut files: Vec<String> = Vec::new();
    let mut seen: Vec<&Path> = Vec::new();
    let mut budget = VERIFY_EXCERPT_TOTAL_CHARS;
    for candidate in batch {
        for state in &candidate.paths {
            let Some(path) = state.path.as_deref() else {
                continue;
            };
            if !state.exists || seen.contains(&path) {
                continue;
            }
            seen.push(path);
            if budget == 0 {
                continue;
            }
            match read_head(path, budget) {
                Ok((bytes, cut)) => {
                    // Bounded before decoding: a cited multi-gigabyte file must
                    // not be read into memory just to keep 4k characters.
                    let text = String::from_utf8_lossy(&bytes);
                    // Char-counted, never byte-indexed: lossy replacement
                    // characters are multi-byte, so a byte slice can split one
                    // and panic.
                    let mut excerpt: String = text
                        .chars()
                        .take(VERIFY_EXCERPT_CHARS.min(budget))
                        .collect();
                    let taken = excerpt.chars().count();
                    if cut || taken < text.chars().count() {
                        excerpt.push_str("\n… (truncated)");
                    }
                    budget = budget.saturating_sub(taken);
                    files.push(format!(
                        "<file path=\"{}\">\n{excerpt}\n</file>",
                        path.display()
                    ));
                }
                Err(error) => {
                    files.push(format!(
                        "<file path=\"{}\">\n(unreadable: {error})\n</file>",
                        path.display()
                    ));
                }
            }
        }
    }
    parts.push("<files>".to_owned());
    parts.extend(files);
    parts.push("</files>".to_owned());
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// verify: the manifest
// ---------------------------------------------------------------------------

/// The parsed `<verify>` manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct VerifyManifest {
    verified: Vec<String>,
    updates: Vec<VerifyUpdate>,
    archives: Vec<VerifyArchive>,
    /// `<skip>` ids ("code cannot decide it"): counted toward coverage and
    /// duplicate checks, and stamped like `verified` so an undecidable memory
    /// leaves the candidate set instead of holding the batch forever.
    skipped: Vec<String>,
}

/// One `<update>`: the replacement text and the model's consolidation flag.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyUpdate {
    id: String,
    content: String,
    consolidation: bool,
}

/// One `<archive>`: the id and the evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyArchive {
    id: String,
    reason: String,
}

/// Parse a `<verify>` manifest, failing CLOSED: no complete root element means
/// the output is truncated or the wrong task's, and nothing is applied (never
/// a prefix). A well-formed but empty manifest is a format miss, a duplicate
/// id is a rejected manifest (across verified/update/archive/skip), ids
/// outside the batch are dropped, and a
/// manifest covering fewer than half the batch is rejected as mostly-wrong —
/// all four are the reference's rules (`manifest-parser.ts`, `verify.ts`).
pub(super) fn parse_verify_manifest(
    text: &str,
    batch: &[Candidate],
) -> Result<VerifyManifest, String> {
    let body = complete_manifest_body(text, "verify")?;
    let mut manifest = VerifyManifest::default();
    for captures in update_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("verify manifest <update> is missing an id")?;
        manifest.updates.push(VerifyUpdate {
            id,
            content: captures.get(2).map_or("", |m| m.as_str()).trim().to_owned(),
            consolidation: attr_of(attrs, "consolidation")
                .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        });
    }
    for captures in archive_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("verify manifest <archive> is missing an id")?;
        manifest.archives.push(VerifyArchive {
            id,
            reason: attr_of(attrs, "reason").unwrap_or_default(),
        });
    }
    for captures in verified_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("verify manifest <verified> is missing an id")?;
        manifest.verified.push(id);
    }
    for captures in skip_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("verify manifest <skip> is missing an id")?;
        manifest.skipped.push(id);
    }

    let batch_ids: Vec<&str> = batch.iter().map(|memory| memory.id.as_str()).collect();
    let mut known: Vec<String> = Vec::new();
    let mut unknown = 0usize;
    let mut error: Option<String> = None;
    manifest
        .verified
        .retain(|id| keep_batch_id(id, &batch_ids, &mut known, &mut unknown, &mut error));
    manifest
        .skipped
        .retain(|id| keep_batch_id(id, &batch_ids, &mut known, &mut unknown, &mut error));
    manifest.updates.retain(|update| {
        keep_batch_id(&update.id, &batch_ids, &mut known, &mut unknown, &mut error)
    });
    manifest.archives.retain(|archive| {
        keep_batch_id(
            &archive.id,
            &batch_ids,
            &mut known,
            &mut unknown,
            &mut error,
        )
    });
    if let Some(error) = error {
        return Err(error);
    }
    if unknown > 0 {
        tracing::warn!(
            unknown,
            "verify manifest named memories outside the batch; dropped"
        );
    }
    if known.is_empty() {
        return Err(format!(
            "verify manifest carried no verdicts for the {} memories of the batch",
            batch_ids.len()
        ));
    }
    // Fewer than half the batch is the shape of a manifest answered for some
    // other prompt; the prompt asks for every id (skip is how "leave it
    // alone" is said), so a real answer covers at least half.
    if known.len() * 2 < batch_ids.len() {
        return Err(format!(
            "verify manifest covers {}/{} batch ids; rejecting a mostly-missing manifest",
            known.len(),
            batch_ids.len()
        ));
    }
    Ok(manifest)
}

/// Apply a parsed `<verify>` manifest through the store, with the safety rules
/// of `dreamer/memory-claim-safety.ts` and `verify.ts` in front of every write:
///
/// - a directive-shaped `PROJECT_RULES` memory refuses BOTH update and archive;
/// - an update is refused when it loses more than half the text unless the
///   model declared it a consolidation;
/// - an empty (or absurdly large) update is not a wipe: the text is left alone
///   and the verdict degrades to a plain re-verify (`verified_at` banked), so
///   the same unsafe rewrite is not re-asked every slot.
///
/// Refusals are logged and counted, never fatal.
pub(super) async fn apply_verify_manifest(
    db: &Db,
    project: &str,
    batch: &[Candidate],
    manifest: &VerifyManifest,
    started_at: i64,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    // A `verified` or `skip` verdict stamps `verified_at` with the run's START,
    // so an unchanged pool is not re-checked on the next slot while anything
    // written during this run stays newer and is picked up then. Memories the
    // manifest omitted keep their old watermark and stay candidates
    // ("omissions keep their old verified_at, the next run picks them up").
    let bank = |ids: &[String]| -> Vec<String> {
        ids.iter()
            .filter(|id| batch.iter().any(|memory| &memory.id == *id))
            // A memory the same manifest also rewrites or archives has a
            // competing verdict; the write wins and banking it first would be
            // noise.
            .filter(|id| {
                !manifest.updates.iter().any(|update| &update.id == *id)
                    && !manifest.archives.iter().any(|archive| &archive.id == *id)
            })
            .cloned()
            .collect()
    };
    let banked = bank(&manifest.verified);
    if !banked.is_empty() {
        report.verified = memory::touch_memories(db, project, &banked, started_at).await?;
    }
    let skip_banked = bank(&manifest.skipped);
    if !skip_banked.is_empty() {
        report.skipped = memory::touch_memories(db, project, &skip_banked, started_at).await?;
    }
    for update in &manifest.updates {
        let Some(original) = batch.iter().find(|memory| memory.id == update.id) else {
            continue;
        };
        if directive_shaped_project_rule(&original.category, &original.content) {
            tracing::warn!(
                memory_id = %update.id,
                verdict = "update",
                reason = "directive-shaped-project-rule",
                "verify safety refusal"
            );
            report.refused += 1;
            continue;
        }
        let content = update.content.trim();
        if content.is_empty() || content.chars().count() > 20_000 {
            // An empty or oversized rewrite is unsafe, but the memory was judged
            // and not contradicted: bank a plain re-verify (text untouched)
            // instead of dropping the verdict, which would re-ask the same
            // unsafe rewrite on every slot.
            tracing::warn!(
                memory_id = %update.id,
                verdict = "update",
                reason = "unsafe-length",
                "verify update refused; banking a re-verify"
            );
            report.verified +=
                memory::touch_memories(db, project, std::slice::from_ref(&update.id), started_at)
                    .await?;
            continue;
        }
        let original_chars = original.content.trim().chars().count();
        if !update.consolidation && content.chars().count() * 2 < original_chars {
            tracing::warn!(
                memory_id = %update.id,
                verdict = "update",
                reason = "content-loss",
                original_chars,
                replacement_chars = content.chars().count(),
                "verify safety refusal"
            );
            report.refused += 1;
            continue;
        }
        // The category is left alone: verify judges truth, not taxonomy.
        if memory::update_memory(db, project, &update.id, content, None).await? {
            report.updated += 1;
        }
    }
    for archive in &manifest.archives {
        let Some(original) = batch.iter().find(|memory| memory.id == archive.id) else {
            continue;
        };
        if directive_shaped_project_rule(&original.category, &original.content) {
            tracing::warn!(
                memory_id = %archive.id,
                verdict = "archive",
                reason = "directive-shaped-project-rule",
                "verify safety refusal"
            );
            report.refused += 1;
            continue;
        }
        let reason = if archive.reason.trim().is_empty() {
            "verify: contradicted by code"
        } else {
            archive.reason.trim()
        };
        if !memory::archive_memories(db, project, std::slice::from_ref(&archive.id), Some(reason))
            .await?
            .is_empty()
        {
            report.archived += 1;
        }
    }
    Ok(report)
}
