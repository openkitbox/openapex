//! The queued-prompts dialog (upstream `openQueuedPrompts`,
//! `routes/session/index.tsx:775-830`).
//!
//! A pure builder over [`Select`]: the store's queue goes in, a ready-to-open
//! dialog comes out. Each row is the prompt text with an `i of N` footer;
//! enter steers it, ctrl+d deletes it, ctrl+u takes it back into the draft.
//!
//! Deviations: no "Search queued prompts" placeholder (the Select's typed
//! filter searches the prompt text), and enter closes the dialog at once
//! instead of after the server answers; a failure still toasts.

use ox_client::store::Queued;

use super::dialog::{Dialog, Hint, Item, Select};

/// `Dialog::tag` of the dialog.
pub const TAG: &str = "queued";
/// Row action key (`ctrl+<key>`): delete the selected prompt.
pub const DELETE_KEY: char = 'd';
/// Row action key (`ctrl+<key>`): move the selected prompt back to the draft.
pub const UNDO_KEY: char = 'u';

/// The dialog over the queued prompts, in queue order.
#[must_use]
pub fn dialog<'a>(queued: impl IntoIterator<Item = &'a Queued>) -> Dialog {
    let prompts: Vec<&Queued> = queued.into_iter().collect();
    let total = prompts.len();
    let items: Vec<Item> = prompts
        .iter()
        .enumerate()
        .map(|(index, prompt)| {
            Item::new(prompt.inbox_id.as_str(), prompt.text.as_ref())
                .footer(format!("{} of {total}", index + 1))
        })
        .collect();
    Dialog::Select(
        Select::new("Queued prompts", items)
            .tag(TAG)
            .action(DELETE_KEY, None)
            .action(UNDO_KEY, None)
            .hints(vec![
                Hint::new("steer", "enter"),
                Hint::new("delete", "ctrl+d"),
                Hint::new("undo", "ctrl+u"),
            ])
            .empty_text("No queued prompts"),
    )
}
