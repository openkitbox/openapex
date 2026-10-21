pub mod command;
pub mod event;
pub mod state;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use ox_client::sse::StreamMsg;
use ox_client::store::{DataStore, Item};
use tokio::sync::{mpsc, watch};

use crate::pipeline;
use crate::terminal::TerminalGuard;
use crate::ui::composer::Composer;
use crate::ui::dock;
use crate::ui::layout;
use crate::ui::timeline::Transcript;
use command::AppCommand;
use event::AppEvent;
use state::ViewState;

pub const RENDER_TICK: Duration = Duration::from_millis(33);
const SCROLL_PAGE: usize = 10;

pub struct App {
    pub view: ViewState,
    pub composer: Composer,
    pub store: DataStore,
    pub transcript: Transcript,
    pub commands: mpsc::UnboundedSender<AppCommand>,
    session: String,
    seq_tx: watch::Sender<Option<i64>>,
    next_reload_id: u64,
    pending_reload: Option<u64>,
    dirty: bool,
}

impl App {
    pub fn new(
        session: String,
        commands: mpsc::UnboundedSender<AppCommand>,
        seq_tx: watch::Sender<Option<i64>>,
    ) -> Self {
        // DataStore has a private field, so FRU (`DataStore { ..Default::default() }`)
        // is E0451; assign the pub fields on a default instance instead.
        let mut store = DataStore::default();
        store.session = Some(session.clone());
        Self {
            view: ViewState::new(),
            composer: Composer::new(),
            store,
            transcript: Transcript::new(80, 20),
            commands,
            session,
            seq_tx,
            next_reload_id: 0,
            pending_reload: None,
            dirty: true,
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Quit => self.view.should_quit = true,
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Stream(msg) => self.handle_stream(msg),
            AppEvent::History { request_id, items } => self.handle_history(request_id, items),
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
        let request_id = self.next_reload_id;
        self.next_reload_id += 1;
        self.pending_reload = Some(request_id);
        let _ = self.commands.send(AppCommand::ReloadHistory { request_id });
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.store.running_since.is_some() {
                let _ = self.commands.send(AppCommand::Interrupt);
            } else {
                self.view.should_quit = true;
            }
            return;
        }
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
        if let Some(text) = self.composer.handle_key(key) {
            // 本地乐观显示：随后一帧就有用户消息；服务端确认后由历史重拉覆盖。
            self.store.push_user(&text);
            self.refresh();
            let _ = self.commands.send(AppCommand::Prompt(text));
        }
    }

    fn handle_stream(&mut self, msg: StreamMsg) {
        match msg {
            StreamMsg::Reconnected => {
                // ephemeral deltas are not replayed → refetch history.
                self.request_reload();
            }
            StreamMsg::Event(envelope) => {
                let typed = envelope.parse_typed();
                if typed.data.session_id() != Some(self.session.as_str()) {
                    return; // 全局流里过滤出本会话
                }
                ox_client::store::reduce(&mut self.store, &typed);
                self.seq_tx.send_replace(self.store.last_seq);
                self.refresh();
            }
        }
    }

    /// Store → blocks → transcript. Only the changed tail is re-wrapped; dock
    /// reads the store directly (no duplicated view state).
    pub fn refresh(&mut self) {
        let changed_from = self.store.claim_changed_from();
        let blocks = pipeline::blocks_from_store(&self.store);
        self.transcript.sync(&blocks, changed_from);
        self.dirty = true;
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

        self.request_reload();

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Some(frames) = script.as_mut() {
                        if let Some(raw) = frames.next() {
                            if let Ok(envelope) = serde_json::from_value::<ox_schema::event::EventEnvelope<serde_json::Value>>(raw) {
                                self.handle_stream(StreamMsg::Event(Box::new(envelope)));
                            }
                        }
                    }
                    if self.dirty {
                        self.dirty = false;
                        terminal.terminal.draw(|frame| {
                            let composer_height = self.composer.height();
                            let (timeline_area, composer_area, dock_area) =
                                layout::split(frame.area(), composer_height);
                            self.composer.render(frame, composer_area);
                            self.transcript.render(frame, timeline_area);
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            let line = dock::chips(&self.store, now_ms, dock_area.width);
                            frame.render_widget(ratatui::widgets::Paragraph::new(line), dock_area);
                        })?;
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
