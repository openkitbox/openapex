pub mod command;
pub mod event;
pub mod keymap;
pub mod state;
mod switching;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ox_client::rest::{FamilyMember, ShellInfo, ShellTail};
use ox_client::sse::StreamMsg;
use ox_client::store::{DataStore, Hydration, Item, ShellRef};
use ox_schema::support::FileInput;
use ratatui::layout::Rect;
use tokio::sync::{mpsc, watch};

use crate::attachment::Loaded;
use crate::clipboard::Clip;
use crate::graphics::Graphics;
use crate::graphics::SizeClass;
use crate::herdr;
use crate::pipeline;
use crate::shell_captures::ShellCaptures;
use crate::terminal::TerminalGuard;
use crate::ui::activity_panel;
use crate::ui::commands::CommandAction;
use crate::ui::composer::{SearchAction, SearchBox, Submission};
use crate::ui::dialog::{Dialog, DialogOutcome};
use crate::ui::dock::{self, DockHover, StatusMode};
use crate::ui::image_preview;
use crate::ui::layout;
use crate::ui::mcp_dialog::{self, McpDialog};
use crate::ui::media;
use crate::ui::output_dialog::{self, Output};
use crate::ui::palette;
use crate::ui::paste;
use crate::ui::prompt::{
    Activity, Chrome, Footer, Live, Prompt, PromptAction, pick_placeholder, placeholder_line,
};
use crate::ui::queued_dialog;
use crate::ui::selection::Outcome;
use crate::ui::session_dialog;
use crate::ui::timeline::Transcript;
use crate::ui::timeline_dialog;
use crate::ui::toast::{self, Toast};
use command::{AppCommand, CancelReason, McpAction, McpIntent, McpStamp};
use event::AppEvent;
use event::Fetched;
use keymap::{Action, Feed, Keymap};
use state::{Route, ViewState};

/// Lines per mouse-wheel notch.
const WHEEL_LINES: usize = 3;

pub const RENDER_TICK: Duration = Duration::from_millis(33);
const SCROLL_PAGE: usize = 10;
/// How many answered form ids are remembered to filter stale `ListForms` answers.
const SETTLED_FORMS_CAP: usize = 32;

/// Home 和 session 各有一套 footer 内容（位置/MCP 数量 vs 按键提示）。
fn footer_of(route: Route) -> Footer {
    match route {
        Route::Home => Footer::Home,
        Route::Session => Footer::Session,
    }
}

/// Three modes, no cursor: `Compose` types into the prompt, `SearchInput`
/// edits the `ctrl+f` query, `Browse` navigates the finished search (n/N, j/k,
/// g/G). Letter keys are global only in `Browse`, so typing a message is never
/// hijacked.
pub enum Mode {
    Compose,
    // Boxed: TextArea is hundreds of bytes next to two unit variants
    // (clippy::large_enum_variant), and the box is allocated only while the
    // search prompt is open.
    SearchInput(Box<SearchBox>),
    Browse,
}

/// The home submission awaiting its session. Only the text is kept for a
/// restore; the attachments travel with the command alone (keeping them here
/// would duplicate up to ~27 MB each), so a failure just counts what is lost.
#[derive(Debug, PartialEq)]
struct PendingHome {
    text: String,
    dropped_files: usize,
}

/// The history reload in flight: its request id, and the store's
/// [`DataStore::lifecycle_epoch`] when it was sent, so an `execution.*` event
/// that lands before the answer keeps the run state it set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingReload {
    request_id: u64,
    lifecycle_epoch: u64,
}

pub struct App {
    pub route: Route,
    pub session: Option<String>,
    pub view: ViewState,
    pub prompt: Prompt,
    pub store: DataStore,
    pub transcript: Transcript,
    pub commands: mpsc::UnboundedSender<AppCommand>,
    seq_tx: watch::Sender<Option<i64>>,
    next_reload_id: u64,
    pending_reload: Option<PendingReload>,
    /// Home 提交后等待会话创建：失败时把这段文本放回 composer。
    pending_home_prompt: Option<PendingHome>,
    /// Title of the open session, for the `/rename` prefill.
    session_title: Option<String>,
    dirty: bool,
    /// Stream events were reduced into the store but not yet turned into
    /// transcript blocks; `flush_store` does that once per frame.
    store_dirty: bool,
    /// The background hint state the transcript blocks were last built with;
    /// the tick compares it to the wall clock to rebuild on a change.
    background_hint: pipeline::BackgroundHint,
    /// Forms this client already answered or cancelled. A `ListForms` answer
    /// fetched before the server saw the reply must not bring them back.
    settled_forms: Vec<String>,
    mode: Mode,
    show_help: bool,
    /// Open modal, if any. Owning the rendered dialog keeps the render path
    /// free of data lookups.
    dialog: Option<Dialog>,
    /// What the open `Select` is for (see `ui::models::DialogPurpose`).
    dialog_purpose: Option<crate::ui::models::DialogPurpose>,
    /// Global bindings: the command palette and the `ctrl+x` leader.
    keymap: Keymap,
    /// The one visible toast; a newer one replaces it (upstream `ui/toast.tsx`).
    toast: Option<Toast>,
    /// `providerID/id` → context window, from the model catalogue. Fetched
    /// once, the first time a context figure needs a percentage.
    context_limits: std::collections::HashMap<String, i64>,
    limits_requested: bool,
    /// OSC 52 sequence of the last copy, written to the terminal before the
    /// next frame (the executor never touches the terminal).
    pending_osc52: Option<String>,
    /// Whether the last copy produced an OSC 52 sequence, so the terminal was
    /// asked to set its clipboard even if the host write fails.
    osc52_attempted: bool,
    /// Stamp of the last MCP list request issued, and of the last one
    /// applied. Each list runs in its own task and the server announces every
    /// status change twice, so answers can land out of order; a list older
    /// than the one shown is dropped instead of resurrecting `Connecting …`.
    mcp_issued: u64,
    mcp_applied: McpStamp,
    /// The family the activity panel lists, fetched when the panel opened and
    /// replaced on every fetch.
    family: Vec<FamilyMember>,
    /// The composer's activity panel while it is open (`ui::activity_panel`),
    /// with the tab it shows; `None` leaves the composer in place.
    panel: Option<activity_panel::Tab>,
    /// The panel's cursor: which subagent filter is listed (one at a time) and
    /// the selected row among the listed ones. It outlives the panel like
    /// upstream's in-memory `subagents-filter`.
    cursor: activity_panel::Cursor,
    /// The panel has just opened and its family is still in flight: upstream
    /// then puts the selection on the row of the session on screen, else on the
    /// first row, as soon as the list has rows.
    selection_pending: bool,
    /// Terminal graphics: the probe's answer, the prepared-image cache and the
    /// escapes queued for the next frame.
    pub graphics: Graphics,
    /// Attachment keys a fetch is already out for. A frame whose cells are
    /// still blank asks again, so the key is only held while the answer is in
    /// flight; a resident image is skipped by the cache, not by this set.
    media_pending: HashSet<(String, SizeClass)>,
    /// Attachment keys given up on, either because the fetch failed or because
    /// the cache refused the payload. Retrying every frame would hammer a 404
    /// (or repeat a refusal) and still draw nothing, so they are dropped until
    /// the session changes.
    media_failed: HashSet<(String, SizeClass)>,
    /// The user just asked for a shell's output viewer and has not seen it
    /// yet. A shell answer may open the viewer only for this; otherwise it may
    /// refresh a viewer that is still open. So a refresh that lands after
    /// `esc` is dropped.
    shell_wanted: bool,
    /// Output fetched for expanded shell blocks of the open session.
    shell_captures: ShellCaptures,
    /// Number of the last `SyncShells` sent. Kept here, not in the store, so
    /// it keeps counting across session switches and an answer to a request
    /// made for an earlier store never matches the current one.
    shell_syncs: u64,
    /// Where the queued-prompt dock was last drawn (empty when hidden), for
    /// the pointer.
    queued_dock_area: Rect,
    /// Whether the pointer is over the queued-prompt dock.
    dock_hover: DockHover,
    /// Whether the pointer is over the prompt footer's live chip.
    live_hover: bool,
    /// Reports the focused session's state to the herdr pane this TUI runs
    /// in; `None` outside herdr.
    herdr: Option<herdr::Reporter>,
}

impl App {
    /// Back the prompt history with a file (startup only; tests stay in memory).
    pub fn attach_history(&mut self, history: crate::ui::history::History) {
        self.prompt.set_history(history);
    }

    /// Report agent state to herdr (startup only, inside a herdr pane).
    pub fn attach_herdr(&mut self, reporter: herdr::Reporter) {
        self.herdr = Some(reporter);
    }

    /// Hand the herdr reporter back so the caller can release the pane after
    /// `run` returns.
    pub fn take_herdr(&mut self) -> Option<herdr::Reporter> {
        self.herdr.take()
    }

