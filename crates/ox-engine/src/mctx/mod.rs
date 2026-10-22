//! magic-context integration: assembly and the `[magic_context]` config
//! section.
//!
//! Task 1 lays down the skeleton only: configuration parsing and the module
//! tree. With `magic_context.enabled = false` (the default) nothing in this
//! module runs and the server behaves exactly as before.

pub use ox_core::config::{EmbeddingConfig, EmbeddingProvider, MctxConfig};

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
    config: &ox_core::config::Config,
    catalog: &ox_core::model_catalog::ModelCatalog,
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

/// The embedding client and its vector helpers.
pub mod embed;

/// cache-core integration: classification predicates, render handoff,
/// whole-value CAS state and LKG.
pub mod cache;

/// §N§ part tagging, `[dropped §N§]` placeholders and `ctx_expand` recovery.
pub mod tagger;

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
    use ox_core::config::{
        default_execute_threshold_percentage, default_history_budget_percentage,
    };

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
