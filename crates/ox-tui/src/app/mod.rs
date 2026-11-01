pub mod command;
pub mod event;
pub mod keymap;
pub mod state;
mod switching;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ox_client::rest::{FamilyMember, ShellInfo, ShellTail};
use ox_client::sse::StreamMsg;
use ox_client::store::{DataStore, Item, ShellRef};
use ox_schema::support::FileInput;
use ratatui::layout::Rect;
use tokio::sync::{mpsc, watch};

use crate::attachment::Loaded;
use crate::clipboard::Clip;
use crate::pipeline;
use crate::shell_captures::ShellCaptures;
use crate::terminal::TerminalGuard;
use crate::ui::commands::CommandAction;
use crate::ui::composer::{SearchAction, SearchBox, Submission};
use crate::ui::dialog::{Dialog, DialogOutcome};
use crate::ui::dock::{self, DockHover, StatusMode};
use crate::ui::layout;
use crate::ui::output_dialog::{self, Output};
use crate::ui::palette;
use crate::ui::paste;
use crate::ui::prompt::{
    Activity, Chrome, Footer, Prompt, PromptAction, pick_placeholder, placeholder_line,
};
use crate::ui::queued_dialog;
use crate::ui::selection::Outcome;
use crate::ui::session_dialog;
use crate::ui::shell_dialog;
use crate::ui::sidebar::{self, TodoOpen};
use crate::ui::subagent_dialog;
use crate::ui::timeline::Transcript;
use crate::ui::timeline_dialog;
use crate::ui::toast::{self, Toast};
use command::{AppCommand, CancelReason};
use event::AppEvent;
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
    pending_reload: Option<u64>,
    /// Home 提交后等待会话创建：失败时把这段文本放回 composer。
    pending_home_prompt: Option<PendingHome>,
    /// Title of the open session, for the `/rename` prefill.
    session_title: Option<String>,
    dirty: bool,
    /// Stream events were reduced into the store but not yet turned into
    /// transcript blocks; `flush_store` does that once per frame.
    store_dirty: bool,
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
    /// The family behind the open subagent picker, fetched when it opened and
    /// replaced on every fetch. Only `ctrl+a` reads it, to rebuild the rows.
    family: Vec<FamilyMember>,
    /// The picker's filter; it outlives the dialog like upstream's in-memory
    /// `subagents-filter`.
    subagents_active_only: bool,
    /// The shell dialog the user just asked for and has not seen yet. A shell
    /// answer may open a dialog only for this; otherwise it may refresh a
    /// dialog that is still open. So a refresh that lands after `esc` is dropped.
    shell_wanted: Option<ShellWanted>,
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
    /// Whether the session sidebar shows (upstream `session.sidebar` config
    /// plus the in-memory `sidebarOpen`). Not persisted.
    sidebar_pref: SidebarPref,
    /// The sidebar Todo section's open state (upstream plugin storage).
    todo_open: TodoOpen,
    /// Where the sidebar was last drawn (empty when hidden), for the pointer.
    sidebar_area: Rect,
    /// Screen row of the clickable Todo header, when the sidebar drew one.
    sidebar_header: Option<u16>,
    /// The open session has a parent: upstream never shows its sidebar.
    session_child: bool,
    /// Width of the last session frame; decides docked vs overlay sidebar.
    screen_width: u16,
}

/// Upstream `session.sidebar` (`auto` | `hide`) folded with `sidebarOpen`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SidebarPref {
    /// Shown when the screen is wide.
    #[default]
    Auto,
    /// Shown at any width (an overlay when narrow).
    Open,
    Hidden,
}

/// Upstream `wide`: past this width the sidebar docks beside the session.
const WIDE_SCREEN: u16 = 120;

/// Upstream `sidebarVisible` for a root session, minus the panels and
/// terminals ox does not have.
#[must_use]
fn sidebar_visible(pref: SidebarPref, width: u16) -> bool {
    match pref {
        SidebarPref::Open => true,
        SidebarPref::Auto => width > WIDE_SCREEN,
        SidebarPref::Hidden => false,
    }
}

