//! The user-facing completion notification of a shell command the user ran
//! through `session.shell` (upstream `packages/core/src/shell/result.ts`
//! `userNotification`).
//!
//! This is not the model-facing completion ([`super::completion`]): the text
//! is a transcript entry for the conversation, and its metadata is the
//! `source: "shell"` envelope the UI keys on. Text and metadata are ported
//! verbatim; the deviations that remain are the capture ones (D1-D3 in
//! `.ox/plans/2026-11-01-cheap-parity-gaps.md`): the capture is ox's bounded
//! in-memory tail, so the truncation notice counts the retained tail and
//! `file` is `ox-shell://{id}`, the signal is the numeric string that prints
//! `Killed by SIG*` when the name is known, and the limits are ox's
//! `MAX_BYTES`/`MAX_LINES` rather than the `tool_output` config key.

use ox_schema::inbox::SyntheticPayload;
use ox_schema::support::{Record, ShellOutput, ShellStatus};
use serde_json::json;

use super::sessions::ShellSession;
use super::{signal_name_of, truncate_output};

/// Upstream `ShellResult.unavailable` output: what a notification says when
/// the capture is gone. `cursor`/`size` are its byte length, as upstream.
#[must_use]
pub fn unavailable_output() -> ShellOutput {
    const MISSING: &str = "Shell command output is no longer available.";
    ShellOutput {
        output: MISSING.to_owned(),
        cursor: i64::try_from(MISSING.len()).unwrap_or(i64::MAX),
        size: i64::try_from(MISSING.len()).unwrap_or(i64::MAX),
        truncated: false,
    }
}

/// The terminal notification for a user shell command: the transcript text
/// plus the metadata the UI reads. Call after the session settled.
#[must_use]
pub fn user_notification(session: &ShellSession) -> SyntheticPayload {
    let info = session.info();
    let (text, truncated, total, shown) = truncate_output(&session.tail());
    let notice = if truncated {
        format!(
            "\n\n[showing lines {}-{total} of {total}; full output saved to {}]",
            total - shown + 1,
            info.file,
        )
    } else {
        String::new()
    };
    let output = format!(
        "{}{notice}",
        if text.is_empty() {
            "(no output)"
        } else {
            text.as_str()
        },
    );
    let timeout = info.status == ShellStatus::Timeout;
    let status = status_line(&info, timeout);
    let state = if info.status == ShellStatus::Killed {
        "cancelled"
    } else {
        "completed"
    };
    let body = [output, status.unwrap_or_default()]
        .into_iter()
        .filter(|item| !item.is_empty())
        .collect::<Vec<String>>()
        .join("\n\n");
    let text = format!(
        "The following shell command was executed by the user:\n\
         <shell id=\"{}\" state=\"{state}\" command=\"{}\">\n{body}\n</shell>",
        info.id, info.command,
    );

    let mut metadata = Record::new();
    metadata.insert("source".to_owned(), json!("shell"));
    metadata.insert("shellID".to_owned(), json!(info.id));
    metadata.insert("state".to_owned(), json!(state));
    metadata.insert("truncated".to_owned(), json!(truncated));
    if let Some(exit) = info.exit {
        metadata.insert("exit".to_owned(), json!(exit));
    }
    if let Some(signal) = info.signal {
        metadata.insert("signal".to_owned(), json!(signal));
    }
    if timeout {
        metadata.insert("timeout".to_owned(), json!(true));
    }

    SyntheticPayload {
        text,
        description: None,
        metadata: Some(metadata),
    }
}

/// Upstream `status`, where a killed run is reported as cancelled instead.
fn status_line(info: &ox_schema::support::ShellInfo, timeout: bool) -> Option<String> {
    if info.status == ShellStatus::Killed {
        return Some("Cancelled".to_owned());
    }
    if info.exit == Some(0) {
        return None;
    }
    if timeout {
        return Some("Timed out before completion".to_owned());
    }
    if let Some(signal) = info.signal.as_deref() {
        // D2: the recorded signal is a number; name it when known.
        let named = signal
            .parse::<i32>()
            .ok()
            .and_then(signal_name_of)
            .map_or(signal, |name| name);
        return Some(format!("Killed by {named}"));
    }
    Some(match info.exit {
        Some(code) => format!("Exited with code {code}"),
        None => "Exited with code unknown".to_owned(),
    })
}
