use ox_tui::{app, bootstrap, terminal, ui};

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
    let terminal_tx = event_tx.clone();
    std::thread::spawn(move || {
        while let Ok(raw) = crossterm::event::read() {
            let Some(event) = app::event::AppEvent::from_terminal(raw) else {
                continue;
            };
            if terminal_tx.send(event).is_err() {
                break;
            }
        }
    });

    let mut guard = terminal::TerminalGuard::new()?;
    terminal::install_panic_hook();

    let demo = std::env::var("OX_TUI_DEMO").ok();

    let route;
    let online;
    let mut home_chrome = ui::prompt::Chrome::default();
    let bootstrap: bootstrap::Bootstrap = match demo {
        Some(_) => {
            online = false;
            route = app::state::Route::Session;
            bootstrap::Bootstrap {
                client: ox_client::Client::new("http://127.0.0.1:1", directory.clone()),
                session: Some("ses_demo".to_string()),
            }
        }
        None => match session {
            Some(_) => {
                online = true;
                route = app::state::Route::Session;
                bootstrap::connect(&server, &directory, session).await?
            }
            None => {
                online = true;
                route = app::state::Route::Home;
                let client = bootstrap::check_reachable(&server, &directory).await?;
                home_chrome = bootstrap::chrome(&client, &directory).await;
                bootstrap::Bootstrap {
                    client,
                    session: None,
                }
            }
        },
    };

    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (seq_tx, seq_rx) = watch::channel(None);

    if online {
        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();

        // 会话已知（--session）就现在挂 SSE；Home 模式推迟到会话创建后。
        let lazy_stream = if bootstrap.session.is_some() {
            ox_client::sse::spawn_events(
                bootstrap.client.clone(),
                bootstrap.session.clone().unwrap_or_default(),
                seq_rx,
                stream_tx,
            );
            None
        } else {
            Some((stream_tx, seq_rx))
        };

        // 命令执行任务：把 AppCommand 变成网络动作，再把结果送回 UI 事件循环。
        {
            let client = bootstrap.client.clone();
            let tx = event_tx.clone();
            let mut lazy_stream = lazy_stream;
            tokio::spawn(async move {
                while let Some(command) = command_rx.recv().await {
                    match command {
                        app::command::AppCommand::CreateSessionAndPrompt(text) => {
                            match client.create_session().await {
                                Ok(session) => {
                                    if let Some((stream_tx, seq_rx)) = lazy_stream.take() {
                                        ox_client::sse::spawn_events(
                                            client.clone(),
                                            session.clone(),
                                            seq_rx,
                                            stream_tx,
                                        );
                                    }
                                    let _ = tx.send(app::event::AppEvent::SessionReady {
                                        session,
                                        prompt: text,
                                    });
                                }
                                Err(err) => {
                                    let _ = tx
                                        .send(app::event::AppEvent::SessionFailed(err.to_string()));
                                }
                            }
                        }
                        app::command::AppCommand::Prompt { session, text } => {
                            if let Err(err) = client.prompt(&session, &text).await {
                                tracing::warn!(%err, "prompt failed");
                            }
                        }
                        app::command::AppCommand::Interrupt { session } => {
                            let _ = client.interrupt(&session).await;
                        }
                        app::command::AppCommand::Quit => break,
                        app::command::AppCommand::ReloadHistory {
                            session,
                            request_id,
                        } => {
                            match client.messages(&session, None).await {
                                Ok(page) => {
                                    let mut messages = page.data;
                                    messages.reverse(); // desc → chronological
                                    let items = ox_client::history::parse_messages(&messages);
                                    let _ = tx
                                        .send(app::event::AppEvent::History { request_id, items });
                                }
                                Err(err) => tracing::warn!(%err, "history reload failed"),
                            }
                        }
                    }
                }
            });
        }

        // 把 SSE 流桥接成 AppEvent（spawn_events 之前流是空的，正常）。
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

    let mut app = App::new(route, bootstrap.session, home_chrome, command_tx, seq_tx);
    app.run(&mut guard, event_rx).await
}
