mod app;
mod terminal;
mod ui;

use tokio::sync::mpsc;

use app::App;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // crossterm's blocking read goes on its own thread so it never blocks the runtime.
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => {
                    if event_tx.send(app::event::AppEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(_) => {} // resize/paste handled in later tasks
                Err(_) => break,
            }
        }
    });

    let mut guard = terminal::TerminalGuard::new()?;
    terminal::install_panic_hook();

    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let mut app = App::new(command_tx);
    app.run(&mut guard, event_rx).await
}
