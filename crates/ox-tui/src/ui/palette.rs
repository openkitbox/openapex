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
//! - no `Toggle sidebar` row: ox has no sidebar;
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
    CommandAction::Memory,
    CommandAction::Forget,
    CommandAction::Slim,
    CommandAction::Models,
    CommandAction::Variants,
    CommandAction::Agents,
    CommandAction::Mcps,
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
    CommandAction::Memory,
    CommandAction::Forget,
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
        CommandAction::Sessions | CommandAction::Models | CommandAction::Variants => true,
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
#[must_use]
pub fn action_of(value: &str) -> Option<CommandAction> {
    let name = value.strip_prefix(SUGGESTED).unwrap_or(value);
    COMMANDS
        .iter()
        .find(|command| command.name == name)
        .map(|command| command.action)
}
