mod app;
mod bootstrap;
mod pipeline;
mod terminal;
mod ui;

use tokio::sync::{mpsc, watch};

use app::App;

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
        if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = arg("--server").unwrap_or_else(|| "http://127.0.0.1:14090".into());
    let directory = arg("--directory")
        .unwrap_or_else(|| std::env::current_dir().unwrap().display().to_string());
    let session = arg("--session");

    // 事件线程：阻塞 read 放独立线程。
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let key_tx = event_tx.clone();
    std::thread::spawn(move || loop {
        match crossterm::event::read() {
            Ok(crossterm::event::Event::Key(key)) => {
                if key_tx.send(app::event::AppEvent::Key(key)).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    let mut guard = terminal::TerminalGuard::new()?;
    terminal::install_panic_hook();

    let demo = std::env::var("OX_TUI_DEMO").ok();

    // demo 模式完全离线：不连 server，会话固定为 ses_demo 以匹配 fixtures。
    let (bootstrap, online) = match demo {
        Some(_) => (
            bootstrap::Bootstrap {
                client: ox_client::Client::new("http://127.0.0.1:1", directory.clone()),
                session: "ses_demo".to_string(),
            },
            false,
        ),
        None => (
            bootstrap::connect(&server, &directory, session).await?,
            true,
        ),
    };

    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (seq_tx, seq_rx) = watch::channel(None);

    if online {
        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
        ox_client::sse::spawn_events(
            bootstrap.client.clone(),
            bootstrap.session.clone(),
            seq_rx,
            stream_tx,
        );

        // 命令执行任务：把 AppCommand 变成网络动作，再把结果送回 UI 事件循环。
        {
            let client = bootstrap.client.clone();
            let session = bootstrap.session.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                while let Some(command) = command_rx.recv().await {
                    match command {
                        app::command::AppCommand::Prompt(text) => {
                            if let Err(err) = client.prompt(&session, &text).await {
                                tracing::warn!(%err, "prompt failed");
                            }
                        }
                        app::command::AppCommand::Interrupt => {
                            let _ = client.interrupt(&session).await;
                        }
                        app::command::AppCommand::Quit => break,
                        app::command::AppCommand::ReloadHistory { request_id } => {
                            match client.messages(&session, None).await {
                                Ok(page) => {
                                    let mut messages = page.data;
                                    messages.reverse(); // desc → chronological
                                    let items = ox_client::history::parse_messages(&messages);
                                    let _ = tx.send(app::event::AppEvent::History {
                                        request_id,
                                        items,
                                    });
                                }
                                Err(err) => tracing::warn!(%err, "history reload failed"),
                            }
                        }
                    }
                }
            });
        }

        // 把 SSE 流桥接成 AppEvent。
        {
            let tx = event_tx.clone();
            tokio::spawn(async move {
                while let Some(msg) = stream_rx.recv().await {
                    if tx.send(app::event::AppEvent::Stream(msg)).is_err() {
                        break;
                    }
                }
            });
        }
    } else {
        drop(command_rx);
        drop(seq_rx);
    }

    let mut app = App::new(bootstrap.session, command_tx, seq_tx);
    app.run(&mut guard, event_rx).await
}
