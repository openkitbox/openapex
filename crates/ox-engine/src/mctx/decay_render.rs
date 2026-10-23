//! Deterministic decay renderer: turns a chronological compartment set into
//! the markdown-heading history bytes that fill `m[0]`/`m[1]`.
//!
//! Faithful port of the reference `mc-module/src/decay_render.rs`. It picks a
//! tier per compartment from age + importance + budget pressure (via
//! [`crate::mctx::decay`], pressure computed ONCE per pass), renders the
//! chosen paraphrase tier (P1..P4; P5 = archived = omitted), and demotes
//! oldest-first under a hard token budget as a drift guard.
//!
//! This is byte-producing, so the byte-identity invariant that matters is
//! intra-module determinism (same compartments + budget → same bytes across
//! passes); the ported `testdata/render-golden.json` cross-checks the
//! paraphrase path, legacy truncation, archive omission and XML escaping
//! against the reference.
//!
//! Documented deviations from the reference port:
//!
//! - **No JS UTF-16 string semantics.** The reference truncates to a
//!   JavaScript `slice` boundary and, when the cut falls inside an astral
//!   scalar, keeps a lone-surrogate *marker* so a later encoder can emit the
//!   `\udxxx` escape JS produces. ox has no TS string boundary, so
//!   [`truncate_with_ellipsis`] cuts on a UTF-8 scalar boundary instead (the
//!   UTF-16 length semantics of the cut are preserved). The golden cases that
//!   only pin the surrogate-marker bytes are not ported.
//! - **No `PreparedDecay` tier cache.** The reference keeps a compose-local
//!   cache because its retry loop re-renders the same compartments at
//!   different pressures. ox renders one pass per drain, so the cache is pure
//!   overhead; [`render_decayed_compartments`] is the single entry point.
//! - **No model-keyed budget calibration.** The reference scales the provider
//!   allowance by a per-model prose ratio (`history_local_budget`). ox's model
//!   catalog carries no such ratio, so the provider budget is used directly —
//!   the reference's own `ratio == 1.0` path.
//! - **The token estimator is injected** ([`estimate_tokens`] is ox's MVP
//!   stand-in): the reference's real Claude-BPE estimator is its own subsystem
//!   (a 600-line crate plus a BPE vocabulary) and a later port. With a budget
//!   loose enough that the guard never fires, the output is
//!   estimator-independent and purely curve-driven — which is what the golden
//!   exercises.

use crate::mctx::decay::{DecayInput, compute_budget_pressure, rendered_tier};

/// The minimal compartment shape the renderer needs. `p1..p4` are the
/// paraphrase tiers (None / empty = not a v2-tiered row); `legacy = Some(1)`
/// marks a pre-v2 flat-content row; `importance` defaults to 50 when absent.
#[derive(Debug, Clone, Default)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct DecayRenderCompartment {
    /// First message index the compartment covers.
    pub start_message: i64,
    /// Last message index the compartment covers.
    pub end_message: i64,
    /// Historian-authored heading title.
    pub title: String,
    /// Flat body (legacy rows, and the fallback for malformed tier rows).
    pub content: String,
    /// Start date (YYYY-MM-DD) of the covered span.
    pub start_date: Option<String>,
    /// End date (YYYY-MM-DD) of the covered span.
    pub end_date: Option<String>,
    /// Paraphrase tier P1 (most verbose).
    pub p1: Option<String>,
    /// Paraphrase tier P2.
    pub p2: Option<String>,
    /// Paraphrase tier P3.
    pub p3: Option<String>,
    /// Paraphrase tier P4 (most compressed body).
    pub p4: Option<String>,
    /// 1..100 emitted importance (decay rate); 50 when absent.
    pub importance: Option<i32>,
    /// `Some(1)` marks a pre-v2 flat-content row (deterministic truncation
    /// rather than the decay curve).
    pub legacy: Option<i32>,
}

impl DecayRenderCompartment {
    /// Empty payload rows advance coverage but are not summaries or decay
    /// inputs.
    pub fn is_no_content(&self) -> bool {
        self.title.is_empty()
            && self.content.is_empty()
            && [&self.p1, &self.p2, &self.p3, &self.p4]
                .iter()
                .all(|p| p.as_deref().unwrap_or("").is_empty())
    }
}

/// Count the tokens in `text` — ox's MVP stand-in for the reference's Claude
/// BPE estimator (see the module docs): four bytes per token, rounded up,
/// which is the usual English-text approximation.
///
/// Only [`render_decayed_compartments`]'s budget guard consumes this, and the
/// reference's own docs note the guard is the one place a budget can be
/// overshot — the curve itself is estimator-independent.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn escape_xml_content(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn format_date_range(start_date: Option<&str>, end_date: Option<&str>) -> String {
    let (Some(start_date), Some(end_date)) = (start_date, end_date) else {
        return String::new();
    };
    if start_date.is_empty() || end_date.is_empty() {
        return String::new();
    }
    if start_date == end_date {
        return start_date.to_string();
    }
    if start_date.get(..7) == end_date.get(..7)
        && let Some(end_day) = end_date.get(8..)
    {
        return format!("{start_date}→{end_day}");
    }
    format!("{start_date}→{end_date}")
}

