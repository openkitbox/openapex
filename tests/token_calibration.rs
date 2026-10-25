//! The token-unit seam: `bytes / 4` estimates corrected by the provider's own
//! `prompt_tokens` ([`ox_engine::mctx::calibration`] and
//! [`ox_engine::mctx::tokens`]).
//!
//! This is the whole decision surface of the budget unit correction, which is
//! why it is pure: no session, no database, no clock.

use ox_engine::mctx::calibration::Calibration;
use ox_engine::mctx::tokens::{counter_fn, heuristic};

#[test]
fn an_unmeasured_calibration_is_neutral() {
    assert_eq!(Calibration::default().ratio(), 1.0);
}

#[test]
fn the_first_sample_is_taken_as_is() {
    let mut calibration = Calibration::default();
    // 3000 provider tokens for a 2000-token local estimate → 1.5×.
    calibration.observe("deepseek-flash", 3000, 2000);
    assert_eq!(calibration.ratio(), 1.5);
}

#[test]
fn samples_average_then_freeze() {
    let mut calibration = Calibration::default();
    calibration.observe("m", 2000, 1000);
    calibration.observe("m", 1000, 1000);
    // Mean of 2.0 and 1.0.
    assert_eq!(calibration.ratio(), 1.5);
    calibration.observe("m", 1000, 1000);
    calibration.observe("m", 1000, 1000);
    assert_eq!(calibration.ratio(), 1.25, "mean of 2.0, 1.0, 1.0, 1.0");
    // A later, wildly different turn cannot move the frozen value.
    calibration.observe("m", 3000, 1000);
    assert_eq!(calibration.ratio(), 1.25);
}

#[test]
fn a_model_switch_restarts_the_measurement() {
    let mut calibration = Calibration::default();
    calibration.observe("m1", 1500, 1000);
    assert_eq!(calibration.ratio(), 1.5);
    calibration.observe("m2", 1000, 1000);
    assert_eq!(calibration.ratio(), 1.0, "m1's ratio must not leak into m2");
}

#[test]
fn implausible_samples_clamp_instead_of_being_trusted() {
    let mut calibration = Calibration::default();
    calibration.observe("m", 100_000, 1000);
    assert_eq!(calibration.ratio(), 2.0);

    let mut calibration = Calibration::default();
    calibration.observe("m", 1, 100_000);
    assert_eq!(calibration.ratio(), 0.5);
}

#[test]
fn zero_sized_samples_are_not_measurements() {
    let mut calibration = Calibration::default();
    calibration.observe("m", 0, 1000);
    calibration.observe("m", 1000, 0);
    assert_eq!(calibration.ratio(), 1.0);
}

#[test]
fn the_estimate_is_four_bytes_per_token_rounded_up() {
    assert_eq!(heuristic(""), 0);
    assert_eq!(heuristic("abc"), 1);
    assert_eq!(heuristic("abcd"), 1);
    assert_eq!(heuristic("abcde"), 2);
    // Multi-byte scalars count by byte, which is the point of the heuristic:
    // CJK costs ~3 bytes per character, so it is not under-counted by character
    // count.
    assert_eq!(heuristic("汉字"), 2);
}

#[test]
fn a_neutral_calibration_counts_exactly_like_the_estimate() {
    let count = counter_fn(1.0);
    for text in ["", "abc", "abcd", "a longer line of prose\n\nwith a break"] {
        assert_eq!(count(text), heuristic(text));
    }
}

#[test]
fn a_measured_ratio_scales_up_and_never_down() {
    let text = "abcdefgh"; // 8 bytes → 2 local tokens.
    assert_eq!(heuristic(text), 2);
    assert_eq!(counter_fn(1.5)(text), 3);
    assert_eq!(counter_fn(1.25)(text), 3, "each piece rounds up");
    // The correction is a scale, so it can also shrink a locally over-counted
    // piece — but never below the estimate by more than the ratio says.
    assert_eq!(counter_fn(0.75)(text), 2, "1.5 rounds up to 2");
}

#[test]
fn a_degenerate_ratio_falls_back_to_neutral() {
    for ratio in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0] {
        assert_eq!(
            counter_fn(ratio)("abcdefgh"),
            heuristic("abcdefgh"),
            "ratio {ratio} must not poison the budget"
        );
    }
}
