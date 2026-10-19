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
    /// Whether the dreamer (the scheduled memory-maintenance loop) runs.
    /// Defaults to `false`, like the master switch: the loop issues hidden
    /// completions, so it is opted into explicitly.
    #[serde(default)]
    pub dreamer_enabled: bool,
    /// Dreamer tick cadence in minutes. The reference's process-wide timer
    /// ticks every 15 minutes (`docs/architecture/dreamer.md`), the same
    /// default here; its per-task cron schedules are deliberately not ported
    /// (see [`crate::mctx::dreamer`]), so this one interval drives every task.
    #[serde(default = "default_dreamer_interval_minutes")]
    pub dreamer_interval_minutes: u64,
}

impl MctxConfig {
    /// The config with its threshold invariant enforced: both bands are
    /// fractions in `(0, 1]` and `soft_threshold < hard_threshold`. A violated
    /// invariant falls back to the default pair with a warning — a misconfigured
    /// pair would otherwise silently collapse the bands (equal bands make
    /// [`crate::mctx::historian::Trigger::Speculative`] unreachable; an inverted
    /// pair makes the proactive band dead) and change when the historian runs
    /// without any error surfacing.
    pub fn validated(mut self) -> Self {
        let bad = |name: &str, value: f32| {
            tracing::warn!(
                setting = name,
                value,
                "invalid [magic_context] threshold (expected a fraction in (0, 1], \
                 soft < hard); using the default pair"
            );
        };
        let in_range = |value: f32| value.is_finite() && value > 0.0 && value <= 1.0;
        // The interval is enforced apart from the pair: a zero interval would
        // make every task due on every tick, i.e. a hot loop of hidden
        // completions at the tick loop's (immediate) cadence. One minute is
        // the smallest sane cadence, so anything below it falls back too.
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
        if !in_range(self.soft_threshold) {
            bad("soft_threshold", self.soft_threshold);
        } else if !in_range(self.hard_threshold) {
            bad("hard_threshold", self.hard_threshold);
        } else if self.soft_threshold >= self.hard_threshold {
            bad("soft_threshold", self.soft_threshold);
        } else {
            return self;
        }
        self.soft_threshold = default_soft_threshold();
        self.hard_threshold = default_hard_threshold();
        self
    }
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
            enabled: false,
            historian_model: None,
            soft_threshold: default_soft_threshold(),
            hard_threshold: default_hard_threshold(),
            dreamer_enabled: false,
            dreamer_interval_minutes: default_dreamer_interval_minutes(),
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
    fn validated_thresholds_keep_a_sane_pair() {
        let ok = MctxConfig {
            soft_threshold: 0.6,
            hard_threshold: 0.9,
            ..MctxConfig::default()
        };
        assert_eq!(ok.validated().soft_threshold, 0.6);

        // Out of range, inverted, and equal pairs all fall back to the default
        // pair: any of them would change when a historian run starts without
        // surfacing an error.
        for (soft, hard) in [(0.0_f32, 0.9_f32), (1.5, 2.0), (0.9, 0.7), (0.8, 0.8)] {
            let cfg = MctxConfig {
                soft_threshold: soft,
                hard_threshold: hard,
                ..MctxConfig::default()
            }
            .validated();
            assert_eq!(
                (cfg.soft_threshold, cfg.hard_threshold),
                (default_soft_threshold(), default_hard_threshold()),
                "soft={soft} hard={hard}"
            );
        }
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
        assert!(!cfg.dreamer_enabled);
        assert_eq!(cfg.dreamer_interval_minutes, 15);
    }
}
