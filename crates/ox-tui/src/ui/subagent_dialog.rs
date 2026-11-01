//! The subagent picker (upstream `composer/subagents-tab.tsx`).
//!
//! A pure builder over [`Select`]: the session family as the server reported
//! it goes in, a ready-to-open dialog comes out. Which rows are shown
//! (running or not) is the caller's state; `ctrl+a` flips it and the caller
//! rebuilds the dialog from the same members.
//!
//! Deviations from upstream: a dialog instead of the composer's bottom panel,
//! and no `@x subagent` title rewrite (ox titles carry no such marker).

use ox_client::rest::FamilyMember;

use super::dialog::{Dialog, Hint, Item, Select};
use super::family::{self, FamilyEntry};
use super::session_dialog::fallback_title;
use super::timeline::titlecase;

/// `Dialog::tag` of the picker.
pub const TAG: &str = "subagents";
/// Row action key (`ctrl+<key>`): flip between running and finished rows.
pub const TOGGLE_KEY: char = 'a';
/// Row action key (`ctrl+<key>`): interrupt the selected running subagent.
pub const INTERRUPT_KEY: char = 'd';

fn row(entry: &FamilyEntry<'_>) -> Item {
    let member = entry.session;
    let agent = member
        .agent
        .as_deref()
        .map_or_else(|| "Subagent".to_owned(), titlecase);
    let title = fallback_title(member.title.as_deref(), member.time.created);
    let item = Item::new(
        member.id.as_str(),
        format!("{}{agent}: {title}", entry.prefix),
    );
    if member.running {
        item.footer("Running")
    } else {
        item
    }
}

/// The picker for the family around `current`. With `active_only` it lists
/// the running subagents, otherwise the finished ones.
pub fn dialog(members: &[FamilyMember], current: &str, active_only: bool) -> Dialog {
    let entries = family::family(members, current);
    let items: Vec<Item> = entries
        .iter()
        .filter(|entry| entry.session.running == active_only)
        .map(row)
        .collect();
    let has_current = items.iter().any(|item| item.value == current);
    let empty = if active_only {
        "No active subagents"
    } else {
        "No inactive subagents"
    };
    let toggle = if active_only {
        "show inactive"
    } else {
        "show active"
    };
    // Only the running list can interrupt: a finished row has nothing to stop.
    let mut hints = vec![Hint::new(toggle, "ctrl+a")];
    let mut select = Select::new("Subagents", items)
        .large()
        .tag(TAG)
        .rowless_action(TOGGLE_KEY)
        .empty_text(empty);
    if active_only {
        select = select.action(INTERRUPT_KEY, None);
        hints.push(Hint::new("interrupt", "ctrl+d"));
    }
    select = select.hints(hints);
    if has_current {
        select = select.current(current);
    }
    Dialog::Select(select)
}
