//! `ox acp` — ACP (Agent Client Protocol) translator.
//!
//! The `acp` subcommand turns the ox HTTP/SSE API into an ACP stdio agent so
//! editors (Zed, JetBrains, …) can drive ox directly. Session translation uses
//! the public HTTP contract; only backend startup composes a local server.
//!
//! With no `--server`, attach through the local Unix socket or configured TCP
//! address before falling back to an in-process server. An explicit URL
//! selects only that endpoint, without a local fallback.

mod backend;
mod bridge;
mod client_requests;
mod config_options;
mod content;
mod lifecycle;
mod session;
mod stream;
mod translate;
mod updates;

use agent_client_protocol as acp;
use agent_client_protocol::v1::schema::{
    CloseSessionRequest, CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ResumeSessionRequest, ResumeSessionResponse, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
};
use ox_core::config::Config;

use self::backend::Backend;

/// Entry point of `ox acp [--server <url>]`.
///
pub async fn run(explicit_base: Option<String>, config: Config) -> anyhow::Result<()> {
    let Backend {
        http,
        base,
        lifetime,
    } = Backend::connect(explicit_base, config).await?;
    let outcome = serve(bridge::Bridge::new(http, base), acp::Stdio::new()).await;
    lifetime.shutdown().await;
    outcome
}

/// The ACP loop over stdin/stdout NDJSON.
///
/// Each handler is a thin closure over [`bridge::Bridge`]; long-running work
/// (a whole prompt turn) is spawned off the dispatch loop per the crate's
/// ordering contract, and the deferred `Responder` settles when the turn does.
async fn serve(
    bridge: bridge::Bridge,
    transport: impl acp::ConnectTo<acp::Agent>,
) -> anyhow::Result<()> {
    acp::Agent
        .v1()
        .name("ox")
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |request: InitializeRequest,
                            responder: acp::v1::Responder<InitializeResponse>,
                            _connection: acp::v1::ConnectionTo<acp::Client>| {
                    responder.respond(bridge.initialize(request))
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |request: ListSessionsRequest,
                            responder: acp::v1::Responder<ListSessionsResponse>,
                            _connection| {
                    match bridge.list_sessions(request).await {
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
                async move |request: DeleteSessionRequest,
                            responder: acp::v1::Responder<DeleteSessionResponse>,
                            _connection| {
                    match bridge.delete_session(request).await {
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
                async move |request: CloseSessionRequest,
                            responder: acp::v1::Responder<CloseSessionResponse>,
                            _connection| {
                    match bridge.close_session(request).await {
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
                async move |request: ResumeSessionRequest,
                            responder: acp::v1::Responder<ResumeSessionResponse>,
                            _connection| {
                    match bridge.resume_session(request).await {
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
                async move |request: SetSessionConfigOptionRequest,
                            responder: acp::v1::Responder<SetSessionConfigOptionResponse>,
                            _connection: acp::v1::ConnectionTo<acp::Client>| {
                    match bridge.set_config_option(request).await {
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
                async move |request: NewSessionRequest,
                            responder: acp::v1::Responder<NewSessionResponse>,
                            connection: acp::v1::ConnectionTo<acp::Client>| {
                    match bridge.new_session(request).await {
                        Ok(response) => {
                            let session = response.session_id.clone();
                            responder.respond(response)?;
                            // Slash-command support lights up from the
                            // catalogue push, even while ox's list is empty.
                            bridge
                                .send_available_commands(&session, &connection)
                                .await?;
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
                            responder: acp::v1::Responder<PromptResponse>,
                            connection: acp::v1::ConnectionTo<acp::Client>| {
                    // The turn outlives the dispatch loop: stream updates from
                    // the spawned task and answer `session/prompt` on settle.
                    connection.spawn({
                        let connection = connection.clone();
                        let bridge = bridge.clone();
                        async move {
                            match bridge.run_prompt(&request, &connection).await {
                                Ok(stop) => responder.respond(PromptResponse::new(stop)),
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
                            responder: acp::v1::Responder<LoadSessionResponse>,
                            connection: acp::v1::ConnectionTo<acp::Client>| {
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
                            responder: acp::v1::Responder<SetSessionModeResponse>,
                            _connection: acp::v1::ConnectionTo<acp::Client>| {
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
                async move |notification: acp::v1::schema::CancelNotification,
                            _connection: acp::v1::ConnectionTo<acp::Client>| {
                    bridge.cancel(&notification.session_id).await;
                    Ok(())
                }
            },
            acp::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
        .map_err(|error| anyhow::anyhow!("ox acp: stdio connection failed: {error}"))?;
    Ok(())
}

/// Build an ACP error from an ox HTTP failure, keeping ox's message verbatim.
pub(crate) fn wire_error(status: u16, body: &str) -> acp::Error {
    if status == 401 {
        return acp::Error::auth_required();
    }
    if status == 400 || status == 404 {
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
    backend::parse_url(base).map(|_| ())
}
