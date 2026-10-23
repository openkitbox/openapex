//! The pi-shell engine behind the session table: one embedded brush session
//! per ox shell session.
//!
//! ox pins `pi-shell` to an oh-my-pi rev (root `Cargo.toml`). Two deliberate
//! choices live here (the deviation ledger in
//! `.opencode/plans/ox-pi-shell-integration.md` has the full list):
//!
//! - **Environment**: pi-shell never inherits implicitly — its session
//!   creation hard-codes `do_not_inherit_env` — so ox snapshots the host
//!   process env at session spawn and passes it as `session_env`, preserving
//!   the old `sh -c` behaviour where the agent shares the user's PATH,
//!   version managers, and credentials.
//! - **Minimizer**: enabled by default. The streamed chunks and the
//!   `/api/shell` pages stay raw; only the model-facing settled text is the
//!   minimized rendering when the engine produced one.

use std::collections::HashMap;

/// One embedded brush session configured for ox.
pub fn shell() -> pi_shell::Shell {
    pi_shell::Shell::new(Some(pi_shell::ShellOptions {
        session_env: Some(env_snapshot()),
        minimizer: Some(minimizer_options()),
        ..Default::default()
    }))
}

/// The host process environment, frozen for one session.
fn env_snapshot() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// Default-on minimizer: every built-in filter active, engine defaults for
/// capture caps and outline levels.
fn minimizer_options() -> pi_shell::minimizer::MinimizerOptions {
    pi_shell::minimizer::MinimizerOptions {
        enabled: Some(true),
        ..Default::default()
    }
}
