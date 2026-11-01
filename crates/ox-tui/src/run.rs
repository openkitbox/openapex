//! The `ox` TUI entry point: argument handling, terminal setup, wiring.

use crate::lifecycle::{self, Transport};
use crate::{app, bootstrap, executor, terminal, ui};

use tokio::sync::{mpsc, watch};

use app::App;

fn arg(name: &str) -> Option<String> {
    arg_in(std::env::args().skip(1), name)
}

fn arg_in(args: impl IntoIterator<Item = String>, name: &str) -> Option<String> {
    let mut args = args.into_iter();
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

/// Mouse capture is on unless `--no-mouse` is given: without it the terminal
/// keeps its native text selection.
fn mouse_enabled(args: impl IntoIterator<Item = String>) -> bool {
    !args.into_iter().any(|arg| arg == "--no-mouse")
}

pub async fn run() -> anyhow::Result<()> {
    let transport = lifecycle::transport_of(arg("--server"));
    let directory = match arg("--directory") {
        Some(directory) => directory,
        None => std::env::current_dir()?.display().to_string(),
    };
    let session = arg("--session");
    // Tool rows show paths relative to the working directory (upstream
    // `usePathFormatter`); symlinks resolved like the server's location.
    crate::ui::tool_text::init_paths(crate::ui::tool_text::Paths {
        directory: Some(
            std::fs::canonicalize(&directory)
                .map_or_else(|_| directory.clone(), |p| p.display().to_string()),
        ),
        home: std::env::var_os("HOME").map(|home| home.to_string_lossy().into_owned()),
    });

    // The terminal's own colours, read once before the input thread starts: the
    // OSC replies arrive on the same stream as the keystrokes.
    crate::theme::init(crate::theme::terminal::query());

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

    let mut guard = terminal::TerminalGuard::new(mouse_enabled(std::env::args().skip(1)))?;
    terminal::install_panic_hook();

    let demo = std::env::var("OX_TUI_DEMO").ok();

    // The client every online route uses: `--server` is plain HTTP; otherwise
    // the unix socket, with the server spawned on demand and a lease held.
    let online_client = match (&demo, transport) {
        (Some(_), _) => None,
        (None, Transport::Explicit(url)) => Some(ox_client::Client::new(url, directory.clone())),
        (None, Transport::Unix) => Some(connect_unix(&directory).await?),
    };

    let route;
    let online;
    let mut home_chrome = ui::prompt::Chrome::default();
    let bootstrap: bootstrap::Bootstrap = match online_client {
        None => {
            online = false;
            route = app::state::Route::Session;
            bootstrap::Bootstrap {
                client: ox_client::Client::new("http://127.0.0.1:1", directory.clone()),
                session: Some("ses_demo".to_string()),
            }
        }
        Some(client) => match session {
            Some(_) => {
                online = true;
                route = app::state::Route::Session;
                let connected = bootstrap::connect_with(client, session).await?;
                // The session footer shows the location stored on the session.
                home_chrome =
                    bootstrap::chrome(&connected.client, &directory, connected.session.as_deref())
                        .await;
                connected
            }
            None => {
                online = true;
                route = app::state::Route::Home;
                let client = bootstrap::check_reachable_with(client).await?;
                home_chrome = bootstrap::chrome(&client, &directory, None).await;
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
        // Attachment 由命令任务持有：切换会话时它中止旧流再挂新流。
        let mut stream =
            ox_client::sse::Attachment::new(bootstrap.client.clone(), seq_rx, stream_tx);
        if let Some(session) = &bootstrap.session {
            stream.attach(session);
        }

        // 命令执行：只有碰 SSE attachment 的命令留在串行循环里，其余各自
        // 一个任务（executor::run），慢请求不会拖住 interrupt / 表单应答。
        {
            let client = bootstrap.client.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                while let Some(command) = command_rx.recv().await {
                    match command {
                        app::command::AppCommand::CreateSessionAndPrompt(submission) => {
                            match client.create_session().await {
                                Ok(session) => {
                                    stream.attach(&session);
                                    let _ = tx.send(app::event::AppEvent::SessionReady {
                                        session,
                                        prompt: submission.text,
                                        files: submission.files,
                                    });
                                }
                                Err(err) => {
                                    let _ = tx
                                        .send(app::event::AppEvent::SessionFailed(err.to_string()));
                                }
                            }
                        }
                        app::command::AppCommand::AttachSession { session } => {
                            stream.attach(&session);
                        }
                        app::command::AppCommand::DetachSession => stream.detach(),
                        app::command::AppCommand::Quit => break,
                        other => {
                            tokio::spawn(executor::run(client.clone(), tx.clone(), other));
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
    if let Some(path) = ui::history::default_path() {
        app.attach_history(ui::history::History::load(path));
    }
    app.run(&mut guard, event_rx).await
}

/// Connect over the unix socket, starting the server if needed, and keep a
/// lease on it for the rest of the process. Returns only once a lease was
/// granted, so the server is not one that is already shutting down.
async fn connect_unix(directory: &str) -> anyhow::Result<ox_client::Client> {
    let socket = ox_schema::paths::socket_path();
    let exe = std::env::current_exe()?;
    let log = ox_schema::paths::server_log_path();
    let client = ox_client::Client::over_unix(&socket, directory)?;
    let reconnect = move || {
        let socket = socket.clone();
        let exe = exe.clone();
        let log = log.clone();
        async move {
            lifecycle::ensure_server(
                &socket,
                || lifecycle::spawn_server(&exe, &log),
                lifecycle::SERVER_START_WAIT,
            )
            .await
        }
    };
    let first = tokio::time::timeout(
        lifecycle::SERVER_START_WAIT,
        lifecycle::acquire_lease(&client, reconnect.clone()),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ox server did not grant a lease; check the server log"))??;
    tokio::spawn(lifecycle::run_lease(client.clone(), first, reconnect));
    Ok(client)
}
