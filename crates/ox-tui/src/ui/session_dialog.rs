//! The `/sessions` switcher and the rename prompt (upstream
//! `component/dialog-session-list.tsx`, `dialog-session-rename.tsx`).
//!
//! Pure builders over [`Select`]/[`Input`]: the clock and time zone are
//! arguments, so grouping and relative times are testable without a machine
//! clock. Network calls and navigation live in `app` and `main`.

use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use ox_client::rest::SessionSummary;

use super::dialog::{Dialog, Hint, Input, Item, Select};

/// `Dialog::tag` of the session list.
pub const LIST_TAG: &str = "sessions";
/// `Dialog::tag` of the rename prompt.
pub const RENAME_TAG: &str = "rename-session";
/// Row action keys (`ctrl+<key>`).
pub const DELETE_KEY: char = 'd';
pub const RENAME_KEY: char = 'r';

/// Upstream's `withTimestampedFallback`: a session nobody named is shown as
/// `New session - <created, ISO-8601 UTC>`.
fn title_of(session: &SessionSummary) -> String {
    match &session.title {
        Some(title) => title.clone(),
        None => match Timestamp::from_millisecond(session.time.created) {
            Ok(created) => format!(
                "New session - {}",
                created
                    .to_zoned(TimeZone::UTC)
                    .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
            ),
            Err(_) => "New session".to_owned(),
        },
    }
}

/// `Today`, else the JS `toDateString()` form (`Fri Oct 31 2026`).
fn category(updated: &Zoned, now: &Zoned) -> String {
    if updated.date() == now.date() {
        "Today".to_owned()
    } else {
        updated.strftime("%a %b %d %Y").to_string()
    }
}

