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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::dialog::DialogOutcome;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn shell(id: &str, session: Option<&str>, status: &str, command: &str) -> ShellInfo {
        let mut metadata = serde_json::Map::new();
        if let Some(session) = session {
            metadata.insert("sessionID".into(), session.into());
        }
        serde_json::from_value(serde_json::json!({
            "id": id,
            "status": status,
            "command": command,
            "cwd": "/",
            "shell": "zsh",
            "file": "/tmp/out",
            "metadata": metadata,
            "time": { "started": 1 },
        }))
        .expect("shell json")
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

    fn shells() -> Vec<ShellInfo> {
        vec![
            shell("sh_1", Some("ses_a"), "running", "npm run dev\nsecond line"),
            shell("sh_2", Some("ses_b"), "running", "other session"),
            shell("sh_3", Some("ses_a"), "exited", "finished"),
            shell("sh_4", None, "running", "no owner"),
            shell("sh_5", Some("ses_a"), "running", "tail -f log"),
        ]
    }

    #[test]
    fn lists_only_running_shells_of_the_session() {
        let dialog = dialog(&shells(), "ses_a");
        let select = select(&dialog);
        assert_eq!(select.item("sh_1").expect("row").title, "npm run dev");
        assert!(select.item("sh_5").is_some());
        for hidden in ["sh_2", "sh_3", "sh_4"] {
            assert!(select.item(hidden).is_none(), "{hidden} must be filtered");
        }
        assert_eq!(dialog.tag(), Some(TAG));
    }

    #[test]
    fn an_empty_list_explains_itself() {
        let dialog = dialog(&shells(), "ses_none");
        assert_eq!(select(&dialog).empty_message(), "No shell commands");
    }

    #[test]
    fn ctrl_d_kills_the_selected_shell() {
        let mut dialog = dialog(&shells(), "ses_a");
        assert_eq!(
            dialog.on_key(ctrl('d')),
            DialogOutcome::Action {
                key: KILL_KEY,
                value: "sh_1".into()
            }
        );
    }

    #[test]
    fn enter_selects_the_shell_id() {
        let mut dialog = dialog(&shells(), "ses_a");
        assert_eq!(
            dialog.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DialogOutcome::Selected("sh_1".into())
        );
    }
}
