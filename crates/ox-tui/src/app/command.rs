// command.rs — App -> server (executed by tokio tasks via ox-client).
#[derive(Debug, Clone)]
pub enum AppCommand {
    /// First prompt from Home: create a session, then the app receives
    /// `AppEvent::SessionReady` and follows up with `Prompt`.
    CreateSessionAndPrompt(String),
    Prompt {
        session: String,
        text: String,
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
    Quit,
}
