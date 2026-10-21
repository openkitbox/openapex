pub mod command;
pub mod event;
pub mod state;

use std::time::Duration;

use tokio::sync::mpsc;

use crate::terminal::TerminalGuard;
use command::AppCommand;
use event::AppEvent;
use state::ViewState;

pub const RENDER_TICK: Duration = Duration::from_millis(33);

pub struct App {
    pub view: ViewState,
    pub commands: mpsc::UnboundedSender<AppCommand>,
}

impl App {
    pub fn new(commands: mpsc::UnboundedSender<AppCommand>) -> Self {
        Self {
            view: ViewState::new(),
            commands,
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Quit => self.view.should_quit = true,
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Stream(msg) => self.handle_stream(msg),
            AppEvent::History { .. } => {} // Task 12 wires the DataStore
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Task 12 turns this into: interrupt when running, quit when idle.
            self.view.should_quit = true;
        }
    }

    fn handle_stream(&mut self, _msg: ox_client::sse::StreamMsg) {
        // Task 12 wires this into DataStore + Transcript.
    }

    pub async fn run(
        &mut self,
        terminal: &mut TerminalGuard,
        mut events: mpsc::UnboundedReceiver<AppEvent>,
    ) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(RENDER_TICK);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    terminal.terminal.draw(|frame| {
                        let area = frame.area();
                        let _ = area; // Tasks 9/10 render here
                    })?;
                }
                Some(event) = events.recv() => {
                    self.handle(event);
                }
            }
            if self.view.should_quit {
                return Ok(());
            }
        }
    }
}
