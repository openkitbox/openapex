//! magic-context integration: assembly and the `[magic_context]` config
//! section.
//!
//! Task 1 lays down the skeleton only: configuration parsing and the module
//! tree. With `magic_context.enabled = false` (the default) nothing in this
//! module runs and the server behaves exactly as before.

use serde::Deserialize;

/// The `[magic_context]` section of `ox.toml`. Every field tolerates a
/// missing key; the section itself is optional and defaults to disabled.
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
    /// the single knob the numbers hang off.
    #[serde(default = "default_execute_threshold_percentage")]
    pub execute_threshold_percentage: f64,
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
    /// Semantic search for project memories (off unless configured).
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    /// Dreamer tick cadence in minutes. The reference's process-wide timer
    /// ticks every 15 minutes (`docs/architecture/dreamer.md`), the same
    /// default here; its per-task cron schedules are deliberately not ported
    /// (see [`crate::mctx::dreamer`]), so this one interval drives every task.
    #[serde(default = "default_dreamer_interval_minutes")]
    pub dreamer_interval_minutes: u64,
}

impl MctxConfig {
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
        if !(budget::MIN_EXECUTE_PCT..=budget::MAX_EXECUTE_PCT)
            .contains(&self.execute_threshold_percentage)
        {
            tracing::warn!(
                setting = "execute_threshold_percentage",
                value = self.execute_threshold_percentage,
                "invalid [magic_context] execute threshold (expected {}..={}); using the default",
                budget::MIN_EXECUTE_PCT,
                budget::MAX_EXECUTE_PCT
            );
            self.execute_threshold_percentage = default_execute_threshold_percentage();
        }
        if !(budget::MIN_HISTORY_SHARE..=budget::MAX_HISTORY_SHARE)
            .contains(&self.history_budget_percentage)
        {
            tracing::warn!(
                setting = "history_budget_percentage",
                value = self.history_budget_percentage,
                "invalid [magic_context] history budget share (expected {}..={}); using the default",
                budget::MIN_HISTORY_SHARE,
                budget::MAX_HISTORY_SHARE
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

fn default_execute_threshold_percentage() -> f64 {
    65.0
}
fn default_history_budget_percentage() -> f64 {
    0.15
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
            history_budget_percentage: default_history_budget_percentage(),
            soft_threshold: default_soft_threshold(),
            hard_threshold: default_hard_threshold(),
            dreamer_enabled: default_enabled(),
            embedding: EmbeddingConfig::default(),
            dreamer_interval_minutes: default_dreamer_interval_minutes(),
        }
    }
}

/// The module's default state: on.
fn default_enabled() -> bool {
    true
}

/// The budgets for a session step running `model_id`.
///
/// The session model's window comes from the catalogue; the fold's window from
/// whichever model [`historian::historian_model`] resolves — the same route the
/// run itself takes, so the chunk budget can never be anchored to a model the
/// completion does not use. A model the catalogue does not know leaves its
/// window unknown, which [`Budget::derive`] answers with the reference's
/// assumed default.
///
/// Lives in `mctx` because it is the one place that knows both models; the
/// runner passes what it has and reads the [`Budget`] fields.
pub(crate) fn budget_for(
    config: &crate::config::Config,
    catalog: &crate::model_catalog::ModelCatalog,
    model_id: &str,
) -> budget::Budget {
    let historian = historian::historian_model(config, catalog)
        .ok()
        .and_then(|id| catalog.window_of(&id));
    budget::Budget::derive(
        &config.magic_context,
        catalog.window_of(model_id),
        historian,
    )
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
    pub fn is_enabled(&self) -> bool {
        self.provider == EmbeddingProvider::OpenAiCompatible
            && self.endpoint.as_deref().is_some_and(|v| !v.is_empty())
            && self.model.as_deref().is_some_and(|v| !v.is_empty())
    }

    /// The identity written beside a stored vector: the model plus the
    /// document prefix, because the same model with a different prefix
    /// produces an incomparable vector space.
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

/// The embedding client and its vector helpers.
pub mod embed;

/// cache-core integration: classification predicates, render handoff,
/// whole-value CAS state and LKG.
pub mod cache;

/// §N§ part tagging, `[dropped §N§]` placeholders and `ctx_expand` recovery.
pub mod tagger;

/// Stripping of `§N§` tag notation that models copy into their own text.
pub mod copied_tags;

/// Project memories (`ctx_memory`) and scratch notes (`ctx_note`): tools,
/// storage and prompt rendering.
pub mod memory;

/// Compartment records, coverage ends and pressure thresholds.
pub mod compartments;

/// The deterministic tier-decay curve (age/importance/budget → tier).
pub mod decay;

/// Decay rendering: turns chronological compartments into the markdown
/// history body that fills `m[0]`/`m[1]`.
pub mod decay_render;

/// Decay rendering and `m[0]`/`m[1]` synthetic-message composition.
pub mod render;

/// Model-window geometry: the pressure bands and the three token budgets every
/// consumer measures against.
pub mod budget;

/// The per-session transform pass: plan → classify → step the cache kernel →
/// CAS write-back (the `m[0]`/`m[1]` head producer).
pub mod pass;

/// Historian: triggering, chunking, prompt, cardinal execution, validation
/// and publication of compartments.
pub mod historian;

/// Validation of the historian's raw completion: the `<output>` parser plus the
/// coverage/boundary rules that decide what is safe to publish.
pub mod historian_validate;

/// `ctx_search`: unified retrieval over memories, notes and compartments.
pub mod search;

/// Dreamer: periodic memory-consolidation task on a tokio interval.
pub mod dreamer;

/// Registration of the `ctx_*` tools into the existing tool registry.
pub mod tools;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_budget_numbers_are_clamped_into_range() {
        let ok = MctxConfig {
            execute_threshold_percentage: 80.0,
            history_budget_percentage: 0.3,
            ..MctxConfig::default()
        }
        .validated();
        assert_eq!(ok.execute_threshold_percentage, 80.0);
        assert_eq!(ok.history_budget_percentage, 0.3);

        // Out of range values fall back to the default: a zero share would make
        // the head carry no history at all, and a 100% threshold would never
        // fold before the window overflows.
        for (execute, history) in [(0.0_f64, 0.15_f64), (100.0, 0.15), (65.0, 0.0), (65.0, 0.9)] {
            let cfg = MctxConfig {
                execute_threshold_percentage: execute,
                history_budget_percentage: history,
                ..MctxConfig::default()
            }
            .validated();
            assert_eq!(
                (
                    cfg.execute_threshold_percentage,
                    cfg.history_budget_percentage
                ),
                (
                    default_execute_threshold_percentage(),
                    default_history_budget_percentage()
                ),
                "execute={execute} history={history}"
            );
        }
    }

    /// The old threshold pair is kept only to be warned about: it must not be
    /// silently read as if it still tuned the bands.
    #[test]
    fn deprecated_thresholds_do_not_change_the_derived_bands() {
        let cfg = MctxConfig {
            soft_threshold: 0.6,
            hard_threshold: 0.9,
            ..MctxConfig::default()
        };
        let validated = cfg.validated();
        assert_eq!(
            validated.execute_threshold_percentage,
            default_execute_threshold_percentage()
        );
        let band = budget::Budget::derive(&validated, Some(100_000), None);
        assert_eq!(band.soft_pct, 63.0, "derived, not 60");
        assert_eq!(band.hard_pct, 85.0, "derived, not 90");
    }

    #[test]
    fn validated_rejects_a_zero_dreamer_interval_and_an_orphan_dreamer() {
        // A zero interval would make every task due on every tick — a hot loop
        // of hidden completions; anything below a minute is treated the same.
        let cfg = MctxConfig {
            dreamer_interval_minutes: 0,
            ..MctxConfig::default()
        }
        .validated();
        assert_eq!(cfg.dreamer_interval_minutes, 15);
        let cfg = MctxConfig {
            dreamer_interval_minutes: 1,
            ..MctxConfig::default()
        }
        .validated();
        assert_eq!(cfg.dreamer_interval_minutes, 1, "one minute is honoured");
        let cfg = MctxConfig {
            enabled: false,
            dreamer_enabled: true,
            ..MctxConfig::default()
        }
        .validated();
        assert!(
            !cfg.dreamer_enabled,
            "a dreamer inside a disabled module can never run"
        );
        let cfg = MctxConfig {
            enabled: true,
            dreamer_enabled: true,
            ..MctxConfig::default()
        }
        .validated();
        assert!(cfg.dreamer_enabled, "an enabled module keeps its dreamer");
    }

    /// The config spelling is `openai-compatible`, not the `kebab-case`
    /// splitting of the variant name (`open-ai-compatible`) — a real config
    /// caught this.
    #[test]
    fn the_embedding_provider_spelling_round_trips() {
        // `MctxConfig` is the section body, so its sub-table is `[embedding]`.
        let cfg: MctxConfig =
            toml::from_str("[embedding]\nprovider = \"openai-compatible\"").unwrap();
        assert_eq!(cfg.embedding.provider, EmbeddingProvider::OpenAiCompatible);
        let off: MctxConfig = toml::from_str("").unwrap();
        assert_eq!(off.embedding.provider, EmbeddingProvider::Off);
    }

    #[test]
    fn defaults() {
        // Serde field defaults and the Default impl must agree — they drift
        // independently (a changed default_* fn vs a forgotten impl arm).
        assert_eq!(
            toml::from_str::<MctxConfig>("[magic_context]").unwrap(),
            MctxConfig::default()
        );
        let cfg = MctxConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.historian_model.is_none());
        assert!(cfg.dreamer_enabled);
        assert_eq!(cfg.dreamer_interval_minutes, 15);
    }
}