    #[must_use]
    pub fn new(
        route: Route,
        session: Option<String>,
        home_chrome: Chrome,
        commands: mpsc::UnboundedSender<AppCommand>,
        seq_tx: watch::Sender<Option<i64>>,
    ) -> Self {
        // DataStore has a private field, so FRU (`DataStore { ..Default::default() }`)
        // is E0451; assign the pub fields on a default instance instead.
        let mut store = DataStore::default();
        store.session = session.clone();
        // Upstream's session prompt carries no placeholder; only Home does.
        let placeholder = match route {
            Route::Home => placeholder_line(pick_placeholder()),
            Route::Session => String::new(),
        };
        Self {
            route,
            session,
            view: ViewState::new(),
            prompt: Prompt::new(&placeholder, home_chrome, footer_of(route)),
            store,
            transcript: Transcript::new(80, 20),
            commands,
            seq_tx,
            next_reload_id: 0,
            pending_reload: None,
            pending_home_prompt: None,
            session_title: None,
            dirty: true,
            store_dirty: false,
            background_hint: pipeline::BackgroundHint::Hidden,
            settled_forms: Vec::new(),
            mode: Mode::Compose,
            show_help: false,
            dialog: None,
            dialog_purpose: None,
            keymap: Keymap::default(),
            toast: None,
            context_limits: std::collections::HashMap::new(),
            limits_requested: false,
            pending_osc52: None,
            osc52_attempted: false,
            mcp_issued: 0,
            mcp_applied: McpStamp::default(),
            family: Vec::new(),
            panel: None,
            cursor: activity_panel::Cursor::new(),
            selection_pending: false,
            shell_wanted: false,
            shell_captures: ShellCaptures::default(),
            shell_syncs: 0,
            queued_dock_area: Rect::default(),
            dock_hover: DockHover::Off,
            live_hover: false,
            graphics: Graphics::new(),
            media_pending: HashSet::new(),
            media_failed: HashSet::new(),
            herdr: None,
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Quit => self.view.should_quit = true,
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Paste(text) => self.handle_paste(text),
            AppEvent::Attachments { text, loaded } => self.handle_attachments(text, loaded),
            AppEvent::ClipboardPasted(clip) => self.handle_clipboard(clip),
            AppEvent::ClipboardWritten { host_ok } => self.handle_clipboard_written(host_ok),
            AppEvent::Resize => self.dirty = true,
            AppEvent::MouseDown { x, y } => self.handle_mouse_down(x, y),
            AppEvent::MouseDrag { x, y } => self.handle_mouse_drag(x, y),
            AppEvent::MouseUp { x, y } => self.handle_mouse_up(x, y),
            AppEvent::Hover { x, y } => self.handle_hover(x, y),
            AppEvent::Wheel(direction) => self.handle_wheel(direction),
            AppEvent::Stream(msg) => self.handle_stream(msg),
            AppEvent::History {
                request_id,
                items,
                running_since,
            } => self.handle_history(request_id, items, running_since),
            AppEvent::ChildRunning { child, running } => {
                if self.store.apply_probe(&child, running) {
                    self.family_state_changed();
                }
            }
            AppEvent::SessionReady {
                session,
                prompt,
                files,
            } => self.handle_session_ready(session, prompt, files),
            AppEvent::SessionFailed(err) => self.handle_session_failed(err),
            AppEvent::PromptFailed { id, error } => {
                // The server never admitted it, so no delivery will come.
                self.store.retract_user(&id);
                self.refresh();
                self.show_toast(Toast::error("Failed to send prompt", error, Instant::now()))
            }
            AppEvent::Forms(forms) => {
                self.store.pending_forms = forms
                    .into_iter()
                    .filter(|form| Some(form.session_id.as_str()) == self.session.as_deref())
                    .filter(|form| !self.settled_forms.contains(&form.id))
                    .collect();
                self.sync_form_dialog();
            }
            AppEvent::FormFailed(err) => {
                self.show_toast(Toast::error("Failed to answer form", err, Instant::now()));
                // The form left the store optimistically; the server knows
                // whether it is still pending, so the filter must let it back.
                self.settled_forms.clear();
                self.request_forms();
            }
            AppEvent::Media { key, class, result } => self.on_media(key, class, result),
            AppEvent::Files { query, entries } => {
                self.prompt.set_files(&query, &entries);
                self.dirty = true;
            }
            AppEvent::McpServers {
                servers,
                intent,
                stamp,
            } => self.on_mcp_servers(servers, intent, stamp),
            AppEvent::McpToggled {
                server,
                action,
                error,
            } => self.on_mcp_toggled(&server, action, error),
            AppEvent::Models { models, providers } => self.on_models(models, providers),
            AppEvent::ContextLimits(limits) => {
                self.context_limits = limits;
                self.refresh_context();
                self.dirty = true;
            }
            AppEvent::Agents { agents, intent } => self.on_agents(agents, intent),
            AppEvent::Switched(switch) => self.on_switched(switch),
            AppEvent::SessionChrome { session, selection } => {
                // 切走之后迟到的答案不能覆盖新会话的显示。
                if self.session.as_deref() == Some(session.as_str()) {
                    self.session_title = selection.title.clone();
                    self.prompt.set_selection(
                        selection.agent,
                        selection.model,
                        selection.provider,
                        selection.variant,
                    );
                    self.dirty = true;
                }
            }
            AppEvent::SwitchFailed { title, err } => {
                self.show_toast(Toast::error(title, err, Instant::now()))
            }
            AppEvent::Sessions { sessions, running } => {
                if !self.form_open() {
                    self.dialog = Some(session_dialog::list(
                        &sessions,
                        self.session.as_deref(),
                        &running,
                        jiff::Timestamp::now(),
                        &jiff::tz::TimeZone::system(),
                    ));
                    self.dialog_purpose = None;
                    self.dirty = true;
                }
            }
            AppEvent::RunningRoots(running) => {
                if self.dialog.as_ref().and_then(Dialog::tag) == Some(session_dialog::LIST_TAG)
                    && let Some(Dialog::Select(select)) = self.dialog.as_mut()
                    && session_dialog::set_running(select, &running)
                {
                    self.dirty = true;
                }
            }
            AppEvent::Family { session, members } => self.on_family(&session, members),
            AppEvent::RunningShells { generation, shells } => {
                if self.store.set_running_shells(generation, shells) {
                    self.child_state_changed();
                }
            }
            AppEvent::ShellOutput {
                session,
                shell,
                tail,
            } => self.on_shell_output(&session, &shell, tail),
            AppEvent::ShellPolled { id, start, read } => {
                if self.shell_captures.merge(&id, start, read) {
                    self.child_state_changed();
                }
            }
            AppEvent::SessionDeleted(id) => self.handle_session_deleted(id),
            AppEvent::SessionForked(id) => {
                self.show_toast(Toast::new(
                    toast::Variant::Success,
                    None,
                    "Forked session",
                    Instant::now(),
                ));
                self.switch_session(id);
            }
            AppEvent::SessionOpFailed { title, error } => {
                self.show_toast(Toast::error(title, error, Instant::now()))
            }
            AppEvent::MemoryMode(mode) => {
                let message = if mode == "off" {
                    "Memory off for this session: nothing injected, nothing extracted"
                } else {
                    "Memory on for this session"
                };
                self.show_toast(Toast::new(
                    toast::Variant::Info,
                    None,
                    message,
                    Instant::now(),
                ));
            }
            AppEvent::MemoryCleared(archived) => self.show_toast(Toast::new(
                toast::Variant::Info,
                None,
                format!("Archived {archived} project memories"),
                Instant::now(),
            )),
        }
    }

    /// Keep the family the panel lists. A late answer (the user moved to
    /// another session) is dropped.
    fn on_family(&mut self, session: &str, members: Vec<FamilyMember>) {
        if self.session.as_deref() != Some(session) {
            return;
        }
        self.family = members;
        self.settle_selection();
        self.dirty = true;
    }

    /// A fresh MCP server list. The home footer's summary follows every
    /// answer; the intent says which dialog it is for. `/status` and `/mcps`
    /// never cover a form, and `/mcps` opens over nothing else either; a
    /// refresh only reaches an open `/mcps` dialog. A refresh older than the
    /// list already applied is dropped; an older `/status` or `/mcps` answer
    /// still opens its dialog but leaves the footer alone.
    fn on_mcp_servers(
        &mut self,
        servers: Vec<ox_client::rest::McpRef>,
        intent: McpIntent,
        stamp: McpStamp,
    ) {
        // A stale answer must not overwrite newer status, but an explicit
        // `/status` or `/mcps` still opens: the user asked for it.
        let stale = stamp < self.mcp_applied;
        if stale && intent == McpIntent::Refresh {
            return;
        }
        if !stale {
            self.mcp_applied = stamp;
            self.prompt.set_mcp(crate::bootstrap::mcp_summary(&servers));
        }
        self.dirty = true;
        match intent {
            McpIntent::Status => {
                if !self.form_open() {
                    self.dialog = Some(crate::ui::dialog::status(&servers));
                    self.dialog_purpose = None;
                }
            }
            McpIntent::Dialog => {
                if self.dialog.is_none() {
                    self.dialog = Some(Dialog::Mcp(Box::new(McpDialog::new(servers))));
                    self.dialog_purpose = None;
                    if stale {
                        self.request_mcp_list(McpIntent::Refresh);
                    }
                }
            }
            McpIntent::Refresh => {
                if let Some(Dialog::Mcp(open)) = &mut self.dialog {
                    open.update(servers);
                }
            }
        }
    }

    /// The stamp for the next MCP list request; later requests get larger
    /// stamps.
    fn next_mcp_stamp(&mut self) -> McpStamp {
        self.mcp_issued += 1;
        McpStamp(self.mcp_issued)
    }

    /// Fetch the MCP server list for `intent`.
    fn request_mcp_list(&mut self, intent: McpIntent) {
        let stamp = self.next_mcp_stamp();
        let _ = self.commands.send(AppCommand::McpList { intent, stamp });
    }

    /// A connect/disconnect finished: its row stops showing `Connecting …`
    /// (the refreshed list was sent first), and a failure is a toast
    /// (`dialog-mcp.tsx:118-127`).
    fn on_mcp_toggled(&mut self, server: &str, action: McpAction, error: Option<String>) {
        if let Some(Dialog::Mcp(open)) = &mut self.dialog {
            open.settle(server);
            self.dirty = true;
        }
        if let Some(error) = error {
            let title = match action {
                McpAction::Connect => "Failed to connect MCP server",
                McpAction::Disconnect => "Failed to disconnect MCP server",
            };
            self.show_toast(Toast::error(title, error, Instant::now()));
        }
    }

    /// A key action of the `/mcps` dialog: toggle a server, copy the error
    /// details, or investigate them in a new session drafted with `value`.
    fn on_mcp_action(&mut self, key: char, value: String) {
        match key {
            mcp_dialog::TOGGLE_KEY => {
                let Some(Dialog::Mcp(open)) = &self.dialog else {
                    return;
                };
                let action = if open.wants_connect(&value) {
                    McpAction::Connect
                } else {
                    McpAction::Disconnect
                };
                let stamp = self.next_mcp_stamp();
                let _ = self.commands.send(AppCommand::McpToggle {
                    server: value,
                    action,
                    stamp,
                });
            }
            mcp_dialog::COPY_KEY => {
                if let Some(Dialog::Mcp(open)) = &mut self.dialog {
                    open.set_copied();
                }
                self.pending_osc52 = crate::clipboard::osc52(&value);
                self.osc52_attempted = self.pending_osc52.is_some();
                let _ = self.commands.send(AppCommand::WriteClipboard(value));
            }
            mcp_dialog::INVESTIGATE_KEY => {
                self.dialog = None;
                self.dialog_purpose = None;
                self.run_command(CommandAction::New);
                self.prompt.restore(&value);
            }
            _ => {}
        }
    }

    /// Open (or refresh) the output viewer. A refresh of the same shell keeps
    /// the reader where they were. A late answer for another session or one
    /// that would cover a form is dropped.
    fn on_shell_output(&mut self, session: &str, shell: &ShellInfo, tail: ShellTail) {
        let opens = self.shell_wanted && self.dialog.is_none();
        let refreshes =
            matches!(&self.dialog, Some(Dialog::Output(open)) if open.shell_id() == shell.id);
        if self.session.as_deref() != Some(session) || !(opens || refreshes) {
            return;
        }
        if opens {
            self.shell_wanted = false;
        }
        let mut output = Output::new(shell, tail);
        if let Some(Dialog::Output(open)) = &self.dialog
            && open.shell_id() == output.shell_id()
        {
            output.keep_scroll_of(open);
        }
        self.dialog = Some(Dialog::Output(output));
        self.dirty = true;
    }

