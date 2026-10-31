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

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str, end: u64) -> Option<ShellRead> {
        Some(ShellRead {
            text: text.to_owned(),
            end,
            skipped: false,
        })
    }

    fn page(id: &str, cursor: u64) -> Option<Poll> {
        Some(Poll {
            id: id.to_owned(),
            cursor,
            mode: ShellReadMode::Page,
        })
    }

    fn drain(id: &str, cursor: u64) -> Option<Poll> {
        Some(Poll {
            id: id.to_owned(),
            cursor,
            mode: ShellReadMode::Drain,
        })
    }

    fn text(captures: &ShellCaptures, id: &str) -> String {
        captures.captures[id].text.as_str().to_owned()
    }

    #[test]
    fn a_running_shell_is_polled_at_most_once_a_second() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        assert_eq!(
            captures.poll("sh", true, t0),
            page("sh", 0),
            "first poll at once"
        );
        assert!(captures.merge("sh", 0, read("a", 1)));
        let soon = t0 + Duration::from_millis(999);
        assert_eq!(captures.poll("sh", true, soon), None, "throttled");
        assert_eq!(
            captures.poll("sh", true, t0 + POLL_INTERVAL),
            page("sh", 1),
            "the next page continues at the merged cursor"
        );
    }

    #[test]
    fn no_request_goes_out_while_one_is_in_flight() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        assert!(captures.poll("sh", true, t0).is_some());
        let later = t0 + 3 * POLL_INTERVAL;
        assert_eq!(captures.poll("sh", true, later), None, "still waiting");
        assert_eq!(
            captures.poll("sh", false, later),
            None,
            "even once it exits"
        );
        assert!(
            !captures.merge("sh", 0, None),
            "a failure shows nothing new"
        );
        assert_eq!(
            captures.poll("sh", false, later),
            drain("sh", 0),
            "the exit seen while waiting drains once the answer is in"
        );
    }

    #[test]
    fn an_answer_for_another_request_is_dropped() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        assert!(captures.poll("sh", true, t0).is_some());
        assert!(!captures.merge("sh", 7, read("stale", 9)), "wrong start");
        assert!(!captures.merge("nope", 0, read("x", 1)), "unknown shell");
        assert!(captures.merge("sh", 0, read("ok", 2)));
        assert!(
            !captures.merge("sh", 0, read("ok", 2)),
            "a duplicate after the answer is late"
        );
        assert_eq!(text(&captures, "sh"), "ok");
        assert_eq!(captures.captures["sh"].cursor, 2);
    }

    #[test]
    fn a_shell_that_stops_running_is_drained_once() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        assert!(captures.poll("sh", true, t0).is_some());
        assert!(captures.merge("sh", 0, read("a", 1)));
        let exited = t0 + Duration::from_millis(100);
        assert_eq!(
            captures.poll("sh", false, exited),
            drain("sh", 1),
            "the exit drains without waiting for the interval"
        );
        assert!(captures.merge("sh", 1, read("b", 2)));
        assert_eq!(text(&captures, "sh"), "ab");
        assert_eq!(captures.poll("sh", false, t0 + 10 * POLL_INTERVAL), None);
    }

    #[test]
    fn a_finished_shell_never_fetched_is_drained_once_even_if_it_fails() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        assert_eq!(captures.poll("sh", false, t0), drain("sh", 0));
        assert!(!captures.merge("sh", 0, None));
        assert_eq!(captures.poll("sh", false, t0 + 10 * POLL_INTERVAL), None);
    }

    #[test]
    fn the_capture_keeps_its_newest_megabyte_on_a_char_boundary() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        assert!(captures.poll("sh", true, t0).is_some());
        // 'é' is two bytes and starts at odd offsets here, so the cut at
        // byte 18 lands inside one.
        let big = format!("x{}z", "é".repeat(DISPLAY_LIMIT / 2 + 8));
        let size = big.len() as u64;
        assert!(captures.merge("sh", 0, read(&big, size)));
        let kept = text(&captures, "sh");
        assert!(kept.len() <= DISPLAY_LIMIT, "kept {} bytes", kept.len());
        assert!(kept.len() > DISPLAY_LIMIT - 4, "kept {} bytes", kept.len());
        assert!(big.ends_with(&kept), "the tail is kept");
        assert!(captures.captures["sh"].truncated);
    }

    #[test]
    fn a_skipped_read_marks_the_capture_truncated() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        assert_eq!(captures.poll("sh", false, t0), drain("sh", 0));
        let skipped = Some(ShellRead {
            text: "tail".into(),
            end: 9_000_000,
            skipped: true,
        });
        assert!(captures.merge("sh", 0, skipped));
        assert!(captures.captures["sh"].truncated);
        assert_eq!(text(&captures, "sh"), "tail");
    }

    #[test]
    fn only_new_bytes_replace_the_shared_text() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        assert!(captures.poll("sh", true, t0).is_some());
        assert!(captures.merge("sh", 0, read("a", 1)));
        let before = Arc::clone(&captures.captures["sh"].text);
        assert!(captures.poll("sh", true, t0 + POLL_INTERVAL).is_some());
        assert!(!captures.merge("sh", 1, read("", 1)), "no new bytes");
        assert!(Arc::ptr_eq(&before, &captures.captures["sh"].text));
        assert!(captures.poll("sh", true, t0 + 2 * POLL_INTERVAL).is_some());
        assert!(captures.merge("sh", 1, read("b", 2)));
        assert!(!Arc::ptr_eq(&before, &captures.captures["sh"].text));
    }

    fn call(state: ToolState, background: bool) -> (ToolPart, ShellRef) {
        let tool = ToolPart {
            id: "part".into(),
            tool: ShellRef::SHELL_TOOL.into(),
            state,
            output: Arc::new("result".into()),
            ..Default::default()
        };
        let shell = ShellRef {
            id: "sh".into(),
            background,
        };
        (tool, shell)
    }

    #[test]
    fn the_block_shows_the_capture_the_result_or_nothing() {
        let t0 = Instant::now();
        let mut captures = ShellCaptures::default();
        let (done, bound) = call(ToolState::Success, false);
        let shown = captures.shown(&done, Some(&bound));
        assert!(Arc::ptr_eq(&shown.text, &done.output), "result shared");
        assert!(!shown.omitted);

        let (running, bound) = call(ToolState::Running, false);
        let shown = captures.shown(&running, Some(&bound));
        assert!(
            Arc::ptr_eq(&shown.text, &running.output),
            "no bytes yet: the progress result"
        );
        let (background, bound) = call(ToolState::Success, true);
        assert_eq!(captures.shown(&background, Some(&bound)).text.as_str(), "");
        assert!(
            Arc::ptr_eq(
                &captures.shown(&background, Some(&bound)).text,
                &captures.shown(&background, Some(&bound)).text
            ),
            "the empty text is shared, so the block stays equal"
        );

        assert!(captures.poll("sh", true, t0).is_some());
        assert!(captures.merge(
            "sh",
            0,
            Some(ShellRead {
                text: "live".into(),
                end: 9,
                skipped: true,
            })
        ));
        let shown = captures.shown(&background, Some(&bound));
        assert!(Arc::ptr_eq(&shown.text, &captures.captures["sh"].text));
        assert!(shown.omitted, "a dropped head is announced");
        let unbound = captures.shown(&background, None);
        assert!(Arc::ptr_eq(&unbound.text, &background.output));
    }

    #[test]
    fn the_source_follows_the_call_state() {
        use ToolState::*;
        let table = [
            (
                Success,
                false,
                false,
                "a finished foreground call has its output",
            ),
            (Success, true, true, "a background call reads the capture"),
            (Running, false, true, "a running call reads the capture"),
            (Progress, false, true, "a running call reads the capture"),
            (Failed, true, false, "a failure shows its message"),
            (Input, false, false, "a streaming call shows nothing anyway"),
        ];
        for (state, background, capture, why) in table {
            assert_eq!(
                shows_capture(state, background),
                capture,
                "{state:?}: {why}"
            );
        }
    }
}
