//! Configuration loaded from `~/.ox/ox.toml` with environment-variable overrides.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::mctx::MctxConfig;

const DEFAULT_CARDINAL_URL: &str = "http://127.0.0.1:10202";
const DEFAULT_LISTEN: &str = "127.0.0.1:8787";

/// Environment variable overrides.
pub const ENV_CARDINAL_URL: &str = "OX_CARDINAL_URL";
pub const ENV_CARDINAL_KEY: &str = "OX_CARDINAL_KEY";
pub const ENV_DB_PATH: &str = "OX_DB_PATH";
pub const ENV_LISTEN: &str = "OX_LISTEN";
pub const ENV_DEFAULT_MODEL: &str = "OX_DEFAULT_MODEL";

fn default_cardinal_url() -> String {
    DEFAULT_CARDINAL_URL.to_owned()
}

fn default_listen() -> String {
    DEFAULT_LISTEN.to_owned()
}

fn default_db_path() -> PathBuf {
    ox_home().join("ox.db")
}

/// One model as the client sees it, built from the gateway's model list
/// ([`crate::model_catalog`]).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModelEntry {
    /// Provider identifier, e.g. `anthropic`.
    pub provider: String,
    /// Model identifier, e.g. `claude-sonnet-4-5`.
    pub id: String,
    /// Human-readable display name.
    pub name: Option<String>,
    /// Context window in tokens. Drives the client's context-usage
    /// percentage (it divides the last assistant turn's tokens by this), so a
    /// missing value means the client shows a raw token count with no
    /// percentage rather than a wrong one.
    #[serde(default)]
    pub context: Option<i64>,
    /// Maximum output tokens, when the provider states one.
    #[serde(default)]
    pub output: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Base URL of the cardinal LLM gateway (cardinal's `listen`, e.g.
    /// `http://127.0.0.1:10202`).
    #[serde(default = "default_cardinal_url")]
    pub cardinal_url: String,
    /// API key for the cardinal gateway (its `gateway_key`). Required unless
    /// cardinal runs with auth disabled.
    #[serde(default)]
    pub cardinal_key: Option<String>,
    /// Path to the SQLite database file.
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    /// Address the HTTP server listens on.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Qualified model reference (`provider/id`, e.g. `cardinal/k3-256k`)
    /// backing sessions without an explicit selection. The catalogue itself is
    /// fetched from the gateway (`GET /v1/models`); this pins only which entry
    /// to use by default, and a name the gateway does not report is sent
    /// verbatim rather than rejected. When unset the first catalogue entry is
    /// the default.
    #[serde(default)]
    pub default_model: Option<String>,
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
}

fn default_event_heartbeat_ms() -> u64 {
    15_000
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cardinal_url: default_cardinal_url(),
            cardinal_key: None,
            db_path: default_db_path(),
            listen: default_listen(),
            default_model: None,
            event_heartbeat_ms: default_event_heartbeat_ms(),
            magic_context: MctxConfig::default(),
        }
    }
}

/// The ox home directory (`~/.ox`), falling back to `./.ox` when the home
/// directory cannot be determined.
pub fn ox_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ox")
}

impl Config {
    /// Default config file location: `~/.ox/ox.toml`.
    pub fn default_path() -> PathBuf {
        ox_home().join("ox.toml")
    }

    /// Load config from the default path, then apply env overrides.
    /// A missing config file is fine — defaults apply.
    pub fn load() -> Result<Self> {
        Self::load_from(Self::default_path())
    }

