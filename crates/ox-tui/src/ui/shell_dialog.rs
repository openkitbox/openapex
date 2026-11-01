//! The shell picker (upstream `composer/shell-tab.tsx`).
//!
//! A pure builder over [`Select`]: the server's shell list goes in, a
//! ready-to-open dialog comes out. Only the running shells of the given
//! session are listed, as upstream does.
//!
//! Deviation from upstream: a dialog instead of the composer's bottom panel.

use ox_client::rest::ShellInfo;

use super::dialog::{Dialog, Hint, Item, Select};

/// `Dialog::tag` of the picker.
pub const TAG: &str = "shells";
/// Row action key (`ctrl+<key>`): kill the selected shell.
pub const KILL_KEY: char = 'd';

/// The picker for the running shells that `session` started.
#[must_use]
pub fn dialog(shells: &[ShellInfo], session: &str) -> Dialog {
    let items: Vec<Item> = shells
        .iter()
        .filter(|shell| shell.session_id() == Some(session) && shell.status == "running")
        .map(|shell| {
            Item::new(
                shell.id.as_str(),
                shell.command.lines().next().unwrap_or(""),
            )
        })
        .collect();
    Dialog::Select(
        Select::new("Shells", items)
            .large()
            .tag(TAG)
            .action(KILL_KEY, None)
            .hints(vec![
                Hint::new("output", "enter"),
                Hint::new("kill", "ctrl+d"),
            ])
            .empty_text("No shell commands"),
    )
}
