//! Network side of [`AppCommand`]: each command becomes one request, and its
//! outcome goes back to the UI loop as an [`AppEvent`].
//!
//! `main` runs [`run`] in its own task per command, so a slow request (model
//! catalogue, history reload) never delays an interrupt or a form reply. The
//! commands that own the SSE attachment (`CreateSessionAndPrompt`,
//! `AttachSession`, `DetachSession`, `Quit`) stay on `main`'s serial loop and
//! are no-ops here.

use std::collections::{HashMap, HashSet};
use std::mem;
use std::sync::Arc;

use ox_client::Client;
use ox_client::store::MediaSource;
use ox_client::store::RunningShell;
use ox_schema::support::FileInput;
use ratatui::layout::Size;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::command::{
    AgentIntent, AppCommand, CancelReason, McpAction, McpIntent, McpStamp, Switch,
};
use crate::app::event::AppEvent;
use crate::app::event::Fetched;
use crate::graphics::cache::SizeClass;

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
        AppCommand::ListRunningRoots => list_running_roots(&client, &tx).await,
        AppCommand::ListFamily { session } => list_family(&client, &tx, session).await,
        AppCommand::ListShells { session } => list_shells(&client, &tx, session).await,
        AppCommand::SyncShells { generation } => sync_shells(&client, generation, &tx).await,
        AppCommand::ShellOutput { session, id } => shell_output(&client, &tx, session, &id).await,
        AppCommand::ShellPoll { id, cursor, mode } => {
            shell_poll(&client, &tx, id, cursor, mode).await
        }
        AppCommand::KillShell { session, id } => kill_shell(&client, &tx, session, &id).await,
        AppCommand::SteerQueued { session, inbox } => {
            if let Err(err) = client.steer_queued(&session, &inbox).await {
                let _ = tx.send(session_failed("Failed to steer pending prompt", err));
            }
        }
        AppCommand::CancelQueued {
            session,
            inbox,
            reason,
        } => {
            if let Err(err) = client.cancel_queued(&session, &inbox).await {
                let title = match reason {
                    CancelReason::Delete => "Failed to delete pending prompt",
                    CancelReason::Undo => "Failed to undo pending prompt",
                };
                let _ = tx.send(session_failed(title, err));
            }
        }
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
        AppCommand::FetchMedia {
            session,
            key,
            source,
            cells,
            class,
        } => fetch_media(&client, &tx, &session, key, source, cells, class).await,
        AppCommand::MediaInline {
            key,
            bytes,
            cells,
            class,
        } => media_inline(&tx, key, bytes, cells, class).await,
        AppCommand::LoadSessionChrome { session } => session_chrome(&client, &tx, session).await,
        AppCommand::McpList { intent, stamp } => mcp_list(&client, &tx, intent, stamp).await,
        AppCommand::McpToggle {
            server,
            action,
            stamp,
        } => mcp_toggle(&client, &tx, server, action, stamp).await,
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
        Ok(sessions) => AppEvent::Sessions {
            sessions,
            running: running_roots(client).await,
        },
        Err(err) => session_failed("Failed to list sessions", err),
    };
    let _ = tx.send(event);
}

async fn list_running_roots(client: &Client, tx: &Events) {
    let _ = tx.send(AppEvent::RunningRoots(running_roots(client).await));
}

/// Root sessions with work in flight in their tree: the session itself or any
/// descendant (upstream marks a row whose `family` has a running member).
/// Each active id climbs `parentID` to its root; the active set is a handful
/// of sessions, and a shared visited map stops repeated climbs. A failed
/// lookup drops that chain only: no spinner is the safe reading.
async fn running_roots(client: &Client) -> HashSet<String> {
    let active = match client.active_sessions().await {
        Ok(active) => active,
        Err(err) => {
            tracing::warn!(%err, "active sessions lookup failed");
            return HashSet::new();
        }
    };
    // Session → its root, for every id seen while climbing.
    let mut root_of: HashMap<String, String> = HashMap::new();
    let mut roots = HashSet::new();
    for start in active {
        let mut chain = Vec::new();
        let mut id = start;
        let root = loop {
            if let Some(root) = root_of.get(&id) {
                break Some(root.clone());
            }
            // A cycle would be a corrupt store; give up on this chain.
            if chain.contains(&id) || chain.len() > MAX_CLIMB {
                break None;
            }
            match client.session_info(&id).await {
                Ok(info) => match info.parent_id {
                    Some(parent) => chain.push(mem::replace(&mut id, parent)),
                    None => break Some(id.clone()),
                },
                Err(err) => {
                    tracing::warn!(%err, id, "session lookup failed");
                    break None;
                }
            }
        };
        let Some(root) = root else { continue };
        chain.push(id);
        for member in chain {
            root_of.insert(member, root.clone());
        }
        roots.insert(root);
    }
    roots
}

