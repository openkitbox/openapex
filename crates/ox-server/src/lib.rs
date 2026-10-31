//! ox library: config, storage, ID generation and HTTP API.
//!
//! Split out from `main.rs` so integration tests can build the router and
//! drive it over HTTP against an ephemeral port.

pub mod api;
pub(crate) mod reload;
mod session_retention;
#[cfg(test)]
mod test_support;
mod tmp_retention;
pub(crate) mod worktree;

pub use ox_core::{USER_AGENT, config, event_bus, projector};
pub use ox_db as db;
pub use ox_engine::{mctx, prompt, runner};
pub use ox_llm::cardinal;
pub use ox_llm::model_catalog;
pub use ox_mcp as mcp;
pub use ox_schema::{id, slug};
pub use ox_tools as tool;

use std::sync::Arc;

use anyhow::Result;
use salvo::prelude::*;

use crate::api::AppState;
use ox_core::config::Config;
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_engine::runner::{Runner, ToolRegistryExt};
use ox_llm::cardinal::client::CardinalClient;
use ox_llm::cardinal::router::EndpointRouter;

/// One transport constructor shared by [`build_state`] and [`run`], so both
/// map init failures identically. `[models.<id>]` routes through ordered
/// candidates; with no `[providers]`/`[models]` configured the router is a
/// pass-through to the `cardinal_url` endpoint (previous behavior).
fn chat_transport(config: &Config) -> Result<Arc<dyn runner::Chat>> {
    let cardinal = CardinalClient::from_config(&config.llm)
        .map_err(|error| anyhow::anyhow!("cardinal client init failed: {error}"))?;
    let router = EndpointRouter::from_config(&config.llm, cardinal)
        .map_err(|error| anyhow::anyhow!("chat endpoint router init failed: {error}"))?;
    Ok(Arc::new(router))
}

/// Build application state: open the DB, run migrations, canonicalize the
/// working directory once (falls back to the raw path on error), and register
/// the eight built-in tools plus the magic-context tools when enabled (the
/// default).
///
/// CBM discovery is deliberately not part of this builder: the optional
/// `codebase-memory-mcp` binary is probed at the composition root ([`run`]),
/// so tests get a deterministic toolset regardless of the host's `PATH`.
pub async fn build_state(config: Config) -> Result<AppState> {
    let chat = chat_transport(&config)?;
    build_state_with(config, chat, runner::ToolRegistry::with_builtins()).await
}

/// [`build_state`] with an injected chat transport and tool registry —
/// the seam tests (and later, alternate providers) use. The registry is
/// passed unwrapped so the wiring can attach the wiring-dependent tools
/// (`register_mctx`, `register_subagent`) after the DB opens. A caller that
/// must not carry a tool removes it by name; `ToolRegistry::remove` exists
/// for exactly that.
///
/// Callers may pre-populate `tools` before handing it over (as [`run`] does
/// with the CBM and todo tools).
pub async fn build_state_with(
    config: Config,
    chat: std::sync::Arc<dyn runner::Chat>,
    tools: runner::ToolRegistry,
) -> Result<AppState> {
    build_state_with_home(config, chat, tools, ox_core::config::ox_home()).await
}

/// [`build_state_with`] with an explicit global home for the runner's global
/// instruction file, set at runner construction (before the subagent handle
/// attach): tests embed a temp home so a developer's `~/.ox/AGENTS.md` never
/// leaks into a test's system prompt.
pub async fn build_state_with_home(
    config: Config,
    chat: std::sync::Arc<dyn runner::Chat>,
    tools: runner::ToolRegistry,
    home: std::path::PathBuf,
) -> Result<AppState> {
    let db = Db::open(&config.db_path).await?;
    // The env block advertises <ox_home>/tmp as the preferred scratch space
    // (upstream `global.tmp`); create it once at boot.
    let _ = std::fs::create_dir_all(ox_core::config::ox_home().join("tmp"));
    build_state_on(config, chat, tools, db, home).await
}

/// The shared tail of [`build_state_with`]: state over an already-open
/// database, so [`run`] can register DB-holding tools before handing the
/// registry over without opening the database twice.
async fn build_state_on(
    config: Config,
    chat: std::sync::Arc<dyn runner::Chat>,
    tools: runner::ToolRegistry,
    db: Db,
    home: std::path::PathBuf,
) -> Result<AppState> {
    let bus = EventBus::new(db.clone());
    build_state_on_with_bus(config, chat, tools, db, bus, home).await
}