    /// Load config from an explicit path, then apply env overrides.
    pub fn load_from(path: PathBuf) -> Result<Self> {
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("failed to parse config file {}", path.display()))?
        } else {
            Config::default()
        };

        if let Ok(url) = std::env::var(ENV_CARDINAL_URL) {
            config.cardinal_url = url;
        }
        if let Ok(key) = std::env::var(ENV_CARDINAL_KEY) {
            config.cardinal_key = Some(key);
        }
        if let Ok(db) = std::env::var(ENV_DB_PATH) {
            config.db_path = PathBuf::from(db);
        }
        if let Ok(listen) = std::env::var(ENV_LISTEN) {
            config.listen = listen;
        }
        if let Ok(model) = std::env::var(ENV_DEFAULT_MODEL) {
            config.default_model = Some(model);
        }

        config.magic_context = config.magic_context.validated();
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_applied_when_file_missing() {
        let cfg = Config::load_from(PathBuf::from("/nonexistent/path/ox.toml")).unwrap();
        assert_eq!(cfg.cardinal_url, DEFAULT_CARDINAL_URL);
        assert_eq!(cfg.listen, DEFAULT_LISTEN);
        assert!(cfg.cardinal_key.is_none());
        assert_eq!(cfg.event_heartbeat_ms, 15_000);
        assert!(cfg.db_path.ends_with("ox.db"));
        assert!(cfg.default_model.is_none());
    }

    #[test]
    fn toml_round_trip() {
        let raw = r#"
cardinal_url = "http://localhost:1234"
db_path = "/tmp/ox-test.db"
listen = "0.0.0.0:9999"
default_model = "anthropic/claude-sonnet-4-5"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.cardinal_url, "http://localhost:1234");
        assert_eq!(cfg.db_path, PathBuf::from("/tmp/ox-test.db"));
        assert_eq!(cfg.listen, "0.0.0.0:9999");
        assert_eq!(
            cfg.default_model.as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
    }

    #[test]
    fn magic_context_defaults_when_section_missing() {
        let cfg: Config = toml::from_str("listen = \"127.0.0.1:9999\"").unwrap();
        // On by default: a fresh install is the memory-integrated server, and
        // `enabled = false` is the pass-through escape.
        assert!(cfg.magic_context.enabled);
        assert!(cfg.magic_context.historian_model.is_none());
        assert_eq!(cfg.magic_context.execute_threshold_percentage, 65.0);
        assert_eq!(cfg.magic_context.history_budget_percentage, 0.15);
        assert!(cfg.magic_context.dreamer_enabled);
        assert_eq!(cfg.magic_context.dreamer_interval_minutes, 15);
    }

    #[test]
    fn magic_context_can_be_turned_off() {
        let cfg: Config = toml::from_str(
            "listen = \"127.0.0.1:9999\"
             [magic_context]
             enabled = false",
        )
        .unwrap();
        assert!(!cfg.magic_context.enabled);
        // The section leaves `dreamer_enabled` at its default; validation is
        // what turns the orphaned dreamer off (it could never run).
        assert!(
            !cfg.magic_context.validated().dreamer_enabled,
            "and the dreamer with it"
        );
    }

    #[test]
    fn magic_context_section_parses() {
        let raw = r#"
[magic_context]
enabled = true
historian_model = "cardinal/k3-256k"
execute_threshold_percentage = 70.0
history_budget_percentage = 0.25
dreamer_enabled = true
dreamer_interval_minutes = 1
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.magic_context.enabled);
        assert_eq!(
            cfg.magic_context.historian_model.as_deref(),
            Some("cardinal/k3-256k")
        );
        assert_eq!(cfg.magic_context.execute_threshold_percentage, 70.0);
        assert_eq!(cfg.magic_context.history_budget_percentage, 0.25);
        assert!(cfg.magic_context.dreamer_enabled);
        assert_eq!(cfg.magic_context.dreamer_interval_minutes, 1);
    }

    #[test]
    fn magic_context_partial_section_tolerates_missing_keys() {
        let raw = r#"
[magic_context]
enabled = true
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.magic_context.enabled);
        assert!(cfg.magic_context.historian_model.is_none());
        assert_eq!(cfg.magic_context.execute_threshold_percentage, 65.0);
        assert_eq!(cfg.magic_context.history_budget_percentage, 0.15);
    }

    /// The old threshold pair must still parse — an existing `ox.toml` is not
    /// an error — but it no longer decides anything.
    #[test]
    fn magic_context_accepts_the_deprecated_threshold_pair() {
        let raw = r#"
[magic_context]
soft_threshold = 0.6
hard_threshold = 0.9
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.magic_context.soft_threshold, 0.6);
        assert_eq!(cfg.magic_context.hard_threshold, 0.9);
        assert_eq!(cfg.magic_context.execute_threshold_percentage, 65.0);
    }

    #[test]
    fn magic_context_invalid_value_is_a_parse_error_like_any_config() {
        let raw = r#"
[magic_context]
soft_threshold = "high"
"#;
        assert!(toml::from_str::<Config>(raw).is_err());
    }
}
