//! `POST /api/experimental/mcp/{server}/connect|disconnect` (upstream
//! `mcp.connect` / `mcp.disconnect`, `core/src/mcp/index.ts:781-797`).
//!
//! connect = stop, then start with the config `disabled` flag bypassed;
//! disconnect = stop, then status `disabled`. Stop only acts on a connected
//! server: it shuts the transport down, drops the server's tools, resources,
//! templates and metadata, and announces `mcp.tools.changed`,
//! `mcp.resources.changed` and `mcp.prompts.changed`.
//!
//! The tool registry is immutable per services generation, so the whole
//! operation runs inside `Live::replace`: the gate serializes it with
//! reloads and other MCP mutations, and the closure works on `old.mcp`, so it
//! never touches a hub a reload just retired. The next generation shares the
//! old config, chat, catalog and the same `Arc<Hub>`; its registry is the old
//! one minus the server's recorded tools plus whatever the restart
//! registered (only `Arc`s are cloned). An unknown server fails the closure,
//! so nothing is installed.
//!
//! The operation mutates the shared hub across awaits, so it runs in a task
//! detached from the request (the handler awaits it): a client disconnect
//! cannot strand the hub half-stopped.
//!
//! Events are ephemeral `{server}` frames stamped with the request
//! directory, like `reload::announce`. `pending` and the stop frames go out
//! while the closure runs (they describe the shared hub, already updated);
//! the final status/catalog frames go out after the new registry is live.
//!
//! Deviations: one coarse lock (the replace gate) instead of upstream's
//! per-server lock; no `needs_auth` status (ox has no MCP OAuth), a failure
//! is always `failed`; prompts are not modeled, `mcp.prompts.changed` is
//! still sent for parity.

use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde_json::json;

use ox_engine::runner::ServiceSet;
use ox_mcp::Status;

use super::AppState;
use super::compat::directory_of;
use crate::register_server_tools;

/// The closure's typed refusal: no server by this name.
#[derive(Debug)]
struct ServerNotFound;

impl fmt::Display for ServerNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MCP server not found")
    }
}

impl std::error::Error for ServerNotFound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Connect,
    Disconnect,
}

/// What the closure did, so the trailing frames match it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Connected,
    Failed,
    Disabled,
}

#[handler] // operation_id: mcp.connect
pub(crate) async fn mcp_connect(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    handle(req, depot, res, Action::Connect).await;
}

#[handler] // operation_id: mcp.disconnect
pub(crate) async fn mcp_disconnect(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    handle(req, depot, res, Action::Disconnect).await;
}

async fn handle(req: &mut Request, depot: &mut Depot, res: &mut Response, action: Action) {
    let Some(state) = super::state(depot, res) else {
        return;
    };
    let server = req.param::<String>("server").unwrap_or_default();
    let directory = directory_of(req);
    let task = tokio::spawn({
        let state = state.clone();
        let server = server.clone();
        async move { run(&state, &directory, &server, action).await }
    });
    let outcome = match task.await {
        Ok(outcome) => outcome,
        Err(join) => Err(anyhow::anyhow!("MCP {action:?} task failed: {join}")),
    };
    match outcome {
        Ok(()) => {
            res.status_code(StatusCode::NO_CONTENT);
        }
        Err(error) if error.downcast_ref::<ServerNotFound>().is_some() => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(json!({
                "_tag": "McpServerNotFoundError",
                "server": server,
                "message": format!("MCP server not found: {server}"),
            })));
        }
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), %server, "MCP connect/disconnect failed");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(json!({
                "_tag": "UnknownError",
                "message": "Unexpected server error. Check server logs for details.",
            })));
        }
    }
}

async fn run(state: &AppState, directory: &str, server: &str, action: Action) -> Result<()> {
    let emit = |kind: &str| {
        state
            .bus
            .broadcast_ephemeral(kind, json!({ "server": server }), Some(directory));
    };
    let mut outcome = Outcome::Disabled;
    let outcome_slot = &mut outcome;
    state
        .live
        .replace(|old| async move {
            if !old.mcp.has_server(server) {
                return Err(ServerNotFound.into());
            }
            let config = match action {
                Action::Connect => match old.config.mcp.servers.get(server) {
                    Some(config) => Some(config),
                    None => return Err(ServerNotFound.into()),
                },
                Action::Disconnect => None,
            };
            let stale = old.mcp.stop(server).await;
            if stale.is_some() {
                emit("mcp.tools.changed");
                emit("mcp.resources.changed");
                emit("mcp.prompts.changed");
            }
            let stale = stale.unwrap_or_default();
            let stale: Vec<&str> = stale.iter().map(String::as_str).collect();
            let mut tools = old.tools.without(&stale);
            match config {
                Some(config) => {
                    old.mcp.set_status(server, Status::Pending);
                    emit("mcp.status.changed");
                    match old.mcp.start(server, config, &state.directory).await {
                        Some(offered) => {
                            let registered = register_server_tools(&mut tools, offered);
                            old.mcp.record_server_tools(server, registered);
                            *outcome_slot = Outcome::Connected;
                        }
                        None => *outcome_slot = Outcome::Failed,
                    }
                }
                None => {
                    old.mcp.set_status(server, Status::Disabled);
                    *outcome_slot = Outcome::Disabled;
                }
            }
            Ok(ServiceSet {
                config: Arc::clone(&old.config),
                chat: Arc::clone(&old.chat),
                model_catalog: Arc::clone(&old.model_catalog),
                mcp: Arc::clone(&old.mcp),
                tools: Arc::new(tools),
            })
        })
        .await?;

    if outcome == Outcome::Connected {
        emit("mcp.tools.changed");
        emit("mcp.resources.changed");
    }
    emit("mcp.status.changed");
    Ok(())
}
