//! Characterization of LLM-endpoint config loading through the real
//! `Config::load_from` (TOML parse + validation), pinned before the endpoint
//! config moves into its own crate. Behavior here must survive the move
//! byte-for-byte: key names, defaults and fail-closed messages.

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use ox::config::{Config, DEFAULT_LIMITED_STALL, DEFAULT_PROVIDER_NAME, EndpointLimits};

fn load(toml: &str) -> anyhow::Result<Config> {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("ox.toml");
    std::fs::write(&path, toml).unwrap();
    Config::load_from(path)
}

fn load_err(toml: &str) -> String {
    format!("{:#}", load(toml).expect_err("config must be rejected"))
}

#[test]
fn full_file_parses_every_llm_key_next_to_the_other_sections() {
    let config = load(
        r#"
cardinal_url = "http://gateway.test:1"
cardinal_key = "sk-test"
default_model = "cardinal/k3"
listen = "127.0.0.1:1234"

[providers.moonshot]
base_url = "https://api.moonshot.ai"
api_key = "mk"

[providers.bare]
base_url = "http://bare.test"

[models."k3"]
candidates = [
  { provider = "moonshot", model = "kimi-k3" },
  { provider = "cardinal", model = "k3-256k" },
]

[context_caps]
k3 = 200000

[mcp]
[subagent]
depth = 3
[events]
retention_days = 9
[magic_context]
enabled = false
"#,
    )
    .expect("full config loads");

    assert_eq!(config.llm.cardinal_url, "http://gateway.test:1");
    assert_eq!(config.llm.cardinal_key.as_deref(), Some("sk-test"));
    assert_eq!(config.llm.default_model.as_deref(), Some("cardinal/k3"));
    assert_eq!(config.listen, "127.0.0.1:1234");
    assert_eq!(config.llm.providers.len(), 2);
    assert_eq!(
        config.llm.providers["moonshot"].base_url,
        "https://api.moonshot.ai"
    );
    assert_eq!(
        config.llm.providers["moonshot"].api_key.as_deref(),
        Some("mk")
    );
    assert_eq!(config.llm.providers["bare"].api_key, None);
    let route = &config.llm.models["k3"];
    assert_eq!(route.candidates.len(), 2);
    assert_eq!(route.candidates[0].provider, "moonshot");
    assert_eq!(route.candidates[0].model, "kimi-k3");
    assert_eq!(route.candidates[1].provider, DEFAULT_PROVIDER_NAME);
    assert_eq!(config.llm.context_caps["k3"], 200_000);
    // The sections that share the file are untouched by the LLM keys.
    assert_eq!(config.subagent.depth, 3);
    assert_eq!(config.events.retention_days, 9);
}

#[test]
fn empty_file_yields_defaults() {
    let config = load("").expect("empty config loads");
    let default = Config::default();
    assert_eq!(config.llm.cardinal_url, default.llm.cardinal_url);
    assert_eq!(config.llm.cardinal_key, None);
    assert!(config.llm.providers.is_empty());
    assert!(config.llm.models.is_empty());
    assert!(config.llm.context_caps.is_empty());
    assert_eq!(config.llm.default_model, None);
    assert_eq!(DEFAULT_PROVIDER_NAME, "cardinal");
}

#[test]
fn provider_base_url_is_validated() {
    assert!(load_err("[providers.p]\nbase_url = \"\"\n").contains("providers.p.base_url is empty"));
    assert!(
        load_err("[providers.p]\nbase_url = \"https://h/v1\"\n")
            .contains("providers.p.base_url must not end with /v1")
    );
    assert!(
        load_err("[providers.p]\nbase_url = \"ftp://h\"\n")
            .contains("providers.p.base_url must be an http(s) URL")
    );
}

