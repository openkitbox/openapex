// command.rs — App -> server (executed by tokio tasks via ox-client).
#[derive(Debug, Clone)]
pub enum AppCommand {
    Prompt(String),
    Interrupt,
    /// `request_id` lets the App drop responses that lost the race against
    /// newer deltas or a newer reload.
    ReloadHistory {
        request_id: u64,
    },
    Quit,
}