/// How long ago `then` was, coarsely: `just now`, `5m ago`, `3h ago`,
/// `2d ago`, and the date once it is a week or more.
fn relative(now: &Zoned, then: &Zoned) -> String {
    let seconds = now.timestamp().as_second() - then.timestamp().as_second();
    match seconds {
        ..60 => "just now".to_owned(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..604_800 => format!("{}d ago", seconds / 86_400),
        _ => then.strftime("%Y-%m-%d").to_string(),
    }
}

/// The session list, newest first as the server sent it. `current` is the
/// session being viewed (marked `●`, cursor starts on it).
pub fn list(
    sessions: &[SessionSummary],
    current: Option<&str>,
    now: Timestamp,
    tz: &TimeZone,
) -> Dialog {
    let now = now.to_zoned(tz.clone());
    let items = sessions
        .iter()
        .map(|session| {
            let updated = Timestamp::from_millisecond(session.time.updated)
                .unwrap_or(Timestamp::UNIX_EPOCH)
                .to_zoned(tz.clone());
            Item::new(session.id.as_str(), title_of(session))
                .category(category(&updated, &now))
                .footer(relative(&now, &updated))
        })
        .collect();
    let mut select = Select::new("Sessions", items)
        .large()
        .tag(LIST_TAG)
        .action(DELETE_KEY, Some("Press ctrl+d again to confirm"))
        .action(RENAME_KEY, None)
        .hints(vec![
            Hint::new("delete", "ctrl+d"),
            Hint::new("rename", "ctrl+r"),
        ]);
    if let Some(current) = current {
        select = select.current(current);
    }
    Dialog::Select(select)
}

/// `DialogSessionRename`: a prompt prefilled with the row's title.
pub fn rename(session: &str, title: &str) -> Dialog {
    Dialog::Input(
        Input::new("Rename session", "Session title", session, title.trim()).tag(RENAME_TAG),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::dialog::DialogOutcome;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ox_client::rest::SessionTime;

    const DAY: i64 = 86_400_000;
    /// 2026-10-31T12:00:00Z
    const NOW: i64 = 1_793_448_000_000;

    fn summary(id: &str, title: Option<&str>, created: i64, updated: i64) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            title: title.map(String::from),
            time: SessionTime {
                created,
                updated,
                ..SessionTime::default()
            },
            location: None,
        }
    }

    fn now() -> Timestamp {
        Timestamp::from_millisecond(NOW).unwrap()
    }

    fn select(dialog: &Dialog) -> &Select {
        match dialog {
            Dialog::Select(select) => select,
            other => panic!("expected a select dialog: {other:?}"),
        }
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn sample() -> Dialog {
        list(
            &[
                summary(
                    "ses_a",
                    Some("Fix the parser"),
                    NOW - 3 * DAY,
                    NOW - 5 * 60_000,
                ),
                summary("ses_b", None, NOW - 4 * DAY, NOW - 3 * 3_600_000),
                summary("ses_c", Some("Old work"), NOW - 20 * DAY, NOW - 9 * DAY),
            ],
            Some("ses_b"),
            now(),
            &TimeZone::UTC,
        )
    }

    #[test]
    fn rows_carry_title_group_and_relative_time() {
        let dialog = sample();
        let select = select(&dialog);
        let a = select.item("ses_a").unwrap();
        assert_eq!(a.title, "Fix the parser");
        assert_eq!(a.category.as_deref(), Some("Today"));
        assert_eq!(a.footer.as_deref(), Some("5m ago"));
        let b = select.item("ses_b").unwrap();
        assert_eq!(b.footer.as_deref(), Some("3h ago"));
        let c = select.item("ses_c").unwrap();
        assert_eq!(c.category.as_deref(), Some("Thu Oct 22 2026"));
        assert_eq!(c.footer.as_deref(), Some("2026-10-22"));
    }

    #[test]
    fn an_unnamed_session_gets_upstreams_timestamped_fallback() {
        let dialog = sample();
        let b = select(&dialog).item("ses_b").unwrap();
        assert_eq!(b.title, "New session - 2026-10-27T12:00:00.000Z");
    }

    #[test]
    fn the_dialog_starts_on_the_current_session_and_is_wide() {
        let dialog = sample();
        assert_eq!(select(&dialog).selected_value(), Some("ses_b"));
        assert_eq!(dialog.tag(), Some(LIST_TAG));
    }

    #[test]
    fn relative_times_step_through_the_units() {
        let at = |offset_ms: i64| {
            let then = Timestamp::from_millisecond(NOW - offset_ms)
                .unwrap()
                .to_zoned(TimeZone::UTC);
            relative(&now().to_zoned(TimeZone::UTC), &then)
        };
        assert_eq!(at(0), "just now");
        assert_eq!(at(59_000), "just now");
        assert_eq!(at(60_000), "1m ago");
        assert_eq!(at(3_599_000), "59m ago");
        assert_eq!(at(3_600_000), "1h ago");
        assert_eq!(at(DAY), "1d ago");
        assert_eq!(at(6 * DAY), "6d ago");
        assert_eq!(at(7 * DAY), "2026-10-24");
    }

    #[test]
    fn yesterday_is_a_dated_group_not_today() {
        // 00:30 UTC on the 31st vs 23:30 UTC on the 30th: one hour apart, two days.
        let early = Timestamp::from_millisecond(NOW - 11 * 3_600_000 - 30 * 60_000).unwrap();
        let before = Timestamp::from_millisecond(NOW - 12 * 3_600_000 - 30 * 60_000).unwrap();
        let tz = TimeZone::UTC;
        let now = early.to_zoned(tz.clone());
        assert_eq!(category(&now, &now), "Today");
        assert_eq!(category(&before.to_zoned(tz), &now), "Fri Oct 30 2026");
    }

    #[test]
    fn ctrl_d_asks_twice_and_moving_cancels_the_confirmation() {
        let mut dialog = sample();
        // First press arms the row: nothing is deleted yet.
        assert_eq!(dialog.on_key(ctrl('d')), DialogOutcome::Changed);
        // Moving the cursor disarms it, so the next press arms again.
        dialog.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(dialog.on_key(ctrl('d')), DialogOutcome::Changed);
        // Second press on the same row deletes it.
        assert_eq!(
            dialog.on_key(ctrl('d')),
            DialogOutcome::Action {
                key: 'd',
                value: "ses_a".into()
            }
        );
        // …and the confirmation is spent.
        assert_eq!(dialog.on_key(ctrl('d')), DialogOutcome::Changed);
    }

    #[test]
    fn ctrl_r_renames_the_selected_row_at_once() {
        let mut dialog = sample();
        assert_eq!(
            dialog.on_key(ctrl('r')),
            DialogOutcome::Action {
                key: 'r',
                value: "ses_b".into()
            }
        );
    }

    #[test]
    fn typing_after_arming_clears_the_confirmation() {
        let mut dialog = sample();
        dialog.on_key(ctrl('d'));
        dialog.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(dialog.on_key(ctrl('d')), DialogOutcome::Changed);
    }

    #[test]
    fn the_rename_prompt_is_prefilled_and_submits_its_target() {
        let mut dialog = rename("ses_a", "  Fix the parser ");
        assert_eq!(dialog.tag(), Some(RENAME_TAG));
        dialog.on_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(
            dialog.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DialogOutcome::Submitted {
                target: "ses_a".into(),
                text: "Fix the parser!".into()
            }
        );
        assert_eq!(
            dialog.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            DialogOutcome::Cancelled
        );
    }

    #[test]
    fn removing_a_row_keeps_the_cursor_in_range() {
        let mut dialog = sample();
        let Dialog::Select(select) = &mut dialog else {
            unreachable!()
        };
        select.remove("ses_b");
        assert!(select.item("ses_b").is_none());
        assert!(select.selected_value().is_some());
    }
}