/// Deepest subagent nesting `running_roots` climbs before giving up.
const MAX_CLIMB: usize = 32;

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

/// The same `GET /api/shell` the picker uses, reduced to the running shells
/// and the session that owns each (the prompt footer's shell count). The
/// status filter is defensive: the server's list already holds only unsettled
/// sessions. A failure is logged only: the `shell.*` frames still drive the
/// map.
async fn sync_shells(client: &Client, generation: u64, tx: &Events) {
    match client.shells().await {
        Ok(shells) => {
            let running: Vec<RunningShell> = shells
                .into_iter()
                .filter(|shell| shell.status == "running")
                .map(|shell| RunningShell {
                    session: shell.session_id().map(str::to_owned),
                    id: shell.id,
                })
                .collect();
            let _ = tx.send(AppEvent::RunningShells {
                generation,
                shells: running,
            });
        }
        Err(err) => tracing::warn!(%err, "shell sync failed"),
    }
}

/// Read a shell block's output. A failure is logged, not toasted (it repeats
/// every second while the server is away), and still answered so the block's
/// in-flight request is released.
async fn shell_poll(
    client: &Client,
    tx: &Events,
    id: String,
    cursor: u64,
    mode: ox_client::rest::ShellReadMode,
) {
    let limit = crate::shell_captures::DISPLAY_LIMIT;
    let read = match client.shell_read(&id, cursor, mode, limit).await {
        Ok(read) => Some(read),
        Err(err) => {
            tracing::warn!(%err, shell = id, "shell output poll failed");
            None
        }
    };
    let _ = tx.send(AppEvent::ShellPolled {
        id,
        start: cursor,
        read,
    });
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

/// One attachment's bytes, and on a sixel terminal the encode for the grid it
/// will be drawn on.
///
/// The encode decodes the image and resamples it, so it runs on a blocking
/// thread: a 4000px photo would otherwise stall the runtime that also carries
/// the session's event stream. Kitty needs none of that — the terminal scales the
/// transmission itself — but the payload still has to be PNG for it.
async fn fetch_media(
    client: &Client,
    tx: &Events,
    session: &str,
    key: String,
    source: MediaSource,
    cells: Option<Size>,
    class: SizeClass,
) {
    let (message, part, index) = match &source {
        MediaSource::Prompt { message, index } => (message.as_ref(), None, *index),
        MediaSource::Tool {
            message,
            part,
            index,
        } => (message.as_ref(), Some(part.as_ref()), *index),
    };
    let result = match client.media(session, message, part, index).await {
        Ok(media) => prepared(Arc::from(media.bytes), cells).await,
        Err(err) => Err(err.to_string()),
    };
    let _ = tx.send(AppEvent::Media { key, class, result });
}

/// One attachment the composer already holds: no request, but a terminal that
/// encodes per grid still needs the encode its grid demands.
async fn media_inline(
    tx: &Events,
    key: String,
    bytes: Arc<[u8]>,
    cells: Option<Size>,
    class: SizeClass,
) {
    let result = prepared(bytes, cells).await;
    let _ = tx.send(AppEvent::Media { key, class, result });
}

/// The bytes a drawing terminal can cache: the payload as PNG for kitty, an
/// encode for the grid for sixel, which cannot reuse one encode at another size.
///
/// Kitty is handed `f=100`, which is PNG data, so a stored JPEG or WebP is
/// re-encoded once here rather than drawn as nothing.
async fn prepared(bytes: Arc<[u8]>, cells: Option<Size>) -> Result<Fetched, String> {
    let Some(cells) = cells else {
        let png = tokio::task::spawn_blocking(move || crate::graphics::png::ensure(bytes))
            .await
            .map_err(|err| err.to_string())?;
        return png
            .map(Fetched::Png)
            .ok_or_else(|| "these bytes are not an image this build can decode".to_string());
    };
    let Some(picker) = crate::graphics::probe::capabilities().picker.clone() else {
        return Err("this terminal cannot encode images".to_string());
    };
    match tokio::task::spawn_blocking(move || {
        crate::graphics::sixel::prepare(&picker, &bytes, cells)
    })
    .await
    {
        Ok(Ok(prepared)) => Ok(Fetched::Sixel(Box::new(prepared))),
        Ok(Err(err)) => Err(err.to_string()),
        Err(err) => Err(err.to_string()),
    }
}

/// Only the `/mcps` dialog the user asked for reports a failed fetch; a
/// refresh or the `/status` dialog leaves it in the log.
async fn mcp_list(client: &Client, tx: &Events, intent: McpIntent, stamp: McpStamp) {
    match client.mcp().await {
        Ok(servers) => {
            let _ = tx.send(AppEvent::McpServers {
                servers,
                intent,
                stamp,
            });
        }
        Err(err) if intent == McpIntent::Dialog => {
            let _ = tx.send(session_failed("Failed to load MCP servers", err));
        }
        Err(err) => tracing::warn!(%err, "mcp list failed"),
    }
}

/// Connect or disconnect, re-list whatever happened (the server's status has
/// settled once the request returns), then report it: the dialog settles the
/// row after the fresh list is in. A list stamped later but read before the
/// change can still win briefly; the server's final `mcp.status.changed`
/// re-list corrects it.
async fn mcp_toggle(
    client: &Client,
    tx: &Events,
    server: String,
    action: McpAction,
    stamp: McpStamp,
) {
    let result = match action {
        McpAction::Connect => client.mcp_connect(&server).await,
        McpAction::Disconnect => client.mcp_disconnect(&server).await,
    };
    let error = result.err().map(|err| {
        tracing::warn!(%err, %server, ?action, "mcp toggle failed");
        err.to_string()
    });
    mcp_list(client, tx, McpIntent::Refresh, stamp).await;
    let _ = tx.send(AppEvent::McpToggled {
        server,
        action,
        error,
    });
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

/// Fetch the session's history together with its run state.
///
/// The stream attaches at the live tail, so a turn already in flight never
/// sends its `execution.started` again: without a snapshot the footer spinner,
/// the reasoning block and the interrupt key all read idle. The runner's active
/// set (`GET /api/session/active`) is the authority for whether a turn is
/// running — the stored `time.busy`/`time.idle` pair can be stale after a crash
/// — and the session's `time.busy` dates the turn it reports. A failed active
/// query counts as not running (fail closed: no spinner for a turn nobody would
/// ever end here); a missing stamp dates the turn from now.
async fn reload_history(client: &Client, tx: &Events, session: &str, request_id: u64) {
    let (history, active, info) = tokio::join!(
        client.messages(session, None),
        client.active_sessions(),
        client.session_info(session),
    );
    let page = match history {
        Ok(page) => page,
        Err(err) => {
            let _ = tx.send(session_failed("Failed to load history", err));
            return;
        }
    };
    let info = match info {
        Ok(info) => Some(info),
        Err(err) => {
            tracing::warn!(%err, session, "session info failed");
            None
        }
    };
    let running_since = match active {
        Ok(active) if active.iter().any(|id| id == session) => Some(
            info.as_ref()
                .and_then(|info| info.time.busy)
                .unwrap_or_else(crate::ui::timeline::wall_ms_now),
        ),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(%err, session, "active session list failed; assuming idle");
            None
        }
    };
    let mut messages = page.data;
    messages.reverse(); // desc → chronological
    let items = ox_client::history::parse_messages(&messages);
    let _ = tx.send(AppEvent::History {
        request_id,
        items,
        running_since,
    });
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
