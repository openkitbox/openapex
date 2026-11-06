// event.rs — UI intent (keyboard/stream), never network actions.
use crossterm::event::{Event, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use std::sync::Arc;

use ox_client::store::Item;
use ox_schema::support::FileInput;

use crate::graphics::Prepared;
use crate::graphics::cache::SizeClass;

/// What a media fetch produced.
pub enum Fetched {
    /// Raw image bytes, for a kitty transmission the graphics cache makes on
    /// the draw thread (one transmission serves every size). Shared: a composer
    /// attachment is handed to the encoder without copying its payload.
    Png(Arc<[u8]>),
    /// A sixel image already encoded for the grid it was asked for: the encode
    /// is expensive, so it happened on a blocking thread.
    Sixel(Box<Prepared>),
}

/// The payload is opaque here: an encoded sixel is megabytes of protocol state
/// that no log line should try to print.
impl std::fmt::Debug for Fetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Png(_) => "Png(..)",
            Self::Sixel(_) => "Sixel(..)",
        })
    }
}

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
    /// One attachment's bytes, or why they could not be read, tagged with the
    /// cache key that asked for them.
    Media {
        key: String,
        /// Which part of the graphics cache asked, so the answer is filed where
        /// the asker will look for it.
        class: SizeClass,
        result: Result<Fetched, String>,
    },
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
    /// `running` holds the root sessions with a turn in flight in their tree.
    Sessions {
        sessions: Vec<ox_client::rest::SessionSummary>,
        running: std::collections::HashSet<String>,
    },
    /// Answer to `AppCommand::ListRunningRoots`: the root sessions with a turn
    /// in flight in their tree, to refresh the open `/sessions` dialog.
    RunningRoots(std::collections::HashSet<String>),
    /// Answer to `AppCommand::ListFamily`: the family `session` belongs to;
    /// the activity panel lists it.
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
    /// Answer to `AppCommand::SyncShells`: the server's running shells and the
    /// session that owns each. Replaces `DataStore`'s running map; a failed
    /// list sends nothing.
    RunningShells {
        generation: u64,
        shells: Vec<ox_client::store::RunningShell>,
    },
    /// Answer to `AppCommand::ShellOutput`: the shell as the server lists it
    /// now, plus the newest part of its output.
    ShellOutput {
        session: String,
        shell: Box<ox_client::rest::ShellInfo>,
        tail: ox_client::rest::ShellTail,
    },
    /// Answer to `AppCommand::ShellPoll`: what was read from `start`, or
    /// `None` when the request failed (so the next poll may go out).
    ShellPolled {
        id: String,
        start: u64,
        read: Option<ox_client::rest::ShellRead>,
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
    #[must_use]
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
