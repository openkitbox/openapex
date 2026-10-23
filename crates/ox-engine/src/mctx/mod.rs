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

/// The `ctx_*` family's shared one-line system-layer guidance. The family's
/// policy is cross-tool (which member to reach for), so it is written once
/// here and every member answers with the same line; `prompt::system_static`
/// renders identical contributions once.
pub(crate) const CTX_GUIDANCE: &str = "ctx_note parks follow-ups, ctx_reduce drops stale items, ctx_expand recovers them, ctx_search finds the archive, ctx_memory stores facts.";

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

/// Temporal awareness: the per-message gap markers (`<!-- +1h 5m -->`).
pub mod temporal;

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