/// [`build_state_on`] over a caller-supplied event bus, so the composition
/// root can publish to the same bus it registers tools against.
pub async fn build_state_on_with_bus(
    config: Config,
    chat: std::sync::Arc<dyn runner::Chat>,
    mut tools: runner::ToolRegistry,
    db: Db,
    bus: EventBus,
    home: std::path::PathBuf,
) -> Result<AppState> {
    if config.magic_context.enabled {
        let embedder = mctx::embed::Embedder::from_config(&config.magic_context.embedding);
        tools.register_mctx(db.clone(), embedder);
    }
    let cwd = std::env::current_dir()?;
    let directory = cwd.canonicalize().unwrap_or(cwd);
    // MCP servers connect before the runner exists: their tools must be in
    // the registry the runner snapshots, and the same hub is shared with the
    // HTTP layer. A server that fails to connect is a status, never a
    // startup error.
    let mcp = Arc::new(ox_mcp::Hub::empty());
    register_mcp_tools(&mcp, &config.mcp, &directory, &mut tools).await;
    let model_catalog = model_catalog::ModelCatalog::new(&config.llm);
    // The TUI renders its question dialog purely from the ephemeral
    // `form.created` / `form.replied` / `form.cancelled` frames, so the
    // process-wide form registry must broadcast on this same bus (the one
    // `GET /api/event` serves).
    ox_tools::form::init(bus.clone());
    // Same for `shell.created|exited|deleted`: the TUI marks a background
    // shell running by its presence in a store those frames fill.
    ox_tools::shell::events::init(bus.clone());
    // One read-only config behind an `Arc`: every actor and spawned task clones
    // the handle, never the struct.
    let config = std::sync::Arc::new(config);
    let tools = Arc::new(tools);
    let db_handle = db.clone();
    let bus_handle = bus.clone();
    let runner = Runner::new_with_mcp_and_home(
        std::sync::Arc::clone(&config),
        directory.clone(),
        db,
        bus,
        chat,
        tools,
        model_catalog,
        mcp,
        home,
    );
    Ok(AppState {
        db: db_handle,
        bus: bus_handle,
        directory,
        live: runner.live(),
        runner,
        config_path: ox_core::config::Config::default_path(),
    })
}

/// Connect every configured MCP server through `hub` and register its tools.
/// A server that fails to connect is a status, never an error; a tool that
/// collides with an existing one is skipped with a warning. The names
/// actually registered are recorded on the hub, so a reload removes exactly
/// those and never a builtin that shadowed a skipped MCP tool.
pub(crate) async fn register_mcp_tools(
    hub: &ox_mcp::Hub,
    config: &ox_core::config::McpConfig,
    directory: &std::path::Path,
    tools: &mut runner::ToolRegistry,
) {
    let mut registered = Vec::new();
    for tool in hub.connect_all(config, directory).await {
        let name = tool.name().to_owned();
        // `ToolRegistry::register` replaces before it reports the duplicate,
        // so check first: a server must never shadow a builtin.
        if tools.get(&name).is_some() {
            tracing::warn!(tool = %name, "MCP tool collides with an existing tool; skipped");
            continue;
        }
        match tools.register(tool) {
            Ok(()) => registered.push(name),
            Err(error) => tracing::warn!(tool = %name, %error, "MCP tool skipped"),
        }
    }
    hub.record_registered_tools(registered);
}

/// Build the salvo router for the given state.
pub fn build_router(state: AppState) -> Router {
    api::build_router(state)
}

/// Open the DB, build the router and serve on `config.listen`.
pub async fn run(config: Config) -> Result<()> {
    let (state, router) = compose(config).await?;
    let acceptor = TcpListener::new(state.services().config.listen.clone())
        .try_bind()
        .await?;
    serve_on(acceptor, state, router).await
}

