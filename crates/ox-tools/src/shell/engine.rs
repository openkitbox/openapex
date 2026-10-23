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
//! - **Smart worktree (`PI_SMART_GIT`)**: on by default unless the host env
//!   says otherwise. `git worktree add` then copy-on-write clones the source
//!   checkout through pi-vcs — ignored build caches (target/, node_modules/)
//!   come along, so a parallel worktree does not rebuild from zero — and
//!   anything the layer does not support falls through to the plain git
//!   binary. `PI_GIT_GUARD` (refusing stash/reset --hard/checkout in a shared
//!   checkout) stays opt-in: the user decided it defaults to off, and the
//!   host env passes straight through when they set it.

use std::collections::HashMap;

/// One embedded brush session configured for ox.
pub fn shell() -> pi_shell::Shell {
    pi_shell::Shell::new(Some(pi_shell::ShellOptions {
        session_env: Some(env_snapshot()),
        minimizer: Some(minimizer_options()),
        ..Default::default()
    }))
}

/// The host process environment, frozen for one session, plus ox's engine
/// defaults for unset pi-shell switches.
///
/// pi-shell reads each switch from the session env first and the process env
/// second, and treats `""`, `"0"` and `"false"` as off — so a host setting of
/// `PI_SMART_GIT=0` survives this default instead of being overridden.
fn env_snapshot() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.entry("PI_SMART_GIT".to_owned())
        .or_insert_with(|| "1".to_owned());
    env
}

/// Default-on minimizer: every built-in filter active, engine defaults for
/// capture caps and outline levels.
fn minimizer_options() -> pi_shell::minimizer::MinimizerOptions {
    pi_shell::minimizer::MinimizerOptions {
        enabled: Some(true),
        ..Default::default()
    }
}
