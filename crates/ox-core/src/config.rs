//! Configuration loaded from `~/.ox/ox.toml` with environment-variable overrides.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub use ox_llm::config::{
    CandidateConfig, DEFAULT_LIMITED_STALL, DEFAULT_PROVIDER_NAME, EndpointLimits, LlmConfig,
    ModelRouteConfig, ProviderConfig,
};
pub use ox_llm::model_catalog::{ModelEntry, ReasoningLevel};

const DEFAULT_LISTEN: &str = "127.0.0.1:8787";

/// Environment variable overrides.
pub const ENV_DB_PATH: &str = "OX_DB_PATH";
pub const ENV_LISTEN: &str = "OX_LISTEN";
pub const ENV_SUBAGENT_DEPTH: &str = "OX_SUBAGENT_DEPTH";

fn default_listen() -> String {
    DEFAULT_LISTEN.to_owned()
}

fn default_db_path() -> PathBuf {
    ox_home().join("ox.db")
}

/// MCP integration (`[mcp]` section). Servers are keyed by name; the name is
/// only used for display and as the tool-name prefix, never as a filesystem
/// path.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// One MCP server. The wire shape mirrors upstream `Mcp.ServerConfig`
/// (`reference/opencode/packages/schema/src/mcp.ts`): a `type` tag plus the
/// transport-specific fields.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Local(McpLocalConfig),
    Remote(McpRemoteConfig),
}

/// MCP protocol negotiation mode. `auto` is the default: try modern discovery
/// first, then fall back to legacy only when modern is unsupported.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub enum McpProtocol {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "legacy")]
    Legacy,
    #[serde(rename = "2026-07-28")]
    Modern,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct McpLocalConfig {
    /// argv, not a shell string: no quoting rules, no injection surface.
    pub command: Vec<String>,
    /// Relative paths resolve from the server's workspace directory.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub environment: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub protocol: McpProtocol,
    #[serde(default)]
    pub timeout: McpTimeout,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct McpRemoteConfig {
    pub url: String,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub protocol: McpProtocol,
    #[serde(default)]
    pub timeout: McpTimeout,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct McpTimeout {
    /// Whole startup budget: spawn + initialize + discovery.
    #[serde(default = "default_mcp_startup_ms")]
    pub startup: u64,
    /// Per `tools/list` / `resources/*` request.
    #[serde(default = "default_mcp_catalog_ms")]
    pub catalog: u64,
    /// Per `tools/call`. `None` means no wall clock — only turn cancellation.
    #[serde(default)]
    pub execution: Option<u64>,
}

fn default_mcp_startup_ms() -> u64 {
    30_000
}

fn default_mcp_catalog_ms() -> u64 {
    30_000
}

impl Default for McpTimeout {
    fn default() -> Self {
        Self {
            startup: default_mcp_startup_ms(),
            catalog: default_mcp_catalog_ms(),
            execution: None,
        }
    }
}

impl McpServerConfig {
    #[must_use]
    pub fn disabled(&self) -> bool {
        match self {
            Self::Local(local) => local.disabled,
            Self::Remote(remote) => remote.disabled,
        }
    }

    #[must_use]
    pub fn timeout(&self) -> &McpTimeout {
        match self {
            Self::Local(local) => &local.timeout,
            Self::Remote(remote) => &remote.timeout,
        }
    }

    #[must_use]
    pub fn protocol(&self) -> McpProtocol {
        match self {
            Self::Local(local) => local.protocol,
            Self::Remote(remote) => remote.protocol,
        }
    }
}

impl McpConfig {
    /// Reject configurations that can never connect. Server-side failures
    /// (missing binary, unreachable URL) are runtime statuses, not config
    /// errors; these are the shapes that are wrong before any I/O happens.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, server) in &self.servers {
            if name.trim().is_empty() {
                anyhow::bail!("mcp server names must not be empty");
            }
            match server {
                McpServerConfig::Local(local) if local.command.is_empty() => {
                    anyhow::bail!("mcp server `{name}`: `command` must not be empty");
                }
                McpServerConfig::Remote(remote)
                    if !(remote.url.starts_with("http://")
                        || remote.url.starts_with("https://")) =>
                {
                    anyhow::bail!("mcp server `{name}`: `url` must start with http:// or https://");
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Subagent behavior (`[subagent]` section).
#[derive(Debug, Clone, Deserialize)]
pub struct SubagentConfig {
    /// Maximum nesting depth. A session with this many ancestor hops (along
    /// `parent_id`) may not spawn another subagent. `1` matches upstream's
    /// `experimental.subagent_depth` default: root→child is allowed,
    /// child→grandchild is not. `0` forbids subagents entirely.
    #[serde(default = "default_subagent_depth")]
    pub depth: usize,
    /// Maximum number of subagent child sessions running at once, across the
    /// whole server. Root foreground calls wait FIFO when full; nested
    /// foreground calls fail with Capacity instead of waiting while ancestors
    /// hold slots. Background calls may queue at any depth. Resuming an already
    /// active child needs no additional slot. Must be at least `1`.
    #[serde(default = "default_subagent_max_active")]
    pub max_active: usize,
}

fn default_subagent_depth() -> usize {
    1
}

fn default_subagent_max_active() -> usize {
    5
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            depth: default_subagent_depth(),
            max_active: default_subagent_max_active(),
        }
    }
}

impl SubagentConfig {
    /// Reject `max_active = 0`: with no slots every subagent call would queue
    /// forever, so the shape is wrong before anything runs (fail closed).
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_active >= 1,
            "[subagent] max_active must be at least 1 (0 would queue every subagent forever)"
        );
        Ok(())
    }
}

/// Event-log retention (`[events]` section). The `event` table is the SSE
/// replay log — the read models it feeds are written in the same transaction —
/// so dropping old rows costs only replay depth, never state.
#[derive(Debug, Clone, Deserialize)]
pub struct EventsConfig {
    /// Days of replay events to keep. `0` keeps everything, which is what
    /// upstream does; the default keeps a week.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
}

fn default_retention_days() -> u64 {
    7
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
        }
    }
}

