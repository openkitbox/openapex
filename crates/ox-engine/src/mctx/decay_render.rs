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
//! - **The token estimator is injected** ([`crate::mctx::tokens::Counter`]):
//!   the pass resolves the session family's exact counter once and threads it
//!   through [`M0Inputs`]. [`crate::mctx::tokens::heuristic`] (the bytes/4
//!   stand-in) covers unknown families and the call sites that count text the
//!   wire never sends verbatim. With a budget loose enough that the guard never
//!   fires, the output is estimator-independent and purely curve-driven — which
//!   is what the golden exercises.
//! - **The budget guard is accounted per compartment, not per body.** The
//!   reference re-renders and re-measures the whole joined body on every
//!   demotion; that is one full tokenization per demotion, and a full
//!   tokenization of a 76 KB history allocates ~50 MB (the `tokenizers`
//!   pipeline costs ~2.6 KB of allocation per token). [`render_decayed_compartments`]
//!   instead renders and measures each compartment once and re-measures only
//!   the demoted one, summing the pieces plus one [`COMPARTMENT_SEAM`] per
//!   join.
//!
//!   The sum is not bit-equal to the count of the joined string: BPE merges run
//!   across the seam, so the pieces cost a little more than their join. Measured
//!   on the deepseek vocabulary over 98 joins of real compartment-sized text
//!   (pairwise plus 5/8/12/20-piece histories): the sum exceeds the joined count
//!   by 0 to +6 tokens per join, mean +0.79, and never fell below it. So the
//!   drift is always an *over*-estimate — the guard can demote one tier earlier
//!   than the reference at a budget boundary (a smaller history, never a larger
//!   prompt), and it can never conclude "fits" for a body that does not. On a
//!   40-case differential (5 corpora × 8 budgets, exact and heuristic counters)
//!   the demoted tiers and the rendered bytes were identical to the reference
//!   in every case; a divergence needs the body to sit within ~6 tokens/join of
//!   the budget. The rendered bytes at a fixed tier set are unaffected — the
//!   same tiers always produce the same body.

use crate::mctx::decay::{DecayInput, compute_budget_pressure, rendered_tier};

/// The minimal compartment shape the renderer needs. `p1..p4` are the
/// paraphrase tiers (None / empty = not a v2-tiered row); `legacy = Some(1)`
/// marks a pre-v2 flat-content row; `importance` defaults to 50 when absent.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct DecayRenderCompartment<'a> {
    /// First message index the compartment covers.
    pub start_message: i64,
    /// Last message index the compartment covers.
    pub end_message: i64,
    /// Historian-authored heading title.
    pub title: &'a str,
    /// Flat body (legacy rows, and the fallback for malformed tier rows).
    pub content: &'a str,
    /// Start date (YYYY-MM-DD) of the covered span.
    pub start_date: Option<&'a str>,
    /// End date (YYYY-MM-DD) of the covered span.
    pub end_date: Option<&'a str>,
    /// Paraphrase tier P1 (most verbose).
    pub p1: Option<&'a str>,
    /// Paraphrase tier P2.
    pub p2: Option<&'a str>,
    /// Paraphrase tier P3.
    pub p3: Option<&'a str>,
    /// Paraphrase tier P4 (most compressed body).
    pub p4: Option<&'a str>,
    /// 1..100 emitted importance (decay rate); 50 when absent.
    pub importance: Option<i32>,
    /// `Some(1)` marks a pre-v2 flat-content row (deterministic truncation
    /// rather than the decay curve).
    pub legacy: Option<i32>,
}

impl DecayRenderCompartment<'_> {
    /// Empty payload rows advance coverage but are not summaries or decay
    /// inputs.
    pub fn is_no_content(&self) -> bool {
        self.title.is_empty()
            && self.content.is_empty()
            && [self.p1, self.p2, self.p3, self.p4]
                .iter()
                .all(|p| p.unwrap_or("").is_empty())
    }
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

