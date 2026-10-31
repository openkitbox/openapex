//! Network side of [`AppCommand`]: each command becomes one request, and its
//! outcome goes back to the UI loop as an [`AppEvent`].
//!
//! `main` runs [`run`] in its own task per command, so a slow request (model
//! catalogue, history reload) never delays an interrupt or a form reply. The
//! commands that own the SSE attachment (`CreateSessionAndPrompt`,
//! `AttachSession`, `DetachSession`, `Quit`) stay on `main`'s serial loop and
//! are no-ops here.

use ox_client::Client;
use ox_schema::support::FileInput;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::command::{AgentIntent, AppCommand, Switch};
use crate::app::event::AppEvent;

type Events = UnboundedSender<AppEvent>;

/// A failed session request as a toast event.
pub fn session_failed(title: &'static str, err: ox_client::Error) -> AppEvent {
    tracing::warn!(%err, title);
    AppEvent::SessionOpFailed {
        title,
        error: err.to_string(),
    }
}

/// Execute one command and report its outcome on `tx`.
pub async fn run(client: Client, tx: Events, command: AppCommand) {
    match command {
        AppCommand::Prompt {
            session,
            text,
            files,
        } => prompt(&client, &tx, &session, &text, &files).await,
        AppCommand::Interrupt { session } => interrupt(&client, &tx, &session).await,
        AppCommand::ListSessions => list_sessions(&client, &tx).await,
        AppCommand::ListFamily { session } => list_family(&client, &tx, session).await,
        AppCommand::ListShells { session } => list_shells(&client, &tx, session).await,
        AppCommand::ShellOutput { session, id } => shell_output(&client, &tx, session, &id).await,
        AppCommand::KillShell { session, id } => kill_shell(&client, &tx, session, &id).await,
        AppCommand::RenameSession { session, title } => {
            rename_session(&client, &tx, &session, &title).await
        }
        AppCommand::DeleteSession { session } => delete_session(&client, &tx, session).await,
        AppCommand::ForkSession { session } => fork_session(&client, &tx, &session).await,
        AppCommand::ReplyForm {
            session,
            form,
            answer,
        } => report_form_result(&tx, client.reply_form(&session, &form, &answer).await),
        AppCommand::CancelForm { session, form } => {
            report_form_result(&tx, client.cancel_form(&session, &form).await)
        }
        AppCommand::ListForms { session } => list_forms(&client, &tx, &session).await,
        AppCommand::FindFiles(query) => find_files(&client, &tx, query).await,
        AppCommand::LoadAttachments(text) => load_attachments(&tx, text).await,
        AppCommand::ReadClipboard => read_clipboard(&tx).await,
        AppCommand::WriteClipboard(text) => write_clipboard(&tx, text).await,
        AppCommand::ProbeChildren { children } => probe_children(&client, &tx, children).await,
        AppCommand::LoadSessionChrome { session } => session_chrome(&client, &tx, session).await,
        AppCommand::McpList => mcp_list(&client, &tx).await,
        AppCommand::ModelList => model_list(&client, &tx).await,
        AppCommand::LoadContextLimits => context_limits(&client, &tx).await,
        AppCommand::AgentList(intent) => agent_list(&client, &tx, intent).await,
        AppCommand::Switch { session, switch } => switch_to(&client, &tx, &session, switch).await,
        AppCommand::ReloadHistory {
            session,
            request_id,
        } => reload_history(&client, &tx, &session, request_id).await,
        // Owned by main's serial loop (they touch the SSE attachment).
        AppCommand::CreateSessionAndPrompt(_)
        | AppCommand::AttachSession { .. }
        | AppCommand::DetachSession
        | AppCommand::Quit => {}
    }
}

async fn prompt(client: &Client, tx: &Events, session: &str, text: &str, files: &[FileInput]) {
    if let Err(err) = client.prompt(session, text, files).await {
        tracing::warn!(%err, "prompt failed");
        let _ = tx.send(AppEvent::PromptFailed(err.to_string()));
    }
}

async fn interrupt(client: &Client, tx: &Events, session: &str) {
    if let Err(err) = client.interrupt(session).await {
        let _ = tx.send(session_failed("Failed to interrupt", err));
    }
}

async fn list_sessions(client: &Client, tx: &Events) {
    let event = match client.sessions().await {
        Ok(sessions) => AppEvent::Sessions(sessions),
        Err(err) => session_failed("Failed to list sessions", err),
    };
    let _ = tx.send(event);
}

async fn list_family(client: &Client, tx: &Events, session: String) {
    let event = match client.session_family(&session).await {
        Ok(members) => AppEvent::Family { session, members },
        Err(err) => session_failed("Failed to list subagents", err),
    };
    let _ = tx.send(event);
}

async fn list_shells(client: &Client, tx: &Events, session: String) {
    let event = match client.shells().await {
        Ok(shells) => AppEvent::Shells { session, shells },
        Err(err) => session_failed("Failed to list shells", err),
    };
    let _ = tx.send(event);
}

/// Newest output kept for the viewer (upstream `PAGE_BYTES`).
const SHELL_TAIL_BYTES: usize = 64 * 1024;