/// Session retention (`[sessions]` section). Whole session trees (a root and
/// its subagent descendants) whose last activity is older than the window are
/// deleted; upstream never deletes sessions, so `0` restores that behavior.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionsConfig {
    /// Days a session tree may sit idle before it is deleted. `0` keeps
    /// everything.
    #[serde(default = "default_session_retention_days")]
    pub retention_days: u32,
}

fn default_session_retention_days() -> u32 {
    30
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            retention_days: default_session_retention_days(),
        }
    }
}

/// Scratch-directory retention (`[tmp]` section). A top-level entry of
/// `~/.ox/tmp` is deleted only when the newest file inside it is older than
/// the window.
#[derive(Debug, Clone, Deserialize)]
pub struct TmpConfig {
    /// Days a top-level `~/.ox/tmp` entry may sit untouched (by its newest
    /// file) before it is deleted. `0` disables cleanup. The server clamps
    /// the upper bound.
    #[serde(default = "default_tmp_retention_days")]
    pub retention_days: u32,
}

fn default_tmp_retention_days() -> u32 {
    7
}

impl Default for TmpConfig {
    fn default() -> Self {
        Self {
            retention_days: default_tmp_retention_days(),
        }
    }
}

/// Per-turn step cap (`[turn]` section). A step is one model call within a
/// turn; a turn keeps stepping while the model requests tools. After
/// `max_steps` steps the next step is a tool-less wrap-up step, so the turn
/// ends with a text report instead of being cut off. `0` means unlimited.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnConfig {
    /// Consecutive model steps allowed in one turn before the wrap-up step.
    /// `0` removes the cap.
    #[serde(default = "default_turn_max_steps")]
    pub max_steps: u32,
}

fn default_turn_max_steps() -> u32 {
    500
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            max_steps: default_turn_max_steps(),
        }
    }
}

