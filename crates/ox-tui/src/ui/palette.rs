//! The command palette (`ctrl+p`, upstream `component/command-palette.tsx`).
//!
//! A pure builder over the [`Select`] base: the registry (`ui::commands`) and
//! a key-hint lookup (`app::keymap::hint`, passed in so `ui` does not depend
//! on `app`) go in, a ready-to-open dialog comes out.
//! Selecting a row hands back the command's primary name, which the app maps
//! to a [`CommandAction`] with [`action_of`] and runs through `run_command`.
//!
//! Deviations from upstream, all because ox lacks the feature behind the row:
//! - no `Connect an integration`, `Open settings` or `Queue prompt` rows;
//! - `View queued prompts` is left out instead of shown disabled while the
//!   queue is empty (the Select has no disabled rows);
//! - `Switch session` is always suggested (upstream: only when sessions exist;
//!   ox has no cached session list at hand);
//! - `Toggle sidebar` has one fixed title (upstream switches between
//!   `Show sidebar` and `Hide sidebar`);
//! - filtering keeps the category headers (upstream flattens the list and
//!   shows `group · keys` as the footer).

use super::commands::{COMMANDS, Command, CommandAction};
use super::dialog::{Item, Select};

/// `Dialog::tag` of the palette.
pub const TAG: &str = "palette";

/// Prefix of a `Suggested` copy's value (upstream `suggested:<id>`).
const SUGGESTED: &str = "suggested:";
const SUGGESTED_GROUP: &str = "Suggested";

/// Row order, following upstream's registration order (`app.tsx:763-1180`).
/// Groups appear in the order their first row does.
const ORDER: &[CommandAction] = &[
    CommandAction::Sessions,
    CommandAction::New,
    CommandAction::Clear,
    CommandAction::Rename,
    CommandAction::Timeline,
    CommandAction::Fork,
    CommandAction::Subagents,
    CommandAction::QueuedPrompts,
    CommandAction::Shells,
    CommandAction::ToggleSidebar,
    CommandAction::Models,
    CommandAction::Agents,
    CommandAction::Status,
    CommandAction::Exit,
];

/// Rows that act on the open session.
const SESSION_ONLY: &[CommandAction] = &[
    CommandAction::Rename,
    CommandAction::Timeline,
    CommandAction::Fork,
    CommandAction::Subagents,
    CommandAction::QueuedPrompts,
    CommandAction::Shells,
    CommandAction::ToggleSidebar,
];

/// What the palette needs to know about the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    /// A session is open (`New session` is only suggested then, and `Fork`
    /// only makes sense then).
    pub in_session: bool,
    /// Prompts wait in the queue (`View queued prompts` is enabled only then).
    pub has_queue: bool,
}

/// Upstream `suggested` per command.
fn suggested(action: CommandAction, context: Context) -> bool {
    match action {
        CommandAction::Sessions | CommandAction::Models => true,
        CommandAction::New => context.in_session,
        _ => false,
    }
}

fn command_of(action: CommandAction) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.action == action)
}

/// The key hint shown right-aligned on a command's row.
pub type HintOf = fn(CommandAction) -> Option<String>;

fn item_of(command: &Command, hint_of: HintOf) -> Item {
    let item = Item::new(command.name, command.title).category(command.group);
    match hint_of(command.action) {
        Some(hint) => item.footer(hint),
        None => item,
    }
}

/// The palette dialog. Without a session, the session-only rows are left out.
pub fn select(context: Context, hint_of: HintOf) -> Select {
    let commands: Vec<&Command> = ORDER
        .iter()
        .filter(|action| context.in_session || !SESSION_ONLY.contains(action))
        .filter(|action| context.has_queue || **action != CommandAction::QueuedPrompts)
        .filter_map(|action| command_of(*action))
        .collect();
    let copies = commands
        .iter()
        .filter(|command| suggested(command.action, context))
        .map(|command| {
            let mut item = item_of(command, hint_of);
            item.value = format!("{SUGGESTED}{}", command.name);
            item.category = Some(SUGGESTED_GROUP.to_owned());
            item.unfiltered_only()
        });
    let items = copies
        .chain(commands.iter().map(|command| item_of(command, hint_of)))
        .collect();
    Select::new("Commands", items).tag(TAG)
}