/// The shell's current state and its recent output, for the viewer. A shell
/// the server no longer lists is reported as not found.
async fn shell_output(client: &Client, tx: &Events, session: String, id: &str) {
    let event = match read_shell(client, id).await {
        Ok((shell, tail)) => AppEvent::ShellOutput {
            session,
            shell: Box::new(shell),
            tail,
        },
        Err(err) => session_failed("Failed to read shell output", err),
    };
    let _ = tx.send(event);
}

async fn read_shell(
    client: &Client,
    id: &str,
) -> ox_client::Result<(ox_client::rest::ShellInfo, ox_client::rest::ShellTail)> {
    let shell = client
        .shells()
        .await?
        .into_iter()
        .find(|shell| shell.id == id)
        .ok_or(ox_client::Error::Status(
            ox_client::reqwest::StatusCode::NOT_FOUND,
        ))?;
    let tail = client.shell_tail(id, SHELL_TAIL_BYTES).await?;
    Ok((shell, tail))
}

/// Kill, then answer with the fresh list so the open picker drops the row.
async fn kill_shell(client: &Client, tx: &Events, session: String, id: &str) {
    if let Err(err) = client.kill_shell(id).await {
        let _ = tx.send(session_failed("Failed to kill shell", err));
        return;
    }
    list_shells(client, tx, session).await;
}

async fn rename_session(client: &Client, tx: &Events, session: &str, title: &str) {
    if let Err(err) = client.rename_session(session, title).await {
        let _ = tx.send(session_failed("Failed to rename session", err));
    }
}

async fn delete_session(client: &Client, tx: &Events, session: String) {
    let event = match client.delete_session(&session).await {
        Ok(()) => AppEvent::SessionDeleted(session),
        Err(err) => session_failed("Failed to delete session", err),
    };
    let _ = tx.send(event);
}

async fn fork_session(client: &Client, tx: &Events, session: &str) {
    let event = match client.fork_session(session).await {
        Ok(forked) => AppEvent::SessionForked(forked),
        Err(err) => session_failed("Failed to fork session", err),
    };
    let _ = tx.send(event);
}

async fn list_forms(client: &Client, tx: &Events, session: &str) {
    match client.forms(session).await {
        Ok(forms) => {
            let _ = tx.send(AppEvent::Forms(forms));
        }
        Err(err) => tracing::warn!(%err, "form list failed"),
    }
}

async fn find_files(client: &Client, tx: &Events, query: String) {
    match client.find_files(&query).await {
        Ok(entries) => {
            let _ = tx.send(AppEvent::Files { query, entries });
        }
        Err(err) => tracing::warn!(%err, "file lookup failed"),
    }
}

/// Read the files a paste names on a blocking thread (up to 20 MiB of disk
/// I/O). Only the parsed paths move into the task, so the text stays here and
/// goes back with the answer even if the task panics.
async fn load_attachments(tx: &Events, text: String) {
    let paths = crate::ui::paste::pasted_attachment_paths(&text);
    let loaded = tokio::task::spawn_blocking(move || crate::attachment::load(&paths))
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "attachment load task failed");
            None
        });
    let _ = tx.send(AppEvent::Attachments { text, loaded });
}

/// Read the system clipboard on a blocking thread (it runs subprocesses, each
/// bounded by a timeout). A panicked task reads as an empty clipboard.
async fn read_clipboard(tx: &Events) {
    let clip = tokio::task::spawn_blocking(crate::clipboard::read)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "clipboard read task failed");
            crate::clipboard::Clip::Empty
        });
    let _ = tx.send(AppEvent::ClipboardPasted(clip));
}

/// Copy `text` to the host clipboard on a blocking thread. The text is moved
/// in; a panicked task or a failed write reports `host_ok: false`.
async fn write_clipboard(tx: &Events, text: String) {
    let host_ok = tokio::task::spawn_blocking(move || crate::clipboard::write(&text).is_ok())
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "clipboard write task failed");
            false
        });
    let _ = tx.send(AppEvent::ClipboardWritten { host_ok });
}

async fn session_chrome(client: &Client, tx: &Events, session: String) {
    // 尽力而为：取不到就保留当前显示，不打扰用户。
    match client.session_info(&session).await {
        Ok(info) => {
            let providers = client.providers().await.unwrap_or_default();
            let selection = crate::bootstrap::selection_of(&info, &providers);
            let _ = tx.send(AppEvent::SessionChrome { session, selection });
        }
        Err(err) => tracing::warn!(%err, "session chrome lookup failed"),
    }
}

/// Probe the children one after another (a handful at most, so one task and
/// no fan-out). A failed lookup is skipped: the event stream still drives
/// that child.
async fn probe_children(client: &Client, tx: &Events, children: Vec<String>) {
    for child in children {
        match client.session_info(&child).await {
            Ok(info) => {
                let running = info.time.is_running();
                let _ = tx.send(AppEvent::ChildRunning { child, running });
            }
            Err(err) => tracing::warn!(%err, child, "child session probe failed"),
        }
    }
}