    /// Ask the executor for `id`'s recent output.
    fn request_shell_output(&self, id: String) {
        if let Some(session) = self.session.clone() {
            let _ = self.commands.send(AppCommand::ShellOutput { session, id });
        }
    }

    /// A form dialog is open. It is the most urgent modal (the agent is
    /// blocked on it), so asynchronous dialog answers never replace it.
    fn form_open(&self) -> bool {
        matches!(self.dialog, Some(Dialog::Form(_)))
    }

    /// Everything tied to the previous active session goes: its form dialog,
    /// the dialog purpose, the answered-form filter and the stream cursor (a
    /// stale one would make the next stream replay from the wrong place).
    /// The new session's pending forms are fetched when there is one.
    fn session_changed(&mut self) {
        self.panel = None;
        self.family.clear();
        self.cursor.selected = 0;
        self.selection_pending = false;
        let purposeful = self.dialog_purpose.take().is_some();
        if purposeful || self.form_open() {
            self.dialog = None;
        }
        self.settled_forms.clear();
        self.session_title = None;
        self.seq_tx.send_replace(None);
        self.request_forms();
    }

    /// Route a dialog key's outcome. The tag says which dialog produced it.
    fn apply_dialog_outcome(&mut self, tag: Option<&'static str>, outcome: DialogOutcome) {
        match outcome {
            DialogOutcome::Idle => {}
            DialogOutcome::Changed => self.dirty = true,
            DialogOutcome::Cancelled => {
                let closed = self.dialog.take();
                self.dialog_purpose = None;
                self.dirty = true;
                if let Some(Dialog::Form(form)) = closed {
                    self.settle_form(form.id(), None);
                }
                self.sync_form_dialog();
            }
            DialogOutcome::FormSubmitted(answer) => {
                let closed = self.dialog.take();
                self.dirty = true;
                if let Some(Dialog::Form(form)) = closed {
                    self.settle_form(form.id(), Some(answer));
                }
                self.sync_form_dialog();
            }
            DialogOutcome::Selected(value) => {
                self.dialog = None;
                self.dirty = true;
                if tag == Some(session_dialog::LIST_TAG) {
                    self.switch_session(value);
                } else if tag == Some(queued_dialog::TAG) {
                    if let Some(session) = self.session.clone() {
                        let _ = self.commands.send(AppCommand::SteerQueued {
                            session,
                            inbox: value,
                        });
                    }
                } else if tag == Some(timeline_dialog::TAG) {
                    if let Ok(index) = value.parse::<usize>() {
                        self.transcript.jump_to_entry(index);
                    }
                } else if tag == Some(palette::TAG) {
                    if let Some(action) = palette::action_of(&value) {
                        self.run_command(action);
                    }
                } else {
                    self.on_selected(value);
                }
                self.sync_form_dialog();
            }
            DialogOutcome::Action { key, value } => {
                self.dirty = true;
                if tag == Some(mcp_dialog::TAG) {
                    self.on_mcp_action(key, value);
                    return;
                }
                if tag == Some(output_dialog::TAG) {
                    if key == output_dialog::REFRESH_KEY {
                        self.request_shell_output(value);
                    }
                    return;
                }
                if tag == Some(queued_dialog::TAG) {
                    self.on_queued_action(key, value);
                    return;
                }
                if tag != Some(session_dialog::LIST_TAG) {
                    return;
                }
                if key == session_dialog::DELETE_KEY {
                    let _ = self
                        .commands
                        .send(AppCommand::DeleteSession { session: value });
                } else if key == session_dialog::RENAME_KEY {
                    let title = match &self.dialog {
                        Some(Dialog::Select(select)) => {
                            select.item(&value).map(|item| item.title.clone())
                        }
                        _ => None,
                    };
                    self.dialog = Some(session_dialog::rename(&value, &title.unwrap_or_default()));
                }
            }
            DialogOutcome::Submitted { target, text } => {
                self.dialog = None;
                self.dirty = true;
                let title = text.trim();
                if tag == Some(session_dialog::RENAME_TAG) && !title.is_empty() {
                    let _ = self.commands.send(AppCommand::RenameSession {
                        session: target,
                        title: title.to_owned(),
                    });
                }
            }
        }
    }

