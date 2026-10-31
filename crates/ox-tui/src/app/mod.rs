pub mod command;
pub mod event;
pub mod keymap;
pub mod state;
mod switching;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ox_client::sse::StreamMsg;
use ox_client::store::{DataStore, Item};
use ox_schema::support::FileInput;
use tokio::sync::{mpsc, watch};

use crate::attachment::Loaded;
use crate::clipboard::Clip;
use crate::pipeline;
use crate::terminal::TerminalGuard;
use crate::ui::commands::CommandAction;
use crate::ui::composer::{SearchAction, SearchBox, Submission};
use crate::ui::dialog::{Dialog, DialogOutcome};
use crate::ui::dock::{self, StatusMode};
use crate::ui::layout;
use crate::ui::palette;
use crate::ui::paste;
use crate::ui::prompt::{Chrome, Footer, Prompt, PromptAction, pick_placeholder, placeholder_line};
use crate::ui::selection::Outcome;
use crate::ui::session_dialog;
use crate::ui::timeline::Transcript;
use crate::ui::timeline_dialog;
use crate::ui::toast::{self, Toast};
use command::AppCommand;
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
}

impl App {
    /// Back the prompt history with a file (startup only; tests stay in memory).
    pub fn attach_history(&mut self, history: crate::ui::history::History) {
        self.prompt.set_history(history);
    }

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
        match self.transcript.release() {
            Outcome::Click => {
                if self.transcript.click(x, y) {
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
        let changed = if self.pointer_enabled() {
            self.transcript.hover(x, y)
        } else {
            self.transcript.clear_hover()
        };
        if changed {
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
            }
            StreamMsg::Event(envelope) => {
                let typed = envelope.parse_typed();
                // `form.*` frames parse as `Unknown` (no session id); the
                // store filters them by `form.sessionID` itself.
                let is_form = envelope.kind.starts_with("form.");
                if !is_form && typed.data.session_id() != self.session.as_deref() {
                    return; // 全局流里过滤出本会话
                }
                ox_client::store::reduce(&mut self.store, &typed);
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
        let blocks = pipeline::blocks_from_store(&self.store);
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
                    let running = self.store.running_since.is_some();
                    if self.prompt.tick_spinner(Instant::now(), running) {
                        self.dirty = true;
                    }
                    if self.route == Route::Session && self.transcript.tick(Instant::now()) {
                        self.dirty = true;
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
                                terminal.terminal.draw(|frame| {
                                    let full = frame.area();
                                    let searching = matches!(self.mode, Mode::SearchInput(_));
                                    let prompt_h = self
                                        .prompt
                                        .height(full.height)
                                        .min(full.height.saturating_sub(1));
                                    let (timeline_area, prompt_area, dock_area) = layout::split(
                                        full,
                                        if searching { 1 } else { prompt_h },
                                    );
                                    match &mut self.mode {
                                        Mode::SearchInput(boxed) => {
                                            boxed.render(frame, prompt_area, self.dialog.is_none())
                                        }
                                        _ => {
                                            let prompt_area =
                                                layout::padded(prompt_area, full.width);
                                            let running = self.store.running_since.is_some();
                                            self.prompt.render(
                                                frame,
                                                prompt_area,
                                                full,
                                                running,
                                            );
                                        }
                                    }
                                    self.transcript
                                        .render(frame, layout::padded(timeline_area, full.width));
                                    let line = dock::status_line(&self.store, self.status_mode());
                                    frame.render_widget(
                                        ratatui::widgets::Paragraph::new(line),
                                        dock_area,
                                    );
                                    if let Some(dialog) = &self.dialog {
                                        crate::ui::dialog::render(frame, full, dialog);
                                    }
                                    if let Some(toast) = &self.toast {
                                        toast::render(frame.buffer_mut(), full, toast, crate::theme::palette());
                                    }
                                }).map(|_| ())
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

/// Send the pending OSC 52 copy sequence, if any, through the writer ratatui
/// draws with, so it cannot interleave with a frame.
fn flush_osc52(pending: &mut Option<String>, out: &mut impl std::io::Write) -> std::io::Result<()> {
    if let Some(sequence) = pending.take() {
        out.write_all(sequence.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{App, PendingHome};
    use crate::app::command::AppCommand;
    use crate::app::event::AppEvent;
    use crate::app::state::Route;
    use crate::attachment::Loaded;
    use crate::ui::composer::Submission;
    use crate::ui::dialog::Dialog;
    use crate::ui::toast::{self, Toast};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ox_client::store::Item;
    use ox_schema::support::FileInput;
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, watch};

    fn app(route: Route, session: Option<&str>) -> (App, mpsc::UnboundedReceiver<AppCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (seq_tx, _seq_rx) = watch::channel(None);
        (
            App::new(
                route,
                session.map(String::from),
                crate::ui::prompt::Chrome::default(),
                tx,
                seq_tx,
            ),
            rx,
        )
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn ctrl(app: &mut App, ch: char) {
        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::CONTROL,
        )));
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            key(app, KeyCode::Char(ch));
        }
    }

    #[test]
    fn double_esc_interrupts_a_running_turn() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.store.running_since = Some(0);
        key(&mut app, KeyCode::Esc);
        assert!(rx.try_recv().is_err(), "one Esc only arms");
        key(&mut app, KeyCode::Esc);
        assert!(matches!(rx.try_recv(), Ok(AppCommand::Interrupt { .. })));
    }

    #[test]
    fn esc_while_idle_sends_nothing() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Esc);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn typing_marks_the_frame_dirty() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.dirty = false;
        key(&mut app, KeyCode::Char('x'));
        assert!(app.dirty, "a typed character must schedule a redraw");
    }

    #[test]
    fn resize_marks_the_frame_dirty() {
        // Idle app: the tick only redraws when `dirty`, so a resize that does
        // not set it leaves the old frame on screen forever.
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.dirty = false;
        app.handle(AppEvent::Resize);
        assert!(app.dirty, "a terminal resize must schedule a redraw");
    }

    #[test]
    fn hover_marks_the_transcript_dirty_only_when_the_hovered_row_changes() {
        use crate::ui::timeline::{Block, Transcript};

        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.transcript = Transcript::new(40, 10);
        app.transcript.sync(&[Block::user("hello")], 0);
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| app.transcript.render(frame, frame.area()))
            .unwrap();

        app.dirty = false;
        app.handle(AppEvent::Hover { x: 5, y: 2 });
        assert!(app.dirty, "entering the user block repaints");
        app.dirty = false;
        app.handle(AppEvent::Hover { x: 9, y: 2 });
        assert!(!app.dirty, "same block: no repaint per pointer cell");
        app.handle(AppEvent::Hover { x: 100, y: 2 });
        assert!(app.dirty, "leaving the transcript clears and repaints");
    }

    #[test]
    fn hover_is_cleared_and_ignored_under_a_dialog_or_off_the_session_route() {
        use crate::ui::timeline::{Block, Transcript};

        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.transcript = Transcript::new(40, 10);
        app.transcript.sync(&[Block::user("hello")], 0);
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| app.transcript.render(frame, frame.area()))
            .unwrap();
        app.handle(AppEvent::Hover { x: 5, y: 2 });

        app.dialog = Some(Dialog::Select(crate::ui::palette::select(
            crate::ui::palette::Context { in_session: true },
            crate::app::keymap::hint,
        )));
        app.dirty = false;
        app.handle(AppEvent::Hover { x: 5, y: 2 });
        assert!(app.dirty, "opening a dialog dropped the existing hover");
        app.dirty = false;
        app.handle(AppEvent::Hover { x: 5, y: 2 });
        assert!(!app.dirty, "a modal swallows hover; nothing left to clear");

