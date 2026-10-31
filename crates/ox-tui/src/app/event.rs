// event.rs — UI intent (keyboard/stream), never network actions.
use crossterm::event::{Event, KeyEvent, MouseButton, MouseEventKind};
use ox_client::store::Item;

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    /// The terminal changed size. It carries no geometry: the redraw re-reads
    /// `frame.area()` and ratatui re-queries the backend on every `draw`.
    Resize,
    /// Left click at a screen cell.
    Click {
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
    /// A lazily created session is ready; `prompt` is the text that triggered
    /// the creation (the app shows it optimistically and sends it).
    SessionReady {
        session: String,
        prompt: String,
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
}

impl AppEvent {
    /// Which terminal events the UI acts on. Everything else (mouse, focus,
    /// paste) is dropped here, in one place, instead of in the reader thread.
    pub fn from_terminal(event: Event) -> Option<Self> {
        match event {
            Event::Key(key) => Some(AppEvent::Key(key)),
            Event::Resize(_, _) => Some(AppEvent::Resize),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Up(MouseButton::Left) => Some(AppEvent::Click {
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
    fn left_release_is_a_click_and_the_wheel_scrolls() {
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
            AppEvent::from_terminal(mouse(MouseEventKind::Up(MouseButton::Left))),
            Some(AppEvent::Click { x: 7, y: 5 })
        ));
        assert!(matches!(
            AppEvent::from_terminal(mouse(MouseEventKind::ScrollUp)),
            Some(AppEvent::Wheel(-1))
        ));
        assert!(AppEvent::from_terminal(mouse(MouseEventKind::Down(MouseButton::Left))).is_none());
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
        assert!(
            AppEvent::from_terminal(Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }))
            .is_none()
        );
    }
}
