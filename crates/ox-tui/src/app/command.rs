// command.rs — App -> server (executed by tokio tasks via ox-client).
use std::sync::Arc;

use ox_client::store::MediaSource;
use ox_schema::support::FileInput;
use ratatui::layout::Size;

use crate::graphics::cache::SizeClass;
use crate::ui::composer::Submission;

/// The model a first session is created with: Home has no session to POST a
/// model switch to, so the selection rides with the creation command and the
/// executor applies it before the first prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModel {
    pub provider_id: String,
    pub model_id: String,
    pub variant: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AppCommand {
    ChangeDirectory {
        session: Option<String>,
        directory: String,
    },
    RefreshDirectory {
        session: String,
    },
    FindDirectories {
        query: String,
        generation: u64,
    },
    /// First prompt from Home: create a session, apply `model` (the selection
    /// the prompt row was showing while no session existed), then the app
    /// receives `AppEvent::SessionReady` and follows up with `Prompt`.
    CreateSessionAndPrompt {
        submission: Submission,
        model: Option<SessionModel>,
    },
    Prompt {
        session: String,
        /// Inbox/message id the local echo is pending under.
        id: String,
        text: String,
        files: Vec<FileInput>,
        /// One-turn logical model override for this prompt only (`/slim`).
        /// `None` keeps the session's model; the engine resolves the id.
        model: Option<String>,
    },
    Interrupt {
        session: String,
    },
    /// Move the session's blocking `shell`/`subagent` calls to the
    /// background (upstream `session.background`, `ctrl+b`).
    Background {
        session: String,
    },
    /// `request_id` lets the App drop responses that lost the race against
    /// newer deltas or a newer reload.
    ReloadHistory {
        session: String,
        request_id: u64,
    },
    /// Fetch one attachment's bytes for a thumbnail; answered with
    /// `AppEvent::Media`. `cells` is the grid the image will be drawn on, which
    /// only a sixel terminal needs: its encode is made for that exact grid.
    FetchMedia {
        session: String,
        key: String,
        source: MediaSource,
        cells: Option<Size>,
        class: SizeClass,
    },
    /// Prepare one attachment the composer already holds: its bytes were
    /// decoded from the pasted `data:` URI, so nothing is fetched; a kitty
    /// terminal still has to be handed PNG, and a sixel one an encode for the
    /// grid the image will be drawn on. Answered with `AppEvent::Media`, like a
    /// stored attachment.
    MediaInline {
        key: String,
        bytes: Arc<[u8]>,
        cells: Option<Size>,
        class: SizeClass,
    },
    /// Fetch the session's stored agent/model for the prompt's metadata row;
    /// answered with `AppEvent::SessionChrome`.
    LoadSessionChrome {
        request: SessionChromeRequest,
    },
    /// Fetch `GET /api/mcp`; the intent says what the answer is for, the
    /// stamp orders it against the other MCP lists.
    McpList {
        intent: McpIntent,
        stamp: McpStamp,
    },
    /// Connect or disconnect one MCP server, answered with a
    /// `McpIntent::Refresh` list carrying `stamp` and then
    /// `AppEvent::McpToggled`.
    McpToggle {
        server: String,
        action: McpAction,
        stamp: McpStamp,
    },
    /// Fetch the model catalogue for the `/models` dialog.
    ModelList,
    /// Fetch the catalogue only for its context windows (the footer's
    /// percentage); answered with `AppEvent::ContextLimits`.
    LoadContextLimits,
    /// Fetch the selected model's supported reasoning levels.
    VariantList {
        request: VariantRequest,
    },
    /// Fetch the agents for the `/agents` dialog or a `Tab` cycle.
    AgentList(AgentIntent),
    /// `AppEvent::Switched` or `AppEvent::SwitchFailed`.
    Switch {
        request: SwitchRequest,
    },
    /// Answer a pending form (`POST …/form/{id}/reply`).
    ReplyForm {
        session: String,
        form: String,
        answer: ox_client::form::Answer,
    },
    /// Dismiss a pending form (`DELETE …/form/{id}`).
    CancelForm {
        session: String,
        form: String,
    },
    /// Re-fetch the session's pending forms; answered with `AppEvent::Forms`.
    ListForms {
        session: String,
    },
    Quit,
    /// List roots in the UI's last committed directory, even during recovery.
    ListSessions {
        directory: String,
        generation: u64,
    },
    /// Re-resolve which root sessions are running; answered with
    /// `AppEvent::RunningRoots`.
    ListRunningRoots,
    /// Fetch the session family of `session`; answered with `AppEvent::Family`.
    ListFamily {
        session: String,
        generation: u64,
    },
    /// Fetch the background shells to rebuild the running set; answered with
    /// `AppEvent::RunningShells`, which never opens a dialog. `generation`
    /// numbers the request; the answer carries it back.
    SyncShells {
        generation: u64,
    },
    /// Kill shell `id`; its `shell.*` frames then drop it from the store.
    KillShell {
        id: String,
    },
    /// Deliver queued prompt `inbox` of `session` at the next step boundary.
    SteerQueued {
        session: String,
        inbox: String,
    },
    /// Drop queued prompt `inbox` of `session`; `reason` names the toast.
    CancelQueued {
        session: String,
        inbox: String,
        reason: CancelReason,
    },
    /// Read shell `id`'s output from `cursor` for its expanded block;
    /// answered with `AppEvent::ShellPolled`, which never opens a dialog.
    ShellPoll {
        id: String,
        cursor: u64,
        mode: ox_client::rest::ShellReadMode,
    },
    /// Fetch shell `id` and its recent output; answered with
    /// `AppEvent::ShellOutput`.
    ShellOutput {
        session: String,
        id: String,
    },
    /// Follow `session`'s event stream instead of the previous one. The app has
    /// already reset the shared seq to `None`.
    AttachSession {
        session: String,
        generation: u64,
    },
    /// Return Home with the directory last committed by the UI.
    DetachSession {
        directory: String,
        generation: u64,
    },
    RenameSession {
        session: String,
        title: String,
    },
    DeleteSession {
        session: String,
    },
    /// Fork through the newest message; the answer is `AppEvent::SessionForked`.
    ForkSession {
        session: String,
    },
    /// Flip the session's memory switch; the answer is `AppEvent::MemoryMode`.
    ToggleMemory {
        session: String,
    },
    /// Archive the project's active memories; the answer is
    /// `AppEvent::MemoryCleared`.
    ClearMemory {
        session: String,
    },
    /// `GET /api/fs/find` for the `@` file menu; the answer is `AppEvent::Files`.
    FindFiles {
        query: String,
        generation: u64,
    },
    /// A paste that looks like image/PDF paths: read the files off the UI
    /// thread. The text is echoed in `AppEvent::Attachments` so a failed read
    /// can still be pasted as text.
    LoadAttachments(String),
    /// `ctrl+v` (or an empty bracketed paste): read the system clipboard off
    /// the UI thread; the answer is `AppEvent::ClipboardPasted`.
    ReadClipboard,
    /// A finished drag selection: copy the text to the host clipboard off the
    /// UI thread; the answer is `AppEvent::ClipboardWritten`.
    WriteClipboard(String),
}