/// What happens when the model calls a tool (`[permissions]` policies).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// Run the call.
    #[default]
    Allow,
    /// Put the call to the user first; a refusal or no answer fails the call.
    Ask,
    /// Fail the call without running it.
    Deny,
}

/// One tool's entry under `[permissions.tools]`: either a bare policy
/// (`shell = "ask"`) or a table with patterns that escalate an `allow` to a
/// prompt (`shell = { policy = "allow", ask_if = ["rm -rf", "git push"] }`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolPermission {
    Policy(Permission),
    Rule(ToolPermissionRule),
}

/// The table form of a [`ToolPermission`].
#[derive(Debug, Clone, Deserialize)]
pub struct ToolPermissionRule {
    /// The base policy; `allow` unless stated.
    #[serde(default)]
    pub policy: Permission,
    /// Regular expressions matched against the call's compact JSON arguments
    /// (for `shell`, that text contains the command). A match on an `allow`
    /// policy turns that one call into `ask`; patterns never relax a policy.
    /// Compiled at load, so a malformed pattern fails the config instead of
    /// a tool call.
    #[serde(default, deserialize_with = "deserialize_regexes")]
    pub ask_if: Vec<regex::Regex>,
}

fn deserialize_regexes<'de, D>(deserializer: D) -> Result<Vec<regex::Regex>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let patterns: Vec<String> = Vec::deserialize(deserializer)?;
    patterns
        .iter()
        .map(|pattern| {
            regex::Regex::new(pattern).map_err(|error| {
                serde::de::Error::custom(format!(
                    "permissions ask_if pattern `{pattern}` is not a valid regex: {error}"
                ))
            })
        })
        .collect()
}

/// Tool permissions (`[permissions]` section). Every tool call — built-in,
/// `ctx_*` or MCP — is checked against this table before it runs. The
/// default is `allow` for everything, which is the pre-permission behaviour;
/// a user opts into prompting or blocking per tool:
///
/// ```toml
/// [permissions]
/// default = "allow"
///
/// [permissions.tools]
/// shell = { ask_if = ["rm -rf", "git push", "sudo"] }
/// webfetch = "ask"
/// write = "deny"
/// ```
///
/// An `ask` prompt is a form on the session's root (the window the user is
/// looking at), with "allow once" / "allow for this session" / "deny". A
/// child session's call therefore asks on its parent's screen rather than
/// hanging on a form no client shows.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PermissionsConfig {
    /// Policy for a tool with no entry under `tools`.
    #[serde(default)]
    pub default: Permission,
    /// Per-tool policies, keyed by the advertised tool name (an MCP tool by
    /// its prefixed name, e.g. `github_create_issue`).
    #[serde(default)]
    pub tools: BTreeMap<String, ToolPermission>,
}

impl PermissionsConfig {
    /// The policy for one call: the tool's entry (or `default`), escalated to
    /// [`Permission::Ask`] when the entry's `ask_if` matches `arguments`.
    #[must_use]
    pub fn policy_for(&self, tool: &str, arguments: &str) -> Permission {
        match self.tools.get(tool) {
            None => self.default,
            Some(ToolPermission::Policy(policy)) => *policy,
            Some(ToolPermission::Rule(rule)) => match rule.policy {
                Permission::Allow if rule.ask_if.iter().any(|re| re.is_match(arguments)) => {
                    Permission::Ask
                }
                policy => policy,
            },
        }
    }

