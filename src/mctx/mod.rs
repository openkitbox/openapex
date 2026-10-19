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
    /// Master switch. Defaults to `false`: the whole integration is inert
    /// until explicitly enabled in the config file.
    #[serde(default)]
    pub enabled: bool,
    /// Model backing historian (and dreamer) sub-completions, as a
    /// qualified `provider/id` reference. When unset, ox's existing default
    /// model resolution applies.
    #[serde(default)]
    pub historian_model: Option<String>,
    /// Pressure fraction (0.0–1.0) at which pre-emptive work may start.
    #[serde(default = "default_soft_threshold")]
    pub soft_threshold: f32,
    /// Pressure fraction at which a historian pass is forced.
    #[serde(default = "default_hard_threshold")]
    pub hard_threshold: f32,
}

fn default_soft_threshold() -> f32 {
    0.70
}

fn default_hard_threshold() -> f32 {
    0.85
}

impl Default for MctxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            historian_model: None,
            soft_threshold: default_soft_threshold(),
            hard_threshold: default_hard_threshold(),
        }
    }
}

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

/// The per-session transform pass: plan → classify → step the cache kernel →
/// CAS write-back (the `m[0]`/`m[1]` head producer).
pub mod pass;

/// Historian: triggering, chunking, prompt, cardinal execution, validation
/// and publication of compartments.
pub mod historian;

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
    fn defaults() {
        // Serde field defaults and the Default impl must agree — they drift
        // independently (a changed default_* fn vs a forgotten impl arm).
        assert_eq!(
            toml::from_str::<MctxConfig>("[magic_context]").unwrap(),
            MctxConfig::default()
        );
        let cfg = MctxConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.historian_model.is_none());
    }
}
