//! Prompt history: the last submitted prompts, walked with Up/Down.
//!
//! Port of upstream `prompt/history.tsx`. The JSONL line shape
//! (`{"text", "files", "agents", "skills", "pasted"}`), the 50-entry cap, the
//! consecutive-duplicate rule and the "append or rewrite when trimmed"
//! persistence are kept; ox only has text prompts, so the part arrays are
//! always written empty and ignored when read.
//!
//! Deviation: upstream lets `move` continue when the input equals the entry at
//! `index 0` (the *oldest* entry, an artefact of `Array.at(0)`); here a fresh
//! walk starts only from an empty input, and later steps only while the input
//! still equals the entry being shown.
//!
//! Persistence is best-effort and synchronous (one small append per submit);
//! the path is a parameter so tests use a tempdir. The file is private to the
//! user (0600, directory 0700). A rewrite goes through a temp file and a
//! rename, so a second instance never reads a half-written file; its own
//! appends in the rewrite window can still be lost, which is accepted.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Upstream `MAX_HISTORY_ENTRIES`.
pub const MAX_ENTRIES: usize = 50;

/// Default location: `~/.local/state/ox-tui/history.jsonl`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".local/state/ox-tui/history.jsonl"))
}

#[derive(Debug, Default)]
pub struct History {
    entries: Vec<String>,
    /// `0` = not browsing; `-n` = the n-th entry from the end is shown.
    index: isize,
    path: Option<PathBuf>,
}

impl History {
    /// History that is never written to disk.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Read `path` (missing or corrupt lines are skipped), keep the newest
    /// [`MAX_ENTRIES`], and rewrite the file so corruption heals.
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let entries = parse(&text);
        let history = Self {
            entries,
            index: 0,
            path: Some(path),
        };
        if !history.entries.is_empty() {
            history.rewrite();
        }
        history
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Step through history. `direction` is `-1` (older) or `1` (newer);
    /// `input` is the current editor text. Returns the text to show, or `None`
    /// when the step is not allowed (typed text in the way, or an end reached).
    pub fn step(&mut self, direction: isize, input: &str) -> Option<String> {
        let shown = self.shown();
        if !input.is_empty() && input != shown {
            return None;
        }
        let next = self.index + direction;
        if next > 0 || next.unsigned_abs() > self.entries.len() {
            return None;
        }
        self.index = next;
        Some(self.shown().to_owned())
    }

    /// Record a submitted prompt and leave browsing mode.
    pub fn append(&mut self, text: &str) {
        self.index = 0;
        if text.is_empty() || self.entries.last().is_some_and(|last| last == text) {
            return;
        }
        self.entries.push(text.to_owned());
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
            self.rewrite();
            return;
        }
        self.append_line(text);
    }

    /// Entry currently shown while browsing; empty when not browsing.
    fn shown(&self) -> &str {
        if self.index == 0 {
            return "";
        }
        let at = self.entries.len() as isize + self.index;
        usize::try_from(at)
            .ok()
            .and_then(|at| self.entries.get(at))
            .map_or("", String::as_str)
    }

    fn rewrite(&self) {
        let Some(path) = &self.path else { return };
        let mut body = String::new();
        for entry in &self.entries {
            body.push_str(&line(entry));
            body.push('\n');
        }
        let _ = ensure_parent(path).and_then(|()| replace_private(path, &body));
    }

    fn append_line(&self, text: &str) {
        let Some(path) = &self.path else { return };
        let _ = ensure_parent(path).and_then(|()| {
            let mut file = private_options().create(true).append(true).open(path)?;
            writeln!(file, "{}", line(text))
        });
    }
}

fn ensure_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent)
    }
}

/// Open options for a file only the owner may read.
fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Write `body` next to `path` and rename it over, so readers see the old or
/// the new file, never a partial one.
fn replace_private(path: &Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let written = private_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut file| file.write_all(body.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn line(text: &str) -> String {
    json!({ "text": text, "files": [], "agents": [], "skills": [], "pasted": [] }).to_string()
}

/// Upstream `parsePromptHistory` + `parsePromptInfo`: a line needs a string
/// `text` and a `pasted` array. Only the newest [`MAX_ENTRIES`] survive.
fn parse(text: &str) -> Vec<String> {
    let mut entries: Vec<String> = text
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| {
            value.get("pasted")?.as_array()?;
            Some(value.get("text")?.as_str()?.to_owned())
        })
        .collect();
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    entries
}
