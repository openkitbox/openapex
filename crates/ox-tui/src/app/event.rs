// event.rs — UI intent (keyboard/stream), never network actions.
use crossterm::event::{Event, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ox_client::store::Item;
use ox_schema::support::FileInput;

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    /// Bracketed paste: the whole pasted text, moved from the terminal event.
    Paste(String),
    /// The terminal changed size. It carries no geometry: the redraw re-reads
    /// `frame.area()` and ratatui re-queries the backend on every `draw`.
    Resize,
    /// Left button pressed at a screen cell (without ctrl, which the terminal
    /// layer keeps for itself, `renderer.ts:3994-3998`).
    MouseDown {
        x: u16,
        y: u16,
    },
    /// The pointer moved to a screen cell with the left button held.
    MouseDrag {
        x: u16,
        y: u16,
    },
    /// Left button released at a screen cell.
    MouseUp {
        x: u16,
        y: u16,
    },
    /// The pointer moved to a screen cell with no button held.
    Hover {
        x: u16,
        y: u16,
    },
    /// Mouse wheel: positive scrolls toward newer lines.
    Wheel(i8),
    Stream(ox_client::sse::StreamMsg),
    /// Authoritative history from a reload, tagged with the request that asked
    /// for it so stale responses can be dropped (they race live deltas).
    History {
        request_id: u64,
        items: Vec<Item>,
    },
    /// Answer to `AppCommand::ProbeChildren` for one child session. Applies
    /// only if no execution event for the child arrived since the probe went
    /// out. A failed probe sends nothing.
    ChildRunning {
        child: String,
        running: bool,
    },
    /// A lazily created session is ready; `prompt` is the text that triggered
    /// the creation (the app shows it optimistically and sends it).
    SessionReady {
        session: String,
        prompt: String,
        files: Vec<FileInput>,
    },
    /// Lazy session creation failed; the draft must be restored.
    SessionFailed(String),
    /// A prompt could not be delivered; shown as a toast.
    PromptFailed(String),
    /// Answer to `AppCommand::LoadSessionChrome`.
    SessionChrome {
        session: String,
        selection: crate::bootstrap::Selection,
    },
    /// Answer to `AppCommand::McpList`; opens the `/status` dialog.
    McpServers(Vec<ox_client::rest::McpRef>),
    /// Answer to `AppCommand::ModelList`; opens the `/models` dialog.
    Models {
        models: Vec<ox_client::rest::ModelInfo>,
        providers: Vec<ox_client::rest::ProviderRef>,
    },
    /// Answer to `AppCommand::LoadContextLimits`: `providerID/id` → window.
    ContextLimits(std::collections::HashMap<String, i64>),
    /// Answer to `AppCommand::AgentList`.
    Agents {
        agents: Vec<ox_client::rest::AgentRef>,
        intent: super::command::AgentIntent,
    },
    /// The server accepted a model/agent change; the chrome follows.
    Switched(super::command::Switch),
    /// A model/agent lookup or change failed; shown as a toast.
    SwitchFailed {
        title: &'static str,
        err: String,
    },
    /// Answer to `AppCommand::ListForms`: the session's pending forms.
    Forms(Vec<ox_client::form::FormInfo>),
    /// A form reply or cancel failed for a reason other than "already settled".
    FormFailed(String),
    Quit,
    /// Answer to `AppCommand::ListSessions`; opens the `/sessions` dialog.
    Sessions(Vec<ox_client::rest::SessionSummary>),
    /// Answer to `AppCommand::ListFamily`: the family `session` belongs to;
    /// opens the subagent picker.
    Family {
        session: String,
        members: Vec<ox_client::rest::FamilyMember>,
    },
    /// Answer to `AppCommand::ListShells`: every shell the server knows;
    /// `session` is the one that asked, and the picker filters by it.
    Shells {
        session: String,
        shells: Vec<ox_client::rest::ShellInfo>,
    },
    /// Answer to `AppCommand::ShellOutput`: the shell as the server lists it
    /// now, plus the newest part of its output.
    ShellOutput {
        session: String,
        shell: Box<ox_client::rest::ShellInfo>,
        tail: ox_client::rest::ShellTail,
    },
    /// A session was deleted on the server.
    SessionDeleted(String),
    /// A fork finished; the app switches to the new session.
    SessionForked(String),
    /// A session request failed: toast title and the error text.
    SessionOpFailed {
        title: &'static str,
        error: String,
    },
    /// Answer to `AppCommand::FindFiles`, tagged with the query it answers.
    Files {
        query: String,
        entries: Vec<ox_client::rest::FileEntry>,
    },
    /// Answer to `AppCommand::LoadAttachments`. `loaded` is `None` when any
    /// file could not be used; `text` is the original paste, to be inserted
    /// as typed in that case.
    Attachments {
        text: String,
        loaded: Option<Vec<crate::attachment::Loaded>>,
    },
    /// Answer to `AppCommand::ReadClipboard`.
    ClipboardPasted(crate::clipboard::Clip),
    /// Answer to `AppCommand::WriteClipboard`: whether a host command took
    /// the text.
    ClipboardWritten {
        host_ok: bool,
    },
}