#[test]
fn model_routes_are_validated() {
    assert!(load_err("[models.m]\ncandidates = []\n").contains("models.m.candidates is empty"));
    assert!(
        load_err(
            "[providers.p]\nbase_url = \"http://h\"\n[models.m]\ncandidates = [{ provider = \"p\", model = \"\" }]\n"
        )
        .contains("models.m: candidate model must not be empty")
    );
    assert!(
        load_err("[models.m]\ncandidates = [{ provider = \"ghost\", model = \"x\" }]\n")
            .contains("models.m: candidate provider `ghost` is not configured under [providers]")
    );
}

#[test]
fn context_caps_must_be_positive_integers() {
    assert!(
        load_err("[context_caps]\nm = 0\n")
            .contains("context_caps.m must be a positive token count, got 0")
    );
    assert!(load_err("[context_caps]\nm = \"x\"\n").contains("invalid type"));
}

#[test]
fn endpoint_limits_default_to_unset() {
    let config = load("[providers.p]\nbase_url = \"http://h\"\n").unwrap();
    let limits = &config.llm.providers["p"].limits;
    assert_eq!(limits.max_concurrent, None);
    assert_eq!(limits.stall_timeout_secs, None);
    assert_eq!(limits.stall_timeout(), None);
    assert_eq!(config.llm.cardinal_limits, EndpointLimits::default());
    assert_eq!(config.llm.cardinal_limits.stall_timeout(), None);
}

#[test]
fn endpoint_limits_parse_for_providers_and_the_implicit_endpoint() {
    let config = load(
        r#"
[providers.p]
base_url = "http://h"
max_concurrent = 4
stall_timeout_secs = 90

[cardinal_limits]
max_concurrent = 8
"#,
    )
    .unwrap();
    let p = &config.llm.providers["p"].limits;
    assert_eq!(p.max_concurrent, NonZeroUsize::new(4));
    assert_eq!(p.stall_timeout_secs, NonZeroU64::new(90));
    let c = &config.llm.cardinal_limits;
    assert_eq!(c.max_concurrent, NonZeroUsize::new(8));
    assert_eq!(c.stall_timeout_secs, None);
}

#[test]
fn endpoint_limits_reject_zero_negative_and_non_integers() {
    for key in ["max_concurrent", "stall_timeout_secs"] {
        for bad in ["0", "-1", "\"x\""] {
            let provider = load_err(&format!(
                "[providers.p]\nbase_url = \"http://h\"\n{key} = {bad}\n"
            ));
            assert!(
                provider.contains("nonzero") || provider.contains("invalid type"),
                "providers.p.{key} = {bad}: {provider}"
            );
            let cardinal = load_err(&format!("[cardinal_limits]\n{key} = {bad}\n"));
            assert!(
                cardinal.contains("nonzero") || cardinal.contains("invalid type"),
                "cardinal_limits.{key} = {bad}: {cardinal}"
            );
        }
    }
    // 0 and -1 must be rejected by the NonZero deserializer itself.
    assert!(load_err("[cardinal_limits]\nmax_concurrent = 0\n").contains("nonzero"));
    assert!(load_err("[cardinal_limits]\nstall_timeout_secs = -1\n").contains("nonzero"));
}

#[test]
fn stall_timeout_derivation_covers_all_four_quadrants() {
    let none = EndpointLimits::default();
    assert_eq!(none.stall_timeout(), None);

    let only_limit = EndpointLimits {
        max_concurrent: NonZeroUsize::new(2),
        stall_timeout_secs: None,
    };
    assert_eq!(only_limit.stall_timeout(), Some(DEFAULT_LIMITED_STALL));
    assert_eq!(DEFAULT_LIMITED_STALL, Duration::from_secs(600));

    let both = EndpointLimits {
        max_concurrent: NonZeroUsize::new(2),
        stall_timeout_secs: NonZeroU64::new(30),
    };
    assert_eq!(both.stall_timeout(), Some(Duration::from_secs(30)));

    let only_stall = EndpointLimits {
        max_concurrent: None,
        stall_timeout_secs: NonZeroU64::new(45),
    };
    assert_eq!(only_stall.stall_timeout(), Some(Duration::from_secs(45)));
}
