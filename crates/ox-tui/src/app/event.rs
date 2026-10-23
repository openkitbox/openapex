// event.rs — UI intent (keyboard/stream), never network actions.
use crossterm::event::{Event, KeyEvent};
use ox_client::store::Item;

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    /// The terminal changed size. It carries no geometry: the redraw re-reads
    /// `frame.area()` and ratatui re-queries the backend on every `draw`.
    Resize,
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
    Quit,
}

impl AppEvent {
    /// Which terminal events the UI acts on. Everything else (mouse, focus,
    /// paste) is dropped here, in one place, instead of in the reader thread.
    pub fn from_terminal(event: Event) -> Option<Self> {
        match event {
            Event::Key(key) => Some(AppEvent::Key(key)),
            Event::Resize(_, _) => Some(AppEvent::Resize),
            _ => None,
        }
    }
}

impl From<KeyEvent> for AppEvent {
    fn from(key: KeyEvent) -> Self {
        AppEvent::Key(key)
    }
}
