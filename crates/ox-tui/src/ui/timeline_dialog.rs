//! The `/timeline` picker (upstream `routes/session/dialog-timeline.tsx`):
//! one row per user message, newest first. Selecting a row jumps the
//! transcript to that message.
//!
//! Deviations: the jump happens on Enter rather than while moving, and the
//! rows have no time footer (the transcript blocks carry no timestamp).

use super::dialog::{Dialog, Item, Select};

/// `Dialog::tag` of the timeline picker.
pub const TAG: &str = "timeline";

/// `messages` are `(entry index, text)` oldest first; the row value is the
/// entry index.
pub fn timeline(messages: &[(usize, &str)]) -> Dialog {
    let items = messages
        .iter()
        .rev()
        .map(|(index, text)| Item::new(index.to_string(), text.replace('\n', " ")))
        .collect();
    Dialog::Select(Select::new("Timeline", items).tag(TAG))
}
