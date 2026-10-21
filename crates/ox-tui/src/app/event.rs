// event.rs — UI intent (keyboard/stream), never network actions.
use crossterm::event::KeyEvent;
use ox_client::store::Item;

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Stream(ox_client::sse::StreamMsg),
    /// Authoritative history from a reload, tagged with the request that asked
    /// for it so stale responses can be dropped (they race live deltas).
    History {
        request_id: u64,
        items: Vec<Item>,
    },
    Quit,
}

impl From<KeyEvent> for AppEvent {
    fn from(key: KeyEvent) -> Self {
        AppEvent::Key(key)
    }
}
