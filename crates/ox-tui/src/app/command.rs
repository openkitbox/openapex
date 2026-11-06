// command.rs — App -> server (executed by tokio tasks via ox-client).
use std::sync::Arc;

use ox_client::store::MediaSource;
use ox_schema::support::FileInput;
use ratatui::layout::Size;

use crate::graphics::cache::SizeClass;
use crate::ui::composer::Submission;

#[derive(Debug, Clone)]
pub enum AppCommand {
    /// First prompt from Home: create a session, then the app receives
    /// `AppEvent::SessionReady` and follows up with `Prompt`.
    CreateSessionAndPrompt(Submission),
    Prompt {
        session: String,
        text: String,
        files: Vec<FileInput>,
    },
    Interrupt {
        session: String,
    },
    /// `request_id` lets the App drop responses that lost the race against
    /// newer deltas or a newer reload.
    ReloadHistory {
        session: String,
        request_id: u64,
    },
    /// Read each child (subagent) session's `time.busy/idle`, one request
    /// after another; every answer is an `AppEvent::ChildRunning`.
    ProbeChildren {
        children: Vec<String>,
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
        session: String,
    },
    /// Fetch `GET /api/mcp` for the `/status` dialog.
    McpList,
    /// Fetch the model catalogue for the `/models` dialog.
    ModelList,
    /// Fetch the catalogue only for its context windows (the footer's
    /// percentage); answered with `AppEvent::ContextLimits`.
    LoadContextLimits,
    /// Fetch the agents for the `/agents` dialog or a `Tab` cycle.
    AgentList(AgentIntent),
    /// Apply a model/agent change to the server session; the answer is
    /// `AppEvent::Switched` or `AppEvent::SwitchFailed`.
    Switch {
        session: String,
        switch: Switch,
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
    /// Fetch `GET /api/session` for the `/sessions` dialog.
    ListSessions,
    /// Re-resolve which root sessions are running; answered with
    /// `AppEvent::RunningRoots`.
    ListRunningRoots,
    /// Fetch the session family of `session`; answered with `AppEvent::Family`.
    ListFamily {
        session: String,
    },
    /// Fetch the background shells; answered with `AppEvent::Shells`.
    ListShells {
        session: String,
    },
    /// Fetch the background shells to rebuild the running set; answered with
    /// `AppEvent::RunningShells`, which never opens a dialog. `generation`
    /// numbers the request; the answer carries it back.
    SyncShells {
        generation: u64,
    },
    /// Kill shell `id`, then re-fetch the list for `session`.
    KillShell {
        session: String,
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
    },
    /// Stop following any session (the app went Home).
    DetachSession,
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
    /// `GET /api/fs/find` for the `@` file menu; the answer is `AppEvent::Files`.
    FindFiles(String),
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

/// Why the agent list is being fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentIntent {
    /// Open the `/agents` dialog.
    Dialog,
    /// Move to the next (`1`) or previous (`-1`) agent.
    Cycle(isize),
}

/// One model or agent change. It carries what the prompt chrome shows
/// (`provider_label`, `label`) next to the ids the server needs, so the
/// success event can update the chrome without another lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Switch {
    Model {
        provider_id: String,
        model_id: String,
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