    /// Whether any call could ever prompt or be refused — `false` for the
    /// default table, so the gate can skip the lookup entirely.
    #[must_use]
    pub fn is_permissive(&self) -> bool {
        self.default == Permission::Allow
            && self.tools.values().all(|entry| match entry {
                ToolPermission::Policy(policy) => *policy == Permission::Allow,
                ToolPermission::Rule(rule) => {
                    rule.policy == Permission::Allow && rule.ask_if.is_empty()
                }
            })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// LLM endpoint configuration. Flattened: its keys (`cardinal_url`,
    /// `[providers]`, `[models]`, `[context_caps]`, `default_model`, …) stay
    /// at the top level of `ox.toml`.
    #[serde(flatten)]
    pub llm: LlmConfig,
    /// Path to the SQLite database file.
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    /// Address the HTTP server listens on.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Heartbeat interval for the `event.subscribe` SSE stream, in
    /// milliseconds. Upstream hardcodes 15 seconds
    /// (`Stream.tick("15 seconds")`); the default matches. Exposed for
    /// tests and tuning.
    #[serde(default = "default_event_heartbeat_ms")]
    pub event_heartbeat_ms: u64,
    /// magic-context integration (`[magic_context]` section). Disabled by
    /// default; while off, the server behaves exactly as if the module did
    /// not exist.
    #[serde(default)]
    pub magic_context: MctxConfig,
    /// MCP servers (`[mcp]` section). Empty by default: an ox without MCP
    /// configuration advertises the same toolset as before.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Subagent behavior (`[subagent]` section).
    #[serde(default)]
    pub subagent: SubagentConfig,
    /// Replay-log retention (`[events]` section).
    #[serde(default)]
    pub events: EventsConfig,
    /// Session retention (`[sessions]` section).
    #[serde(default)]
    pub sessions: SessionsConfig,
    /// Scratch-directory retention (`[tmp]` section).
    #[serde(default)]
    pub tmp: TmpConfig,
    /// Per-turn step cap (`[turn]` section).
    #[serde(default)]
    pub turn: TurnConfig,
    /// Tool permissions (`[permissions]` section). Allow-all by default.
    #[serde(default)]
    pub permissions: PermissionsConfig,
}

fn default_event_heartbeat_ms() -> u64 {
    15_000
}

impl Default for Config {
    fn default() -> Self {
        Self {
            llm: LlmConfig::default(),
            db_path: default_db_path(),
            listen: default_listen(),
            event_heartbeat_ms: default_event_heartbeat_ms(),
            magic_context: MctxConfig::default(),
            mcp: McpConfig::default(),
            subagent: SubagentConfig::default(),
            events: EventsConfig::default(),
            sessions: SessionsConfig::default(),
            tmp: TmpConfig::default(),
            turn: TurnConfig::default(),
            permissions: PermissionsConfig::default(),
        }
    }
}

/// The user's home directory, falling back to `.` when it cannot be
/// determined.
///
/// This is the one home source for model-visible paths: injected text renders
/// it as `~` and [`crate::home_paths::expand_tilde`] expands it back, so a
/// redacted path stays resolvable.
#[must_use]
pub fn user_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// The ox home directory (`~/.ox`), falling back to `./.ox` when the home
/// directory cannot be determined.
#[must_use]
pub fn ox_home() -> PathBuf {
    user_home().join(".ox")
}

/// A parse failure naming the file, the toml message and the line/column,
/// without toml's source-line snippet (the line could hold `cardinal_key`).
fn parse_error(path: &Path, raw: &str, error: &toml::de::Error) -> anyhow::Error {
    let at = error.span().map_or(String::new(), |span| {
        let before = &raw.as_bytes()[..span.start.min(raw.len())];
        let line = before.iter().filter(|b| **b == b'\n').count() + 1;
        let column = before.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        format!(" (line {line}, column {column})")
    });
    anyhow::anyhow!(
        "failed to parse config file {}: {}{at}",
        path.display(),
        error.message()
    )
}

impl Config {
    /// Default config file location: `~/.ox/ox.toml`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        ox_home().join("ox.toml")
    }

    /// Load config from the default path, then apply env overrides.
    /// A missing config file is fine — defaults apply.
    pub fn load() -> Result<Self> {
        Self::load_from(Self::default_path())
    }

    /// Load config from an explicit path, then apply env overrides.
    ///
    /// Stored credentials (the `credential` table `/connect` writes) are
    /// **not** applied here: opening the database is async and happens after
    /// this synchronous load, so the server calls
    /// `LlmConfig::apply_credentials` itself once the DB is open. Until then a
    /// builtin provider with only a stored key is intentionally absent.
    pub fn load_from(path: PathBuf) -> Result<Self> {
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {}", path.display()))?;
            toml::from_str(&raw).map_err(|error| parse_error(&path, &raw, &error))?
        } else {
            Config::default()
        };

