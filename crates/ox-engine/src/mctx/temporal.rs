//! Temporal awareness: the gap markers between authored turns.
//!
//! Port of magic-context
//! `packages/plugin/src/hooks/magic-context/temporal-awareness.ts`:
//! [`TEMPORAL_THRESHOLD_SECONDS`] from `TEMPORAL_AWARENESS_THRESHOLD_SECONDS`,
//! [`format_gap`] from `formatGap`, and [`marker_prefix`] from
//! `temporalMarkerPrefix`. The tabulated branches (minute/hour/day/week
//! boundaries, `+Xh Ym` → `+Xh` when the remainder is zero) and the HTML
//! comment wrapper are byte-for-byte upstream.
//!
//! Upstream persists the first injection decision per message and replays it
//! because its compaction can remove the predecessor. ox needs none of that:
//! the marker is applied while the request is assembled
//! (`runner::step::build_chat_messages`) and never stored, so re-rendering the
//! same rows yields the same bytes and there is no double-injection path.
//!
//! Compartment headings already carry their date range through
//! `mctx::decay_render`; this module is the per-message half only.

/// User-message gaps below this many seconds get no marker. Upstream
/// `TEMPORAL_AWARENESS_THRESHOLD_SECONDS` (5 minutes).
pub const TEMPORAL_THRESHOLD_SECONDS: i64 = 300;

const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 60 * 60;
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;
const SECONDS_PER_WEEK: i64 = 7 * 24 * 60 * 60;

/// Format a gap in seconds as the compact adaptive string upstream emits:
///
/// - below [`TEMPORAL_THRESHOLD_SECONDS`] (and for negative input) → `None`;
/// - `< 1h` → `+{m}m`;
/// - `< 1d` → `+{h}h {m}m`, or `+{h}h` when the minute part is 0;
/// - `< 1w` → `+{d}d {h}h`, or `+{d}d` when the hour part is 0;
/// - else → `+{w}w {d}d`, or `+{w}w` when the day part is 0.
///
/// Every unit is floored, never rounded.
#[must_use]
pub fn format_gap(seconds: i64) -> Option<String> {
    if seconds < TEMPORAL_THRESHOLD_SECONDS {
        return None;
    }

    if seconds < SECONDS_PER_HOUR {
        let minutes = seconds / SECONDS_PER_MINUTE;
        return Some(format!("+{minutes}m"));
    }

    if seconds < SECONDS_PER_DAY {
        let hours = seconds / SECONDS_PER_HOUR;
        let minutes = (seconds - hours * SECONDS_PER_HOUR) / SECONDS_PER_MINUTE;
        return Some(if minutes == 0 {
            format!("+{hours}h")
        } else {
            format!("+{hours}h {minutes}m")
        });
    }

    if seconds < SECONDS_PER_WEEK {
        let days = seconds / SECONDS_PER_DAY;
        let hours = (seconds - days * SECONDS_PER_DAY) / SECONDS_PER_HOUR;
        return Some(if hours == 0 {
            format!("+{days}d")
        } else {
            format!("+{days}d {hours}h")
        });
    }

    let weeks = seconds / SECONDS_PER_WEEK;
    let days = (seconds - weeks * SECONDS_PER_WEEK) / SECONDS_PER_DAY;
    Some(if days == 0 {
        format!("+{weeks}w")
    } else {
        format!("+{weeks}w {days}d")
    })
}

/// The HTML comment line that precedes a user turn (`<!-- +1h 5m -->\n`), or
/// `None` when the gap is below the threshold. Upstream `temporalMarkerPrefix`.
#[must_use]
pub fn marker_prefix(seconds: i64) -> Option<String> {
    Some(format!("<!-- {} -->\n", format_gap(seconds)?))
}