/// Why the MCP server list is being fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpIntent {
    /// Open the `/status` dialog.
    Status,
    /// Open the `/mcps` dialog.
    Dialog,
    /// Refresh the open `/mcps` dialog (after a toggle or a
    /// `mcp.status.changed` frame); the home footer follows either way.
    Refresh,
}

/// Issue order of an MCP list request. Every fetch runs in its own task and
/// `mcp.status.changed` fires twice per connect (pending, then final), so
/// answers can land out of order; the app drops one older than the list it
/// already shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct McpStamp(pub u64);

/// What toggling an MCP server asks the server to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAction {
    Connect,
    Disconnect,
}

/// Why the agent list is being fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentIntent {
    /// Open the `/agents` dialog.
    Dialog,
    /// Move to the next (`1`) or previous (`-1`) agent.
    Cycle(isize),
}

/// Identity and context captured when a session chrome read is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChromeRequest {
    pub session: String,
    pub request_id: u64,
    pub switch_generation: u64,
    pub navigation_generation: u64,
}

/// Identity and context captured when a model or agent switch is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchRequest {
    pub session: String,
    pub switch: Switch,
    pub request_id: u64,
    pub navigation_generation: u64,
}

/// Identity and context captured when a variant catalogue request is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantRequest {
    pub provider_id: String,
    pub model_id: String,
    pub provider_label: String,
    pub cycle: bool,
    pub request_id: u64,
    pub session: Option<String>,
    pub navigation_generation: u64,
}

/// One model or agent change. It carries what the prompt chrome shows
/// (`provider_label`, `label`) next to the ids the server needs, so the
/// success event can update the chrome without another lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Switch {
    Model {
        provider_id: String,
        model_id: String,
        variant: Option<String>,
        provider_label: String,
    },
    Agent {
        id: String,
        label: String,
    },
}

impl Switch {
    /// Toast title when the server refuses this change.
    #[must_use]
    pub fn failure_title(&self) -> &'static str {
        match self {
            Switch::Model { .. } => "Failed to switch model",
            Switch::Agent { .. } => "Failed to switch agent",
        }
    }
}

/// Why a queued prompt is cancelled: only the failure toast differs
/// (upstream `Failed to delete|undo pending prompt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    Delete,
    Undo,
}
