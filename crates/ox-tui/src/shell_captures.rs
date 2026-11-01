//! Background shell output for expanded shell blocks: upstream
//! `ShellDisplay`'s `loadBackgroundOutput` scheduling and its output source
//! rule (`routes/session/index.tsx:3014-3078`). View state owned by `App`,
//! never by `DataStore` (plan D4): losing it only costs a re-fetch.
//!
//! Pure: callers pass the clock in, so every scheduling rule is tested with
//! fixed instants. `App` asks [`ShellCaptures::poll`] once per UI tick for
//! each expanded, bound shell call whose block draws the capture
//! ([`shows_capture`]); the executor answers each [`Poll`] with
//! one `AppEvent::ShellPolled`, merged by [`ShellCaptures::merge`]. The
//! pipeline reads the result through [`ShellCaptures::shown`].
//!
//! Rules:
//! - Running: one page per [`POLL_INTERVAL`] at most, never while a request
//!   is in flight. Upstream polls on a 1 s timer; ox reuses the 33 ms UI tick
//!   and throttles (plan V3).
//! - Not running: one drain (read up to the capture's size, jumping to the
//!   newest [`DISPLAY_LIMIT`]) when the shell was seen running or was never
//!   fetched; then nothing.
//! - An answer merges only into the request it answers: the capture must be
//!   waiting and the answer's start cursor must equal the capture's cursor.
//! - Over [`DISPLAY_LIMIT`] the capture keeps its tail and is marked
//!   truncated (`[earlier output omitted]`).
//!
//! Deviations:
//! - Collapsed blocks are never polled (plan Task 5). Upstream also polls a
//!   collapsed foreground call while its shell runs; ox shows that call's
//!   tool result until it is expanded.
//! - The capture stores the raw text; ANSI is stripped when the block is
//!   drawn (`shell_block`), so an escape sequence split across pages is
//!   still removed. The limit therefore counts raw bytes, where upstream
//!   counts UTF-16 units of the stripped text.
//! - A failed request is not retried at once: running shells retry on the
//!   next interval, and a failed drain is not repeated (upstream likewise
//!   drains once).
//! - With no capture yet, a running foreground call shows its tool result
//!   (upstream `backgroundOutput() || props.output`; ox's result text is the
//!   progress output). A background call shows nothing until bytes arrive:
//!   its result text is the model-facing "still running" notice.
//! - A failed call shows its failure message, never the capture.
//! - A completed foreground call shows its tool result, and is therefore
//!   never polled. Upstream `Shell` passes `output={undefined}` for every
//!   call with a `shellID` and always shows the fetched shell output; ox's
//!   result text for a finished foreground call is that same output, so it
//!   is kept and the fetch skipped.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ox_client::rest::{ShellRead, ShellReadMode};
use ox_client::store::{ShellRef, ToolPart, ToolState};

/// Newest bytes a capture keeps, and the page size of each read (upstream
/// `SHELL_DISPLAY_LIMIT`).
pub const DISPLAY_LIMIT: usize = 1024 * 1024;
/// Least time between two requests for one shell (upstream's 1 s interval).
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What one shell's block has fetched so far.
#[derive(Debug, Default)]
pub struct Capture {
    /// Byte offset the next read starts at.
    cursor: u64,
    /// The newest output, shared with the blocks drawn from it. Replaced,
    /// never mutated, so block equality can compare pointers.
    text: Arc<String>,
    /// Output before `text` exists but is not held.
    truncated: bool,
    last_request: Option<Instant>,
    in_flight: bool,
    /// A request was ever sent.
    fetched: bool,
    /// The shell was running at a poll since the last drain.
    seen_running: bool,
}

/// One request to send: read shell `id` from `cursor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poll {
    pub id: String,
    pub cursor: u64,
    pub mode: ShellReadMode,
}

/// The text a shell block shows and whether its head was dropped.
#[derive(Debug, Clone)]
pub struct ShellText {
    pub text: Arc<String>,
    pub omitted: bool,
}