        config.llm.apply_env();
        if let Ok(db) = std::env::var(ENV_DB_PATH) {
            config.db_path = PathBuf::from(db);
        }
        if let Ok(listen) = std::env::var(ENV_LISTEN) {
            config.listen = listen;
        }
        if let Ok(raw) = std::env::var(ENV_SUBAGENT_DEPTH) {
            match raw.trim().parse::<usize>() {
                Ok(depth) => config.subagent.depth = depth,
                Err(_) => tracing::warn!(
                    value = %raw,
                    "OX_SUBAGENT_DEPTH is not a non-negative integer; keeping configured depth"
                ),
            }
        }

        config.magic_context = config.magic_context.validated();
        config.mcp.validate()?;
        config.llm.validate()?;
        config.subagent.validate()?;
        Ok(config)
    }

    /// Restore every key that cannot be hot-swapped to its running value; return the
    /// names of the keys whose new value was rejected (empty when nothing differed).
    ///
    /// Names come back in a fixed order: `listen`, `db_path`, `event_heartbeat_ms`,
    /// `events`, `sessions`, `tmp`, `turn`, `subagent`, `magic_context`. `llm` and `mcp` are
    /// hot-swappable and never touched. Clones happen only for keys that differ, at a reload
    /// boundary.
    pub fn pin_restart_only(&mut self, running: &Config) -> Vec<&'static str> {
        let mut rejected = Vec::new();
        if self.listen != running.listen {
            self.listen.clone_from(&running.listen);
            rejected.push("listen");
        }
        if self.db_path != running.db_path {
            self.db_path.clone_from(&running.db_path);
            rejected.push("db_path");
        }
        if self.event_heartbeat_ms != running.event_heartbeat_ms {
            self.event_heartbeat_ms = running.event_heartbeat_ms;
            rejected.push("event_heartbeat_ms");
        }
        if self.events.retention_days != running.events.retention_days {
            self.events = running.events.clone();
            rejected.push("events");
        }
        if self.sessions.retention_days != running.sessions.retention_days {
            self.sessions = running.sessions.clone();
            rejected.push("sessions");
        }
        if self.tmp.retention_days != running.tmp.retention_days {
            self.tmp.retention_days = running.tmp.retention_days;
            rejected.push("tmp");
        }
        if self.turn.max_steps != running.turn.max_steps {
            self.turn.max_steps = running.turn.max_steps;
            rejected.push("turn");
        }
        if self.subagent.depth != running.subagent.depth
            || self.subagent.max_active != running.subagent.max_active
        {
            self.subagent = running.subagent.clone();
            rejected.push("subagent");
        }
        if self.magic_context != running.magic_context {
            self.magic_context.clone_from(&running.magic_context);
            rejected.push("magic_context");
        }
        rejected
    }
}

// ---------------------------------------------------------------------------
// [magic_context] section
// ---------------------------------------------------------------------------

/// Bounds of the fold trigger, in percent of the model window. They belong
/// with the config value they constrain: `budget` derives every band from it.
pub const MIN_EXECUTE_PCT: f64 = 20.0;
pub const MAX_EXECUTE_PCT: f64 = 90.0;

/// Bounds of the fold's low-water target, in percent of the model window. The
/// lower floor prevents a zero target; the derived budget additionally caps the
/// effective target below the proactive floor, so a high configured target
/// cannot re-arm folding at every step boundary.
pub const MIN_FOLD_TARGET_PCT: f64 = 5.0;
pub const MAX_FOLD_TARGET_PCT: f64 = 80.0;

/// Bounds of the `m[0]` history share.
pub const MIN_HISTORY_SHARE: f64 = 0.05;
pub const MAX_HISTORY_SHARE: f64 = 0.5;