    /// `ctrl+d` / `ctrl+u` on a queued prompt. Delete closes the dialog only
    /// after the last row (upstream `last`); undo moves the text into the
    /// draft and closes it. The store drops the row only when the server's
    /// `session.inbox.cancelled` lands, so the dialog is rebuilt without it
    /// here at once.
    fn on_queued_action(&mut self, key: char, inbox: String) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let reason = match key {
            queued_dialog::DELETE_KEY => CancelReason::Delete,
            queued_dialog::UNDO_KEY => CancelReason::Undo,
            _ => return,
        };
        if reason == CancelReason::Undo {
            let Some(queued) = self.store.queued.iter().find(|q| q.inbox_id == inbox) else {
                return;
            };
            let draft = self.prompt.text();
            let text = if draft.is_empty() {
                queued.text.to_string()
            } else {
                format!("{draft}\n\n{}", queued.text)
            };
            self.prompt.restore(&text);
            self.dialog = None;
        } else {
            let rest = self.store.queued.iter().filter(|q| q.inbox_id != inbox);
            let rest: Vec<&ox_client::store::Queued> = rest.collect();
            self.dialog = if rest.is_empty() {
                None
            } else {
                Some(queued_dialog::dialog(rest))
            };
        }
        let _ = self.commands.send(AppCommand::CancelQueued {
            session,
            inbox,
            reason,
        });
    }

    /// The queued-prompt dock shows above the prompt while prompts wait and
    /// no form holds the prompt (upstream `index.tsx:1581`).
    fn queued_dock_shown(&self) -> bool {
        !self.store.queued.is_empty() && self.store.pending_forms.is_empty()
    }

    fn handle_session_deleted(&mut self, id: String) {
        if let Some(Dialog::Select(select)) = &mut self.dialog {
            select.remove(&id);
        }
        if self.session.as_deref() == Some(id.as_str()) {
            self.run_command(CommandAction::New);
        }
        self.dirty = true;
    }

    /// Follow another session: drop everything that belonged to the old one
    /// (store, transcript, the in-flight history reload), restart the stream
    /// from the live tail and fetch the new history.
    pub fn switch_session(&mut self, id: String) {
        if self.session.as_deref() == Some(id.as_str()) {
            return;
        }
        let mut store = DataStore::default();
        store.session = Some(id.clone());
        self.store = store;
        self.session = Some(id.clone());
        self.transcript = Transcript::new(80, 20);
        // The cells naming the old session's images are gone with its
        // transcript, so the terminal's copies go too, and a new session cannot
        // inherit a fetch already in flight for the old one.
        self.graphics.clear();
        self.media_pending.clear();
        self.media_failed.clear();
        self.shell_captures = ShellCaptures::default();
        // `next_reload_id` keeps counting, so an answer for the old session
        // can never match the new request.
        self.pending_reload = None;
        self.pending_home_prompt = None;
        self.show_help = false;
        self.mode = Mode::Compose;
        self.route = Route::Session;
        self.prompt.set_footer(footer_of(self.route));
        self.prompt.set_placeholder("");
        self.session_changed();
        let _ = self.commands.send(AppCommand::AttachSession {
            session: id.clone(),
        });
        self.request_reload();
        let _ = self
            .commands
            .send(AppCommand::LoadSessionChrome { session: id });
        self.request_running_shells();
        self.refresh();
    }

    fn handle_session_ready(&mut self, session: String, prompt: String, files: Vec<FileInput>) {
        self.pending_home_prompt = None;
        self.session = Some(session.clone());
        self.store.session = Some(session.clone());
        self.route = Route::Session;
        self.prompt.set_footer(footer_of(self.route));
        self.prompt.set_placeholder("");
        let id = ox_schema::id::id("msg");
        self.store.push_user(&id, &prompt);
        let _ = self.commands.send(AppCommand::Prompt {
            session,
            id,
            text: prompt,
            files,
        });
        self.request_running_shells();
        self.refresh();
    }

    fn handle_session_failed(&mut self, err: String) {
        let mut message = err;
        if let Some(pending) = self.pending_home_prompt.take() {
            self.prompt.restore(&pending.text);
            if pending.dropped_files > 0 {
                message = format!("{message} (attachments were dropped)");
            }
        }
        self.show_toast(Toast::error(
            "Creating a session failed",
            message,
            Instant::now(),
        ));
    }

    /// The host write finished. Like upstream, a failed host write is only an
    /// error when the terminal was not asked to copy either.
    fn handle_clipboard_written(&mut self, host_ok: bool) {
        let toast = if host_ok || self.osc52_attempted {
            Toast::new(
                toast::Variant::Info,
                None,
                "Copied to clipboard",
                Instant::now(),
            )
        } else {
            Toast::new(
                toast::Variant::Warning,
                Some("Clipboard".to_owned()),
                "Could not copy to the clipboard",
                Instant::now(),
            )
        };
        self.show_toast(toast);
    }

    /// Replace the current toast and schedule a redraw.
    fn show_toast(&mut self, toast: Toast) {
        self.toast = Some(toast);
        self.dirty = true;
    }

    /// Drop the toast once its deadline has passed; `now` is injected so the
    /// tick passes the clock and tests pass whatever instant they need.
    fn expire_toast(&mut self, now: Instant) {
        if self.toast.as_ref().is_some_and(|t| t.expired(now)) {
            self.toast = None;
            self.dirty = true;
        }
    }

    /// Authoritative history, accepted only if it answers the newest request.
    /// Otherwise it lost the race against newer deltas (or a newer reload).
    /// Its run-state snapshot applies unless an `execution.*` event arrived
    /// since the request went out: that event is newer, so the store keeps it.
    fn handle_history(&mut self, request_id: u64, items: Vec<Item>, running_since: Option<i64>) {
        let Some(pending) = self.pending_reload else {
            return;
        };
        if pending.request_id != request_id {
            return;
        }
        self.pending_reload = None;
        let hydration = if pending.lifecycle_epoch == self.store.lifecycle_epoch() {
            Hydration::Snapshot(running_since)
        } else {
            Hydration::KeepLive
        };
        self.store.replace_items(items, hydration);
        self.refresh();
        // Starts missed while the history was loading, or while the stream
        // was down, are not replayed: ask the server about every child.
        let children = self.store.referenced_children();
        self.probe_children(children);
    }

    /// Ask the server whether each of `children` is running. The store is told
    /// first so an execution event that arrives while the request is in
    /// flight wins over its (older) answer; see `DataStore::apply_probe`.
    fn probe_children(&mut self, children: Vec<String>) {
        if children.is_empty() {
            return;
        }
        self.store.mark_probing(&children);
        let _ = self.commands.send(AppCommand::ProbeChildren { children });
    }

    fn request_reload(&mut self) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let request_id = self.next_reload_id;
        self.next_reload_id += 1;
        self.pending_reload = Some(PendingReload {
            request_id,
            lifecycle_epoch: self.store.lifecycle_epoch(),
        });
        let _ = self.commands.send(AppCommand::ReloadHistory {
            session,
            request_id,
        });
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        self.handle_key_at(key, Instant::now());
    }

    /// Bracketed paste goes where a key would: an open dialog first (text
    /// only), then the search prompt (one line), then the main prompt.
    fn handle_paste(&mut self, text: String) {
        self.keymap.reset();
        if let Some(dialog) = &mut self.dialog {
            let tag = dialog.tag();
            let outcome = dialog.on_paste(&text);
            self.apply_dialog_outcome(tag, outcome);
            return;
        }
        if let Mode::SearchInput(boxed) = &mut self.mode {
            boxed.insert_paste(&text);
            self.transcript.set_query(&boxed.text());
            self.dirty = true;
            return;
        }
        // An empty paste is how some terminals (Windows Terminal) report an
        // image-only clipboard; the clipboard itself has to be read.
        if text.is_empty() {
            self.request_clipboard();
            return;
        }
        self.focus_prompt();
        // Image/PDF paths become attachments; reading them is the executor's
        // job, and its answer comes back as `AppEvent::Attachments`.
        if paste::pasted_attachment_paths(&text).is_empty() {
            self.paste_into_prompt(text);
        } else if let Err(mpsc::error::SendError(AppCommand::LoadAttachments(text))) =
            self.commands.send(AppCommand::LoadAttachments(text))
        {
            // No executor (offline): the command hands the text back.
            self.paste_into_prompt(text);
        }
    }

    /// Leave browse mode for typing: like any other key, a paste or a
    /// clipboard read puts the user back in the prompt.
    fn focus_prompt(&mut self) {
        if matches!(self.mode, Mode::Browse) {
            self.transcript.clear_search();
            self.show_help = false;
            self.mode = Mode::Compose;
        }
    }

    /// Ask the executor to read the system clipboard; the answer arrives as
    /// `AppEvent::ClipboardPasted`.
    fn request_clipboard(&mut self) {
        self.focus_prompt();
        let _ = self.commands.send(AppCommand::ReadClipboard);
        self.dirty = true;
    }

    /// The clipboard's content: an image becomes an attachment, text takes the
    /// same road as a bracketed paste, and nothing usable is explained in a
    /// toast. If a modal opened while the read ran, the answer is dropped
    /// rather than landing behind it.
    fn handle_clipboard(&mut self, clip: Clip) {
        if self.another_input_active() {
            self.drop_late_answer("Clipboard content dropped: another input is active");
            return;
        }
        match clip {
            Clip::Image(input) => {
                self.focus_prompt();
                self.prompt.attach(vec![Loaded::File {
                    name: "clipboard".to_owned(),
                    input,
                }]);
                self.flush_file_request();
                self.dirty = true;
            }
            Clip::Text(text) => self.handle_paste(text),
            Clip::Empty => self.show_toast(Toast::new(
                toast::Variant::Warning,
                Some("Clipboard".to_owned()),
                "The clipboard has no image or text",
                Instant::now(),
            )),
            Clip::Unsupported => self.show_toast(Toast::new(
                toast::Variant::Warning,
                Some("Clipboard".to_owned()),
                "Pasting from the clipboard is not supported on this platform",
                Instant::now(),
            )),
        }
    }

    /// The executor's answer to a paste of file paths: the files as labels,
    /// or, when any could not be read, the pasted text as it was.
    fn handle_attachments(&mut self, text: String, loaded: Option<Vec<Loaded>>) {
        if self.another_input_active() {
            self.drop_late_answer("Attachments dropped: another input is active");
            return;
        }
        match loaded {
            Some(loaded) => {
                self.prompt.attach(loaded);
                self.flush_file_request();
                self.dirty = true;
            }
            None => self.paste_into_prompt(text),
        }
    }

    fn paste_into_prompt(&mut self, text: String) {
        self.prompt.handle_paste(text);
        self.flush_file_request();
        self.dirty = true;
    }

    /// A dialog or the search box owns the keyboard, so a slow answer (the
    /// clipboard, loaded files) must not land in the prompt behind it.
    fn another_input_active(&self) -> bool {
        self.dialog.is_some() || matches!(self.mode, Mode::SearchInput(_))
    }

    fn drop_late_answer(&mut self, message: &str) {
        self.show_toast(Toast::new(
            toast::Variant::Info,
            None,
            message,
            Instant::now(),
        ));
    }

    /// Send the prompt's pending `@` file lookup, if text just changed it.
    fn flush_file_request(&mut self) {
        if let Some(query) = self.prompt.take_file_request() {
            let _ = self.commands.send(AppCommand::FindFiles(query));
        }
    }

    /// `handle_key` with the clock injected (the leader's 2s window).
    fn handle_key_at(&mut self, key: crossterm::event::KeyEvent, now: Instant) {
        // A selection takes the key first (upstream `handleSelectionKey`):
        // esc only clears it; every other key clears it and carries on.
        if key.kind != crossterm::event::KeyEventKind::Release && self.transcript.clear_selection()
        {
            self.dirty = true;
            if key.code == KeyCode::Esc {
                return;
            }
        }
        // A question form takes the composer's place rather than covering
        // the session (upstream `routes/session/index.tsx:1625-1631`), so the
        // transcript keeps its page keys (`session.page.up/down`).
        if self.form_open() && key.kind != crossterm::event::KeyEventKind::Release {
            match key.code {
                KeyCode::PageUp => {
                    self.transcript.scroll_up(SCROLL_PAGE);
                    self.dirty = true;
                    return;
                }
                KeyCode::PageDown => {
                    self.transcript.scroll_down(SCROLL_PAGE);
                    self.dirty = true;
                    return;
                }
                _ => {}
            }
        }
        // A dialog is modal: it eats every key, and only dismissal leaves it.
        if let Some(dialog) = &mut self.dialog {
            self.keymap.reset();
            // `ctrl+c` stays a way out of anything, including a modal.
            if keymap::is_cancel(&key) {
                self.quit();
                self.dirty = true;
                return;
            }
            let tag = dialog.tag();
            let outcome = dialog.on_key(key);
            self.apply_dialog_outcome(tag, outcome);
            return;
        }

        // The search prompt owns the keyboard while it is open.
        if let Mode::SearchInput(boxed) = &mut self.mode {
            self.keymap.reset();
            if keymap::is_cancel(&key) {
                self.transcript.clear_search();
                self.mode = Mode::Compose;
                self.dirty = true;
                return;
            }
            match boxed.handle_key(key) {
                SearchAction::Cancel => {
                    self.transcript.clear_search();
                    self.mode = Mode::Compose;
                }
                SearchAction::Confirm => self.mode = Mode::Browse,
                SearchAction::Edited => {
                    let query = match &self.mode {
                        Mode::SearchInput(boxed) => boxed.text(),
                        _ => String::new(),
                    };
                    self.transcript.set_query(&query);
                }
            }
            self.dirty = true;
            return;
        }

        // The activity panel takes the composer's place, so it owns the
        // keyboard: there is no visible text field to type into, and typing
        // into a hidden composer would be invisible. `ctrl+c` still gets out.
        if let Some(tab) = self.panel {
            self.keymap.reset();
            if keymap::is_cancel(&key) {
                self.quit();
            } else {
                self.on_activity_key(key, tab);
            }
            self.dirty = true;
            return;
        }

        // Global bindings. The slash menu keeps `ctrl+p`/`ctrl+n` for its own
        // navigation, so it owns the keyboard while it is open.
        let global = !self.prompt.autocomplete_visible();
        match self.keymap.feed(&key, now, global) {
            Feed::Pass => {}
            Feed::Consumed => {
                self.dirty = true;
                return;
            }
            Feed::Action(action) => {
                // Upstream binds `<leader>q` to both `session.queued_prompts`
                // and `app.exit`; with nothing queued the queue command is
                // disabled and declines the key (`keymap.tsx:231-232`), so
                // the exit binding runs. Only the key falls through: the
                // slash menu, the palette and a dock click never exit.
                if action == Action::Run(CommandAction::QueuedPrompts)
                    && !self.queued_dialog_available()
                {
                    self.view.should_quit = true;
                    self.dirty = true;
                    return;
                }
                self.run_global(action);
                self.dirty = true;
                return;
            }
        }

        // `ctrl+v` reads the system clipboard (upstream `prompt.paste`). The
        // modal and search boxes above never get here, so only the prompt acts.
        if keymap::is_paste(&key) {
            self.request_clipboard();
            return;
        }

        // `ctrl+b` backgrounds the running tools (upstream
        // `session.background`). After the keymap feed, so `ctrl+x b` still
        // opens the Shell tab; only while a shell or subagent call runs, so
        // the prompt keeps `ctrl+b` as cursor-left otherwise
        // (`keymap::is_background`).
        if keymap::is_background(&key)
            && self.store.backgroundable_tool().is_some()
            && let Some(session) = self.session.clone()
        {
            let _ = self.commands.send(AppCommand::Background { session });
            return;
        }

        // Browse mode: single-letter navigation is safe here because the
        // prompt is not receiving keys.
        if matches!(self.mode, Mode::Browse) {
            if keymap::is_cancel(&key) {
                self.quit();
                self.dirty = true;
                return;
            }
            match key.code {
                KeyCode::Esc => {
                    self.transcript.clear_search();
                    self.show_help = false;
                    self.mode = Mode::Compose;
                }
                KeyCode::Char('n') => {
                    self.transcript.next_match(true);
                }
                KeyCode::Char('N') => {
                    self.transcript.next_match(false);
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.transcript.scroll_down(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.transcript.scroll_up(1);
                }
                KeyCode::Char('g') => {
                    self.transcript.scroll_to_top();
                }
                KeyCode::Char('G') => {
                    self.transcript.scroll_to_bottom();
                }
                KeyCode::Char('?') => self.show_help = !self.show_help,
                KeyCode::Char('r') => {
                    self.transcript.toggle_last_reasoning();
                }
                KeyCode::Char('o') => {
                    self.transcript.toggle_tool_output();
                }
                KeyCode::Char(']') => {
                    self.transcript.focus_tool(true);
                }
                KeyCode::Char('[') => {
                    self.transcript.focus_tool(false);
                }
                _ => {
                    // Any other key leaves browse mode and goes back to typing.
                    self.transcript.clear_search();
                    self.show_help = false;
                    self.mode = Mode::Compose;
                    self.handle_compose_key(key);
                    return;
                }
            }
            self.dirty = true;
            return;
        }

        // Compose. `ctrl+f` opens the transcript search; upstream has no such
        // feature, and `/` now belongs to the slash-command menu.
        if keymap::is_search(&key) && self.route == Route::Session {
            self.transcript.clear_search();
            self.mode = Mode::SearchInput(Box::default());
            self.dirty = true;
            return;
        }
        self.handle_compose_key(key);
    }

    fn run_global(&mut self, action: Action) {
        match action {
            Action::OpenPalette => {
                let context = palette::Context {
                    in_session: self.route == Route::Session,
                    has_queue: !self.store.queued.is_empty(),
                };
                self.dialog = Some(Dialog::Select(palette::select(context, keymap::hint)));
                self.dialog_purpose = None;
            }
            Action::Run(command) => self.run_command(command),
        }
    }

    fn handle_compose_key(&mut self, key: KeyEvent) {
        // A creation already in flight: swallow the submit but keep whatever is
        // being typed (the composer must not consume the draft).
        if self.route == Route::Home
            && self.pending_home_prompt.is_some()
            && key.code == KeyCode::Enter
            && key.modifiers.is_empty()
        {
            self.dirty = true;
            return;
        }
        // Scrollback keys stay global while the slash menu is closed.
        if !self.prompt.autocomplete_visible() {
            match key.code {
                KeyCode::PageUp => {
                    self.transcript.scroll_up(SCROLL_PAGE);
                    self.dirty = true;
                    return;
                }
                KeyCode::PageDown => {
                    self.transcript.scroll_down(SCROLL_PAGE);
                    self.dirty = true;
                    return;
                }
                KeyCode::End => {
                    self.transcript.scroll_to_bottom();
                    self.dirty = true;
                    return;
                }
                _ => {}
            }
        }
        if self.cycle_agent_key(&key) {
            self.dirty = true;
            return;
        }
        match self.prompt.handle_key(key) {
            PromptAction::Ignored => {}
            PromptAction::Declined => self.declined_key(key.code),
            PromptAction::CtrlC => self.quit(),
            PromptAction::Esc => self.press_esc(Instant::now()),
            PromptAction::Submit(submission) => self.submit(submission),
            PromptAction::Run(action) => self.run_command(action),
        }
        self.flush_file_request();
        self.dirty = true;
    }

    /// Upstream's fallthrough for a key the prompt declined: `↓` at the end of
    /// the history opens the subagent picker (`session.child.first`,
    /// `config/keybind.ts:138`). `↑` declines too, but ox has no parent session
    /// for it to reach (upstream `session.parent`), so it stays unbound.
    fn declined_key(&mut self, code: KeyCode) {
        if code == KeyCode::Down {
            self.run_command(CommandAction::Subagents);
        }
    }

    fn submit(&mut self, submission: Submission) {
        if self.route == Route::Home {
            if self.pending_home_prompt.is_some() {
                // A creation is already in flight; keep whatever is typed.
                return;
            }
            self.pending_home_prompt = Some(PendingHome {
                text: submission.text.clone(),
                dropped_files: submission.files.len(),
            });
            let _ = self
                .commands
                .send(AppCommand::CreateSessionAndPrompt(submission));
            return;
        }
        let Submission { text, files } = submission;
        // 本地乐观显示：随后一帧就有用户消息；`session.inbox.delivered` 按 id 把它标为已送达。
        let id = ox_schema::id::id("msg");
        self.store.push_user(&id, &text);
        self.refresh();
        if let Some(session) = self.session.clone() {
            let _ = self.commands.send(AppCommand::Prompt {
                session,
                id,
                text,
                files,
            });
        }
    }

    /// Whether the queued-prompts command is enabled right now.
    fn queued_dialog_available(&self) -> bool {
        self.route == Route::Session && !self.store.queued.is_empty()
    }

    fn run_command(&mut self, action: CommandAction) {
        match action {
            CommandAction::Exit => self.view.should_quit = true,
            CommandAction::Status => {
                self.request_mcp_list(McpIntent::Status);
            }
            CommandAction::Mcps => {
                self.request_mcp_list(McpIntent::Dialog);
            }
            CommandAction::Models => {
                let _ = self.commands.send(AppCommand::ModelList);
            }
            CommandAction::Agents => {
                let _ = self
                    .commands
                    .send(AppCommand::AgentList(command::AgentIntent::Dialog));
            }
            CommandAction::Sessions => {
                let _ = self.commands.send(AppCommand::ListSessions);
            }
            CommandAction::Subagents => {
                if let Some(session) = self.session.clone() {
                    // Upstream's tabs take the composer's place; the panel is
                    // up at once and fills in when the family lands, with the
                    // selection moving to the session on screen.
                    self.panel = Some(activity_panel::Tab::Subagents);
                    self.cursor.selected = 0;
                    self.selection_pending = true;
                    self.dirty = true;
                    let _ = self.commands.send(AppCommand::ListFamily { session });
                }
            }
            CommandAction::Images => {
                // Upstream's `prompt.images.view` is enabled only with an image
                // attached (`component/prompt/index.tsx:496-499`); with none
                // there is nothing to open, and no empty dialog either.
                let images = self.prompt.images();
                if !images.is_empty() {
                    self.dialog = Some(Dialog::Preview(preview_of(&images, 0)));
                    self.dialog_purpose = None;
                    self.dirty = true;
                }
            }
            CommandAction::QueuedPrompts => {
                // Disabled with nothing queued, as upstream (`index.tsx:1400`).
                if self.queued_dialog_available() {
                    self.dialog = Some(queued_dialog::dialog(&self.store.queued));
                    self.dialog_purpose = None;
                    self.dirty = true;
                }
            }
            CommandAction::Shells => {
                // The rows come from the store's running shells, so the tab is
                // complete at once and needs no fetch.
                if self.session.is_some() {
                    self.panel = Some(activity_panel::Tab::Shell);
                    self.cursor.selected = 0;
                    self.selection_pending = false;
                    self.dirty = true;
                }
            }
            CommandAction::Rename => {
                if let Some(session) = self.session.clone() {
                    let title = self.session_title.clone().unwrap_or_default();
                    self.dialog = Some(session_dialog::rename(&session, &title));
                    self.dirty = true;
                }
            }
            CommandAction::Timeline => {
                let messages = self.transcript.user_messages();
                if messages.is_empty() {
                    self.show_toast(Toast::new(
                        toast::Variant::Error,
                        None,
                        "Nothing to jump to",
                        Instant::now(),
                    ));
                } else {
                    self.dialog = Some(timeline_dialog::timeline(&messages));
                }
                self.dirty = true;
            }
            CommandAction::Fork => {
                if let Some(session) = self.session.clone() {
                    let _ = self.commands.send(AppCommand::ForkSession { session });
                }
            }
            CommandAction::Memory => {
                if let Some(session) = self.session.clone() {
                    let _ = self.commands.send(AppCommand::ToggleMemory { session });
                }
            }
            CommandAction::Forget => {
                if let Some(session) = self.session.clone() {
                    let _ = self.commands.send(AppCommand::ClearMemory { session });
                }
            }
            CommandAction::New | CommandAction::Clear => {
                self.session = None;
                self.store = DataStore::default();
                self.transcript = Transcript::new(80, 20);
                self.graphics.clear();
                self.media_pending.clear();
                self.media_failed.clear();
                self.shell_captures = ShellCaptures::default();
                self.pending_reload = None;
                self.pending_home_prompt = None;
                self.show_help = false;
                self.route = Route::Home;
                self.prompt
                    .set_placeholder(&placeholder_line(pick_placeholder()));
                self.session_changed();
                let _ = self.commands.send(AppCommand::DetachSession);
            }
        }
    }

    /// A key while the activity panel is up: `esc` closes it, `←`/`→` and
    /// `tab`/`shift+tab` switch tab, `↑` from the first row closes it and
    /// otherwise moves up, `↓` moves down and wraps. On `Subagents` `enter`
    /// opens that subagent's session, `ctrl+d` interrupts it when it runs and
    /// `ctrl+a` flips the filter; on `Shell` `enter` opens the selected
    /// shell's output and `ctrl+d` kills it. Everything else is swallowed: the panel has
    /// no text field, and the composer it replaced is not on screen. The keys
    /// are upstream's (`config/keybind.ts:249-256` and the tab's own layer).
    fn on_activity_key(&mut self, key: KeyEvent, tab: activity_panel::Tab) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let len = self.panel_len(tab);
        // Rows can vanish under the open panel (a shell exits): bring the
        // stored index back in range first, as upstream's clamp effect does
        // (`shell-tab.tsx:34-36`), so `↑` moves from the row that is drawn.
        self.cursor = self.cursor.clamp(len);
        match key.code {
            KeyCode::Esc => self.close_panel(),
            KeyCode::Left | KeyCode::BackTab => {
                self.panel = Some(tab.prev());
                self.settle_selection();
            }
            KeyCode::Right | KeyCode::Tab => {
                self.panel = Some(tab.next());
                self.settle_selection();
            }
            // Upstream's toggle lives in the subagents tab's own keymap layer.
            KeyCode::Char('a')
                if key.modifiers == KeyModifiers::CONTROL
                    && tab == activity_panel::Tab::Subagents =>
            {
                self.cursor = self.cursor.toggle();
            }
            KeyCode::Up => match self.cursor.up(len) {
                activity_panel::Up::Close => self.close_panel(),
                activity_panel::Up::Row(selected) => self.cursor.selected = selected,
            },
            KeyCode::Down => self.cursor = self.cursor.down(len),
            KeyCode::Enter => match tab {
                activity_panel::Tab::Subagents => self.open_selected(tab),
                activity_panel::Tab::Shell => self.view_selected_shell(),
            },
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => match tab {
                activity_panel::Tab::Subagents => self.interrupt_selected(tab),
                activity_panel::Tab::Shell => self.kill_selected_shell(),
            },
            _ => {}
        }
    }

    /// Close the panel (`esc`, or `↑` from the first row, where upstream closes
    /// the composer). The selection goes back to the top the way upstream
    /// resets it on close; the filter is remembered for the next open.
    fn close_panel(&mut self) {
        self.panel = None;
        self.cursor.selected = 0;
        self.selection_pending = false;
        self.dirty = true;
    }

    /// The subagent rows `tab` lists for the family it last fetched, in draw
    /// order; none on the `Shell` tab, whose rows are [`App::panel_shells`].
    fn panel_rows(&self, tab: activity_panel::Tab) -> Vec<activity_panel::Row<'_>> {
        if tab != activity_panel::Tab::Subagents {
            return Vec::new();
        }
        activity_panel::rows(
            &self.family,
            self.session.as_deref().unwrap_or_default(),
            self.cursor.active,
        )
    }

    /// The `Shell` tab's rows: `(id, command)` of the session's running shells.
    fn panel_shells(&self) -> Vec<(&str, &str)> {
        self.session
            .as_deref()
            .map_or_else(Vec::new, |session| self.store.shells_of(session))
    }

    /// How many rows `tab` lists.
    fn panel_len(&self, tab: activity_panel::Tab) -> usize {
        match tab {
            activity_panel::Tab::Subagents => self.panel_rows(tab).len(),
            activity_panel::Tab::Shell => self.panel_shells().len(),
        }
    }

    /// The id of the shell under the selection, clamped like the block.
    fn selected_shell(&self) -> Option<String> {
        let shells = self.panel_shells();
        shells
            .get(self.cursor.clamp(shells.len()).selected)
            .map(|&(id, _)| id.to_owned())
    }

    /// `enter` on the `Shell` tab (`composer.shell.select`): open the output
    /// viewer for the selected shell once its output lands.
    fn view_selected_shell(&mut self) {
        if let Some(id) = self.selected_shell() {
            self.shell_wanted = true;
            self.request_shell_output(id);
        }
    }

    /// `ctrl+d` on the `Shell` tab (`composer.shell.kill`): kill the selected
    /// shell; its `shell.*` frames then drop the row.
    fn kill_selected_shell(&mut self) {
        if let Some(id) = self.selected_shell() {
            let _ = self.commands.send(AppCommand::KillShell { id });
        }
    }

    /// The subagent under the selection, if one is listed. The index is clamped
    /// the way the block clamps it, so a list that shrank under an open panel
    /// (a child finishing) selects the row the panel actually draws.
    fn panel_selected(&self, tab: activity_panel::Tab) -> Option<&FamilyMember> {
        let rows = self.panel_rows(tab);
        rows.get(self.cursor.clamp(rows.len()).selected)
            .map(|row| row.member)
    }

    /// Keep the selection on a listed row, and put it on the session on screen
    /// when the panel has just opened (upstream selects the current route
    /// entry, else the first row, once the list has rows).
    fn settle_selection(&mut self) {
        let tab = self.panel.unwrap_or_default();
        let (position, len) = match tab {
            activity_panel::Tab::Subagents => {
                let rows = self.panel_rows(tab);
                let current = self.session.as_deref().unwrap_or_default();
                (
                    rows.iter().position(|row| row.member.id == current),
                    rows.len(),
                )
            }
            activity_panel::Tab::Shell => (None, self.panel_shells().len()),
        };
        if self.selection_pending && len > 0 && tab == activity_panel::Tab::Subagents {
            self.cursor.selected = position.unwrap_or(0);
            self.selection_pending = false;
        }
        self.cursor = self.cursor.clamp(len);
    }

    /// `enter` (`composer.subagent.select`): open the selected subagent's
    /// session. The old picker did it through `DialogOutcome::Selected` →
    /// [`App::switch_session`], which also closes the panel (the session it
    /// belongs to just changed).
    fn open_selected(&mut self, tab: activity_panel::Tab) {
        let Some(id) = self.panel_selected(tab).map(|member| member.id.clone()) else {
            return;
        };
        self.switch_session(id);
        self.dirty = true;
    }

    /// `ctrl+d` (`composer.subagent.interrupt`): interrupt the selected
    /// subagent. The old picker sent the command for the child's own session,
    /// not the one on screen; only a running child has a turn to stop, which is
    /// also the only case upstream arms the key for.
    fn interrupt_selected(&mut self, tab: activity_panel::Tab) {
        let Some(member) = self.panel_selected(tab) else {
            return;
        };
        if !member.running {
            return;
        }
        let _ = self.commands.send(AppCommand::Interrupt {
            session: member.id.clone(),
        });
        self.dirty = true;
    }

    /// Re-list the session's pending forms (after a reconnect or a failed reply).
    fn request_forms(&self) {
        if let Some(session) = self.session.clone() {
            let _ = self.commands.send(AppCommand::ListForms { session });
        }
    }

    /// Ask for the output each expanded shell block needs at `now`
    /// (`ShellCaptures::poll` decides; called once per UI tick).
    fn poll_shells(&mut self, now: Instant) {
        for item in &self.store.items {
            let Item::Assistant(assistant) = item else {
                continue;
            };
            for tool in &assistant.tools {
                if tool.tool != ShellRef::SHELL_TOOL || !self.transcript.is_tool_expanded(&tool.id)
                {
                    continue;
                }
                let Some(shell) = tool.shell_view() else {
                    continue;
                };
                // A call whose block shows its tool result needs no capture.
                if !crate::shell_captures::shows_capture(tool.state, shell.background) {
                    continue;
                }
                let running = self.store.is_shell_running(shell.id);
                if let Some(poll) = self.shell_captures.poll(shell.id, running, now) {
                    let _ = self.commands.send(AppCommand::ShellPoll {
                        id: poll.id,
                        cursor: poll.cursor,
                        mode: poll.mode,
                    });
                }
            }
        }
    }

    /// Rebuild the running-shell set from `GET /api/shell` (session open and
    /// reconnect: `shell.*` frames are ephemeral, so missed ones are gone).
    /// Its answer takes its own path, never the viewer's `shell_wanted` one.
    fn request_running_shells(&mut self) {
        if self.session.is_some() {
            self.shell_syncs += 1;
            self.store.begin_shell_sync(self.shell_syncs);
            let _ = self.commands.send(AppCommand::SyncShells {
                generation: self.shell_syncs,
            });
        }
    }

    /// Send the user's decision and drop the form from the store at once, so
    /// the dialog does not reopen before the server's `form.*` frame arrives.
    fn settle_form(&mut self, id: &str, answer: Option<ox_client::form::Answer>) {
        let Some(at) = self
            .store
            .pending_forms
            .iter()
            .position(|form| form.id == id)
        else {
            return;
        };
        let form = self.store.pending_forms.remove(at);
        self.remember_settled(form.id.clone());
        let session = form.session_id;
        let _ = self.commands.send(match answer {
            Some(answer) => AppCommand::ReplyForm {
                session,
                form: form.id,
                answer,
            },
            None => AppCommand::CancelForm {
                session,
                form: form.id,
            },
        });
    }

    fn remember_settled(&mut self, id: String) {
        if self.settled_forms.len() == SETTLED_FORMS_CAP {
            self.settled_forms.remove(0);
        }
        self.settled_forms.push(id);
    }

    /// Keep the form dialog in step with the store: close it once its form is
    /// settled elsewhere, and open the oldest pending form when no other
    /// dialog is up.
    fn sync_form_dialog(&mut self) {
        if self.route != Route::Session {
            return;
        }
        if let Some(Dialog::Form(open)) = &self.dialog {
            if self
                .store
                .pending_forms
                .iter()
                .any(|form| form.id == open.id())
            {
                return;
            }
            self.dialog = None;
            self.dirty = true;
        }
        if self.dialog.is_none()
            && let Some(form) = self.store.pending_form()
        {
            self.dialog = Some(Dialog::Form(Box::new(crate::ui::form::FormState::new(
                form,
            ))));
            self.dirty = true;
        }
    }

    /// A child's running state may have flipped. It lives beside the parts, not
    /// in them, so no part version moved and `changed_from` is untouched:
    /// lower it so the next `refresh` re-derives the subagent rows' `background`
    /// flag (a compare of `(id, version, background)` per block, no re-wrap
    /// unless a flag actually differs).
    fn child_state_changed(&mut self) {
        self.store.changed_from = 0;
        self.store_dirty = true;
        self.dirty = true;
    }

    /// A child's own turn started or ended. Its `running` state lives on the
    /// session row the family endpoint reports, so a panel that is open over it
    /// must be re-listed: upstream's tab is a live memo over the session list,
    /// while ox holds the family it last fetched. Only a child the store names
    /// flips it (see `DataStore::observe_foreign`), which no shell or capture
    /// update does, so this cannot turn a poll into a request loop.
    fn family_state_changed(&mut self) {
        self.child_state_changed();
        if self.panel == Some(activity_panel::Tab::Subagents)
            && let Some(session) = self.session.clone()
        {
            let _ = self.commands.send(AppCommand::ListFamily { session });
        }
    }

    /// What the footer shows as going on: the focused session's own turn
    /// only, like upstream. Running children are shown on their tool rows.
    fn activity(&self) -> Activity {
        if self.store.running_since.is_some() {
            Activity::Turn
        } else {
            Activity::Idle
        }
    }

    /// Esc while a turn runs: the second press inside the window interrupts.
    fn press_esc(&mut self, now: Instant) {
        if self.store.running_since.is_none() {
            return;
        }
        if self.prompt.press_interrupt(now)
            && let Some(session) = self.session.clone()
        {
            let _ = self.commands.send(AppCommand::Interrupt { session });
        }
    }

    /// ctrl+c with an empty draft: quit (upstream `app.exit`). Stopping a
    /// running turn is Esc's job alone (`press_esc`).
    fn quit(&mut self) {
        self.view.should_quit = true;
    }

    /// The pointer reaches the transcript only when nothing modal is on top.
    fn pointer_enabled(&self) -> bool {
        self.route == Route::Session
            && self.dialog.is_none()
            && !matches!(self.mode, Mode::SearchInput(_))
    }

    /// The gate closed (a modal opened, the route changed): a selection left
    /// behind would stay highlighted and keep following a drag nobody sees.
    fn close_pointer_gate(&mut self) {
        if self.transcript.clear_selection() {
            self.dirty = true;
        }
    }

    fn handle_mouse_down(&mut self, x: u16, y: u16) {
        self.handle_mouse_down_at(x, y, Instant::now());
    }

    /// `handle_mouse_down` with the clock injected (the multi-click window).
    fn handle_mouse_down_at(&mut self, x: u16, y: u16, now: Instant) {
        if !self.pointer_enabled() {
            return self.close_pointer_gate();
        }
        if self.transcript.press(x, y, now) {
            self.dirty = true;
        }
    }

    fn handle_mouse_drag(&mut self, x: u16, y: u16) {
        if !self.pointer_enabled() {
            return self.close_pointer_gate();
        }
        if self.transcript.drag(x, y) {
            self.dirty = true;
        }
    }

    /// A release without a drag is the old click; a drag that selected text
    /// hands it over for the clipboard.
    fn handle_mouse_up(&mut self, x: u16, y: u16) {
        if !self.pointer_enabled() {
            return self.close_pointer_gate();
        }
        if contains(self.queued_dock_area, x, y) {
            self.transcript.release();
            self.run_command(CommandAction::QueuedPrompts);
            return;
        }
        if contains(self.prompt.live_area(), x, y) {
            // Upstream's chip is an `onMouseUp` on the footer box
            // (`feature-plugins/prompt/footer.tsx:78`) dispatching
            // `session.child.first`, so a click opens the subagent picker.
            self.transcript.release();
            self.run_command(CommandAction::Subagents);
            return;
        }
        match self.transcript.release() {
            Outcome::Click => {
                // A subagent notice opens its child session; the owned id
                // ends the transcript borrow before `switch_session`.
                if let Some(child) = self.transcript.child_at(x, y).map(str::to_owned) {
                    self.switch_session(child);
                    self.dirty = true;
                } else if let Some(preview) = self.preview_at(x, y) {
                    self.dialog = Some(crate::ui::dialog::Dialog::Preview(preview));
                    self.dirty = true;
                } else if self.transcript.click(x, y) {
                    self.dirty = true;
                }
            }
            Outcome::Copy(text) => {
                // Encode by borrowing, then move the text to the writer.
                self.pending_osc52 = crate::clipboard::osc52(&text);
                self.osc52_attempted = self.pending_osc52.is_some();
                let _ = self.commands.send(AppCommand::WriteClipboard(text));
                self.dirty = true;
            }
            Outcome::Nothing => self.dirty = true,
        }
    }

    /// The pointer lights up transcript rows under the same gate as a click;
    /// anywhere else it clears the hover. Repaint only when that changed.
    fn handle_hover(&mut self, x: u16, y: u16) {
        let enabled = self.pointer_enabled();
        let changed = if enabled {
            self.transcript.hover(x, y)
        } else {
            self.transcript.clear_hover()
        };
        let hover = if enabled && contains(self.queued_dock_area, x, y) {
            DockHover::On
        } else {
            DockHover::Off
        };
        let dock_changed = hover != self.dock_hover;
        self.dock_hover = hover;
        // The footer chip lights its counts up while the pointer is over it
        // (`footer.tsx:76-81`); the rect is empty while it does not draw.
        let live = enabled && contains(self.prompt.live_area(), x, y);
        let live_changed = live != self.live_hover;
        self.live_hover = live;
        if changed || dock_changed || live_changed {
            self.dirty = true;
        }
    }

    fn handle_wheel(&mut self, direction: i8) {
        // A question form leaves the transcript scrollable (see the page
        // keys in `handle_key_at`); every other dialog is modal.
        if self.route != Route::Session || (self.dialog.is_some() && !self.form_open()) {
            return;
        }
        if direction < 0 {
            self.transcript.scroll_up(WHEEL_LINES);
        } else {
            self.transcript.scroll_down(WHEEL_LINES);
        }
        // A drag in progress follows the content under the pointer.
        self.transcript.refocus_selection();
        if self.pointer_enabled() {
            self.transcript.rehover();
        }
        self.dirty = true;
    }

    fn handle_stream(&mut self, msg: StreamMsg) {
        match msg {
            StreamMsg::Reconnected => {
                // ephemeral deltas are not replayed → refetch history.
                self.request_reload();
                // form.* frames missed while down: reconcile with REST.
                self.request_forms();
                // shell.* frames missed while down: rebuild the running set.
                self.request_running_shells();
                // mcp.status.changed frames missed while down: re-list.
                self.request_mcp_list(McpIntent::Refresh);
            }
            StreamMsg::Event(envelope) => {
                // `shell.*` frames carry no session id: they would fail the
                // foreign-session filter below, so they are handled first.
                if envelope.kind.starts_with("shell.") {
                    if self.store.apply_shell_frame(&envelope.kind, &envelope.data) {
                        self.child_state_changed();
                    }
                    return;
                }
                // `mcp.status.changed` names a server, not a session: re-list
                // so an open `/mcps` dialog and the footer follow changes made
                // elsewhere (a config reload, another client).
                if envelope.kind == "mcp.status.changed" {
                    self.request_mcp_list(McpIntent::Refresh);
                    return;
                }
                let typed = envelope.parse_typed();
                // A turn started or ended somewhere: the open session list
                // re-reads which trees are running.
                if self.dialog.as_ref().and_then(Dialog::tag) == Some(session_dialog::LIST_TAG)
                    && execution_edge(&typed.data)
                {
                    let _ = self.commands.send(AppCommand::ListRunningRoots);
                }
                // `form.*` frames parse as `Unknown` (no session id); the
                // store filters them by `form.sessionID` itself.
                let is_form = envelope.kind.starts_with("form.");
                if !is_form && typed.data.session_id() != self.session.as_deref() {
                    // 其它会话不进 reducer；子会话的执行起止由 store 自己筛选。
                    if self.store.observe_foreign(&typed) {
                        self.family_state_changed();
                    }
                    return;
                }
                ox_client::store::reduce(&mut self.store, &typed);
                // A tool event that names a child session may have missed
                // that child's `execution.started`: probe it once.
                if let ox_schema::event::SessionEventData::ToolSuccess {
                    assistant_message_id,
                    id,
                    ..
                }
                | ox_schema::event::SessionEventData::ToolProgress {
                    assistant_message_id,
                    id,
                    ..
                } = &typed.data
                    && let Some(child) = self.store.tool_child(assistant_message_id, id)
                {
                    let child = child.session_id.clone();
                    self.probe_children(vec![child]);
                }
                // 服务端换了会话的 model / agent：metadata 行跟着刷新。
                if let Some(session) = self.session.clone()
                    && matches!(
                        typed.data,
                        ox_schema::event::SessionEventData::SessionModelSelected { .. }
                            | ox_schema::event::SessionEventData::SessionAgentSelected { .. }
                    )
                {
                    let _ = self
                        .commands
                        .send(AppCommand::LoadSessionChrome { session });
                }
                self.seq_tx.send_replace(self.store.last_seq);
                // Deltas arrive far faster than frames: rebuilding the blocks
                // waits for `flush_store`, once per drawn frame.
                self.store_dirty = true;
                self.dirty = true;
                if is_form {
                    self.sync_form_dialog();
                }
            }
        }
    }

    /// Store → blocks → transcript. Only the changed tail is re-wrapped; dock
    /// reads the store directly (no duplicated view state).
    pub fn refresh(&mut self) {
        self.store_dirty = false;
        let changed_from = self.store.claim_changed_from();
        self.background_hint = self.background_hint_now();
        let projection = pipeline::blocks_from_store(
            &self.store,
            &self.shell_captures,
            changed_from,
            self.background_hint,
        );
        self.transcript
            .sync(&projection.blocks, projection.first_block);
        self.refresh_context();
        let cost = self.store.total_cost();
        self.prompt
            .set_cost((cost > 0.0).then(|| format!("${cost:.2}")));
        self.dirty = true;
    }

    /// Whether the background hint is due now (wall clock; the store owns
    /// the running-call and delay rules).
    fn background_hint_now(&self) -> pipeline::BackgroundHint {
        if self
            .store
            .background_hint_due(crate::ui::timeline::wall_ms_now())
        {
            pipeline::BackgroundHint::Shown
        } else {
            pipeline::BackgroundHint::Hidden
        }
    }

    /// Footer context figure: tokens, plus `(N%)` once the producing model's
    /// window is known. The catalogue is requested on first need.
    fn refresh_context(&mut self) {
        let usage = self.store.context_usage().map(|(tokens, model)| {
            let limit = model.and_then(|model| self.context_limits.get(model).copied());
            crate::ui::dock::format_context(tokens, limit)
        });
        if usage.is_some() && !self.limits_requested {
            self.limits_requested = true;
            let _ = self.commands.send(AppCommand::LoadContextLimits);
        }
        self.prompt.set_ctx(usage);
    }

    /// Apply the stream events reduced since the last frame.
    pub(crate) fn flush_store(&mut self) {
        if self.store_dirty {
            self.refresh();
        }
    }

    /// Publish the focused session's state to herdr. Called every tick: not
    /// every form or execution change marks the frame dirty, and the
    /// reporter only wakes its task when the value differs.
    fn report_herdr(&self) {
        if let Some(reporter) = &self.herdr {
            let state = herdr::state_of(
                self.store.running_since.is_some(),
                !self.store.pending_forms.is_empty(),
            );
            reporter.update(state, self.session.as_deref());
        }
    }

    fn status_mode(&self) -> StatusMode<'_> {
        if self.show_help {
            return StatusMode::Help;
        }
        match self.mode {
            Mode::SearchInput(_) | Mode::Browse => {
                let search = self.transcript.search();
                StatusMode::Search {
                    query: &search.query,
                    matches: search.matches.len(),
                    current: search.current,
                }
            }
            Mode::Compose => StatusMode::Normal,
        }
    }

    /// Adopt one attachment's bytes: a kitty transmission is cheap enough for
    /// the draw thread, a sixel arrived already encoded for its grid.
    ///
    /// The key leaves `media_pending` either way, so a resident image that is
    /// later evicted is asked for again, and a fetch the cache then refuses —
    /// a payload too large for the whole budget — is given up on like a failed
    /// one: asking again would repeat the same refusal on every frame.
    fn on_media(&mut self, key: String, class: SizeClass, result: Result<Fetched, String>) {
        self.media_pending.remove(&(key.clone(), class));
        let kept = match result {
            Ok(Fetched::Png(png)) => self.graphics.image(&key, class, &png),
            Ok(Fetched::Sixel(prepared)) => self.graphics.adopt(&key, class, *prepared),
            Err(err) => {
                tracing::warn!(key, %err, "attachment bytes not served");
                None
            }
        };
        if kept.is_none() {
            // A preview of this image would otherwise wait on a fetch that never
            // repeats: let it say `No preview`.
            if let Some(Dialog::Preview(preview)) = &mut self.dialog {
                preview.mark_failed(&key);
            }
            self.media_failed.insert((key, class));
        }
        self.dirty = true;
    }

    /// The enlarged preview of the thumbnail under `(x, y)`, when the pointer is
    /// on one: every image of that strip, opening at the clicked one. The
    /// composer's strip is checked first because it sits over nothing the
    /// transcript draws.
    fn preview_at(&self, x: u16, y: u16) -> Option<image_preview::Preview> {
        let (images, initial) = self
            .prompt
            .image_at(x, y)
            .or_else(|| self.transcript.image_at(x, y))?;
        Some(preview_of(images, initial))
    }

    /// Ask for the images the frame could not paint, transcript or dialog.
    ///
    /// The cache is the authority on what is resident, so a miss means the
    /// bytes are not here and the ask is legitimate: what must not repeat is
    /// the ask itself, and `media_pending` holds a key exactly while its answer
    /// is in flight (`media_failed` holds the ones given up on). A stored
    /// attachment belongs to the session on screen, which the home route does
    /// not have: there is nothing to fetch.
    fn request_media(&mut self, requests: Vec<media::Request>) {
        let App {
            commands,
            media_pending,
            media_failed,
            session,
            graphics,
            ..
        } = self;
        for request in requests {
            let cells = graphics.encodes_sixel().then_some(request.cells);
            let key = (request.key.clone(), request.class);
            if media_pending.contains(&key) || media_failed.contains(&key) {
                continue;
            }
            let command = match request.bytes {
                media::Bytes::Stored(source) => {
                    let Some(session) = session.as_ref() else {
                        continue;
                    };
                    AppCommand::FetchMedia {
                        session: session.clone(),
                        key: request.key,
                        source,
                        cells,
                        class: request.class,
                    }
                }
                media::Bytes::Inline(bytes) => AppCommand::MediaInline {
                    key: request.key,
                    bytes,
                    cells,
                    class: request.class,
                },
            };
            // A closed channel never answers, so the key must not stay in the
            // pending set: it would be asked for exactly once and then drop out
            // of the cache's reach for the life of the process.
            if commands.send(command).is_ok() {
                media_pending.insert(key);
            }
        }
    }

    /// One frame of the session route: transcript, docks, prompt (or the
    /// activity panel in its place), then the dialog and toast on top.
    fn draw_session(&mut self, frame: &mut ratatui::Frame) {
        let full = frame.area();
        // The transcript's strips are what the cache has to hold, so the window's
        // own height is what sizes the thumbnail class this frame.
        self.graphics
            .reserve_thumbnails(media::capacity(full.height));
        let searching = matches!(self.mode, Mode::SearchInput(_));
        let prompt_h = self
            .prompt
            .height(layout::padded(full, full.width).width, full)
            .min(full.height.saturating_sub(1));
        // The panel's rows are width independent, so they are built before the
        // layout needs their height.
        let panel = self.panel.map(|tab| {
            activity_panel::lines(
                &self.family,
                &self.panel_shells(),
                self.session.as_deref().unwrap_or_default(),
                tab,
                self.cursor,
            )
        });
        // A terminal shorter than the panel must not squeeze the transcript to
        // no rows, so the panel gives way the same way the prompt does.
        let bottom_h = match &panel {
            Some(block) => block.height().min(full.height.saturating_sub(1)),
            None => prompt_h,
        };
        let queued_h = if self.queued_dock_shown() {
            dock::queued_dock_height(&self.store.queued)
        } else {
            0
        };
        let (timeline_area, queued_area, prompt_area, dock_area) =
            layout::split_with_queue(full, if searching { 1 } else { bottom_h }, queued_h);
        let queued_area = layout::padded(queued_area, full.width);
        let queued_area = Rect {
            height: queued_area.height.min(dock::QUEUED_DOCK_ROWS),
            ..queued_area
        };
        self.queued_dock_area = if queued_h == 0 {
            Rect::default()
        } else {
            queued_area
        };
        // The transcript goes first: the slash menu pops up over it and must
        // end up on top (upstream draws the menu at `zIndex={100}`).
        let queued = self.transcript.render(
            frame,
            layout::padded(timeline_area, full.width),
            &mut self.graphics,
        );
        self.request_media(queued);
        if queued_h > 0 {
            let lines = dock::queued_dock(&self.store.queued, queued_area.width, self.dock_hover);
            frame.render_widget(ratatui::widgets::Paragraph::new(lines), queued_area);
        }
        // Only the prompt draws the live chip, so a panel or the search box in
        // its place must not leave the pointer a rect from an earlier frame.
        self.prompt.reset_live_area();
        if panel.is_some() || searching {
            // Nothing on the replaced prompt can be hovered, and the pointer may
            // never move again to say so: drop the highlight here or the chip
            // comes back lit when the panel closes.
            self.live_hover = false;
        }
        // The chip describes the session on screen. `App::session` and
        // `DataStore::session` hold that same id (`switch_session`,
        // `handle_session_ready` and New/Clear all set both), so the screen's
        // own field is the one to read. Derived per frame, never cached.
        let live = Live {
            subagents: self.store.running_subagents(),
            shells: self
                .session
                .as_deref()
                .map_or(0, |session| self.store.running_shells_of(session)),
            hovered: self.live_hover,
        };
        match &mut self.mode {
            Mode::SearchInput(boxed) => boxed.render(frame, prompt_area, self.dialog.is_none()),
            _ => {
                let prompt_area = layout::padded(prompt_area, full.width);
                match panel {
                    Some(block) => activity_panel::render(frame, prompt_area, block),
                    None => {
                        let activity = self.activity();
                        let from_prompt = self.prompt.render(
                            frame,
                            prompt_area,
                            full,
                            activity,
                            live,
                            &mut self.graphics,
                        );
                        self.request_media(from_prompt);
                    }
                }
            }
        }
        let line = dock::status_line(self.status_mode());
        frame.render_widget(ratatui::widgets::Paragraph::new(line), dock_area);
        let from_dialog = match &self.dialog {
            Some(dialog) => crate::ui::dialog::render(frame, full, dialog, &mut self.graphics),
            None => Vec::new(),
        };
        self.request_media(from_dialog);
        if let Some(toast) = &self.toast {
            toast::render(frame.buffer_mut(), full, toast, crate::theme::palette());
        }
    }

    pub async fn run(
        &mut self,
        terminal: &mut TerminalGuard,
        mut events: mpsc::UnboundedReceiver<AppEvent>,
    ) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(RENDER_TICK);
        let mut script: Option<std::vec::IntoIter<serde_json::Value>> = None;
        if let Ok(path) = std::env::var("OX_TUI_DEMO") {
            let content = std::fs::read_to_string(&path)?;
            let frames = content
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
                .map(serde_json::from_str)
                .collect::<Result<Vec<serde_json::Value>, _>>()?;
            script = Some(frames.into_iter());
        }

        if self.route == Route::Session {
            self.request_reload();
            self.request_running_shells();
        }

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Some(frames) = script.as_mut()
                        && let Some(raw) = frames.next()
                            && let Ok(envelope) = serde_json::from_value::<ox_schema::event::EventEnvelope<serde_json::Value>>(raw) {
                                self.handle_stream(StreamMsg::Event(Box::new(envelope)));
                            }
                    // 运行中的扫描条按时间走帧，空闲时不再重绘。
                    if self.prompt.tick_spinner(Instant::now(), self.activity()) {
                        self.dirty = true;
                    }
                    if self.route == Route::Session && self.transcript.tick(Instant::now()) {
                        self.dirty = true;
                    }
                    if let Some(Dialog::Select(select)) = self.dialog.as_mut()
                        && select.tick_spinner(Instant::now())
                    {
                        self.dirty = true;
                    }
                    if self.route == Route::Session {
                        self.poll_shells(Instant::now());
                    }
                    self.expire_toast(Instant::now());
                    // The hint row appears after a delay with no store event;
                    // a per-tick compare rebuilds the blocks only on a change.
                    if self.route == Route::Session
                        && self.background_hint_now() != self.background_hint
                    {
                        self.store_dirty = true;
                        self.dirty = true;
                    }
                    if self.dirty {
                        self.flush_store();
                        self.dirty = false;
                        self.prompt
                            .yield_cursor(self.dialog.is_some() || self.panel.is_some());
                        // The terminal applies the whole frame at once, so the
                        // cursor hopping around while cells are written is
                        // never seen (an IME anchors on it). Terminals that
                        // do not know the sequence ignore it.
                        flush_osc52(&mut self.pending_osc52, terminal.terminal.backend_mut())?;
                        execute!(terminal.terminal.backend_mut(), BeginSynchronizedUpdate)?;
                        let mut from_home = Vec::new();
                        let drawn = match self.route {
                            Route::Home => {
                                terminal.terminal.draw(|frame| {
                                    from_home = crate::ui::home::render(
                                        frame,
                                        frame.area(),
                                        &mut self.prompt,
                                        &mut self.graphics,
                                    );
                                    // Home has no transcript to fetch for; the
                                    // dialog's own images still are, and a
                                    // composer attachment carries its bytes.
                                    if let Some(dialog) = &self.dialog {
                                        let from_dialog = crate::ui::dialog::render(
                                            frame,
                                            frame.area(),
                                            dialog,
                                            &mut self.graphics,
                                        );
                                        from_home.extend(from_dialog);
                                    }
                                    if let Some(toast) = &self.toast {
                                        let area = frame.area();
                                        toast::render(frame.buffer_mut(), area, toast, crate::theme::palette());
                                    }
                                }).map(|_| ())
                            }
                            Route::Session => {
                                terminal.terminal.draw(|frame| self.draw_session(frame)).map(|_| ())
                            }
                        };
                        self.request_media(from_home);
                        execute!(terminal.terminal.backend_mut(), EndSynchronizedUpdate)?;
                        drawn?;
                    }
                    self.report_herdr();
                }
                Some(event) = events.recv() => {
                    self.handle(event);
                    if self.view.should_quit {
                        // The terminal keeps every image it was handed; nothing
                        // else will ever delete them.
                        self.graphics.release()?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// The enlarged preview of `images` starting at `initial`, as the dialog wants
/// it: the sources are shared, so nothing but the small names is copied.
fn preview_of(images: &[media::Image], initial: usize) -> image_preview::Preview {
    let images = images
        .iter()
        .map(|image| image_preview::Image {
            key: image.key.clone(),
            bytes: image.bytes.clone(),
            name: image.name.clone(),
        })
        .collect();
    image_preview::Preview::new(images, initial)
}

/// Whether the cell `(x, y)` lies in `area`.
fn contains(area: Rect, x: u16, y: u16) -> bool {
    area.contains(ratatui::layout::Position { x, y })
}

/// A session's turn started or ended (`session.execution.*`).
fn execution_edge(data: &ox_schema::event::SessionEventData) -> bool {
    use ox_schema::event::SessionEventData;
    matches!(
        data,
        SessionEventData::ExecutionStarted { .. }
            | SessionEventData::ExecutionSucceeded { .. }
            | SessionEventData::ExecutionFailed { .. }
            | SessionEventData::ExecutionInterrupted { .. }
    )
}

/// Send the pending OSC 52 copy sequence, if any, through the writer ratatui
/// draws with, so it cannot interleave with a frame.
fn flush_osc52(pending: &mut Option<String>, out: &mut impl std::io::Write) -> std::io::Result<()> {
    if let Some(sequence) = pending.take() {
        out.write_all(sequence.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}
