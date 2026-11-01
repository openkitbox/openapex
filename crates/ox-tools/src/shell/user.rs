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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::shell::sessions::{ShellSessions, SpawnRequest};

    /// Spawn `command` with no owner.
    fn spawn(command: &str, timeout_ms: Option<u64>) -> std::sync::Arc<ShellSession> {
        let sessions = std::sync::Arc::new(ShellSessions::new());
        sessions
            .spawn(SpawnRequest {
                command: command.to_owned(),
                cwd: std::env::temp_dir(),
                timeout_ms,
                completion: None,
                foreground: false,
                owner: None,
                metadata: Record::new(),
            })
            .expect("spawn shell")
    }

    /// Spawn `command`, then wait (bounded) for the terminal state a
    /// notification reads.
    async fn run(command: &str, timeout_ms: Option<u64>) -> std::sync::Arc<ShellSession> {
        let session = spawn(command, timeout_ms);
        assert!(
            session.wait_settled(Duration::from_secs(30)).await,
            "shell `{command}` never settled",
        );
        session
    }

    fn envelope(session: &ShellSession, body: &str) -> String {
        format!(
            "The following shell command was executed by the user:\n\
             <shell id=\"{}\" state=\"{}\" command=\"{}\">\n{body}\n</shell>",
            session.info().id,
            if session.info().status == ShellStatus::Killed {
                "cancelled"
            } else {
                "completed"
            },
            session.info().command,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exit_zero_omits_the_status_line() {
        let session = run("printf hi", None).await;
        let payload = user_notification(&session);
        assert_eq!(
            payload.text,
            envelope(&session, "hi"),
            "exit 0 has no status line"
        );
        let metadata = payload.metadata.clone().expect("metadata");
        assert_eq!(metadata["source"], json!("shell"), "upstream source tag");
        assert_eq!(
            metadata["state"],
            json!("completed"),
            "a finished command is completed"
        );
        assert_eq!(
            metadata["truncated"],
            json!(false),
            "two bytes never truncate"
        );
        assert_eq!(metadata["exit"], json!(0), "exit code is recorded");
        assert!(!metadata.contains_key("signal"), "no signal on exit 0");
        assert!(
            payload.description.is_none(),
            "upstream sets no description"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nonzero_exit_reports_the_code() {
        let session = run("printf done; exit 3", None).await;
        let payload = user_notification(&session);
        assert_eq!(
            payload.text,
            envelope(&session, "done\n\nExited with code 3"),
            "nonzero exit appends the code",
        );
        assert_eq!(
            payload.metadata.expect("metadata")["exit"],
            json!(3),
            "exit code is recorded",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn killed_reports_cancelled() {
        let session = spawn("sleep 30", None);
        // `stop` returns once the terminal state is published.
        session.stop(true).await;
        let payload = user_notification(&session);
        assert_eq!(
            payload.text,
            envelope(&session, "(no output)\n\nCancelled"),
            "a killed command reads as cancelled",
        );
        let metadata = payload.metadata.expect("metadata");
        assert_eq!(
            metadata["state"],
            json!("cancelled"),
            "killed maps to cancelled"
        );
        assert_eq!(metadata["signal"], json!("9"), "SIGKILL is recorded as 9");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_reports_the_notice() {
        let session = run("sleep 30", Some(100)).await;
        let payload = user_notification(&session);
        assert!(
            payload
                .text
                .contains("\n\nTimed out before completion\n</shell>"),
            "timeout line missing: {}",
            payload.text,
        );
        assert_eq!(
            payload.metadata.expect("metadata")["timeout"],
            json!(true),
            "timeout flag is recorded",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_output_says_so() {
        let session = run("true", None).await;
        let payload = user_notification(&session);
        assert_eq!(
            payload.text,
            envelope(&session, "(no output)"),
            "empty output is spelled out",
        );
    }
}
