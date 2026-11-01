//! `ox acp` — ACP (Agent Client Protocol) translator.
//!
//! The `acp` subcommand turns the ox HTTP/SSE API into an ACP stdio agent so
//! editors (Zed, JetBrains, …) can drive ox directly. It is a pure translator:
//! it depends only on ox's public wire contract (the same surface the
//! opencode CLI/TUI consume) plus the `agent-client-protocol` crate — never on
//! the runner, the projector, or the database.
//!
//! Topology: attach to an already-running ox server when one answers
//! `GET /api/info`, otherwise own the server runtime in-process on an
//! ephemeral loopback port (turso holds an exclusive lock on `ox.db`, so a
//! spawned child server could never coexist with the user's server).
//!
//! Known limitation: the idle server the TUI spawns (`--exit-when-idle`)
//! binds only the unix socket (the `exit_when_idle` branch of
//! `ox_server::run_with`), while the probe here only tries TCP on
//! `config.listen`. The probe therefore misses that server and falls through
//! to own-mode, where opening `ox.db` fails on turso's exclusive lock and
//! `ox acp` exits. This fails closed (two processes never share the
//! database), but attach does not work; the fix is to probe the unix socket
//! too.

mod bridge;
mod stream;
mod translate;

use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SetSessionModeRequest,
    SetSessionModeResponse,
};
use anyhow::Context;

use ox_core::config::Config;

/// Entry point of `ox acp [--server <url>]`.
///
/// `config` is loaded by `main.rs` (the translator itself never reads config
/// files): the default base URL comes from its `listen` address, and the
/// whole struct is needed only when owning the server runtime.
pub async fn run(explicit_base: Option<String>, config: Config) -> anyhow::Result<()> {
    let base = explicit_base.unwrap_or_else(|| format!("http://{}", config.listen));

    let base = if probe(&base).await {
        base
    } else {
        // No server answered: own the runtime in-process on an ephemeral
        // port. The composition is the same code path `ox::run` uses.
        let (state, router) = ox_server::compose(config)
            .await
            .context("ox acp: composing the in-process server failed")?;
        use salvo::conn::Listener as _;
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await
            .context("ox acp: binding an ephemeral port failed")?;
        let addr = acceptor
            .local_addr()
            .context("ox acp: reading the bound port failed")?;
        tokio::spawn(ox_server::serve_on(acceptor, state, router));
        tracing::info!(%addr, "ox acp owns an in-process server");
        format!("http://{addr}")
    };

    serve_stdio(base).await
}

/// `GET {base}/api/info` with a short timeout decides attach vs own.
async fn probe(base: &str) -> bool {
    let url = format!("{base}/api/info");
    reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_millis(500))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// The ACP loop over stdin/stdout NDJSON.
///
/// Each handler is a thin closure over [`bridge::Bridge`]; long-running work
/// (a whole prompt turn) is spawned off the dispatch loop per the crate's
/// ordering contract, and the deferred `Responder` settles when the turn does.
async fn serve_stdio(base: String) -> anyhow::Result<()> {
    let bridge = bridge::Bridge::new(base);

    acp::Agent
        .builder()
        .name("ox")
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |request: InitializeRequest,
                            responder: acp::Responder<InitializeResponse>,
                            _connection: acp::ConnectionTo<acp::Client>| {
                    responder.respond(bridge.initialize(request))
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |request: NewSessionRequest,
                            responder: acp::Responder<NewSessionResponse>,
                            connection: acp::ConnectionTo<acp::Client>| {
                    match bridge.new_session(request).await {
                        Ok(response) => {
                            let session = response.session_id.clone();
                            responder.respond(response)?;
                            // Slash-command support lights up from the
                            // catalogue push, even while ox's list is empty.
                            bridge.send_available_commands(&session, &connection).await;
                            Ok(())
                        }
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |request: PromptRequest,
                            responder: acp::Responder<PromptResponse>,
                            connection: acp::ConnectionTo<acp::Client>| {
                    // The turn outlives the dispatch loop: stream updates from
                    // the spawned task and answer `session/prompt` on settle.
                    connection.spawn({
                        let connection = connection.clone();
                        let bridge = bridge.clone();
                        async move {
                            match bridge.run_prompt(&request, &connection).await {
                                Ok(stop) => {
                                    responder.respond(acp::schema::v1::PromptResponse::new(stop))
                                }
                                Err(error) => responder.respond_with_error(error),
                            }
                        }
                    })
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |request: LoadSessionRequest,
                            responder: acp::Responder<LoadSessionResponse>,
                            connection: acp::ConnectionTo<acp::Client>| {
                    match bridge.load_session(request, &connection).await {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |request: SetSessionModeRequest,
                            responder: acp::Responder<SetSessionModeResponse>,
                            _connection: acp::ConnectionTo<acp::Client>| {
                    match bridge.set_session_mode(request).await {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let bridge = bridge.clone();
                async move |notification: acp::schema::v1::CancelNotification,
                            _connection: acp::ConnectionTo<acp::Client>| {
                    bridge.cancel(&notification.session_id).await;
                    Ok(())
                }
            },
            acp::on_receive_notification!(),
        )
        .connect_to(acp::Stdio::new())
        .await
        .map_err(|error| anyhow::anyhow!("ox acp: stdio connection failed: {error}"))?;
    Ok(())
}

/// Build an ACP error from an ox HTTP failure, keeping ox's message verbatim.
pub(crate) fn wire_error(status: u16, body: &str) -> acp::Error {
    if status == 404 {
        return acp::Error::invalid_params().data(body.to_string());
    }
    acp::Error::internal_error().data(format!("ox returned {status}: {body}"))
}

/// Map any displayable failure to an ACP internal error.
pub(crate) fn internal(error: impl std::fmt::Display) -> acp::Error {
    acp::Error::internal_error().data(error.to_string())
}

/// Refuse obviously malformed explicit URLs early with a named failure.
pub fn validate_base(base: &str) -> anyhow::Result<()> {
    if !base.starts_with("http://") && !base.starts_with("https://") {
        anyhow::bail!("ox acp: --server must be an http(s) URL, got `{base}`");
    }
    Ok(())
}