fn sanitize_compartment_title(title: &str) -> String {
    // Historian-authored titles are untrusted: controls and Unicode
    // line/paragraph separators must collapse or they can forge a visually
    // multiline heading.
    let mut single_line = String::with_capacity(title.len());
    let mut replacing_control_run = false;
    for ch in title.chars() {
        if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
            if !replacing_control_run {
                single_line.push(' ');
                replacing_control_run = true;
            }
        } else {
            single_line.push(ch);
            replacing_control_run = false;
        }
    }
    escape_xml_content(&single_line)
}

fn compartment_heading(c: &DecayRenderCompartment) -> String {
    let date_range = format_date_range(c.start_date.as_deref(), c.end_date.as_deref());
    let date_segment = if date_range.is_empty() {
        String::new()
    } else {
        format!(" · {date_range}")
    };
    format!(
        "## {}-{}{date_segment} · {}",
        c.start_message,
        c.end_message,
        sanitize_compartment_title(&c.title)
    )
}

fn guard_compartment_body(body: &str) -> String {
    // A rendered body cannot open a new compartment; indent heading-like lines
    // so the next unindented `## ` line remains an unambiguous compartment
    // boundary.
    let guarded = body.replace("\n## ", "\n ## ");
    if guarded.starts_with("## ") {
        format!(" {guarded}")
    } else {
        guarded
    }
}

/// A row is v2-tiered ONLY when `p1` is a non-empty string. Rows with
/// empty/null `p1` (legacy rows, or the malformed pseudo-v2 state left by an
/// interrupted upgrade — `legacy=0` but tiers never populated) render via flat
/// `content`, never as an empty tier body. A VALID v2 row can still have an
/// empty `p4` (a legitimate title-only heading); that is handled by the
/// tier-body path, since such a row has a non-empty `p1`.
fn is_tiered_row(c: &DecayRenderCompartment) -> bool {
    c.p1.as_deref().is_some_and(|p| !p.is_empty())
}

/// The v2 paraphrase tier body, with denser-tier and content fallbacks: the
/// requested tier if present, else the densest populated denser tier, else
/// flat content.
fn tier_body(c: &DecayRenderCompartment, tier: u8) -> String {
    let tiers = [
        c.p1.as_deref(),
        c.p2.as_deref(),
        c.p3.as_deref(),
        c.p4.as_deref(),
    ];
    let idx = (tier as usize).saturating_sub(1);
    if let Some(requested) = tiers.get(idx).copied().flatten() {
        return requested.trim().to_string();
    }
    // walk denser (lower-index) tiers for a non-empty body
    for i in (0..idx).rev() {
        if let Some(t) = tiers[i]
            && !t.is_empty()
        {
            return t.trim().to_string();
        }
    }
    c.content.trim().to_string()
}

/// Truncate to at most `max` UTF-16 code units — the same *length* semantics
/// as JavaScript's `String.prototype.slice` — but never splitting a UTF-8
/// scalar: a cut inside an astral scalar drops the whole scalar instead of
/// keeping a lone surrogate (see the module docs).
fn truncate_with_ellipsis(content: &str, max: usize) -> String {
    let units = content.encode_utf16().collect::<Vec<_>>();
    if units.len() <= max {
        return content.to_string();
    }
    let mut prefix_units = units[..max].to_vec();
    // `encode_utf16` only ever pairs a low surrogate after a high one, so a cut
    // can leave a high surrogate (never a lone low) at the end.
    if prefix_units
        .last()
        .is_some_and(|unit| (0xD800..=0xDBFF).contains(unit))
    {
        prefix_units.pop();
    }
    let prefix = String::from_utf16(&prefix_units)
        .unwrap_or_else(|_| String::from_utf16_lossy(&prefix_units));
    format!("{}…", prefix.trim_end())
}

/// Legacy flat-content tier rendering (no paraphrase columns): P1 = full,
/// P2 = ≤1200 chars, P3+ = ≤420 chars.
fn legacy_body_for_tier(content: &str, tier: u8) -> String {
    if tier <= 1 {
        content.to_string()
    } else if tier == 2 {
        truncate_with_ellipsis(content, 1_200)
    } else {
        truncate_with_ellipsis(content, 420)
    }
}

/// Legacy compartments start at P3 if the body has a `U:` line, else P4.
fn legacy_tier(c: &DecayRenderCompartment) -> u8 {
    if c.content.lines().any(|l| l.starts_with("U:")) {
        3
    } else {
        4
    }
}

