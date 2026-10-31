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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ui::dialog::DialogOutcome;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn queue() -> Vec<Queued> {
        ["first", "second"]
            .iter()
            .enumerate()
            .map(|(index, text)| Queued {
                inbox_id: format!("inb_{index}"),
                text: Arc::from(*text),
            })
            .collect()
    }

    fn select(dialog: &Dialog) -> &Select {
        match dialog {
            Dialog::Select(select) => select,
            other => panic!("expected a select dialog: {other:?}"),
        }
    }

    #[test]
    fn rows_are_the_prompts_with_their_position() {
        let queue = queue();
        let dialog = dialog(&queue);
        let select = select(&dialog);
        let first = select.item("inb_0").expect("row");
        assert_eq!(first.title, "first");
        assert_eq!(
            first.footer.as_deref(),
            Some("1 of 2"),
            "position among queued prompts"
        );
        assert_eq!(
            select.item("inb_1").and_then(|item| item.footer.as_deref()),
            Some("2 of 2"),
            "position among queued prompts"
        );
        assert_eq!(dialog.tag(), Some(TAG), "the app routes actions by tag");
    }

    #[test]
    fn keys_steer_delete_and_undo_the_selected_prompt() {
        let queue = queue();
        let mut dialog = dialog(&queue);
        let ctrl = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
        assert_eq!(
            dialog.on_key(ctrl('d')),
            DialogOutcome::Action {
                key: DELETE_KEY,
                value: "inb_0".into()
            },
            "ctrl+d deletes the selected prompt"
        );
        assert_eq!(
            dialog.on_key(ctrl('u')),
            DialogOutcome::Action {
                key: UNDO_KEY,
                value: "inb_0".into()
            },
            "ctrl+u takes the selected prompt back"
        );
        assert_eq!(
            dialog.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DialogOutcome::Selected("inb_0".into()),
            "enter steers the selected prompt"
        );
    }
}