/// The `[magic_context]` section of `ox.toml`. Every field tolerates a
/// missing key; the section itself is optional and defaults to enabled.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MctxConfig {
    /// Master switch. On by default: the integration is what ox is for, and a
    /// silent default-off made a fresh install look like a plain chat server.
    /// `enabled = false` turns the whole thing (pinboard, §N§ overlay, frozen
    /// head, historian) back into a pass-through.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Model backing historian (and dreamer) sub-completions, as a
    /// qualified `provider/id` reference. When unset, ox's existing default
    /// model resolution applies.
    #[serde(default)]
    pub historian_model: Option<String>,
    /// Context usage (percent of the model window) at which folding is due.
    /// Everything else — the proactive floor, the force band, the protected
    /// tail, the trigger and history budgets — is derived from it, so this is
    /// the knob the numbers hang off; `fold_target_percentage` below sets only
    /// where a pressure event stops folding.
    #[serde(default = "default_execute_threshold_percentage")]
    pub execute_threshold_percentage: f64,
    /// The fold's low-water target, as a percent of the model window: after a
    /// fold the historian keeps folding (within the same pressure event) until
    /// the estimated prompt is at or below it. One fold per event only removes
    /// one chunk, so a tail that grew far above the target would otherwise sit
    /// above it until the next event — which is what made folds fire back to
    /// back in production. The derived target is additionally kept five points
    /// below the proactive floor (`execute_threshold_percentage - 2`), so a
    /// configured target at or above that floor cannot re-arm every step.
    #[serde(default = "default_fold_target_percentage")]
    pub fold_target_percentage: f64,
    /// Share of the usable window the `m[0]` history block may carry. Upstream
    /// default 0.15, range 0.05–0.5.
    #[serde(default = "default_history_budget_percentage")]
    pub history_budget_percentage: f64,
    /// **Deprecated, ignored** (kept only so an old `ox.toml` is warned about
    /// instead of silently misread): the bands now come from
    /// [`Self::execute_threshold_percentage`].
    #[serde(default = "default_soft_threshold")]
    pub soft_threshold: f32,
    /// **Deprecated, ignored** — see [`Self::soft_threshold`].
    #[serde(default = "default_hard_threshold")]
    pub hard_threshold: f32,
    /// Whether the dreamer (the scheduled memory-maintenance loop) runs. On by
    /// default with the module: it only issues a hidden completion when a
    /// project actually holds memories whose cited files changed.
    #[serde(default = "default_enabled")]
    pub dreamer_enabled: bool,
    /// Whether the historian's hidden completion reuses the live request's wire
    /// prefix instead of sending the chunk as an independent two-message
    /// request. Off by default, because the independent shape already measures
    /// well: on DeepSeek-V4.1-Flash the fold's chunk is served ~95–97% from the
    /// provider cache, the endpoint matching repeated spans rather than only a
    /// request prefix.
    ///
    /// Turn it on for an endpoint whose cache is prefix-only (the OpenAI and
    /// Gemini families document that contract): there the independent chunk is
    /// a full miss while the prefixed shape re-reads the conversation's opening
    /// bytes at the cache price. Measured on DeepSeek, the prefixed shape served
    /// 97.9% of a 28.9k-token fold from cache but sent ~1.7× the tokens the
    /// independent shape sends, so it is not a win where spans already hit.
    #[serde(default)]
    pub historian_prefix_cache: bool,
    /// Semantic search for project memories (off unless configured).
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    /// Dreamer tick cadence in minutes. The reference's process-wide timer
    /// ticks every 15 minutes (`docs/architecture/dreamer.md`), the same
    /// default here; its per-task cron schedules are deliberately not ported
    /// (see [`crate::mctx::dreamer`]), so this one interval drives every task.
    #[serde(default = "default_dreamer_interval_minutes")]
    pub dreamer_interval_minutes: u64,
    /// Tools whose output is content from outside the project — web pages,
    /// remote documents — rather than the repository or the user. Once one of
    /// them has run in a session, the session's historian folds still
    /// summarize but no longer promote facts into project memory: a fact
    /// stated by a fetched page is not knowledge about this project. Names
    /// match exactly; a trailing `*` matches a prefix (MCP tools are named
    /// `server_tool`, so `docs_*` covers one server). Default: `webfetch`.
    #[serde(default = "default_external_content_tools")]
    pub external_content_tools: Vec<String>,
}