/// Compose the full server state and router without binding anything —
/// the shared seam between [`run`] (binds `config.listen`) and `ox acp`
/// own-mode (binds an ephemeral port and reports the real address).
pub async fn compose(config: Config) -> Result<(AppState, Router)> {
    let mut tools = runner::ToolRegistry::with_builtins();
    // The todowrite tool needs the DB (it resets the todo reminder budget and
    // the reminder scans the transcript), so it joins here at the composition
    // root rather than in `with_builtins`.
    // Tests that inject their own registry keep a deterministic toolset.
    // `subagent` joins the same way below: it needs the runner, which only
    // exists after `build_state_on`, so `run` registers it through the
    // runner's handle slot and the registry here.
    let db = Db::open(&config.db_path).await?;
    let bus = EventBus::new(db.clone());
    tools.register_todowrite(db.clone());
    tools.register_subagent(db.clone(), bus.clone(), config.subagent.depth);
    // Background `shell` completions need the same db/bus; the runner attaches
    // its waker when it is built below.
    tools.register_shell_completion(db.clone(), bus.clone());
    let chat = chat_transport(&config)?;
    let state =
        build_state_on_with_bus(config, chat, tools, db, bus, ox_core::config::ox_home()).await?;
    spawn_runner_memory_stats(state.runner.clone());
    // Fetch the model list before serving, so the first client that asks
    // (`/api/model` at TUI startup) gets models instead of an empty list. A
    // failure only warns: the request path retries on its own TTL.
    let svc = state.services();
    svc.model_catalog.warm().await;
    // Background memory consolidation. Idle unless `[magic_context]` enables
    // it, and deliberately not part of `build_state_with`: tests must not
    // acquire a background loop that writes to their database.
    if svc.config.magic_context.enabled && svc.config.magic_context.dreamer_enabled {
        mctx::dreamer::Dreamer::new(
            state.db.clone(),
            Arc::clone(&svc.config),
            state.runner.chat(),
            Arc::clone(&svc.model_catalog),
        )
        .spawn();
    }
    // A fold that was in flight when the previous process died left its
    // `mc_historian_run` row `running`, and "one pending run per session" reads
    // that row as a claim: without settling it here, every session that was
    // folding at exit would never fold again.
    match mctx::historian::settle_running_runs(&state.db).await {
        Ok(0) => {}
        Ok(settled) => tracing::info!(settled, "settled interrupted historian runs"),
        Err(error) => {
            tracing::warn!(error = %error, "settle interrupted historian runs failed");
        }
    }
    // Replay-log retention: the `event` table is history for SSE reconnect, not
    // state, so old rows are swept on a fixed window (`[events] retention_days`,
    // 0 keeps everything). Deliberately started here rather than in
    // `build_state_with`: tests must not acquire a background writer.
    event_bus::spawn_pruner(state.db.clone(), svc.config.events.retention_days);
    // Session trees past `[sessions] retention_days` (0 keeps everything) are
    // deleted by the same path as `DELETE /api/session/{id}`. Started here for
    // the same reason as the pruner: tests must not acquire a background writer.
    session_retention::spawn_sweeper(state.clone(), svc.config.sessions.retention_days);
    // `<ox_home>/tmp` (the dir `build_state_with` creates) is swept on
    // `[tmp] retention_days` (0 keeps everything); started here for the same
    // reason: tests must not acquire a background deleter.
    tmp_retention::spawn_sweeper(
        ox_core::config::ox_home().join("tmp"),
        svc.config.tmp.retention_days,
    );
    let router = build_router(state.clone());
    Ok((state, router))
}

/// Emit runner-owned table sizes alongside the binary's allocator statistics.
/// Disabled unless `OX_MEMORY_STATS_MS` selects a positive interval.
fn spawn_runner_memory_stats(runner: Runner) {
    let Ok(raw) = std::env::var("OX_MEMORY_STATS_MS") else {
        return;
    };
    let Ok(milliseconds) = raw.parse::<u64>() else {
        return;
    };
    if milliseconds == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(milliseconds));
        loop {
            interval.tick().await;
            let (sessions, baselines) = runner.diagnostic_table_sizes();
            tracing::info!(sessions, baselines, "OX_RUNNER_STATS");
        }
    });
}

/// Serve the composed router on an already-bound listener — the `ox acp`
/// own-mode seam (the ephemeral acceptor is bound by the caller so the real
/// port is known before the loop starts).
pub async fn serve_on(
    acceptor: impl salvo::conn::Acceptor,
    state: AppState,
    router: Router,
) -> Result<()> {
    let _ = state; // signature symmetry: state owns the runtime handles, the router holds them
    Server::new(acceptor).serve(router).await;
    Ok(())
}