fn compartment_heading(c: &DecayRenderCompartment<'_>) -> String {
    let date_range = format_date_range(c.start_date, c.end_date);
    let date_segment = if date_range.is_empty() {
        String::new()
    } else {
        format!(" · {date_range}")
    };
    format!(
        "## {}-{}{date_segment} · {}",
        c.start_message,
        c.end_message,
        sanitize_compartment_title(c.title)
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
fn is_tiered_row(c: &DecayRenderCompartment<'_>) -> bool {
    c.p1.is_some_and(|p| !p.is_empty())
}

/// The v2 paraphrase tier body, with denser-tier and content fallbacks: the
/// requested tier if present, else the densest populated denser tier, else
/// flat content.
fn tier_body(c: &DecayRenderCompartment<'_>, tier: u8) -> String {
    let tiers = [c.p1, c.p2, c.p3, c.p4];
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
    let mut units = content.encode_utf16().collect::<Vec<_>>();
    if units.len() <= max {
        return content.to_string();
    }
    units.truncate(max);
    // `encode_utf16` only ever pairs a low surrogate after a high one, so a cut
    // can leave a high surrogate (never a lone low) at the end.
    if units
        .last()
        .is_some_and(|unit| (0xD800..=0xDBFF).contains(unit))
    {
        units.pop();
    }
    let prefix = String::from_utf16(&units).unwrap_or_else(|_| String::from_utf16_lossy(&units));
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
fn legacy_tier(c: &DecayRenderCompartment<'_>) -> u8 {
    if c.content.lines().any(|l| l.starts_with("U:")) {
        3
    } else {
        4
    }
}

/// Render a single compartment at an explicit tier. Exposed for the m1 "new
/// compartments" block, which always renders newest compartments at P1 (full
/// fidelity — no decay applies to brand-new deltas).
pub fn render_compartment_at_tier(c: &DecayRenderCompartment<'_>, tier: u8) -> String {
    render_one_compartment(c, tier)
}

fn render_one_compartment(c: &DecayRenderCompartment<'_>, tier: u8) -> String {
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
fn compute_tiers(compartments: &[DecayRenderCompartment<'_>], history_budget: f64) -> Vec<u8> {
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

/// The join between rendered compartments. Counted once per seam by the guard
/// instead of re-measuring the whole joined body on every demotion.
const COMPARTMENT_SEAM: &str = "\n\n";

/// The joined body's token cost from its parts: every piece plus one seam per
/// join. Not bit-equal to counting the joined string — BPE merges across the
/// seam make the pieces cost slightly more than their join (measured: 0 to +6
/// tokens per seam, mean +0.79, never below the joined count; see the module's
/// deviation note). The alternative, counting the joined body, costs one full
/// tokenization per demotion.
fn body_cost(costs: &[usize], kept: usize, seam: usize) -> usize {
    costs.iter().sum::<usize>() + seam * kept.saturating_sub(1)
}

/// Render the decayed compartment-history body (no `<session-history>` wrapper
/// — callers add their own framing). Demotes oldest-first under the budget as
/// a drift guard, measured by the injected `estimate_tokens`. Never renders
/// session facts (v2 faithful).
///
/// Guard accounting is per piece, not per body: each compartment is rendered
/// and measured once, and a demotion re-renders only the demoted compartment,
/// so a guard loop that demotes N times costs one render + one measurement per
/// compartment plus N small ones — not N+1 full-body tokenizations.
pub fn render_decayed_compartments(
    compartments: &[DecayRenderCompartment<'_>],
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
            .copied()
            .collect::<Vec<_>>();
        filtered.as_slice()
    } else {
        compartments
    };
    let mut tiers = compute_tiers(compartments, history_budget_tokens);

    let mut pieces: Vec<String> = compartments
        .iter()
        .enumerate()
        .map(|(i, c)| render_one_compartment(c, tiers[i]))
        .collect();
    let cost_of = |piece: &str| -> usize {
        if piece.is_empty() {
            0
        } else {
            estimate_tokens(piece)
        }
    };
    let mut costs: Vec<usize> = pieces.iter().map(|p| cost_of(p)).collect();
    let seam = estimate_tokens(COMPARTMENT_SEAM);

    // Budget guard: the curve already targets the budget, but estimate drift or
    // a very tight budget can overshoot. Demote oldest-first until it fits.
    let mut guard = compartments.len() * 5;
    while history_budget_tokens > 0.0 && guard > 0 {
        let kept = pieces.iter().filter(|p| !p.is_empty()).count();
        if body_cost(&costs, kept, seam) as f64 <= history_budget_tokens {
            break;
        }
        let Some(index) = tiers.iter().position(|tier| *tier < 5) else {
            break;
        };
        tiers[index] += 1;
        let rendered = render_one_compartment(&compartments[index], tiers[index]);
        costs[index] = cost_of(&rendered);
        pieces[index] = rendered;
        guard -= 1;
    }

    // Join in place: the demoted set is already rendered, so only the
    // separators and one final copy are allocated.
    let mut body =
        String::with_capacity(pieces.iter().map(String::len).sum::<usize>() + 2 * pieces.len());
    let mut first = true;
    for piece in pieces {
        if piece.is_empty() {
            continue;
        }
        if !first {
            body.push_str(COMPARTMENT_SEAM);
        }
        body.push_str(&piece);
        first = false;
    }
    body
}