impl MctxConfig {
    /// Whether `tool` is one of [`Self::external_content_tools`].
    #[must_use]
    pub fn is_external_content_tool(&self, tool: &str) -> bool {
        self.external_content_tools
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => tool.starts_with(prefix),
                None => pattern == tool,
            })
    }

    /// The config with its invariants enforced: the new budget numbers are
    /// clamped into their documented ranges, the deprecated threshold pair is
    /// warned about and dropped, and a dreamer requested while the module is
    /// off is turned back off.
    ///
    /// Clamping (not rejecting) is the established treatment for a
    /// `[magic_context]` number: a bad value must not stop the server, and a
    /// degenerate one (a zero window share) must not change when the historian
    /// runs.
    pub fn validated(mut self) -> Self {
        // The bands are derived from the execute threshold, so an out-of-range
        // value would silently move every budget with it.
        // The ranges are owned by `budget`: they are the ranges its arithmetic
        // is written against.
        if !(MIN_EXECUTE_PCT..=MAX_EXECUTE_PCT).contains(&self.execute_threshold_percentage) {
            tracing::warn!(
                setting = "execute_threshold_percentage",
                value = self.execute_threshold_percentage,
                "invalid [magic_context] execute threshold (expected {}..={}); using the default",
                MIN_EXECUTE_PCT,
                MAX_EXECUTE_PCT
            );
            self.execute_threshold_percentage = default_execute_threshold_percentage();
        }
        if !(MIN_FOLD_TARGET_PCT..=MAX_FOLD_TARGET_PCT).contains(&self.fold_target_percentage) {
            tracing::warn!(
                setting = "fold_target_percentage",
                value = self.fold_target_percentage,
                "invalid [magic_context] fold target (expected {}..={}); using the default",
                MIN_FOLD_TARGET_PCT,
                MAX_FOLD_TARGET_PCT
            );
            self.fold_target_percentage = default_fold_target_percentage();
        }
        if !(MIN_HISTORY_SHARE..=MAX_HISTORY_SHARE).contains(&self.history_budget_percentage) {
            tracing::warn!(
                setting = "history_budget_percentage",
                value = self.history_budget_percentage,
                "invalid [magic_context] history budget share (expected {}..={}); using the default",
                MIN_HISTORY_SHARE,
                MAX_HISTORY_SHARE
            );
            self.history_budget_percentage = default_history_budget_percentage();
        }
        // A deprecated key that was actually set is a real misconfiguration:
        // the operator believes they tuned the bands and nothing reads them.
        for (name, set) in [
            (
                "soft_threshold",
                self.soft_threshold != default_soft_threshold(),
            ),
            (
                "hard_threshold",
                self.hard_threshold != default_hard_threshold(),
            ),
        ] {
            if set {
                tracing::warn!(
                    setting = name,
                    "deprecated [magic_context] key ignored; the pressure bands now follow \
                     execute_threshold_percentage"
                );
            }
        }
        // The interval is enforced apart from everything else: a zero interval
        // would make every dreamer task due on every tick, i.e. a hot loop of
        // hidden completions at the tick loop's (immediate) cadence. One minute
        // is the smallest sane cadence, so anything below it falls back too.
        if self.dreamer_interval_minutes < 1 {
            tracing::warn!(
                setting = "dreamer_interval_minutes",
                value = self.dreamer_interval_minutes,
                "invalid [magic_context] dreamer interval (expected a positive number of \
                 minutes); using the default"
            );
            self.dreamer_interval_minutes = default_dreamer_interval_minutes();
        }
        // The dreamer lives inside the module: requesting it while the module
        // is off can never run it, so it is turned back off with a warning
        // instead of staying a silently inert `true`.
        if self.dreamer_enabled && !self.enabled {
            tracing::warn!(
                setting = "dreamer_enabled",
                "enabled [magic_context] dreamer while the module is disabled; the dreamer \
                 cannot run and stays off"
            );
            self.dreamer_enabled = false;
        }
        self
    }
}

