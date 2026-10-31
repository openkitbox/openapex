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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::dialog::DialogOutcome;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ox_client::rest::SessionTime;

    fn member(
        id: &str,
        parent: Option<&str>,
        agent: Option<&str>,
        title: Option<&str>,
        running: bool,
    ) -> FamilyMember {
        FamilyMember {
            id: id.into(),
            title: title.map(String::from),
            agent: agent.map(String::from),
            parent_id: parent.map(String::from),
            time: SessionTime {
                created: 1_793_448_000_000,
                ..SessionTime::default()
            },
            running,
        }
    }

    fn family_of() -> Vec<FamilyMember> {
        vec![
            member("root", None, None, Some("Main"), true),
            member("a", Some("root"), Some("explore"), Some("Find it"), true),
            member("a1", Some("a"), None, Some("Deeper"), true),
            member("b", Some("root"), Some("general"), None, false),
        ]
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

    #[test]
    fn active_rows_show_agent_title_prefix_and_running_footer() {
        let dialog = dialog(&family_of(), "root", true);
        let select = select(&dialog);
        let a = select.item("a").expect("running child");
        assert_eq!(a.title, "Explore: Find it");
        assert_eq!(a.footer.as_deref(), Some("Running"));
        let a1 = select.item("a1").expect("running grandchild");
        assert_eq!(a1.title, "└─ Subagent: Deeper");
        assert!(select.item("b").is_none(), "finished rows are filtered out");
        assert!(select.item("root").is_none(), "the root is not a subagent");
        assert_eq!(dialog.tag(), Some(TAG));
    }

    #[test]
    fn inactive_rows_have_no_running_footer_and_a_fallback_title() {
        let dialog = dialog(&family_of(), "root", false);
        let select = select(&dialog);
        let b = select.item("b").expect("finished child");
        assert_eq!(b.title, "General: New session - 2026-10-31T12:00:00.000Z");
        assert_eq!(b.footer, None);
        assert!(select.item("a").is_none());
    }

    #[test]
    fn ctrl_a_toggles_with_or_without_rows() {
        let mut dialog = dialog(&family_of(), "root", true);
        assert_eq!(
            dialog.on_key(ctrl('a')),
            DialogOutcome::Action {
                key: TOGGLE_KEY,
                value: "a".into()
            }
        );
        let lone = vec![member("root", None, None, None, true)];
        let mut empty = super::dialog(&lone, "root", true);
        assert_eq!(
            empty.on_key(ctrl('a')),
            DialogOutcome::Action {
                key: TOGGLE_KEY,
                value: String::new()
            },
            "an empty list must not trap the user in it"
        );
    }

    #[test]
    fn ctrl_d_is_offered_only_for_running_rows() {
        let mut active = dialog(&family_of(), "root", true);
        assert_eq!(
            active.on_key(ctrl('d')),
            DialogOutcome::Action {
                key: INTERRUPT_KEY,
                value: "a".into()
            }
        );
        let mut inactive = dialog(&family_of(), "root", false);
        assert_eq!(inactive.on_key(ctrl('d')), DialogOutcome::Idle);
    }

    #[test]
    fn the_cursor_starts_on_the_current_session_when_it_is_listed() {
        let dialog = dialog(&family_of(), "a1", true);
        assert_eq!(select(&dialog).selected_value(), Some("a1"));
    }

    #[test]
    fn an_empty_filter_explains_itself() {
        let lone = vec![member("root", None, None, None, true)];
        let Dialog::Select(active) = dialog(&lone, "root", true) else {
            panic!("select");
        };
        assert_eq!(active.empty_message(), "No active subagents");
        let Dialog::Select(inactive) = dialog(&lone, "root", false) else {
            panic!("select");
        };
        assert_eq!(inactive.empty_message(), "No inactive subagents");
    }
}