        app.dialog = None;
        app.route = Route::Home;
        app.dirty = false;
        app.handle(AppEvent::Hover { x: 5, y: 2 });
        assert!(!app.dirty, "the home route has no transcript to hover");
    }

    #[test]
    fn browse_o_toggles_the_tool_row_output() {
        use super::Mode;
        use crate::ui::timeline::{Block, Transcript};
        use ox_client::store::ToolState;

        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.transcript = Transcript::new(60, 10);
        app.transcript.sync(
            &[Block::Tool {
                id: "p1".into(),
                version: 0,
                tool: "shell".into(),
                state: ToolState::Success,
                summary: "cargo test".into(),
                output: std::sync::Arc::new("ok\ndone".into()),
            }],
            0,
        );
        let collapsed = app.transcript.total_lines();
        app.mode = Mode::Browse;
        key(&mut app, KeyCode::Char('o'));
        assert!(
            app.transcript.total_lines() > collapsed,
            "o expands the tool row in browse mode"
        );

        key(&mut app, KeyCode::Char('o'));
        assert_eq!(
            app.transcript.total_lines(),
            collapsed,
            "o collapses it again"
        );
    }

    #[test]
    fn typing_an_at_asks_the_server_for_files_and_the_answer_fills_the_menu() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "read @sr");
        // One lookup per query change: "", "s", "sr".
        for expected in ["", "s", "sr"] {
            match rx.try_recv().expect("a lookup must be sent") {
                AppCommand::FindFiles(query) => assert_eq!(query, expected),
                other => panic!("unexpected command: {other:?}"),
            }
        }
        assert!(rx.try_recv().is_err());
        app.handle(AppEvent::Files {
            query: "sr".into(),
            entries: vec![ox_client::rest::FileEntry {
                path: "src/".into(),
                kind: "directory".into(),
            }],
        });
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.prompt.text(), "read @src/ ");
    }

    #[test]
    fn the_status_command_asks_the_server_for_the_mcp_list() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "/status");
        key(&mut app, KeyCode::Enter);
        match rx.try_recv().expect("a command must be sent") {
            AppCommand::McpList => {}
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(
            app.dialog.is_none(),
            "the dialog waits for the server answer"
        );
    }

    #[test]
    fn the_mcp_answer_opens_the_status_dialog_and_esc_closes_it() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::McpServers(vec![ox_client::rest::McpRef {
            name: "context7".into(),
            status: ox_client::rest::McpStatus::Connected,
        }]));
        let Some(crate::ui::dialog::Dialog::Info(dialog)) = app.dialog.as_ref() else {
            panic!("the status dialog opens");
        };
        assert_eq!(dialog.title, "Status");
        assert_eq!(dialog.rows.len(), 1);
        assert_eq!(dialog.rows[0].label, "context7");

        key(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none(), "esc dismisses the dialog");
    }

    fn paste(app: &mut App, text: &str) {
        app.handle(AppEvent::Paste(text.to_owned()));
    }

    #[test]
    fn paste_in_compose_mode_reaches_the_prompt() {
        let (mut app, _rx) = app(Route::Home, None);
        app.dirty = false;
        paste(&mut app, "第一行\r\n第二行");
        assert_eq!(app.prompt.text(), "第一行\n第二行");
        assert!(app.dirty, "the pasted text must repaint");
    }

    #[test]
    fn pasting_image_paths_asks_the_executor_to_load_them() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        paste(&mut app, "'/tmp/my shot.png'");
        let commands = drain(&mut rx);
        assert!(
            matches!(commands.as_slice(), [AppCommand::LoadAttachments(t)] if t == "'/tmp/my shot.png'"),
            "{commands:?}"
        );
        assert!(app.prompt.is_empty(), "nothing is typed until the answer");
    }

    #[test]
    fn pasting_ordinary_text_never_loads_files() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        paste(&mut app, "see /tmp/notes.txt and a.png");
        let commands = drain(&mut rx);
        assert!(commands.is_empty(), "{commands:?}");
        assert_eq!(app.prompt.text(), "see /tmp/notes.txt and a.png");
    }

    #[test]
    fn dialogs_and_search_never_load_files() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut app, 'f');
        paste(&mut app, "/tmp/a.png");
        assert!(drain(&mut rx).is_empty());
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn a_failed_load_pastes_the_text_as_typed() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.dirty = false;
        app.handle(AppEvent::Attachments {
            text: "/tmp/gone.png".into(),
            loaded: None,
        });
        assert_eq!(app.prompt.text(), "/tmp/gone.png");
        assert!(app.dirty);
    }

    #[test]
    fn a_failed_load_flushes_the_file_menu_request_at_once() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::Attachments {
            text: "see @sr".into(),
            loaded: None,
        });
        let commands = drain(&mut rx);
        assert!(
            matches!(commands.as_slice(), [AppCommand::FindFiles(q)] if q == "sr"),
            "{commands:?}"
        );
    }

    fn one_png() -> Vec<Loaded> {
        vec![Loaded::File {
            name: "shot.png".into(),
            input: FileInput {
                uri: "data:image/png;base64,AAAA".into(),
                source: None,
                name: Some("shot.png".into()),
                description: None,
                mention: None,
            },
        }]
    }

    #[test]
    fn attachments_arriving_behind_a_modal_are_dropped_with_a_toast() {
        let (mut dialog, _rx) = app(Route::Session, Some("ses_test"));
        dialog.dialog = Some(crate::ui::session_dialog::rename("ses_test", "old"));
        dialog.handle(AppEvent::Attachments {
            text: "/tmp/shot.png".into(),
            loaded: Some(one_png()),
        });
        assert!(dialog.prompt.is_empty(), "nothing lands behind the dialog");
        let toast = dialog.toast.as_ref().expect("the drop is explained");
        assert!(toast.message.contains("dropped"), "{toast:?}");

        let (mut search, _rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut search, 'f');
        search.handle(AppEvent::Attachments {
            text: "/tmp/gone.png".into(),
            loaded: None,
        });
        assert!(search.prompt.is_empty());
        let toast = search.toast.as_ref().expect("the drop is explained");
        assert!(toast.message.contains("dropped"), "{toast:?}");
    }

    #[test]
    fn a_clipboard_answer_behind_a_modal_is_dropped_with_a_toast() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.dialog = Some(crate::ui::session_dialog::rename("ses_test", "old"));
        app.handle(AppEvent::ClipboardPasted(crate::clipboard::Clip::Text(
            "late".into(),
        )));
        assert!(app.prompt.is_empty());
        let toast = app.toast.as_ref().expect("the drop is explained");
        assert!(toast.message.contains("dropped"), "{toast:?}");
    }

    #[test]
    fn loaded_files_show_labels_and_ride_along_with_the_prompt() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::Attachments {
            text: "/tmp/shot.png".into(),
            loaded: Some(vec![Loaded::File {
                name: "shot.png".into(),
                input: FileInput {
                    uri: "data:image/png;base64,AAAA".into(),
                    source: None,
                    name: Some("shot.png".into()),
                    description: None,
                    mention: None,
                },
            }]),
        });
        assert_eq!(app.prompt.text(), "[Image 1] ");
        type_text(&mut app, "what is this");
        key(&mut app, KeyCode::Enter);
        let commands = drain(&mut rx);
        let Some(AppCommand::Prompt { text, files, .. }) = commands
            .iter()
            .find(|c| matches!(c, AppCommand::Prompt { .. }))
        else {
            panic!("a prompt command expected: {commands:?}");
        };
        assert_eq!(text, "[Image 1] what is this");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].uri, "data:image/png;base64,AAAA");
    }

    #[test]
    fn without_an_executor_a_path_paste_falls_back_to_text() {
        let (mut app, rx) = app(Route::Session, Some("ses_test"));
        drop(rx);
        paste(&mut app, "/tmp/a.png");
        assert_eq!(app.prompt.text(), "/tmp/a.png");
    }

    fn clipboard_png() -> crate::clipboard::Clip {
        crate::clipboard::Clip::Image(FileInput {
            uri: "data:image/png;base64,AAAA".into(),
            source: None,
            name: Some("clipboard".into()),
            description: None,
            mention: None,
        })
    }

    fn read_clipboard_requests(commands: &[AppCommand]) -> usize {
        commands
            .iter()
            .filter(|c| matches!(c, AppCommand::ReadClipboard))
            .count()
    }

    #[test]
    fn ctrl_v_in_the_prompt_asks_the_executor_for_the_clipboard() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut app, 'v');
        let commands = drain(&mut rx);
        assert_eq!(read_clipboard_requests(&commands), 1, "{commands:?}");
        assert!(app.prompt.is_empty(), "nothing is typed until the answer");
    }

    #[test]
    fn ctrl_v_from_browse_mode_goes_back_to_the_prompt_and_asks() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut app, 'f');
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, super::Mode::Browse));
        ctrl(&mut app, 'v');
        assert!(matches!(app.mode, super::Mode::Compose));
        assert_eq!(read_clipboard_requests(&drain(&mut rx)), 1);
    }

    #[test]
    fn ctrl_v_in_a_dialog_or_the_search_box_never_reads_the_clipboard() {
        let (mut searching, mut rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut searching, 'f');
        ctrl(&mut searching, 'v');
        assert_eq!(read_clipboard_requests(&drain(&mut rx)), 0, "search box");

        let (mut modal, mut rx) = app(Route::Session, Some("ses_test"));
        modal.dialog = Some(crate::ui::session_dialog::rename("ses_test", "old"));
        ctrl(&mut modal, 'v');
        assert_eq!(read_clipboard_requests(&drain(&mut rx)), 0, "dialog");
    }

    #[test]
    fn an_empty_bracketed_paste_reads_the_clipboard_in_the_prompt_only() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        paste(&mut app, "");
        assert_eq!(read_clipboard_requests(&drain(&mut rx)), 1);

        ctrl(&mut app, 'f');
        paste(&mut app, "");
        assert_eq!(read_clipboard_requests(&drain(&mut rx)), 0, "search box");
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn a_clipboard_image_becomes_a_labelled_attachment_that_rides_the_prompt() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.dirty = false;
        app.handle(AppEvent::ClipboardPasted(clipboard_png()));
        assert_eq!(app.prompt.text(), "[Image 1] ");
        assert!(app.dirty);
        type_text(&mut app, "what is this");
        key(&mut app, KeyCode::Enter);
        let commands = drain(&mut rx);
        let Some(AppCommand::Prompt { text, files, .. }) = commands
            .iter()
            .find(|c| matches!(c, AppCommand::Prompt { .. }))
        else {
            panic!("a prompt command expected: {commands:?}");
        };
        assert_eq!(text, "[Image 1] what is this");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].uri, "data:image/png;base64,AAAA");
        assert_eq!(files[0].name.as_deref(), Some("clipboard"));
    }

    #[test]
    fn clipboard_text_goes_through_the_paste_entry() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::ClipboardPasted(crate::clipboard::Clip::Text(
            "第一行\r\n第二行".into(),
        )));
        assert_eq!(app.prompt.text(), "第一行\n第二行");

        // Image paths on the clipboard attach like a pasted path.
        app.handle(AppEvent::ClipboardPasted(crate::clipboard::Clip::Text(
            "'/tmp/my shot.png'".into(),
        )));
        let commands = drain(&mut rx);
        assert!(
            matches!(commands.as_slice(), [AppCommand::LoadAttachments(t)] if t == "'/tmp/my shot.png'"),
            "{commands:?}"
        );
    }

    #[test]
    fn an_empty_or_unsupported_clipboard_shows_a_toast_and_changes_nothing() {
        use crate::clipboard::Clip;
        for (clip, needle) in [
            (Clip::Empty, "no image or text"),
            (Clip::Unsupported, "not supported"),
        ] {
            let (mut app, _rx) = app(Route::Session, Some("ses_test"));
            app.dirty = false;
            app.handle(AppEvent::ClipboardPasted(clip));
            let toast = app
                .toast
                .as_ref()
                .expect("the user is told why nothing happened");
            assert!(toast.message.contains(needle), "{toast:?}");
            assert!(app.prompt.is_empty());
            assert!(app.dirty);
        }
    }

    #[test]
    fn paste_in_search_input_mode_filters_the_transcript_on_one_line() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut app, 'f');
        paste(&mut app, "needle\nhay");
        let super::Mode::SearchInput(search) = &app.mode else {
            panic!("search stays open");
        };
        assert_eq!(search.text(), "needle hay");
        assert!(app.prompt.is_empty(), "the draft is untouched");
    }

    #[test]
    fn paste_in_browse_mode_goes_back_to_the_prompt() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut app, 'f');
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, super::Mode::Browse));
        paste(&mut app, "typed");
        assert!(matches!(app.mode, super::Mode::Compose));
        assert_eq!(app.prompt.text(), "typed");
    }

    #[test]
    fn paste_into_a_dialog_input_stays_in_the_dialog() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.dialog = Some(crate::ui::session_dialog::rename("ses_test", "old"));
        paste(&mut app, " new\ntitle");
        assert!(app.prompt.is_empty(), "a modal must not leak paste");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        let mut commands = Vec::new();
        while let Ok(command) = rx.try_recv() {
            commands.push(command);
        }
        assert!(
            commands.iter().any(|c| matches!(c,
                AppCommand::RenameSession { title, .. } if title == "old new title")),
            "{commands:?}"
        );
    }

    #[test]
    fn paste_into_a_dialog_without_a_text_field_is_swallowed() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::McpServers(Vec::new()));
        app.dirty = false;
        paste(&mut app, "hello");
        assert!(app.prompt.is_empty());
        assert!(app.dialog.is_some());
        assert!(!app.dirty, "nothing changed, so nothing repaints");
    }

    #[test]
    fn paste_into_a_form_text_field_is_one_line() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.dialog = Some(Dialog::Form(Box::new(crate::ui::form::FormState::new(
            &serde_json::from_value(serde_json::json!({
                "id": "frm_1", "sessionID": "ses_test", "title": "T",
                "fields": [{"type": "string", "key": "name", "title": "Name"}]
            }))
            .unwrap(),
        ))));
        paste(&mut app, "a\nb");
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none(), "the form submits");
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn an_open_dialog_swallows_message_keys() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::McpServers(Vec::new()));
        app.dirty = false;
        type_text(&mut app, "hello");
        assert!(
            app.prompt.is_empty(),
            "a modal must not leak keys into the composer"
        );
        assert!(!app.dirty, "nothing changed, so nothing repaints");
    }

    #[test]
    fn ctrl_c_still_quits_while_a_dialog_is_open() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::McpServers(Vec::new()));
        ctrl(&mut app, 'c');
        assert!(app.view.should_quit, "a modal must not trap the quit chord");
    }

    #[test]
    fn home_enter_asks_for_session_creation_and_keeps_the_route() {
        let (mut app, mut rx) = app(Route::Home, None);
        type_text(&mut app, "hi");
        key(&mut app, KeyCode::Enter);
        match rx.try_recv().expect("a command must be sent") {
            AppCommand::CreateSessionAndPrompt(submission) => {
                assert_eq!(submission, Submission::text("hi"))
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(app.route, Route::Home);
        assert!(
            app.prompt.is_empty(),
            "the draft moves to the pending prompt"
        );
        assert_eq!(
            app.pending_home_prompt,
            Some(PendingHome {
                text: "hi".into(),
                dropped_files: 0
            })
        );
    }

    #[test]
    fn session_ready_switches_route_and_sends_the_prompt() {
        let (mut app, mut rx) = app(Route::Home, None);
        type_text(&mut app, "hi");
        key(&mut app, KeyCode::Enter);
        let _ = rx.try_recv();
        app.handle(AppEvent::SessionReady {
            session: "ses_new".into(),
            prompt: "hi".into(),
            files: Vec::new(),
        });
        assert_eq!(app.route, Route::Session);
        assert_eq!(app.session.as_deref(), Some("ses_new"));
        assert_eq!(app.store.session.as_deref(), Some("ses_new"));
        match rx.try_recv().expect("prompt must follow") {
            AppCommand::Prompt {
                session,
                text,
                files,
            } => {
                assert_eq!(session, "ses_new");
                assert_eq!(text, "hi");
                assert!(files.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // 乐观显示：store 里已有用户消息
        assert!(format!("{:?}", app.store.items).contains("hi"));
    }

    fn fake_file() -> FileInput {
        FileInput {
            uri: "data:image/png;base64,AAAA".into(),
            source: None,
            name: Some("shot.png".into()),
            description: None,
            mention: None,
        }
    }

    /// Drive the home chain by hand: `submit` -> command -> `SessionReady`
    /// (what `main` emits) -> `Prompt`. Returns the files the final command
    /// carries.
    fn home_chain(files: Vec<FileInput>) -> Vec<FileInput> {
        let (mut app, mut rx) = app(Route::Home, None);
        app.submit(Submission {
            text: "look".into(),
            files: files.clone(),
        });
        let AppCommand::CreateSessionAndPrompt(submission) =
            rx.try_recv().expect("creation command")
        else {
            panic!("expected CreateSessionAndPrompt");
        };
        assert_eq!(submission.files, files);
        app.handle(AppEvent::SessionReady {
            session: "ses_new".into(),
            prompt: submission.text,
            files: submission.files,
        });
        match rx.try_recv().expect("prompt must follow") {
            AppCommand::Prompt {
                session,
                text,
                files,
            } => {
                assert_eq!((session.as_str(), text.as_str()), ("ses_new", "look"));
                files
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn home_submit_carries_the_same_files_through_session_creation() {
        assert_eq!(home_chain(vec![fake_file()]), vec![fake_file()]);
        assert!(home_chain(Vec::new()).is_empty());
    }

    #[test]
    fn session_submit_sends_the_files_with_the_prompt() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.submit(Submission {
            text: "see".into(),
            files: vec![fake_file()],
        });
        match rx.try_recv().expect("prompt command") {
            AppCommand::Prompt { files, .. } => assert_eq!(files, vec![fake_file()]),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn session_failure_with_files_restores_the_text_and_says_attachments_were_dropped() {
        let (mut app, _rx) = app(Route::Home, None);
        app.submit(Submission {
            text: "keep me".into(),
            files: vec![fake_file()],
        });
        app.handle(AppEvent::SessionFailed("server down".into()));
        assert_eq!(app.prompt.text(), "keep me");
        let toast = app.toast.as_ref().expect("the failure must surface");
        assert!(toast.message.contains("server down"), "{}", toast.message);
        assert!(
            toast.message.contains("attachments were dropped"),
            "{}",
            toast.message
        );
    }

    #[test]
    fn session_failure_restores_the_draft_and_shows_an_error() {
        let (mut app, _rx) = app(Route::Home, None);
        type_text(&mut app, "keep me");
        key(&mut app, KeyCode::Enter);
        app.handle(AppEvent::SessionFailed("server down".into()));
        assert_eq!(app.route, Route::Home);
        assert_eq!(app.prompt.text(), "keep me");
        let toast = app.toast.as_ref().expect("the failure must surface");
        assert_eq!(toast.variant, toast::Variant::Error);
        assert_eq!(toast.title.as_deref(), Some("Creating a session failed"));
        assert_eq!(toast.message, "server down");
        assert!(app.pending_home_prompt.is_none());
    }

    #[test]
    fn prompt_failure_surfaces_as_a_toast() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.dirty = false;
        app.handle(AppEvent::PromptFailed("503 unavailable".into()));
        let toast = app.toast.as_ref().expect("the failure must surface");
        assert_eq!(toast.title.as_deref(), Some("Failed to send prompt"));
        assert_eq!(toast.message, "503 unavailable");
        assert!(app.dirty);
    }

    #[test]
    fn a_newer_toast_replaces_the_current_one() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::PromptFailed("first".into()));
        app.handle(AppEvent::PromptFailed("second".into()));
        assert_eq!(
            app.toast.as_ref().map(|t| t.message.as_str()),
            Some("second")
        );
    }

    #[test]
    fn the_toast_clears_only_after_its_deadline() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        let t0 = Instant::now();
        app.show_toast(Toast::error("Failed", "boom", t0));
        app.dirty = false;
        app.expire_toast(t0 + toast::DEFAULT_DURATION - Duration::from_millis(1));
        assert!(app.toast.is_some());
        assert!(!app.dirty, "a live toast needs no extra frame");
        app.expire_toast(t0 + toast::DEFAULT_DURATION);
        assert!(app.toast.is_none());
        assert!(app.dirty, "clearing the toast must repaint");
    }

    #[test]
    fn ctrl_c_on_home_quits_when_the_draft_is_empty() {
        let (mut app, _rx) = app(Route::Home, None);
        ctrl(&mut app, 'c');
        assert!(app.view.should_quit);
    }

    #[test]
    fn ctrl_c_with_a_draft_clears_it_instead_of_quitting() {
        let (mut app, _rx) = app(Route::Home, None);
        type_text(&mut app, "keep me");
        ctrl(&mut app, 'c');
        assert!(!app.view.should_quit);
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn ctrl_c_while_running_interrupts_instead_of_quitting() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_live"));
        app.store.running_since = Some(0); // a turn is in flight
        ctrl(&mut app, 'c');
        assert!(!app.view.should_quit);
        match rx.try_recv().expect("interrupt must be sent") {
            AppCommand::Interrupt { session } => assert_eq!(session, "ses_live"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn home_submit_is_suppressed_while_a_creation_is_in_flight() {
        let (mut app, mut rx) = app(Route::Home, None);
        type_text(&mut app, "first");
        key(&mut app, KeyCode::Enter);
        let _ = rx.try_recv(); // CreateSessionAndPrompt("first")
        key(&mut app, KeyCode::Char('x')); // typing continues
        key(&mut app, KeyCode::Enter); // suppressed
        assert!(rx.try_recv().is_err(), "no second creation command");
        assert_eq!(
            app.pending_home_prompt.as_ref().map(|p| p.text.as_str()),
            Some("first")
        );
        assert_eq!(app.prompt.text(), "x");
    }

    #[test]
    fn slash_opens_the_command_menu_on_home() {
        let (mut app, _rx) = app(Route::Home, None);
        key(&mut app, KeyCode::Char('/'));
        assert!(matches!(app.mode, super::Mode::Compose));
        assert!(app.prompt.autocomplete_visible());
        assert_eq!(app.prompt.text(), "/");
    }

    #[test]
    fn slash_opens_the_command_menu_instead_of_the_search() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        key(&mut app, KeyCode::Char('/'));
        assert!(matches!(app.mode, super::Mode::Compose));
        assert!(app.prompt.autocomplete_visible());
    }

    #[test]
    fn ctrl_f_opens_the_transcript_search() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut app, 'f');
        assert!(matches!(app.mode, super::Mode::SearchInput(_)));
    }

    #[test]
    fn ctrl_f_does_nothing_on_home() {
        let (mut app, _rx) = app(Route::Home, None);
        ctrl(&mut app, 'f');
        assert!(matches!(app.mode, super::Mode::Compose));
    }

    #[test]
    fn select_exit_command_quits_without_sending_a_prompt() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "/exit");
        key(&mut app, KeyCode::Enter);
        assert!(app.view.should_quit);
        assert!(rx.try_recv().is_err(), "a command is not a prompt");
    }

    #[test]
    fn select_new_command_returns_to_home() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "/new");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.route, Route::Home);
        assert!(app.session.is_none());
        assert!(app.store.items.is_empty());
        assert!(app.prompt.is_empty());
        assert_eq!(
            app.prompt
                .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            crate::ui::prompt::PromptAction::Ignored,
            "an empty prompt does not submit"
        );
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [AppCommand::DetachSession]
        ));
    }

    #[test]
    fn menu_enter_never_submits_the_typed_command_as_a_message() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "/new");
        key(&mut app, KeyCode::Enter);
        let sent = drain(&mut rx);
        assert!(
            sent.iter().all(|c| matches!(c, AppCommand::DetachSession)),
            "{sent:?}"
        );
    }

    fn model_info(provider: &str, id: &str) -> ox_client::rest::ModelInfo {
        ox_client::rest::ModelInfo {
            model_id: id.into(),
            provider_id: provider.into(),
            name: id.into(),
            enabled: true,
            limit: Default::default(),
        }
    }

    fn agent_ref(id: &str) -> ox_client::rest::AgentRef {
        ox_client::rest::AgentRef {
            id: id.into(),
            name: id.into(),
            mode: Some("primary".into()),
            hidden: false,
        }
    }

    fn open_models(app: &mut App) {
        app.handle(AppEvent::Models {
            models: vec![model_info("acme", "k3"), model_info("acme", "k4")],
            providers: vec![ox_client::rest::ProviderRef {
                id: "acme".into(),
                name: "Acme".into(),
            }],
        });
    }

    #[test]
    fn models_and_agents_commands_ask_the_executor_for_their_lists() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "/models");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(rx.try_recv(), Ok(AppCommand::ModelList)));
        type_text(&mut app, "/agents");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(
            rx.try_recv(),
            Ok(AppCommand::AgentList(
                crate::app::command::AgentIntent::Dialog
            ))
        ));
    }

    #[test]
    fn picking_a_model_sends_a_switch_and_the_ack_updates_the_metadata_line() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        open_models(&mut app);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none(), "the pick closes the dialog");
        let Ok(AppCommand::Switch { session, switch }) = rx.try_recv() else {
            panic!("a pick must send a switch");
        };
        assert_eq!(session, "ses_test");
        assert_eq!(
            switch,
            crate::app::command::Switch::Model {
                provider_id: "acme".into(),
                model_id: "k4".into(),
                provider_label: "Acme".into(),
            }
        );
        // Nothing changes until the server confirms.
        assert_eq!(app.prompt.chrome().model, None);
        app.handle(AppEvent::Switched(switch));
        assert_eq!(app.prompt.chrome().model.as_deref(), Some("k4"));
        assert_eq!(app.prompt.chrome().provider.as_deref(), Some("Acme"));
    }

    #[test]
    fn escape_closes_the_model_dialog_without_a_switch() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        open_models(&mut app);
        key(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());
        assert!(rx.try_recv().is_err());
    }

    fn summary(id: &str, title: &str) -> ox_client::rest::SessionSummary {
        ox_client::rest::SessionSummary {
            id: id.into(),
            title: Some(title.into()),
            time: ox_client::rest::SessionTime {
                created: 1_793_448_000_000,
                updated: 1_793_448_000_000,
            },
            location: None,
        }
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<AppCommand>) -> Vec<AppCommand> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn ctrl_key(app: &mut App, ch: char) {
        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn the_sessions_command_and_its_aliases_ask_for_the_list() {
        for name in ["/sessions", "/resume", "/continue"] {
            let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
            type_text(&mut app, name);
            key(&mut app, KeyCode::Enter);
            assert!(
                matches!(rx.try_recv(), Ok(AppCommand::ListSessions)),
                "{name}"
            );
        }
    }

    #[test]
    fn the_fork_command_forks_the_current_session_and_needs_one() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "/fork");
        key(&mut app, KeyCode::Enter);
        match rx.try_recv() {
            Ok(AppCommand::ForkSession { session }) => assert_eq!(session, "ses_test"),
            other => panic!("unexpected: {other:?}"),
        }
        let (mut home, mut home_rx) = self::app(Route::Home, None);
        type_text(&mut home, "/fork");
        key(&mut home, KeyCode::Enter);
        assert!(
            home_rx.try_recv().is_err(),
            "nothing to fork on the home screen"
        );
    }

    #[test]
    fn picking_a_session_resets_the_view_and_follows_the_new_stream() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_old"));
        app.store.push_user("old message");
        app.refresh();
        app.seq_tx.send_replace(Some(41));
        let seq_rx = app.seq_tx.subscribe();
        app.handle(AppEvent::Sessions(vec![
            summary("ses_old", "Old"),
            summary("ses_new", "New"),
        ]));
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);

        assert!(app.dialog.is_none());
        assert_eq!(app.session.as_deref(), Some("ses_new"));
        assert_eq!(app.store.session.as_deref(), Some("ses_new"));
        assert!(app.store.items.is_empty(), "the old transcript is gone");
        assert_eq!(*seq_rx.borrow(), None, "the new stream starts at the tail");
        let commands: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter(|c| !matches!(c, AppCommand::ListForms { .. }))
            .collect();
        assert!(
            matches!(&commands[0], AppCommand::AttachSession { session } if session == "ses_new"),
            "{commands:?}"
        );
        assert!(
            matches!(&commands[1], AppCommand::ReloadHistory { session, .. } if session == "ses_new"),
            "{commands:?}"
        );
    }

    #[test]
    fn history_requested_before_a_switch_is_dropped() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_old"));
        app.request_reload();
        let stale = match rx.try_recv() {
            Ok(AppCommand::ReloadHistory { request_id, .. }) => request_id,
            other => panic!("unexpected: {other:?}"),
        };
        app.switch_session("ses_new".into());
        app.handle(AppEvent::History {
            request_id: stale,
            items: vec![Item::User {
                text: "from the old session".into(),
                created: None,
            }],
        });
        assert!(app.store.items.is_empty(), "stale history must not land");
        let fresh = drain(&mut rx)
            .into_iter()
            .find_map(|command| match command {
                AppCommand::ReloadHistory { request_id, .. } => Some(request_id),
                _ => None,
            })
            .expect("the switch reloads history");
        assert_ne!(fresh, stale);
        app.handle(AppEvent::History {
            request_id: fresh,
            items: vec![Item::User {
                text: "hello".into(),
                created: None,
            }],
        });
        assert_eq!(app.store.items.len(), 1);
    }

    #[test]
    fn picking_the_current_session_only_closes_the_dialog() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_a"));
        app.store.push_user("keep me");
        app.handle(AppEvent::Sessions(vec![summary("ses_a", "A")]));
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(app.store.items.len(), 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_switch_from_home_enters_the_session_route() {
        let (mut app, _rx) = app(Route::Home, None);
        app.switch_session("ses_x".into());
        assert_eq!(app.route, Route::Session);
        assert_eq!(app.session.as_deref(), Some("ses_x"));
    }

    #[test]
    fn ctrl_d_twice_deletes_and_the_answer_removes_the_row() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_a"));
        app.handle(AppEvent::Sessions(vec![
            summary("ses_a", "A"),
            summary("ses_b", "B"),
        ]));
        key(&mut app, KeyCode::Down);
        ctrl_key(&mut app, 'd');
        assert!(rx.try_recv().is_err(), "the first press only arms the row");
        ctrl_key(&mut app, 'd');
        match rx.try_recv() {
            Ok(AppCommand::DeleteSession { session }) => assert_eq!(session, "ses_b"),
            other => panic!("unexpected: {other:?}"),
        }
        app.handle(AppEvent::SessionDeleted("ses_b".into()));
        let Some(Dialog::Select(select)) = app.dialog.as_ref() else {
            panic!("the list stays open");
        };
        assert!(select.item("ses_b").is_none());
        assert!(select.item("ses_a").is_some());
    }

    #[test]
    fn deleting_the_open_session_goes_home() {
        let (mut app, _rx) = app(Route::Session, Some("ses_a"));
        app.handle(AppEvent::SessionDeleted("ses_a".into()));
        assert_eq!(app.route, Route::Home);
        assert!(app.session.is_none());
    }

    #[test]
    fn ctrl_r_renames_through_a_prefilled_input() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_a"));
        app.handle(AppEvent::Sessions(vec![summary("ses_a", "Old")]));
        ctrl_key(&mut app, 'r');
        assert!(matches!(app.dialog, Some(Dialog::Input(_))));
        for _ in 0..3 {
            key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, " Fresh ");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        match rx.try_recv() {
            Ok(AppCommand::RenameSession { session, title }) => {
                assert_eq!(session, "ses_a");
                assert_eq!(title, "Fresh", "the title is trimmed");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn rename_command_opens_a_prompt_prefilled_with_the_session_title() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_a"));
        app.handle(AppEvent::SessionChrome {
            session: "ses_a".into(),
            selection: crate::bootstrap::Selection {
                title: Some("Old title".into()),
                ..Default::default()
            },
        });
        app.run_command(crate::ui::commands::CommandAction::Rename);
        match &app.dialog {
            Some(Dialog::Input(input)) => assert_eq!(input.text(), "Old title"),
            other => panic!("expected the rename prompt, got {other:?}"),
        }
        key(&mut app, KeyCode::Enter);
        assert!(matches!(
            rx.try_recv(),
            Ok(AppCommand::RenameSession { session, title }) if session == "ses_a" && title == "Old title"
        ));
    }

    #[test]
    fn timeline_without_messages_toasts_and_opens_nothing() {
        let (mut app, _rx) = app(Route::Session, Some("ses_a"));
        app.run_command(crate::ui::commands::CommandAction::Timeline);
        assert!(app.dialog.is_none());
        assert!(app.toast.is_some());
    }

    #[test]
    fn timeline_lists_user_messages_and_jumps_on_enter() {
        use crate::ui::timeline::Block;
        let (mut app, _rx) = app(Route::Session, Some("ses_a"));
        app.transcript.set_viewport(40, 5);
        let mut blocks = vec![Block::user("first")];
        for i in 0..12 {
            blocks.push(Block::assistant(
                &format!("a{i}"),
                1,
                "line one\n\nline two",
            ));
        }
        blocks.push(Block::user("last"));
        app.transcript.sync(&blocks, 0);
        app.run_command(crate::ui::commands::CommandAction::Timeline);
        assert!(matches!(app.dialog, Some(Dialog::Select(_))));
        key(&mut app, KeyCode::Down); // newest first: move to "first"
        app.transcript.scroll_to_bottom();
        let before = app.transcript.total_lines();
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert!(!app.transcript.sticky_bottom, "jumping leaves the bottom");
        assert_eq!(app.transcript.total_lines(), before);
    }

    #[test]
    fn an_empty_rename_is_ignored() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_a"));
        app.handle(AppEvent::Sessions(vec![summary("ses_a", "Old")]));
        ctrl_key(&mut app, 'r');
        for _ in 0..3 {
            key(&mut app, KeyCode::Backspace);
        }
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn on_home_a_pick_applies_to_the_chrome_directly() {
        let (mut app, mut rx) = app(Route::Home, None);
        open_models(&mut app);
        key(&mut app, KeyCode::Enter);
        assert!(rx.try_recv().is_err(), "no session, nothing to POST");
        assert_eq!(app.prompt.chrome().model.as_deref(), Some("k3"));
    }

    #[test]
    fn a_failed_switch_shows_an_error_toast_and_keeps_the_chrome() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::SwitchFailed {
            title: "Failed to switch model",
            err: "404 not found".into(),
        });
        let toast = app.toast.as_ref().expect("the failure must surface");
        assert_eq!(toast.variant, toast::Variant::Error);
        assert_eq!(toast.title.as_deref(), Some("Failed to switch model"));
        assert_eq!(app.prompt.chrome().model, None);
    }

    #[test]
    fn tab_cycles_agents_only_when_there_is_more_than_one() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.prompt.set_agent("Build".into());
        key(&mut app, KeyCode::Tab);
        assert!(matches!(
            rx.try_recv(),
            Ok(AppCommand::AgentList(
                crate::app::command::AgentIntent::Cycle(1)
            ))
        ));
        let cycle = crate::app::command::AgentIntent::Cycle(1);

        app.handle(AppEvent::Agents {
            agents: vec![agent_ref("build")],
            intent: cycle,
        });
        assert!(rx.try_recv().is_err(), "one agent: nothing to switch to");

        app.handle(AppEvent::Agents {
            agents: vec![agent_ref("build"), agent_ref("plan")],
            intent: cycle,
        });
        let Ok(AppCommand::Switch { switch, .. }) = rx.try_recv() else {
            panic!("two agents: Tab moves to the next one");
        };
        app.handle(AppEvent::Switched(switch));
        assert_eq!(app.prompt.chrome().agent.as_deref(), Some("Plan"));
    }

    #[test]
    fn tab_still_completes_the_slash_menu() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "/mod");
        key(&mut app, KeyCode::Tab);
        assert!(
            matches!(rx.try_recv(), Ok(AppCommand::ModelList)),
            "Tab runs the menu row instead of cycling the agent"
        );
        assert!(rx.try_recv().is_err(), "no agent cycle on top");
    }

    #[test]
    fn picking_an_agent_in_the_dialog_sends_a_switch() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::Agents {
            agents: vec![agent_ref("build"), agent_ref("plan")],
            intent: crate::app::command::AgentIntent::Dialog,
        });
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let Ok(AppCommand::Switch { switch, .. }) = rx.try_recv() else {
            panic!("a pick must send a switch");
        };
        assert_eq!(
            switch,
            crate::app::command::Switch::Agent {
                id: "plan".into(),
                label: "Plan".into()
            }
        );
    }

    #[test]
    fn a_fork_answer_switches_to_the_new_session() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_a"));
        app.handle(AppEvent::SessionForked("ses_fork".into()));
        assert_eq!(app.session.as_deref(), Some("ses_fork"));
        assert!(app.toast.is_some());
        assert!(
            drain(&mut rx).iter().any(
                |c| matches!(c, AppCommand::AttachSession { session } if session == "ses_fork")
            )
        );
    }

    #[test]
    fn a_failed_session_request_shows_an_error_toast() {
        let (mut app, _rx) = app(Route::Session, Some("ses_a"));
        app.handle(AppEvent::SessionOpFailed {
            title: "Failed to delete session",
            error: "boom".into(),
        });
        let toast = app.toast.as_ref().expect("toast");
        assert_eq!(toast.title.as_deref(), Some("Failed to delete session"));
        assert_eq!(toast.message, "boom");
    }

    // ---- forms -----------------------------------------------------------

    /// A wire `form.created` frame, exactly as the stream delivers it.
    fn form_frame(kind: &str, data: serde_json::Value) -> AppEvent {
        let envelope = serde_json::from_value(serde_json::json!({
            "id": "evt_1", "created": 1, "type": kind, "data": data
        }))
        .unwrap();
        AppEvent::Stream(ox_client::sse::StreamMsg::Event(Box::new(envelope)))
    }

    fn created(id: &str, session: &str) -> AppEvent {
        form_frame(
            "form.created",
            serde_json::json!({"form": {
                "id": id, "sessionID": session, "title": "Pick",
                "fields": [{"type": "string", "key": "stack", "title": "Stack",
                    "options": [{"value": "rust", "label": "Rust"}, {"value": "go", "label": "Go"}]}]
            }}),
        )
    }

    fn open_form_id(app: &App) -> Option<String> {
        match app.dialog.as_ref() {
            Some(crate::ui::dialog::Dialog::Form(form)) => Some(form.id().to_owned()),
            _ => None,
        }
    }

    #[test]
    fn a_form_created_frame_opens_the_form_dialog() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        assert_eq!(open_form_id(&app).as_deref(), Some("frm_1"));
    }

    #[test]
    fn a_form_for_another_session_is_ignored() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_other"));
        assert!(app.dialog.is_none());
    }

    #[test]
    fn enter_answers_the_form_and_the_dialog_does_not_reopen() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        match rx.try_recv().expect("a reply command") {
            AppCommand::ReplyForm {
                session,
                form,
                answer,
            } => {
                assert_eq!((session.as_str(), form.as_str()), ("ses_test", "frm_1"));
                assert_eq!(
                    answer.get("stack"),
                    Some(&ox_client::form::FormValue::String("rust".into()))
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // The server's own frame arrives afterwards and changes nothing.
        app.handle(form_frame(
            "form.replied",
            serde_json::json!({"id": "frm_1", "sessionID": "ses_test"}),
        ));
        assert!(app.dialog.is_none());
    }

    #[test]
    fn esc_cancels_the_form() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        key(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());
        assert!(matches!(
            rx.try_recv().expect("a cancel command"),
            AppCommand::CancelForm { form, .. } if form == "frm_1"
        ));
    }

    #[test]
    fn the_next_pending_form_opens_after_the_first_settles() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        app.handle(created("frm_2", "ses_test"));
        assert_eq!(open_form_id(&app).as_deref(), Some("frm_1"));
        key(&mut app, KeyCode::Esc);
        assert_eq!(open_form_id(&app).as_deref(), Some("frm_2"));
    }

    #[test]
    fn a_form_settled_elsewhere_closes_the_dialog() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        app.handle(form_frame(
            "form.cancelled",
            serde_json::json!({"id": "frm_1", "sessionID": "ses_test"}),
        ));
        assert!(app.dialog.is_none());
    }

    #[test]
    fn reconnecting_refetches_the_forms_and_the_answer_reconciles_the_dialog() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::Stream(ox_client::sse::StreamMsg::Reconnected));
        let mut seen = false;
        while let Ok(command) = rx.try_recv() {
            seen |=
                matches!(command, AppCommand::ListForms { ref session } if session == "ses_test");
        }
        assert!(seen, "a reconnect re-lists the pending forms");

        let form: ox_client::form::FormInfo = serde_json::from_value(serde_json::json!({
            "id": "frm_9", "sessionID": "ses_test", "title": "Missed",
            "fields": [{"type": "boolean", "key": "ok"}]
        }))
        .unwrap();
        app.handle(AppEvent::Forms(vec![form]));
        assert_eq!(open_form_id(&app).as_deref(), Some("frm_9"));
        app.handle(AppEvent::Forms(Vec::new()));
        assert!(app.dialog.is_none(), "a form answered while offline closes");
    }

    #[test]
    fn a_failed_reply_toasts_and_relists() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::FormFailed("server returned 500".into()));
        assert!(app.toast.is_some());
        assert!(matches!(rx.try_recv(), Ok(AppCommand::ListForms { .. })));
    }

    #[test]
    fn ctrl_c_still_quits_over_a_form() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        ctrl(&mut app, 'c');
        assert!(app.view.should_quit);
    }

    // ---- review fixes ------------------------------------------------------

    fn form_info(id: &str, session: &str) -> ox_client::form::FormInfo {
        serde_json::from_value(serde_json::json!({
            "id": id, "sessionID": session, "title": "T",
            "fields": [{"type": "boolean", "key": "ok"}]
        }))
        .unwrap()
    }

    #[test]
    fn switching_session_drops_the_old_form_dialog_and_relists_forms() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_old"));
        app.handle(created("frm_1", "ses_old"));
        assert!(open_form_id(&app).is_some());
        app.dialog_purpose = Some(crate::ui::models::DialogPurpose::Agent(Vec::new()));
        drain(&mut rx);

        app.switch_session("ses_new".into());

        assert!(app.dialog.is_none(), "the old session's form must go");
        assert!(app.dialog_purpose.is_none());
        let commands = drain(&mut rx);
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, AppCommand::ListForms { session } if session == "ses_new")),
            "{commands:?}"
        );
    }

    #[test]
    fn going_home_clears_the_form_dialog_and_detaches_the_stream() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_old"));
        app.handle(created("frm_1", "ses_old"));
        app.seq_tx.send_replace(Some(41));
        let seq_rx = app.seq_tx.subscribe();
        drain(&mut rx);

        app.run_command(crate::ui::commands::CommandAction::New);

        assert_eq!(app.route, Route::Home);
        assert!(app.dialog.is_none(), "the old session's form must go");
        assert_eq!(*seq_rx.borrow(), None, "a stale cursor must not survive");
        let commands = drain(&mut rx);
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, AppCommand::DetachSession)),
            "{commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, AppCommand::ListForms { .. })),
            "there is no session to list forms for"
        );
    }

    #[test]
    fn async_dialog_answers_do_not_replace_an_open_form() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        app.handle(AppEvent::McpServers(Vec::new()));
        app.handle(AppEvent::Sessions(vec![summary("ses_a", "A")]));
        app.handle(AppEvent::Models {
            models: Vec::new(),
            providers: Vec::new(),
        });
        app.handle(AppEvent::Agents {
            agents: Vec::new(),
            intent: crate::app::command::AgentIntent::Dialog,
        });
        assert_eq!(open_form_id(&app).as_deref(), Some("frm_1"));
        assert!(app.dialog_purpose.is_none());
    }

    #[test]
    fn sessions_and_status_answers_reset_the_dialog_purpose() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.dialog_purpose = Some(crate::ui::models::DialogPurpose::Agent(Vec::new()));
        app.handle(AppEvent::McpServers(Vec::new()));
        assert!(app.dialog_purpose.is_none());
        app.dialog_purpose = Some(crate::ui::models::DialogPurpose::Agent(Vec::new()));
        app.handle(AppEvent::Sessions(Vec::new()));
        assert!(app.dialog_purpose.is_none());
    }

    #[test]
    fn a_settled_form_does_not_come_back_from_a_stale_list() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        key(&mut app, KeyCode::Enter); // answer it
        app.handle(AppEvent::Forms(vec![form_info("frm_1", "ses_test")]));
        assert!(app.dialog.is_none(), "the list raced the reply");
        assert!(app.store.pending_forms.is_empty());
        // A form the user never settled still arrives.
        app.handle(AppEvent::Forms(vec![
            form_info("frm_1", "ses_test"),
            form_info("frm_2", "ses_test"),
        ]));
        assert_eq!(open_form_id(&app).as_deref(), Some("frm_2"));
    }

    #[test]
    fn the_settled_list_is_capped_and_cleared_on_a_session_switch() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        for n in 0..(super::SETTLED_FORMS_CAP + 5) {
            app.remember_settled(format!("frm_{n}"));
        }
        assert_eq!(app.settled_forms.len(), super::SETTLED_FORMS_CAP);
        assert!(!app.settled_forms.iter().any(|id| id == "frm_0"));
        app.switch_session("ses_other".into());
        assert!(app.settled_forms.is_empty());
    }

    #[test]
    fn a_failed_reply_lets_the_relisted_form_return() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.handle(created("frm_1", "ses_test"));
        key(&mut app, KeyCode::Enter);
        app.handle(AppEvent::FormFailed("500".into()));
        app.handle(AppEvent::Forms(vec![form_info("frm_1", "ses_test")]));
        assert_eq!(open_form_id(&app).as_deref(), Some("frm_1"));
    }

    #[test]
    fn the_context_figure_gains_a_percent_once_the_window_is_known() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(form_frame(
            "session.step.started",
            serde_json::json!({"sessionID":"ses_test","assistantMessageID":"msg_1","agent":"build",
                "model":{"id":"k3","providerID":"acme"},"started":1}),
        ));
        app.handle(form_frame(
            "session.step.ended",
            serde_json::json!({"sessionID":"ses_test","assistantMessageID":"msg_1","finish":"stop",
                "cost":0.0,"tokens":{"input":10_000,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}),
        ));
        app.flush_store();
        assert_eq!(app.prompt.ctx_usage(), Some("10.0K"));
        let mut asked = 0;
        while let Ok(command) = rx.try_recv() {
            asked += usize::from(matches!(command, AppCommand::LoadContextLimits));
        }
        assert_eq!(asked, 1, "the catalogue is requested once");

        app.handle(AppEvent::ContextLimits(
            [("acme/k3".to_owned(), 200_000)].into(),
        ));
        assert_eq!(app.prompt.ctx_usage(), Some("10.0K (5%)"));
    }

    #[test]
    fn stream_events_only_mark_the_store_dirty_until_flushed() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        app.store_dirty = false;
        app.handle(form_frame(
            "session.reasoning.started",
            serde_json::json!({"sessionID":"ses_test","assistantMessageID":"msg_1","ordinal":0}),
        ));
        assert!(app.store_dirty, "the delta is reduced but not re-rendered");
        app.flush_store();
        assert!(!app.store_dirty);
        assert!(app.dirty);
        app.dirty = false;
        app.flush_store();
        assert!(!app.dirty, "a clean store costs nothing");
    }

    fn press_at(app: &mut App, ch: char, ctrl: bool, now: Instant) {
        let mods = if ctrl {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        app.handle_key_at(KeyEvent::new(KeyCode::Char(ch), mods), now);
    }

    fn palette_open(app: &App) -> bool {
        app.dialog.as_ref().and_then(Dialog::tag) == Some(crate::ui::palette::TAG)
    }

    #[test]
    fn ctrl_p_opens_the_command_palette() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut app, 'p');
        assert!(palette_open(&app));
    }

    #[test]
    fn selecting_a_palette_row_runs_that_command() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        ctrl(&mut app, 'p');
        type_text(&mut app, "switch model");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none(), "the palette closes before it runs");
        assert!(matches!(drain(&mut rx).as_slice(), [AppCommand::ModelList]));
    }

    #[test]
    fn selecting_exit_in_the_palette_quits() {
        let (mut app, _rx) = app(Route::Home, None);
        ctrl(&mut app, 'p');
        type_text(&mut app, "exit");
        key(&mut app, KeyCode::Enter);
        assert!(app.view.should_quit);
    }

    #[test]
    fn leader_sequences_run_their_commands() {
        let cases = [
            ('l', "sessions"),
            ('m', "models"),
            ('a', "agents"),
            ('s', "status"),
        ];
        for (follow, what) in cases {
            let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
            let t = Instant::now();
            press_at(&mut app, 'x', true, t);
            assert!(rx.try_recv().is_err(), "the leader alone does nothing");
            press_at(&mut app, follow, false, t);
            let sent = drain(&mut rx);
            let ok = match follow {
                'l' => matches!(sent.as_slice(), [AppCommand::ListSessions]),
                'm' => matches!(sent.as_slice(), [AppCommand::ModelList]),
                'a' => matches!(sent.as_slice(), [AppCommand::AgentList(_)]),
                _ => matches!(sent.as_slice(), [AppCommand::McpList]),
            };
            assert!(ok, "{what}: {sent:?}");
        }
    }

    #[test]
    fn leader_n_goes_home_and_leader_q_quits() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        let t = Instant::now();
        press_at(&mut app, 'x', true, t);
        press_at(&mut app, 'n', false, t);
        assert_eq!(app.route, Route::Home);
        press_at(&mut app, 'x', true, t);
        press_at(&mut app, 'q', false, t);
        assert!(app.view.should_quit);
    }

    #[test]
    fn an_expired_leader_lets_the_follow_up_key_through_as_text() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        let t = Instant::now();
        press_at(&mut app, 'x', true, t);
        press_at(&mut app, 'l', false, t + Duration::from_secs(3));
        assert!(rx.try_recv().is_err());
        assert_eq!(app.prompt.text(), "l");
    }

    #[test]
    fn an_unbound_follow_up_cancels_the_leader_and_is_swallowed() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        let t = Instant::now();
        press_at(&mut app, 'x', true, t);
        press_at(&mut app, 'z', false, t);
        press_at(&mut app, 'l', false, t);
        assert!(rx.try_recv().is_err(), "the leader is gone");
        assert_eq!(app.prompt.text(), "l", "only the later key is typed");
    }

    #[test]
    fn the_leader_is_ignored_inside_a_dialog() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.handle(AppEvent::McpServers(Vec::new()));
        let t = Instant::now();
        press_at(&mut app, 'x', true, t);
        key(&mut app, KeyCode::Esc);
        press_at(&mut app, 'l', false, t);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ctrl_p_navigates_the_autocomplete_menu_instead_of_the_palette() {
        let (mut app, _rx) = app(Route::Session, Some("ses_test"));
        type_text(&mut app, "/");
        assert!(app.prompt.autocomplete_visible());
        ctrl(&mut app, 'p');
        assert!(!palette_open(&app));
    }

    #[test]
    fn switching_session_reloads_the_prompt_chrome() {
        let (mut app, mut rx) = app(Route::Session, Some("ses_a"));
        app.switch_session("ses_b".into());
        let mut asked = false;
        while let Ok(command) = rx.try_recv() {
            asked |= matches!(command, AppCommand::LoadSessionChrome { ref session } if session == "ses_b");
        }
        assert!(asked, "the new session's agent/model must be fetched");
    }

    #[test]
    fn a_session_chrome_answer_updates_the_metadata_row_unless_it_is_stale() {
        let (mut app, _rx) = app(Route::Session, Some("ses_b"));
        let selection = |model: &str| crate::bootstrap::Selection {
            agent: Some("Plan".into()),
            model: Some(model.into()),
            provider: Some("Cardinal".into()),
            variant: Some("high".into()),
            title: None,
        };
        app.handle(AppEvent::SessionChrome {
            session: "ses_b".into(),
            selection: selection("k3"),
        });
        assert_eq!(app.prompt.chrome().model.as_deref(), Some("k3"));
        assert_eq!(app.prompt.chrome().variant.as_deref(), Some("high"));

        app.handle(AppEvent::SessionChrome {
            session: "ses_old".into(),
            selection: selection("stale"),
        });
        assert_eq!(app.prompt.chrome().model.as_deref(), Some("k3"));
    }

    fn pointer_app(texts: &[&str]) -> (App, mpsc::UnboundedReceiver<AppCommand>) {
        use crate::ui::timeline::{Block, Transcript};
        let (mut app, rx) = app(Route::Session, Some("ses_test"));
        app.transcript = Transcript::new(40, 10);
        let blocks: Vec<Block> = texts.iter().map(|t| Block::user(t)).collect();
        app.transcript.sync(&blocks, 0);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| app.transcript.render(frame, frame.area()))
            .unwrap();
        (app, rx)
    }

    fn drag_over(app: &mut App, from: (u16, u16), to: (u16, u16)) {
        app.handle(AppEvent::MouseDown {
            x: from.0,
            y: from.1,
        });
        app.handle(AppEvent::MouseDrag { x: to.0, y: to.1 });
        app.handle(AppEvent::MouseUp { x: to.0, y: to.1 });
    }

    fn copy_texts(commands: &[AppCommand]) -> Vec<&str> {
        commands
            .iter()
            .filter_map(|c| match c {
                AppCommand::WriteClipboard(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn toast_of(app: &App) -> &Toast {
        app.toast.as_ref().expect("a toast")
    }

    #[test]
    fn a_copy_sends_one_write_command_and_queues_the_osc_sequence() {
        let (mut app, mut rx) = pointer_app(&["hello world"]);
        drag_over(&mut app, (3, 2), (7, 2));
        let commands = drain(&mut rx);
        assert_eq!(copy_texts(&commands), ["hello"]);
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert_eq!(app.pending_osc52.as_deref(), Some("\x1b]52;c;aGVsbG8=\x07"));
    }

    #[test]
    fn the_pending_osc_sequence_is_written_once_and_cleared() {
        let mut pending = Some("\x1b]52;c;aGk=\x07".to_owned());
        let mut out: Vec<u8> = Vec::new();
        super::flush_osc52(&mut pending, &mut out).unwrap();
        assert_eq!(out, b"\x1b]52;c;aGk=\x07");
        assert_eq!(pending, None);
        super::flush_osc52(&mut pending, &mut out).unwrap();
        assert_eq!(out, b"\x1b]52;c;aGk=\x07", "nothing more to write");
    }

    #[test]
    fn a_host_write_shows_the_copied_toast() {
        let (mut app, _rx) = pointer_app(&["hello world"]);
        app.dirty = false;
        app.handle(AppEvent::ClipboardWritten { host_ok: true });
        assert_eq!(toast_of(&app).variant, toast::Variant::Info);
        assert_eq!(toast_of(&app).message, "Copied to clipboard");
        assert!(app.dirty);
    }

    #[test]
    fn a_failed_host_write_is_fine_when_the_osc_sequence_went_out() {
        let (mut app, _rx) = pointer_app(&["hello world"]);
        drag_over(&mut app, (3, 2), (7, 2));
        app.handle(AppEvent::ClipboardWritten { host_ok: false });
        assert_eq!(toast_of(&app).variant, toast::Variant::Info);
        assert_eq!(toast_of(&app).message, "Copied to clipboard");
    }

    #[test]
    fn a_failed_host_write_without_an_osc_sequence_warns() {
        let (mut app, _rx) = pointer_app(&["hello world"]);
        app.handle(AppEvent::ClipboardWritten { host_ok: false });
        assert_eq!(toast_of(&app).variant, toast::Variant::Warning);
    }

    #[test]
    fn dragging_over_text_hands_over_the_selected_text_and_repaints() {
        let (mut app, mut rx) = pointer_app(&["hello world"]);
        app.dirty = false;
        drag_over(&mut app, (3, 2), (7, 2));
        assert_eq!(copy_texts(&drain(&mut rx)), ["hello"]);
        assert!(app.dirty);
    }

    #[test]
    fn a_press_and_release_in_place_is_still_a_click() {
        use crate::ui::timeline::{Block, Explored, ExploredCall, Transcript};
        use ox_client::store::ToolState;
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.transcript = Transcript::new(40, 10);
        app.transcript.sync(
            &[Block::Explored(Explored {
                reads: 1,
                group_id: "a".into(),
                calls: vec![ExploredCall {
                    id: "a".into(),
                    version: 1,
                    tool: "read".into(),
                    state: ToolState::Success,
                    summary: "a.txt".into(),
                }],
                ..Explored::default()
            })],
            0,
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| app.transcript.render(frame, frame.area()))
            .unwrap();
        let rows = app.transcript.total_lines();
        app.dirty = false;
        app.handle(AppEvent::MouseDown { x: 6, y: 1 });
        app.handle(AppEvent::MouseUp { x: 6, y: 1 });
        assert!(app.dirty, "the click expanded the group");
        assert!(app.transcript.total_lines() > rows);
        assert!(copy_texts(&drain(&mut rx)).is_empty());
    }

    #[test]
    fn a_double_click_on_a_toggle_row_toggles_once_and_selects_instead() {
        use crate::ui::timeline::{Block, Explored, ExploredCall, Transcript};
        use ox_client::store::ToolState;
        let (mut app, mut rx) = app(Route::Session, Some("ses_test"));
        app.transcript = Transcript::new(40, 10);
        app.transcript.sync(
            &[Block::Explored(Explored {
                reads: 1,
                group_id: "a".into(),
                calls: vec![ExploredCall {
                    id: "a".into(),
                    version: 1,
                    tool: "read".into(),
                    state: ToolState::Success,
                    summary: "a.txt".into(),
                }],
                ..Explored::default()
            })],
            0,
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| app.transcript.render(frame, frame.area()))
            .unwrap();
        let collapsed = app.transcript.total_lines();
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        // First click: an ordinary click, the group opens.
        app.handle_mouse_down_at(6, 1, t0);
        app.handle_mouse_up(6, 1);
        let open = app.transcript.total_lines();
        assert!(open > collapsed);

        // Second click within the window selects a word and does not toggle
        // again (upstream click handlers skip when text is selected).
        app.handle_mouse_down_at(6, 1, t0 + ms(100));
        app.handle_mouse_up(6, 1);
        assert_eq!(app.transcript.total_lines(), open, "still open");
        assert!(app.transcript.has_selection(), "the word is highlighted");

        // Third click selects the line, still no toggle.
        app.handle_mouse_down_at(6, 1, t0 + ms(200));
        app.handle_mouse_up(6, 1);
        assert_eq!(app.transcript.total_lines(), open, "still open");

        // After the window lapses a click is a plain click again.
        app.handle_mouse_down_at(6, 1, t0 + ms(900));
        app.handle_mouse_up(6, 1);
        assert_eq!(app.transcript.total_lines(), collapsed, "closed again");
        drain(&mut rx);
    }

    #[test]
    fn a_double_click_copies_the_word() {
        let (mut app, mut rx) = pointer_app(&["hello world"]);
        let t0 = Instant::now();
        app.handle_mouse_down_at(10, 2, t0);
        app.handle_mouse_up(10, 2);
        assert!(
            copy_texts(&drain(&mut rx)).is_empty(),
            "one click copies nothing"
        );
        app.handle_mouse_down_at(10, 2, t0 + Duration::from_millis(120));
        app.handle_mouse_up(10, 2);
        assert_eq!(copy_texts(&drain(&mut rx)), ["world"]);
        app.handle_mouse_down_at(10, 2, t0 + Duration::from_millis(240));
        app.handle_mouse_up(10, 2);
        assert_eq!(copy_texts(&drain(&mut rx)), ["hello world"]);
    }

    #[test]
    fn a_modal_swallows_the_pointer() {
        let (mut app, mut rx) = pointer_app(&["hello world"]);
        app.route = Route::Home;
        drag_over(&mut app, (3, 2), (7, 2));
        assert!(copy_texts(&drain(&mut rx)).is_empty());
    }

    #[test]
    fn esc_clears_a_selection_and_is_swallowed_other_keys_clear_and_carry_on() {
        let (mut app, _rx) = pointer_app(&["hello world"]);
        drag_over(&mut app, (3, 2), (7, 2));
        assert!(app.transcript.has_selection());
        app.dirty = false;
        key(&mut app, KeyCode::Esc);
        assert!(!app.transcript.has_selection());
        assert!(app.dirty);

        drag_over(&mut app, (3, 2), (7, 2));
        ctrl(&mut app, 'c');
        assert!(!app.transcript.has_selection());
        assert!(
            app.view.should_quit,
            "ctrl+c clears the selection and then does its normal job"
        );

        drag_over(&mut app, (3, 2), (7, 2));
        key(&mut app, KeyCode::Char('x'));
        assert!(!app.transcript.has_selection());
        assert_eq!(app.prompt.text(), "x", "the key still reached the prompt");
    }

    #[test]
    fn the_wheel_moves_the_focus_of_a_drag_in_progress() {
        let (mut app, _rx) = pointer_app(&["aaa", "bbb", "ccc", "ddd"]);
        app.handle(AppEvent::MouseDown { x: 3, y: 2 });
        app.handle(AppEvent::MouseDrag { x: 5, y: 7 });
        let before = app.transcript.selection_focus();
        app.handle(AppEvent::Wheel(-1));
        assert_ne!(app.transcript.selection_focus(), before);
    }

    #[test]
    fn a_modal_opening_mid_drag_drops_the_selection() {
        let (mut app, mut rx) = pointer_app(&["hello world"]);
        app.handle(AppEvent::MouseDown { x: 3, y: 2 });
        app.handle(AppEvent::MouseDrag { x: 7, y: 2 });
        assert!(app.transcript.has_selection());
        app.mode = crate::app::Mode::SearchInput(Box::default());
        app.dirty = false;
        app.handle(AppEvent::MouseDrag { x: 8, y: 2 });
        assert!(!app.transcript.has_selection());
        assert!(app.dirty, "the highlight went away, repaint");
        app.mode = crate::app::Mode::Browse;
        app.handle(AppEvent::MouseUp { x: 8, y: 2 });
        assert!(
            copy_texts(&drain(&mut rx)).is_empty(),
            "the gesture is over, nothing is copied"
        );

        // The same through a dialog, ended by the release.
        app.handle(AppEvent::MouseDown { x: 3, y: 2 });
        app.handle(AppEvent::MouseDrag { x: 7, y: 2 });
        app.dialog = Some(Dialog::Select(crate::ui::palette::select(
            crate::ui::palette::Context { in_session: true },
            crate::app::keymap::hint,
        )));
        app.handle(AppEvent::MouseUp { x: 7, y: 2 });
        assert!(!app.transcript.has_selection());
        assert!(copy_texts(&drain(&mut rx)).is_empty());
    }

    #[test]
    fn esc_without_a_selection_is_not_swallowed() {
        let (mut app, _rx) = pointer_app(&["hello world"]);
        assert!(!app.transcript.has_selection());
        app.dialog = Some(Dialog::Select(crate::ui::palette::select(
            crate::ui::palette::Context { in_session: true },
            crate::app::keymap::hint,
        )));
        key(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none(), "esc reached the dialog and closed it");
    }
}