#[must_use]
pub fn default_execute_threshold_percentage() -> f64 {
    55.0
}
#[must_use]
pub fn default_fold_target_percentage() -> f64 {
    28.0
}
#[must_use]
pub fn default_history_budget_percentage() -> f64 {
    0.15
}
fn default_external_content_tools() -> Vec<String> {
    vec!["webfetch".to_owned()]
}

fn default_soft_threshold() -> f32 {
    0.70
}

fn default_hard_threshold() -> f32 {
    0.85
}

fn default_dreamer_interval_minutes() -> u64 {
    15
}

impl Default for MctxConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            historian_model: None,
            execute_threshold_percentage: default_execute_threshold_percentage(),
            fold_target_percentage: default_fold_target_percentage(),
            history_budget_percentage: default_history_budget_percentage(),
            soft_threshold: default_soft_threshold(),
            hard_threshold: default_hard_threshold(),
            dreamer_enabled: default_enabled(),
            historian_prefix_cache: false,
            embedding: EmbeddingConfig::default(),
            dreamer_interval_minutes: default_dreamer_interval_minutes(),
            external_content_tools: default_external_content_tools(),
        }
    }
}

/// The module's default state: on.
fn default_enabled() -> bool {
    true
}

/// `[magic_context.embedding]`: where memory vectors come from.
///
/// Only the OpenAI embeddings shape is supported (`POST {endpoint}/embeddings`,
/// `{"model", "input"}` → `data[].embedding`): it is what every provider ox is
/// deployed against speaks, and a dialect layer for one request shape would be
/// more machinery than the four fields it replaces.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EmbeddingConfig {
    /// `"off"` (default) keeps `ctx_search` lexical.
    #[serde(default)]
    pub provider: EmbeddingProvider,
    /// Base URL, without the trailing `/embeddings` (e.g.
    /// `https://open.bigmodel.cn/api/paas/v4`).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Bearer token for the endpoint.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Embedding model id (e.g. `embedding-3`).
    #[serde(default)]
    pub model: Option<String>,
    /// Prefix for the *query* text. Instruction-tuned models (Qwen3, e5, gte,
    /// Nomic) expect a task instruction on the query side but not on stored
    /// passages; leaving it unset sends both sides bare.
    #[serde(default)]
    pub query_instruction: Option<String>,
    /// Prefix for *stored* text. Changing it changes every stored vector, so
    /// the model identity written with a vector includes it.
    #[serde(default)]
    pub document_prefix: Option<String>,
}

impl EmbeddingConfig {
    /// Whether semantic search can run at all.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.provider == EmbeddingProvider::OpenAiCompatible
            && self.endpoint.as_deref().is_some_and(|v| !v.is_empty())
            && self.model.as_deref().is_some_and(|v| !v.is_empty())
    }

    /// The identity written beside a stored vector: the model plus the
    /// document prefix, because the same model with a different prefix
    /// produces an incomparable vector space.
    #[must_use]
    pub fn identity(&self) -> String {
        match self.document_prefix.as_deref().filter(|p| !p.is_empty()) {
            Some(prefix) => format!("{}|{prefix}", self.model.as_deref().unwrap_or("")),
            None => self.model.clone().unwrap_or_default(),
        }
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::Off,
            endpoint: None,
            api_key: None,
            model: None,
            query_instruction: None,
            document_prefix: None,
        }
    }
}

/// Which embedding backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingProvider {
    /// No vectors: `ctx_search` stays lexical.
    #[default]
    Off,
    /// Any endpoint speaking the OpenAI `/embeddings` shape.
    ///
    /// Spelled `openai-compatible`, which `rename_all = "kebab-case"` would
    /// split into `open-ai-compatible`.
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}
