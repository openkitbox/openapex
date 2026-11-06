//! The `/sessions` switcher and the rename prompt (upstream
//! `component/dialog-session-list.tsx`, `dialog-session-rename.tsx`).
//!
//! Pure builders over [`Select`]/[`Input`]: the clock and time zone are
//! arguments, so grouping and relative times are testable without a machine
//! clock. Network calls and navigation live in `app` and `main`.

use std::collections::HashSet;

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
    fallback_title(session.title.as_deref(), session.time.created)
}

/// [`title_of`] for any session shape: its title, else the timestamped fallback.
#[must_use]
pub fn fallback_title(title: Option<&str>, created: i64) -> String {
    match title {
        Some(title) => title.to_owned(),
        None => match Timestamp::from_millisecond(created) {
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
/// session being viewed (marked `●`, cursor starts on it). Rows in `running`
/// show a spinner instead (upstream `sessionStatusGutter`).
#[must_use]
pub fn list(
    sessions: &[SessionSummary],
    current: Option<&str>,
    running: &HashSet<String>,
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
                .running(running.contains(&session.id))
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

/// Refresh the running marks of an open list from `running`; `true` when a
/// row changed.
pub fn set_running(select: &mut Select, running: &HashSet<String>) -> bool {
    select.mark_running(|value| running.contains(value))
}

/// `DialogSessionRename`: a prompt prefilled with the row's title.
#[must_use]
pub fn rename(session: &str, title: &str) -> Dialog {
    Dialog::Input(
        Input::new("Rename session", "Session title", session, title.trim()).tag(RENAME_TAG),
    )
}