/// Which shell dialog an in-flight request will open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellWanted {
    Picker,
    Viewer,
}

impl App {
    /// Back the prompt history with a file (startup only; tests stay in memory).
    pub fn attach_history(&mut self, history: crate::ui::history::History) {
        self.prompt.set_history(history);
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
            family: Vec::new(),
            subagents_active_only: true,
            shell_wanted: None,
            shell_captures: ShellCaptures::default(),
            shell_syncs: 0,
            queued_dock_area: Rect::default(),
            dock_hover: DockHover::Off,
            sidebar_pref: SidebarPref::default(),
            todo_open: TodoOpen::default(),
            sidebar_area: Rect::default(),
            sidebar_header: None,
            session_child: false,
            screen_width: 0,
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
            AppEvent::History { request_id, items } => self.handle_history(request_id, items),
            AppEvent::ChildRunning { child, running } => {
                if self.store.apply_probe(&child, running) {
                    self.child_state_changed();
                }
            }
            AppEvent::SessionReady {
                session,
                prompt,
                files,
            } => self.handle_session_ready(session, prompt, files),
            AppEvent::SessionFailed(err) => self.handle_session_failed(err),
            AppEvent::PromptFailed(err) => {
                self.show_toast(Toast::error("Failed to send prompt", err, Instant::now()))
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
            AppEvent::Files { query, entries } => {
                self.prompt.set_files(&query, &entries);
                self.dirty = true;
            }
            AppEvent::McpServers(servers) => {
                if !self.form_open() {
                    self.dialog = Some(crate::ui::dialog::status(&servers));
                    self.dialog_purpose = None;
                    self.dirty = true;
                }
            }
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
                    self.session_child = selection.child;
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
            AppEvent::Sessions(sessions) => {
                if !self.form_open() {
                    self.dialog = Some(session_dialog::list(
                        &sessions,
                        self.session.as_deref(),
                        jiff::Timestamp::now(),
                        &jiff::tz::TimeZone::system(),
                    ));
                    self.dialog_purpose = None;
                    self.dirty = true;
                }
            }
            AppEvent::Family { session, members } => self.on_family(&session, members),
            AppEvent::Shells { session, shells } => self.on_shells(&session, &shells),
            AppEvent::RunningShells { generation, ids } => {
                if self.store.set_running_shells(generation, ids) {
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
        }
    }

    /// Open the subagent picker for the family just fetched. A late answer
    /// (the user moved to another session, a form needs answering) is dropped,
    /// and so is one for a child session: upstream only closes the picker there.
    fn on_family(&mut self, session: &str, members: Vec<FamilyMember>) {
        if self.session.as_deref() != Some(session) || self.form_open() {
            return;
        }
        let is_child = members
            .iter()
            .any(|member| member.id == session && member.parent_id.is_some());
        if is_child {
            return;
        }
        self.dialog = Some(subagent_dialog::dialog(
            &members,
            session,
            self.subagents_active_only,
        ));
        self.family = members;
        self.dirty = true;
    }

    /// Open (or refresh) the shell picker. A late answer for another session
    /// or one that would cover a form is dropped.
    fn on_shells(&mut self, session: &str, shells: &[ShellInfo]) {
        let opens = self.shell_wanted == Some(ShellWanted::Picker) && self.dialog.is_none();
        let refreshes = self.dialog.as_ref().and_then(Dialog::tag) == Some(shell_dialog::TAG);
        if self.session.as_deref() != Some(session) || !(opens || refreshes) {
            return;
        }
        if opens {
            self.shell_wanted = None;
        }
        self.dialog = Some(shell_dialog::dialog(shells, session));
        self.dirty = true;
    }

    /// Open (or refresh) the output viewer. A refresh of the same shell keeps
    /// the reader where they were. A late answer for another session or one
    /// that would cover a form is dropped.
    fn on_shell_output(&mut self, session: &str, shell: &ShellInfo, tail: ShellTail) {
        let opens = self.shell_wanted == Some(ShellWanted::Viewer) && self.dialog.is_none();
        let refreshes =
            matches!(&self.dialog, Some(Dialog::Output(open)) if open.shell_id() == shell.id);
        if self.session.as_deref() != Some(session) || !(opens || refreshes) {
            return;
        }
        if opens {
            self.shell_wanted = None;
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

    /// `ctrl+a` in the picker: flip running/finished and rebuild from the
    /// members fetched when it opened.
    fn toggle_subagents(&mut self) {
        self.subagents_active_only = !self.subagents_active_only;
        if let Some(session) = self.session.as_deref() {
            self.dialog = Some(subagent_dialog::dialog(
                &self.family,
                session,
                self.subagents_active_only,
            ));
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
        let purposeful = self.dialog_purpose.take().is_some();
        if purposeful || self.form_open() {
            self.dialog = None;
        }
        self.settled_forms.clear();
        self.session_title = None;
        self.session_child = false;
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
                if tag == Some(session_dialog::LIST_TAG) || tag == Some(subagent_dialog::TAG) {
                    self.switch_session(value);
                } else if tag == Some(queued_dialog::TAG) {
                    if let Some(session) = self.session.clone() {
                        let _ = self.commands.send(AppCommand::SteerQueued {
                            session,
                            inbox: value,
                        });
                    }
                } else if tag == Some(shell_dialog::TAG) {
                    self.shell_wanted = Some(ShellWanted::Viewer);
                    self.request_shell_output(value);
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
                if tag == Some(subagent_dialog::TAG) {
                    if key == subagent_dialog::TOGGLE_KEY {
                        self.toggle_subagents();
                    } else if key == subagent_dialog::INTERRUPT_KEY {
                        // The child's own session, not the one on screen.
                        let _ = self.commands.send(AppCommand::Interrupt { session: value });
                    }
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
                if tag == Some(shell_dialog::TAG) {
                    if key == shell_dialog::KILL_KEY
                        && let Some(session) = self.session.clone()
                    {
                        let _ = self
                            .commands
                            .send(AppCommand::KillShell { session, id: value });
                    }
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

    /// Whether the sidebar shows on the last drawn screen.
    fn sidebar_shown(&self) -> bool {
        self.route == Route::Session
            && !self.session_child
            && sidebar_visible(self.sidebar_pref, self.screen_width)
    }

    /// The sidebar shows over the transcript because the screen is narrow.
    fn sidebar_overlaid(&self) -> bool {
        self.sidebar_shown() && self.screen_width <= WIDE_SCREEN
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
        self.store.push_user(&prompt);
        let _ = self.commands.send(AppCommand::Prompt {
            session,
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
    fn handle_history(&mut self, request_id: u64, items: Vec<Item>) {
        if self.pending_reload != Some(request_id) {
            return;
        }
        self.pending_reload = None;
        self.store.replace_items(items);
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
        self.pending_reload = Some(request_id);
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
        // A dialog is modal: it eats every key, and only dismissal leaves it.
        if let Some(dialog) = &mut self.dialog {
            self.keymap.reset();
            // `ctrl+c` stays a way out of anything, including a modal.
            if keymap::is_cancel(&key) {
                self.interrupt_or_quit();
                self.dirty = true;
                return;
            }
            let tag = dialog.tag();
            let outcome = dialog.on_key(key);
            self.apply_dialog_outcome(tag, outcome);
            return;
        }

        // The narrow overlay sidebar closes on esc / ctrl+c before anything
        // else sees them (upstream `index.tsx:256-260`, layer priority 10).
        if self.sidebar_overlaid()
            && key.kind != crossterm::event::KeyEventKind::Release
            && (key.code == KeyCode::Esc || keymap::is_cancel(&key))
        {
            self.keymap.reset();
            self.sidebar_pref = SidebarPref::Hidden;
            self.dirty = true;
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

        // Browse mode: single-letter navigation is safe here because the
        // prompt is not receiving keys.
        if matches!(self.mode, Mode::Browse) {
            if keymap::is_cancel(&key) {
                self.interrupt_or_quit();
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
            PromptAction::CtrlC => self.interrupt_or_quit(),
            PromptAction::Esc => self.press_esc(Instant::now()),
            PromptAction::Submit(submission) => self.submit(submission),
            PromptAction::Run(action) => self.run_command(action),
        }
        self.flush_file_request();
        self.dirty = true;
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
        // 本地乐观显示：随后一帧就有用户消息；服务端确认后由历史重拉覆盖。
        self.store.push_user(&text);
        self.refresh();
        if let Some(session) = self.session.clone() {
            let _ = self.commands.send(AppCommand::Prompt {
                session,
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
                let _ = self.commands.send(AppCommand::McpList);
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
                    let _ = self.commands.send(AppCommand::ListFamily { session });
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
                if let Some(session) = self.session.clone() {
                    self.shell_wanted = Some(ShellWanted::Picker);
                    let _ = self.commands.send(AppCommand::ListShells { session });
                }
            }
            CommandAction::ToggleSidebar => {
                // Upstream `toggleSidebar`: hide what shows, open what does not.
                self.sidebar_pref = if self.sidebar_shown() {
                    SidebarPref::Hidden
                } else {
                    SidebarPref::Open
                };
                self.dirty = true;
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
            CommandAction::New | CommandAction::Clear => {
                self.session = None;
                self.store = DataStore::default();
                self.transcript = Transcript::new(80, 20);
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
    /// Its answer takes its own path, never the picker's `shell_wanted` one.
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

    /// What the footer shows as going on. Display only: the own turn
    /// (`running_since`) wins, otherwise running child sessions still light
    /// the scanner. `press_esc` and `interrupt_or_quit` below deliberately
    /// stay on `running_since`: child events never touch it, so Esc / ctrl+c
    /// keep acting on the own turn alone.
    fn activity(&self) -> Activity {
        if self.store.running_since.is_some() {
            Activity::Turn
        } else if self.store.busy() {
            Activity::Children
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

    /// ctrl+c with an empty draft: interrupt a running turn, otherwise quit.
    fn interrupt_or_quit(&mut self) {
        if self.store.running_since.is_some() {
            if let Some(session) = self.session.clone() {
                let _ = self.commands.send(AppCommand::Interrupt { session });
            }
        } else {
            self.view.should_quit = true;
        }
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
        if contains(self.sidebar_area, x, y) {
            self.transcript.release();
            if self.sidebar_header == Some(y) {
                self.todo_open = self.todo_open.toggled();
            }
            self.dirty = true;
            return;
        }
        match self.transcript.release() {
            Outcome::Click => {
                // A subagent notice opens its child session; the owned id
                // ends the transcript borrow before `switch_session`.
                if let Some(child) = self.transcript.child_at(x, y).map(str::to_owned) {
                    self.switch_session(child);
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
        if changed || dock_changed {
            self.dirty = true;
        }
    }

    fn handle_wheel(&mut self, direction: i8) {
        if self.route != Route::Session || self.dialog.is_some() {
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
                let typed = envelope.parse_typed();
                // `form.*` frames parse as `Unknown` (no session id); the
                // store filters them by `form.sessionID` itself.
                let is_form = envelope.kind.starts_with("form.");
                if !is_form && typed.data.session_id() != self.session.as_deref() {
                    // 其它会话不进 reducer；子会话的执行起止由 store 自己筛选。
                    if self.store.observe_foreign(&typed) {
                        self.child_state_changed();
                    }
                    return;
                }
                ox_client::store::reduce(&mut self.store, &typed);
                // A tool result that names a child session may have missed
                // that child's `execution.started`: probe it once.
                if let ox_schema::event::SessionEventData::ToolSuccess {
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
        let blocks = pipeline::blocks_from_store(&self.store, &self.shell_captures);
        self.transcript.sync(&blocks, changed_from);
        self.refresh_context();
        let cost = self.store.total_cost();
        self.prompt
            .set_cost((cost > 0.0).then(|| format!("${cost:.2}")));
        self.dirty = true;
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

    /// One frame of the session route: transcript, docks, prompt, then the
    /// sidebar, dialog and toast on top.
    fn draw_session(&mut self, frame: &mut ratatui::Frame) {
        let screen = frame.area();
        self.screen_width = screen.width;
        let sidebar = self.sidebar_shown().then(|| Rect {
            x: screen.right().saturating_sub(sidebar::SIDEBAR_WIDTH),
            width: screen.width.min(sidebar::SIDEBAR_WIDTH),
            ..screen
        });
        // Wide: the session column gives the sidebar its width.
        let full = match sidebar {
            Some(panel) if !self.sidebar_overlaid() => Rect {
                width: screen.width.saturating_sub(panel.width),
                ..screen
            },
            _ => screen,
        };
        let searching = matches!(self.mode, Mode::SearchInput(_));
        let prompt_h = self
            .prompt
            .height(full.height)
            .min(full.height.saturating_sub(1));
        let queued_h = if self.queued_dock_shown() {
            dock::queued_dock_height(&self.store.queued)
        } else {
            0
        };
        let (timeline_area, queued_area, prompt_area, dock_area) =
            layout::split_with_queue(full, if searching { 1 } else { prompt_h }, queued_h);
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
        if queued_h > 0 {
            let lines = dock::queued_dock(&self.store.queued, queued_area.width, self.dock_hover);
            frame.render_widget(ratatui::widgets::Paragraph::new(lines), queued_area);
        }
        match &mut self.mode {
            Mode::SearchInput(boxed) => boxed.render(frame, prompt_area, self.dialog.is_none()),
            _ => {
                let prompt_area = layout::padded(prompt_area, full.width);
                let activity = self.activity();
                self.prompt.render(frame, prompt_area, full, activity);
            }
        }
        self.transcript
            .render(frame, layout::padded(timeline_area, full.width));
        let line = dock::status_line(&self.store, self.status_mode());
        frame.render_widget(ratatui::widgets::Paragraph::new(line), dock_area);
        self.sidebar_area = sidebar.unwrap_or_default();
        self.sidebar_header = sidebar.and_then(|panel| {
            sidebar::render(
                self.session_title.as_deref().unwrap_or("New session"),
                self.store.session_todos(),
                self.todo_open,
                panel,
                frame.buffer_mut(),
            )
        });
        if let Some(dialog) = &self.dialog {
            crate::ui::dialog::render(frame, screen, dialog);
        }
        if let Some(toast) = &self.toast {
            toast::render(frame.buffer_mut(), screen, toast, crate::theme::palette());
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
                    if self.route == Route::Session {
                        self.poll_shells(Instant::now());
                    }
                    self.expire_toast(Instant::now());
                    if self.dirty {
                        self.flush_store();
                        self.dirty = false;
                        self.prompt.yield_cursor(self.dialog.is_some());
                        // The terminal applies the whole frame at once, so the
                        // cursor hopping around while cells are written is
                        // never seen (an IME anchors on it). Terminals that
                        // do not know the sequence ignore it.
                        flush_osc52(&mut self.pending_osc52, terminal.terminal.backend_mut())?;
                        execute!(terminal.terminal.backend_mut(), BeginSynchronizedUpdate)?;
                        let drawn = match self.route {
                            Route::Home => {
                                terminal.terminal.draw(|frame| {
                                    crate::ui::home::render(
                                        frame,
                                        frame.area(),
                                        &mut self.prompt,
                                    );
                                    if let Some(dialog) = &self.dialog {
                                        crate::ui::dialog::render(frame, frame.area(), dialog);
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
                        execute!(terminal.terminal.backend_mut(), EndSynchronizedUpdate)?;
                        drawn?;
                    }
                }
                Some(event) = events.recv() => {
                    self.handle(event);
                    if self.view.should_quit {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Whether the cell `(x, y)` lies in `area`.
fn contains(area: Rect, x: u16, y: u16) -> bool {
    area.contains(ratatui::layout::Position { x, y })
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
