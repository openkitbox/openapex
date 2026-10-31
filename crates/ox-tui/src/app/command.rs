// command.rs — App -> server (executed by tokio tasks via ox-client).
use ox_schema::support::FileInput;

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
    pub fn failure_title(&self) -> &'static str {
        match self {
            Switch::Model { .. } => "Failed to switch model",
            Switch::Agent { .. } => "Failed to switch agent",
        }
    }
}