/// Render a single compartment at an explicit tier. Exposed for the m1 "new
/// compartments" block, which always renders newest compartments at P1 (full
/// fidelity — no decay applies to brand-new deltas).
pub fn render_compartment_at_tier(c: &DecayRenderCompartment, tier: u8) -> String {
    render_one_compartment(c, tier)
}

fn render_one_compartment(c: &DecayRenderCompartment, tier: u8) -> String {
    if c.is_no_content() || tier >= 5 {
        return String::new(); // archived
    }
    let heading = compartment_heading(c);

    // Legacy rows AND malformed pseudo-v2 rows (legacy=0 but no usable p1)
    // render via flat `content`, never as an empty title-only heading —
    // otherwise a `legacy=0, p1=''` row would silently drop the compartment
    // body from m0/m1.
    if c.legacy == Some(1) || !is_tiered_row(c) {
        let flat = c.content.trim();
        if tier >= 4 || flat.is_empty() {
            return heading;
        }
        let body = guard_compartment_body(&escape_xml_content(&legacy_body_for_tier(flat, tier)));
        return format!("{heading}\n{body}");
    }

    let body = tier_body(c, tier);
    if body.is_empty() {
        return heading;
    }
    format!(
        "{heading}\n{}",
        guard_compartment_body(&escape_xml_content(&body))
    )
}

/// Compute the rendered tier for each compartment, given budget pressure
/// derived once from the whole set. `compartments` are chronological (oldest
/// first); the decay curve indexes from newest (1 = newest). Legacy rows are
/// governed by deterministic truncation, not the curve, and are EXCLUDED from
/// the pressure inputs so unrelated legacy cost can't demote v2 paraphrases
/// (budget honesty for mixed sessions).
fn compute_tiers(compartments: &[DecayRenderCompartment], history_budget: f64) -> Vec<u8> {
    let v2_indices: Vec<usize> = compartments
        .iter()
        .enumerate()
        .filter(|(_, c)| c.legacy != Some(1))
        .map(|(i, _)| i)
        .collect();
    let v2_total = v2_indices.len();

    // curve index per original index: 1-based from newest v2 row.
    let mut curve_index_by_original = std::collections::HashMap::new();
    let mut curve_inputs = Vec::with_capacity(v2_total);
    for (v2_ordinal, &original_index) in v2_indices.iter().enumerate() {
        let curve_index = (v2_total - v2_ordinal) as u32;
        curve_index_by_original.insert(original_index, curve_index);
        let importance = compartments[original_index]
            .importance
            .unwrap_or(50)
            .clamp(1, 100);
        curve_inputs.push(DecayInput {
            index: curve_index,
            importance,
        });
    }
    let pressure = if history_budget > 0.0 {
        compute_budget_pressure(&curve_inputs, history_budget)
    } else {
        1.0
    };

    compartments
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if c.legacy == Some(1) {
                legacy_tier(c)
            } else {
                rendered_tier(
                    *curve_index_by_original.get(&i).unwrap_or(&1),
                    c.importance.unwrap_or(50),
                    pressure,
                    0.0,
                )
            }
        })
        .collect()
}

/// Render the decayed compartment-history body (no `<session-history>` wrapper
/// — callers add their own framing). Demotes oldest-first under the budget as
/// a drift guard, measured by the injected `estimate_tokens`. Never renders
/// session facts (v2 faithful).
pub fn render_decayed_compartments(
    compartments: &[DecayRenderCompartment],
    history_budget_tokens: f64,
    estimate_tokens: impl Fn(&str) -> usize,
) -> String {
    if compartments.is_empty() {
        return String::new();
    }
    let filtered;
    let compartments = if compartments
        .iter()
        .any(DecayRenderCompartment::is_no_content)
    {
        filtered = compartments
            .iter()
            .filter(|c| !c.is_no_content())
            .cloned()
            .collect::<Vec<_>>();
        filtered.as_slice()
    } else {
        compartments
    };
    let mut tiers = compute_tiers(compartments, history_budget_tokens);

    let render = |tiers: &[u8]| -> String {
        let mut parts = Vec::new();
        for (i, c) in compartments.iter().enumerate() {
            let rendered = render_one_compartment(c, tiers[i]);
            if !rendered.is_empty() {
                parts.push(rendered);
            }
        }
        parts.join("\n\n")
    };

    let mut body = render(&tiers);
    // Budget guard: the curve already targets the budget, but estimate drift or
    // a very tight budget can overshoot. Demote oldest-first until it fits.
    let mut guard = compartments.len() * 5;
    while history_budget_tokens > 0.0
        && estimate_tokens(&body) as f64 > history_budget_tokens
        && guard > 0
    {
        let mut demoted = false;
        for t in tiers.iter_mut() {
            if *t < 5 {
                *t += 1;
                demoted = true;
                break;
            }
        }
        if !demoted {
            break;
        }
        body = render(&tiers);
        guard -= 1;
    }
    body
}