async fn mcp_list(client: &Client, tx: &Events) {
    match client.mcp().await {
        Ok(servers) => {
            let _ = tx.send(AppEvent::McpServers(servers));
        }
        Err(err) => tracing::warn!(%err, "mcp list failed"),
    }
}

async fn context_limits(client: &Client, tx: &Events) {
    // 尽力而为：取不到就只显示 token 数，和上游缺 limit 时一致。
    match client.models().await {
        Ok(models) => {
            let limits = models
                .into_iter()
                .filter(|model| model.limit.context > 0)
                .map(|model| {
                    (
                        format!("{}/{}", model.provider_id, model.model_id),
                        model.limit.context,
                    )
                })
                .collect();
            let _ = tx.send(AppEvent::ContextLimits(limits));
        }
        Err(err) => tracing::warn!(%err, "context limit lookup failed"),
    }
}

async fn model_list(client: &Client, tx: &Events) {
    match tokio::try_join!(client.models(), client.providers()) {
        Ok((models, providers)) => {
            let _ = tx.send(AppEvent::Models { models, providers });
        }
        Err(err) => {
            tracing::warn!(%err, "model list failed");
            let _ = tx.send(AppEvent::SwitchFailed {
                title: "Failed to load models",
                err: err.to_string(),
            });
        }
    }
}

async fn agent_list(client: &Client, tx: &Events, intent: AgentIntent) {
    match client.agents().await {
        Ok(agents) => {
            let _ = tx.send(AppEvent::Agents { agents, intent });
        }
        Err(err) => {
            tracing::warn!(%err, "agent list failed");
            let _ = tx.send(AppEvent::SwitchFailed {
                title: "Failed to load agents",
                err: err.to_string(),
            });
        }
    }
}

async fn switch_to(client: &Client, tx: &Events, session: &str, switch: Switch) {
    let result = match &switch {
        Switch::Model {
            provider_id,
            model_id,
            ..
        } => client.switch_model(session, provider_id, model_id).await,
        Switch::Agent { id, .. } => client.switch_agent(session, id).await,
    };
    match result {
        Ok(()) => {
            let _ = tx.send(AppEvent::Switched(switch));
        }
        Err(err) => {
            tracing::warn!(%err, "switch failed");
            let _ = tx.send(AppEvent::SwitchFailed {
                title: switch.failure_title(),
                err: err.to_string(),
            });
        }
    }
}

async fn reload_history(client: &Client, tx: &Events, session: &str, request_id: u64) {
    match client.messages(session, None).await {
        Ok(page) => {
            let mut messages = page.data;
            messages.reverse(); // desc → chronological
            let items = ox_client::history::parse_messages(&messages);
            let _ = tx.send(AppEvent::History { request_id, items });
        }
        Err(err) => {
            let _ = tx.send(session_failed("Failed to load history", err));
        }
    }
}

/// 409 (already settled) and 404 (gone) mean the form is done either way; any
/// other failure is shown and the form list is re-fetched.
fn report_form_result(tx: &Events, result: ox_client::Result<()>) {
    match result {
        Ok(()) => {}
        Err(ox_client::Error::Status(status)) if matches!(status.as_u16(), 404 | 409) => {}
        Err(err) => {
            tracing::warn!(%err, "form reply failed");
            let _ = tx.send(AppEvent::FormFailed(err.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    /// Nothing listens on port 1: every request fails with a connect error,
    /// which is all these tests need.
    fn offline() -> Client {
        Client::new("http://127.0.0.1:1", ".")
    }

    async fn first_event(command: AppCommand) -> AppEvent {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run(offline(), tx, command).await;
        rx.try_recv().expect("the failure must be reported")
    }

    #[tokio::test]
    async fn a_failed_interrupt_becomes_a_toast_event() {
        let event = first_event(AppCommand::Interrupt {
            session: "ses_x".into(),
        })
        .await;
        assert!(matches!(
            event,
            AppEvent::SessionOpFailed {
                title: "Failed to interrupt",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_failed_shell_output_read_becomes_a_toast_event() {
        let event = first_event(AppCommand::ShellOutput {
            session: "ses_x".into(),
            id: "sh_1".into(),
        })
        .await;
        assert!(matches!(
            event,
            AppEvent::SessionOpFailed {
                title: "Failed to read shell output",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_failed_history_reload_becomes_a_toast_event() {
        let event = first_event(AppCommand::ReloadHistory {
            session: "ses_x".into(),
            request_id: 3,
        })
        .await;
        assert!(matches!(
            event,
            AppEvent::SessionOpFailed {
                title: "Failed to load history",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn an_unreachable_server_yields_no_probe_answers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run(
            offline(),
            tx,
            AppCommand::ProbeChildren {
                children: vec!["ses_a".into(), "ses_b".into()],
            },
        )
        .await;
        assert!(rx.try_recv().is_err(), "a failed probe says nothing");
    }

    #[tokio::test]
    async fn serial_commands_are_left_to_the_main_loop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run(offline(), tx, AppCommand::DetachSession).await;
        assert!(rx.try_recv().is_err());
    }
}
