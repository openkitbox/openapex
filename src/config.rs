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

fn default_models() -> Vec<ModelEntry> {
    Vec::new()
}

/// A statically configured model. Cardinal integration (Task 5) will replace
/// this with a live model catalogue fetched from the gateway.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// Provider identifier, e.g. `anthropic`.
    pub provider: String,
    /// Model identifier, e.g. `claude-sonnet-4-5`.
    pub id: String,
    /// Human-readable display name.
    pub name: Option<String>,
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
    /// Statically configured models exposed by `model.list`.
    #[serde(default = "default_models")]
    pub models: Vec<ModelEntry>,
    /// Qualified model reference (`provider/id`, e.g. `cardinal/k3-256k`)
    /// backing sessions without an explicit selection. Should match one of
    /// `[[models]]`; an unmatched value is warned about and the first entry
    /// is used instead. When unset the first `[[models]]` entry is the
    /// default.
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
            models: default_models(),
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
    /// The model backing sessions without an explicit selection: the
    /// `default_model` entry when set and configured, else the first
    /// `[[models]]` entry. `None` when nothing is configured. A
    /// `default_model` that matches no entry warns and falls back to the
    /// first entry.
    pub fn default_model_entry(&self) -> Option<&ModelEntry> {
        if let Some(qualified) = &self.default_model {
            let matched = qualified.split_once('/').and_then(|(provider, id)| {
                self.models
                    .iter()
                    .find(|m| m.provider == provider && m.id == id)
            });
            if matched.is_none() {
                if self.models.is_empty() {
                    tracing::warn!(
                        default_model = %qualified,
                        "default_model matches no [[models]] entry; no models configured"
                    );
                } else {
                    tracing::warn!(
                        default_model = %qualified,
                        "default_model matches no [[models]] entry; using the first"
                    );
                }
            }
            return matched.or_else(|| self.models.first());
        }
        self.models.first()
    }

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
        assert!(cfg.models.is_empty());
        assert!(cfg.default_model.is_none());
    }

    #[test]
    fn default_model_entry_precedence() {
        let entry = |provider: &str, id: &str| ModelEntry {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: None,
        };
        let mut cfg = Config {
            models: vec![entry("cardinal", "k3-256k"), entry("cardinal", "glm-5")],
            ..Config::default()
        };
        // Unset → first entry.
        assert_eq!(cfg.default_model_entry().unwrap().id, "k3-256k");
        // Set and matching → that entry.
        cfg.default_model = Some("cardinal/glm-5".to_owned());
        assert_eq!(cfg.default_model_entry().unwrap().id, "glm-5");
        // Set but unknown → falls back to the first entry.
        cfg.default_model = Some("cardinal/nope".to_owned());
        assert_eq!(cfg.default_model_entry().unwrap().id, "k3-256k");
        // No models at all → None.
        cfg.models.clear();
        cfg.default_model = None;
        assert!(cfg.default_model_entry().is_none());
    }

    #[test]
    fn toml_round_trip() {
        let raw = r#"
cardinal_url = "http://localhost:1234"
db_path = "/tmp/ox-test.db"
listen = "0.0.0.0:9999"
default_model = "anthropic/claude-sonnet-4-5"

[[models]]
provider = "anthropic"
id = "claude-sonnet-4-5"
name = "Claude Sonnet 4.5"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.cardinal_url, "http://localhost:1234");
        assert_eq!(cfg.db_path, PathBuf::from("/tmp/ox-test.db"));
        assert_eq!(cfg.listen, "0.0.0.0:9999");
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].provider, "anthropic");
        assert_eq!(cfg.models[0].id, "claude-sonnet-4-5");
        assert_eq!(cfg.models[0].name.as_deref(), Some("Claude Sonnet 4.5"));
        assert_eq!(
            cfg.default_model.as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
    }

    #[test]
    fn magic_context_defaults_when_section_missing() {
        let cfg: Config = toml::from_str("listen = \"127.0.0.1:9999\"").unwrap();
        assert!(!cfg.magic_context.enabled);
        assert!(cfg.magic_context.historian_model.is_none());
        assert_eq!(cfg.magic_context.soft_threshold, 0.70);
        assert_eq!(cfg.magic_context.hard_threshold, 0.85);
        assert!(!cfg.magic_context.dreamer_enabled);
        assert_eq!(cfg.magic_context.dreamer_interval_minutes, 15);
    }

    #[test]
    fn magic_context_section_parses() {
        let raw = r#"
[magic_context]
enabled = true
historian_model = "cardinal/k3-256k"
soft_threshold = 0.6
hard_threshold = 0.9
dreamer_enabled = true
dreamer_interval_minutes = 1
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.magic_context.enabled);
        assert_eq!(
            cfg.magic_context.historian_model.as_deref(),
            Some("cardinal/k3-256k")
        );
        assert_eq!(cfg.magic_context.soft_threshold, 0.6);
        assert_eq!(cfg.magic_context.hard_threshold, 0.9);
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
        assert_eq!(cfg.magic_context.soft_threshold, 0.70);
        assert_eq!(cfg.magic_context.hard_threshold, 0.85);
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