/// The action behind a palette row's value.
pub fn action_of(value: &str) -> Option<CommandAction> {
    let name = value.strip_prefix(SUGGESTED).unwrap_or(value);
    COMMANDS
        .iter()
        .find(|command| command.name == name)
        .map(|command| command.action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::keymap;
    use crate::ui::dialog::{Dialog, DialogOutcome};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(select: &mut Select, code: KeyCode) -> DialogOutcome {
        select.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(select: &mut Select, text: &str) {
        for ch in text.chars() {
            press(select, KeyCode::Char(ch));
        }
    }

    #[test]
    fn every_registered_command_is_listed_once_in_the_order_table() {
        for command in COMMANDS {
            let count = ORDER.iter().filter(|a| **a == command.action).count();
            assert_eq!(count, 1, "{}", command.name);
        }
        assert_eq!(ORDER.len(), COMMANDS.len());
    }

    #[test]
    fn the_palette_lists_every_command_with_its_key_hint() {
        let select = select(
            Context {
                in_session: true,
                has_queue: true,
            },
            keymap::hint,
        );
        for command in COMMANDS {
            let item = select
                .item(command.name)
                .unwrap_or_else(|| panic!("{}", command.name));
            assert_eq!(item.title, command.title);
            assert_eq!(item.category.as_deref(), Some(command.group));
            assert_eq!(item.footer, keymap::hint(command.action));
        }
        assert_eq!(
            select.item("sessions").unwrap().footer.as_deref(),
            Some("ctrl+x l")
        );
        assert_eq!(
            select.item("new").unwrap().footer.as_deref(),
            Some("ctrl+x n")
        );
        assert_eq!(
            select.item("models").unwrap().footer.as_deref(),
            Some("ctrl+x m")
        );
    }

    #[test]
    fn suggested_rows_lead_and_follow_the_session_state() {
        let mut in_session = select(
            Context {
                in_session: true,
                has_queue: false,
            },
            keymap::hint,
        );
        assert_eq!(in_session.selected_value(), Some("suggested:sessions"));
        for expected in ["suggested:new", "suggested:models"] {
            press(&mut in_session, KeyCode::Down);
            assert_eq!(in_session.selected_value(), Some(expected));
        }
        let mut home = select(
            Context {
                in_session: false,
                has_queue: false,
            },
            keymap::hint,
        );
        press(&mut home, KeyCode::Down);
        assert_eq!(home.selected_value(), Some("suggested:models"));
        assert!(home.item("fork").is_none(), "nothing to fork on Home");
    }

    #[test]
    fn typing_drops_the_suggested_copies() {
        let mut select = select(
            Context {
                in_session: true,
                has_queue: false,
            },
            keymap::hint,
        );
        type_text(&mut select, "switch");
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.extend(select.selected_value().map(str::to_owned));
            press(&mut select, KeyCode::Down);
        }
        assert!(seen.iter().all(|v| !v.starts_with(SUGGESTED)), "{seen:?}");
        assert!(seen.contains(&"sessions".to_owned()), "{seen:?}");
        assert!(seen.contains(&"models".to_owned()), "{seen:?}");
    }

    #[test]
    fn enter_hands_back_a_value_that_maps_to_its_action() {
        let mut select = select(
            Context {
                in_session: true,
                has_queue: false,
            },
            keymap::hint,
        );
        let DialogOutcome::Selected(value) = press(&mut select, KeyCode::Enter) else {
            panic!("enter selects");
        };
        assert_eq!(action_of(&value), Some(CommandAction::Sessions));
        assert_eq!(action_of("exit"), Some(CommandAction::Exit));
        assert_eq!(action_of("suggested:models"), Some(CommandAction::Models));
        assert_eq!(action_of("setting:x"), None);
    }

    #[test]
    fn the_dialog_carries_the_palette_tag() {
        let dialog = Dialog::Select(select(
            Context {
                in_session: true,
                has_queue: false,
            },
            keymap::hint,
        ));
        assert_eq!(dialog.tag(), Some(TAG));
    }
}