impl AppEvent {
    /// Which terminal events the UI acts on. Everything else (focus, other
    /// mouse kinds) is dropped here, in one place, instead of in the reader
    /// thread. Paste is accepted here only; the app routes it like a key.
    pub fn from_terminal(event: Event) -> Option<Self> {
        match event {
            Event::Key(key) => Some(AppEvent::Key(key)),
            Event::Paste(text) => Some(AppEvent::Paste(text)),
            Event::Resize(_, _) => Some(AppEvent::Resize),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if !mouse.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    Some(AppEvent::MouseDown {
                        x: mouse.column,
                        y: mouse.row,
                    })
                }
                MouseEventKind::Drag(MouseButton::Left) => Some(AppEvent::MouseDrag {
                    x: mouse.column,
                    y: mouse.row,
                }),
                MouseEventKind::Up(MouseButton::Left) => Some(AppEvent::MouseUp {
                    x: mouse.column,
                    y: mouse.row,
                }),
                MouseEventKind::Moved => Some(AppEvent::Hover {
                    x: mouse.column,
                    y: mouse.row,
                }),
                MouseEventKind::ScrollUp => Some(AppEvent::Wheel(-1)),
                MouseEventKind::ScrollDown => Some(AppEvent::Wheel(1)),
                _ => None,
            },
            _ => None,
        }
    }
}

impl From<KeyEvent> for AppEvent {
    fn from(key: KeyEvent) -> Self {
        AppEvent::Key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::AppEvent;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn resize_reaches_the_app() {
        assert!(
            matches!(
                AppEvent::from_terminal(Event::Resize(120, 40)),
                Some(AppEvent::Resize)
            ),
            "a SIGWINCH-driven resize must not be filtered out"
        );
    }

    #[test]
    fn bracketed_paste_reaches_the_app_with_its_text() {
        match AppEvent::from_terminal(Event::Paste("你好\r\nworld".to_owned())) {
            Some(AppEvent::Paste(text)) => assert_eq!(text, "你好\r\nworld"),
            other => panic!("paste must survive untouched: {other:?}"),
        }
    }

    #[test]
    fn left_button_press_drag_release_and_the_wheel_translate() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 7,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(matches!(
            AppEvent::from_terminal(mouse(MouseEventKind::Down(MouseButton::Left))),
            Some(AppEvent::MouseDown { x: 7, y: 5 })
        ));
        assert!(matches!(
            AppEvent::from_terminal(mouse(MouseEventKind::Drag(MouseButton::Left))),
            Some(AppEvent::MouseDrag { x: 7, y: 5 })
        ));
        assert!(matches!(
            AppEvent::from_terminal(mouse(MouseEventKind::Up(MouseButton::Left))),
            Some(AppEvent::MouseUp { x: 7, y: 5 })
        ));
        assert!(matches!(
            AppEvent::from_terminal(mouse(MouseEventKind::ScrollUp)),
            Some(AppEvent::Wheel(-1))
        ));
    }

    #[test]
    fn ctrl_left_press_and_other_buttons_are_dropped() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mouse = |kind, modifiers| {
            Event::Mouse(MouseEvent {
                kind,
                column: 1,
                row: 1,
                modifiers,
            })
        };
        assert!(
            AppEvent::from_terminal(mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::CONTROL
            ))
            .is_none(),
            "ctrl+left-down never starts a selection"
        );
        for button in [MouseButton::Right, MouseButton::Middle] {
            for kind in [
                MouseEventKind::Down(button),
                MouseEventKind::Drag(button),
                MouseEventKind::Up(button),
            ] {
                assert!(AppEvent::from_terminal(mouse(kind, KeyModifiers::NONE)).is_none());
            }
        }
    }

    #[test]
    fn keys_survive_and_unhandled_events_are_dropped() {
        match AppEvent::from_terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        ))) {
            Some(AppEvent::Key(key)) => assert_eq!(key.code, KeyCode::Char('x')),
            other => panic!("key events must survive: {other:?}"),
        }
        assert!(AppEvent::from_terminal(Event::FocusGained).is_none());
    }

    #[test]
    fn pointer_motion_becomes_a_hover_and_other_mouse_kinds_stay_dropped() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 9,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(matches!(
            AppEvent::from_terminal(mouse(MouseEventKind::Moved)),
            Some(AppEvent::Hover { x: 9, y: 4 })
        ));
        assert!(
            AppEvent::from_terminal(mouse(MouseEventKind::Drag(MouseButton::Right))).is_none(),
            "only the left button drags"
        );
    }
}
