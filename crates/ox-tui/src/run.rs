//! The `ox` TUI entry point: argument handling, terminal setup, wiring.

use crate::lifecycle::{self, Transport};
use crate::{app, bootstrap, executor, herdr, terminal, ui};

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

/// TUI options a herdr resume replays before `--session <id>`: only the
/// server the session lives on. `--no-mouse` is not replayed because the `ox`
/// command line does not accept it (`TUI_OPTIONS` in `src/main.rs`).
fn resume_flags(transport: &Transport) -> Vec<String> {
    match transport {
        Transport::Explicit(url) => vec!["--server".to_string(), url.clone()],
        Transport::Unix => Vec::new(),
    }
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

    // Graphics support is one more stdio query in the same pre-TUI window, and
    // for the same reason it has to happen before the event thread below: the
    // replies to the kitty/sixel and cell-size queries arrive on stdin. The
    // query enters and restores raw mode itself, so the two run in sequence.
    crate::graphics::probe::init();

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
    // Demo mode replays a script with no real session: nothing to report.
    let herdr_flags = demo.is_none().then(|| resume_flags(&transport));

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
    let mut bootstrap: bootstrap::Bootstrap = match online_client {
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

    if let Some(session) = &bootstrap.session
        && online
    {
        bootstrap.client.directory = bootstrap
            .client
            .session_directory(session)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Session has no working directory"))?;
    }
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (seq_tx, seq_rx) = watch::channel(None);

    if online {
        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();

        // 会话已知（--session）就现在挂 SSE；Home 模式推迟到会话创建后。
        // Attachment 由命令任务持有：切换会话时它中止旧流再挂新流。
        let mut stream = ox_client::sse::Attachment::new(
            bootstrap.client.clone(),
            seq_rx.clone(),
            stream_tx.clone(),
        );
        if let Some(session) = &bootstrap.session {
            stream.attach(session);
        }

        // ID-only controls bypass directory synchronization; the serial owner
        // orders every other command against request and SSE context changes.
        {
            tokio::spawn(commands(
                bootstrap.client.clone(),
                bootstrap.session.clone(),
                command_rx,
                event_tx.clone(),
                seq_rx,
                stream_tx,
                stream,
            ));
        }

        // Bridge the stream into the UI event loop.
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

    let mut app = App::new(
        route,
        bootstrap.session,
        home_chrome,
        bootstrap.client.directory,
        command_tx,
        seq_tx,
    );
    if let Some(path) = ui::history::default_path() {
        app.attach_history(ui::history::History::load(path));
    }
    if let Some(reporter) = herdr_flags.and_then(herdr::Reporter::from_env) {
        app.attach_herdr(reporter);
    }
    let result = app.run(&mut guard, event_rx).await;
    let reporter = app.take_herdr();
    drop(app);
    drop(guard);
    if let Some(reporter) = reporter {
        reporter.finish().await;
    }
    result
}

/// Keep ID-only controls responsive while the serial owner resolves directory
/// context. No directory-dependent request may bypass that owner's gate.
async fn commands(
    client: ox_client::Client,
    active_session: Option<String>,
    mut command_rx: mpsc::UnboundedReceiver<app::command::AppCommand>,
    tx: mpsc::UnboundedSender<app::event::AppEvent>,
    seq_rx: watch::Receiver<Option<i64>>,
    stream_tx: mpsc::UnboundedSender<ox_client::sse::StreamMsg>,
    stream: ox_client::sse::Attachment,
) {
    use app::command::AppCommand;

    let (serial_tx, serial_rx) = mpsc::unbounded_channel();
    let owner = serial_commands(
        client.clone(),
        active_session,
        serial_rx,
        tx.clone(),
        seq_rx,
        stream_tx,
        stream,
    );
    tokio::pin!(owner);
    loop {
        tokio::select! {
            () = &mut owner => return,
            command = command_rx.recv() => match command {
                Some(AppCommand::Quit) => return, // Dropping owner also detaches SSE.
                Some(command @ (AppCommand::Interrupt { .. }
                    | AppCommand::Background { .. }
                    | AppCommand::ReplyForm { .. }
                    | AppCommand::CancelForm { .. }
                    | AppCommand::ListForms { .. }
                    | AppCommand::KillShell { .. }
                    | AppCommand::SteerQueued { .. }
                    | AppCommand::CancelQueued { .. })) => {
                    tokio::spawn(executor::run(client.clone(), tx.clone(), command));
                }
                Some(command) => {
                    if serial_tx.send(command).is_err() {
                        return;
                    }
                }
                None => {
                    drop(serial_tx);
                    owner.await;
                    return;
                }
            },
        }
    }
}

/// Sole owner of request and stream context; failed resolution disables
/// directory-dependent I/O until authoritative refresh or navigation succeeds.
async fn serial_commands(
    mut client: ox_client::Client,
    mut active_session: Option<String>,
    mut command_rx: mpsc::UnboundedReceiver<app::command::AppCommand>,
    tx: mpsc::UnboundedSender<app::event::AppEvent>,
    seq_rx: watch::Receiver<Option<i64>>,
    stream_tx: mpsc::UnboundedSender<ox_client::sse::StreamMsg>,
    mut stream: ox_client::sse::Attachment,
) {
    let mut generation = 0;
    let mut directory_ready = true;
    // Intent belongs to active_session, not to a cached session or directory.
    // Navigation clears it; successful directory recovery consumes it once.
    let mut chrome_pending = false;
    while let Some(command) = command_rx.recv().await {
        if !directory_ready
            && matches!(
                command,
                app::command::AppCommand::CreateSessionAndPrompt(_)
                    | app::command::AppCommand::Prompt { .. }
                    | app::command::AppCommand::ChangeDirectory { .. }
                    | app::command::AppCommand::FindDirectories { .. }
                    | app::command::AppCommand::FindFiles { .. }
                    | app::command::AppCommand::McpList { .. }
                    | app::command::AppCommand::McpToggle { .. }
                    | app::command::AppCommand::ModelList
                    | app::command::AppCommand::LoadContextLimits
                    | app::command::AppCommand::AgentList(_)
            )
        {
            let error = "Working directory unavailable; reselect the session to retry".to_owned();
            let event = match command {
                app::command::AppCommand::Prompt { id, .. } => {
                    app::event::AppEvent::PromptFailed { id, error }
                }
                app::command::AppCommand::CreateSessionAndPrompt(_) => {
                    app::event::AppEvent::SessionFailed(error)
                }
                _ => app::event::AppEvent::SessionOpFailed {
                    title: "Directory unavailable",
                    error,
                },
            };
            let _ = tx.send(event);
            continue;
        }
        match command {
            app::command::AppCommand::LoadSessionChrome { session } => {
                if active_session.as_deref() == Some(&session) {
                    chrome_pending = true;
                }
            }
            app::command::AppCommand::ChangeDirectory { session, directory } => {
                if session != active_session {
                    continue;
                }
                let result = match &session {
                    Some(id) => {
                        let moved = client.move_session(id, &directory).await;
                        match moved {
                            // These handlers reject before publishing a move.
                            Err(err @ ox_client::Error::Status(status))
                                if matches!(status.as_u16(), 400 | 404) =>
                            {
                                Err(err)
                            }
                            moved => {
                                // A lost response (or server error after publishing)
                                // cannot prove the move failed. Stop using the old
                                // context until the authoritative read succeeds.
                                directory_ready = false;
                                stream.detach();
                                if let Err(err) = moved {
                                    tracing::warn!(%err, "move outcome unknown; reading session directory");
                                }
                                session_directory(&client, id).await
                            }
                        }
                    }
                    None => client.validate_directory(&directory).await,
                };
                match result {
                    Ok(directory) => {
                        directory_ready = true;
                        client.directory = directory.clone();
                        stream = ox_client::sse::Attachment::new(
                            client.clone(),
                            seq_rx.clone(),
                            stream_tx.clone(),
                        );
                        if let Some(session) = &session {
                            stream.attach(session);
                        }
                        let _ = tx.send(app::event::AppEvent::DirectoryChanged {
                            session,
                            generation,
                            directory,
                        });
                    }
                    Err(err) => {
                        if directory_ready {
                            let _ = tx.send(app::event::AppEvent::DirectoryChanged {
                                session,
                                generation,
                                directory: client.directory.clone(),
                            });
                        }
                        let _ =
                            tx.send(executor::session_failed("Failed to change directory", err));
                    }
                }
            }
            app::command::AppCommand::RefreshDirectory { session } => {
                if active_session.as_deref() != Some(&session) {
                    continue;
                }
                match session_directory(&client, &session).await {
                    Ok(directory) => {
                        if client.directory != directory || !directory_ready {
                            client.directory = directory.clone();
                            stream = ox_client::sse::Attachment::new(
                                client.clone(),
                                seq_rx.clone(),
                                stream_tx.clone(),
                            );
                            stream.attach(&session);
                        }
                        directory_ready = true;
                        let _ = tx.send(app::event::AppEvent::DirectoryChanged {
                            session: Some(session),
                            generation,
                            directory,
                        });
                    }
                    Err(err) => {
                        directory_ready = false;
                        let _ = tx.send(executor::session_failed(
                            "Failed to read directory; reselect session to retry",
                            err,
                        ));
                    }
                }
            }
            app::command::AppCommand::CreateSessionAndPrompt(submission) => {
                match client.create_session().await {
                    Ok(session) => {
                        chrome_pending = false;
                        active_session = Some(session.clone());
                        stream.attach(&session);
                        let _ = tx.send(app::event::AppEvent::SessionReady {
                            session,
                            prompt: submission.text,
                            files: submission.files,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(app::event::AppEvent::SessionFailed(err.to_string()));
                    }
                }
            }
            app::command::AppCommand::AttachSession {
                session,
                generation: next,
            } => {
                generation = next;
                chrome_pending = false;
                active_session = Some(session.clone());
                directory_ready = false;
                stream.detach();
                match session_directory(&client, &session).await {
                    Ok(directory) => {
                        client.directory = directory.clone();
                        directory_ready = true;
                        stream = ox_client::sse::Attachment::new(
                            client.clone(),
                            seq_rx.clone(),
                            stream_tx.clone(),
                        );
                        stream.attach(&session);
                        let _ = tx.send(app::event::AppEvent::DirectoryChanged {
                            session: Some(session),
                            generation,
                            directory,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(executor::session_failed(
                            "Failed to read directory; reselect session to retry",
                            err,
                        ));
                    }
                }
            }
            app::command::AppCommand::DetachSession {
                directory,
                generation: next,
            } => {
                generation = next;
                chrome_pending = false;
                active_session = None;
                client.directory = directory;
                directory_ready = true;
                stream = ox_client::sse::Attachment::new(
                    client.clone(),
                    seq_rx.clone(),
                    stream_tx.clone(),
                );
            }
            app::command::AppCommand::Quit => break,
            other => {
                tokio::spawn(executor::run(client.clone(), tx.clone(), other));
            }
        }
        if directory_ready && chrome_pending {
            chrome_pending = false;
            if let Some(session) = &active_session {
                tokio::spawn(executor::run(
                    client.clone(),
                    tx.clone(),
                    app::command::AppCommand::LoadSessionChrome {
                        session: session.clone(),
                    },
                ));
            }
        }
    }
}

async fn session_directory(client: &ox_client::Client, session: &str) -> ox_client::Result<String> {
    client
        .session_directory(session)
        .await?
        .filter(|directory| !directory.is_empty())
        .ok_or(ox_client::Error::Status(
            ox_client::reqwest::StatusCode::NOT_FOUND,
        ))
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