/// Every capture of the open session, keyed by shell id.
#[derive(Debug, Default)]
pub struct ShellCaptures {
    captures: HashMap<String, Capture>,
    /// Shown by a background call before its first bytes; shared so the
    /// block stays equal across projections.
    empty: Arc<String>,
}

/// Upstream U4: whether a bound call's block reads the capture rather than
/// its tool result. A completed foreground call has its whole output in the
/// result; a failed one shows its failure message.
#[must_use]
pub fn shows_capture(state: ToolState, background: bool) -> bool {
    match state {
        ToolState::Running | ToolState::Progress => true,
        ToolState::Success => background,
        ToolState::Input | ToolState::Failed => false,
    }
}

impl ShellCaptures {
    /// The request an expanded, bound shell call needs at `now`, if any.
    /// `running` is `DataStore::is_shell_running` for `id`.
    pub fn poll(&mut self, id: &str, running: bool, now: Instant) -> Option<Poll> {
        // Called every tick: look up before inserting, so a known shell costs
        // no key allocation.
        if !self.captures.contains_key(id) {
            self.captures.insert(id.to_owned(), Capture::default());
        }
        let capture = self
            .captures
            .get_mut(id)
            .expect("the capture was inserted above");
        capture.seen_running |= running;
        if capture.in_flight {
            return None;
        }
        let mode = if running {
            let recent = capture
                .last_request
                .is_some_and(|at| now.saturating_duration_since(at) < POLL_INTERVAL);
            if recent {
                return None;
            }
            ShellReadMode::Page
        } else if capture.seen_running || !capture.fetched {
            capture.seen_running = false;
            ShellReadMode::Drain
        } else {
            return None;
        };
        capture.fetched = true;
        capture.in_flight = true;
        capture.last_request = Some(now);
        Some(Poll {
            id: id.to_owned(),
            cursor: capture.cursor,
            mode,
        })
    }

    /// Apply the answer to the request that read `id` from `start`; `None`
    /// is a failed request. Returns whether the shown text changed.
    #[must_use]
    pub fn merge(&mut self, id: &str, start: u64, read: Option<ShellRead>) -> bool {
        let Some(capture) = self.captures.get_mut(id) else {
            return false;
        };
        if !capture.in_flight || capture.cursor != start {
            return false;
        }
        capture.in_flight = false;
        let Some(read) = read else {
            return false;
        };
        capture.cursor = read.end;
        let unchanged = if read.skipped {
            read.text.is_empty() && capture.text.is_empty() && capture.truncated
        } else {
            read.text.is_empty()
        };
        if unchanged {
            return false;
        }
        // Bytes were skipped between the held text and the new text, so the
        // held text is no longer the output's contiguous tail.
        let mut text = if read.skipped {
            read.text
        } else {
            let mut joined = String::with_capacity(capture.text.len() + read.text.len());
            joined.push_str(&capture.text);
            joined.push_str(&read.text);
            joined
        };
        capture.truncated |= read.skipped;
        if text.len() > DISPLAY_LIMIT {
            let mut cut = text.len() - DISPLAY_LIMIT;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            text.drain(..cut);
            capture.truncated = true;
        }
        capture.text = Arc::new(text);
        true
    }

    /// The text `tool`'s block shows (upstream `output()` for a bound call).
    #[must_use]
    pub fn shown(&self, tool: &ToolPart, shell: Option<&ShellRef>) -> ShellText {
        let result = || ShellText {
            text: Arc::clone(&tool.output),
            omitted: false,
        };
        let Some(shell) = shell else {
            return result();
        };
        if !shows_capture(tool.state, shell.background) {
            return result();
        }
        match self.captures.get(&shell.id) {
            Some(capture) if !capture.text.is_empty() || capture.truncated => ShellText {
                text: Arc::clone(&capture.text),
                omitted: capture.truncated,
            },
            _ if shell.background => ShellText {
                text: Arc::clone(&self.empty),
                omitted: false,
            },
            _ => result(),
        }
    }
}
