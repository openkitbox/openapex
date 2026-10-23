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
pub fn marker_prefix(seconds: i64) -> Option<String> {
    Some(format!("<!-- {} -->\n", format_gap(seconds)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_threshold_and_negative_gaps_have_no_marker() {
        assert_eq!(format_gap(299), None);
        assert_eq!(format_gap(0), None);
        assert_eq!(format_gap(-1), None);
        assert_eq!(format_gap(-86_400), None);
    }

    #[test]
    fn minute_band_floors() {
        assert_eq!(format_gap(300).as_deref(), Some("+5m"));
        assert_eq!(format_gap(3599).as_deref(), Some("+59m"));
    }

    #[test]
    fn hour_band_floors_and_drops_a_zero_minute_part() {
        assert_eq!(format_gap(3600).as_deref(), Some("+1h"));
        assert_eq!(format_gap(3900).as_deref(), Some("+1h 5m"));
        assert_eq!(format_gap(86_399).as_deref(), Some("+23h 59m"));
    }

    #[test]
    fn day_band_floors_and_drops_a_zero_hour_part() {
        assert_eq!(format_gap(86_400).as_deref(), Some("+1d"));
        assert_eq!(format_gap(90_000).as_deref(), Some("+1d 1h"));
    }

    #[test]
    fn week_band_floors_and_drops_a_zero_day_part() {
        assert_eq!(format_gap(604_800).as_deref(), Some("+1w"));
        assert_eq!(format_gap(691_200).as_deref(), Some("+1w 1d"));
    }

    #[test]
    fn marker_prefix_wraps_the_gap_in_an_html_comment() {
        assert_eq!(marker_prefix(3900).as_deref(), Some("<!-- +1h 5m -->\n"));
        assert_eq!(marker_prefix(60), None);
    }
}
