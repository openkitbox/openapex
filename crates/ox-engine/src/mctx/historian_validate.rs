//! Historian output validation — Task 6b's half of the historian.
//!
//! The historian returns XML-ish text; this module decides whether that text is
//! safe to publish and in what shape. It is deliberately pure: it takes the raw
//! completion plus caller-provided chunk/store metadata and returns either a
//! publish plan ([`ValidatedChunk`]) or a [`HistorianValidationError`]. That
//! keeps persistence fail-closed — every range, gap and boundary is settled
//! before any write is possible.
//!
//! ## Port map (reference = `reference/magic-context/crates/mc-module/src/`)
//!
//! - [`parse_compartment_output`] ← `historian_validate.rs` line ~272: the
//!   `<output>` envelope, the `<compartment …>` elements and their `p1..p4`
//!   tiers, the `<facts>` and `<events>` side channels, `<unprocessed_from>`.
//! - [`validate_historian_output`] ← same file line ~518: the rule order is
//!   chunk coverage, stored-range sanity, chunk/store continuity, gap healing,
//!   terminal tool-arc healing, ordinal mapping, per-compartment validation, the
//!   terminal boundary check, discard-last boundary healing, forward progress.
//! - the helpers each rule needs: `validate_dense_chunk_coverage` ~787,
//!   `validate_chunk_coverage_against` ~817, `validate_strictly_increasing_ordinals`
//!   ~755, `next_present_after` ~773, `heal_compartment_gaps` ~961,
//!   `heal_terminal_completed_tool_arc` ~926, `validate_compartment_mapping`
//!   ~996 (the ordinal-endpoint half of the reference's
//!   `map_parsed_compartments_to_chunk`; the conversion is
//!   `From<ParsedCompartment> for ValidatedCompartment`),
//!   `validate_parsed_compartments` ~1045, `parse_events` ~879.
//!   [`completed_tool_arc_crosses_boundary`] ← `boundary.rs` line ~1265.
//!
//! ## Deviations from the reference (all forced by ox's data model)
//!
//! - **Ordinals are `i64`, not `u64`.** ox's `session_message.seq` and
//!   `mc_compartments.coverage_end` are `i64`, so the port keeps that width
//!   rather than casting back and forth.
//! - **No message-id endpoints and no id-ordering check.** ox publishes the
//!   compartment's end *ordinal* as its coverage end and the request assembly
//!   reads that ordinal; the reference instead resolves the boundary to a
//!   provider message id (`ChunkLine.message_id`/`anchorable`, the
//!   anchorability check in `map_parsed_compartments_to_chunk`, and
//!   `validate_stored_compartments`) so publication cannot mint an impossible
//!   boundary. An ordinal boundary is well formed by construction, so those
//!   checks have no ox counterpart.
//! - **No `sequence`.** The reference numbers the compartments of one publish;
//!   ox orders `mc_compartments` rows by `created_at`, `id`.
//! - **[`ChunkView`] replaces `HistorianChunk`.** ox's chunk is a formatted
//!   transcript, not a list of `ChunkLine`s, so `line_ordinals` and
//!   `present_ordinals` are the same set in production; the dense-coverage rule
//!   stays a unit-testable rule rather than the production path.
//! - **[`ValidateOptions`] keeps only what the ported rules read**
//!   (`in_emergency`, `force_keep_last_compartment`). The memory/privacy switches
//!   gated fact promotion and user-memory collection, neither of which ox has.
//! - **`<user_observations>` / `<primer_candidates>` are not parsed.** Their only
//!   consumer is the dreamer's user-memory review (Task 8). `<facts>` and
//!   `<events>` are parsed and carried on [`ValidatedChunk`] because ox's
//!   `mc_compartments` has those columns; publication writes them as neutral
//!   payloads for now (see [`crate::mctx::historian`]).
//! - **The dropped-category report goes through `tracing`** with the reference's
//!   message text.
//! - **The `debug_assert` on the emitted count is dropped** with the count.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

/// A raw ordinal range, inclusive on both ends
/// (`historian_validate.rs::MessageRange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageRange {
    pub start: i64,
    pub end: i64,
}

/// A tail of `BOUNDARY_HEALING_SLACK` messages or fewer is not enough lookahead
/// to settle the newest boundary, so the terminal compartment is withheld
/// (`historian_validate.rs::BOUNDARY_HEALING_SLACK`).
const BOUNDARY_HEALING_SLACK: i64 = 2;

/// The chunk the historian was asked to summarize, from validation's point of
/// view (`historian_validate.rs::HistorianChunk`).
#[derive(Debug, Clone, Copy)]
pub struct ChunkView<'a> {
    /// First covered ordinal, inclusive.
    pub start_ordinal: i64,
    /// Last covered ordinal, inclusive.
    pub end_ordinal: i64,
    /// Ordinals the chunk's formatted transcript displays. In ox this is the
    /// same set as `present_ordinals`; it is separate so the dense-coverage rule
    /// stays expressible (and testable) when `present_ordinals` is empty.
    pub line_ordinals: &'a [i64],
    /// Every non-synthetic ordinal visible when the chunk was built. `session_message.seq`
    /// can have holes (retired rows), so an empty set means "assume dense".
    pub present_ordinals: &'a [i64],
    /// Gaps fully inside one of these ranges are tool-only transcript noise and
    /// may be healed at any size.
    pub tool_only_ranges: &'a [MessageRange],
    /// Completed invocation/result ranges whose terminal publication boundary
    /// must stay atomic.
    pub completed_tool_arcs: &'a [MessageRange],
}

/// Options known to the runner but not present in the historian XML
/// (`historian_validate.rs::ValidateOptions`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidateOptions {
    /// Emergency recovery favors fast raw-history reduction over the best final
    /// boundary for the newest compartment.
    pub in_emergency: bool,
    /// An explicit wrapup run keeps its final compartment instead of discarding
    /// it during boundary healing.
    pub force_keep_last_compartment: bool,
}

/// A parsed compartment before it is mapped to a chunk range
/// (`historian_validate.rs::ParsedCompartment`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCompartment {
    pub start_message: i64,
    pub end_message: i64,
    pub title: String,
    /// In v2 compartments the body is duplicated into `p1`; v1/legacy
    /// compartments keep their body text only here.
    pub content: String,
    pub p1: Option<String>,
    pub p2: Option<String>,
    pub p3: Option<String>,
    pub p4: Option<String>,
    pub importance: Option<i64>,
    pub episode_type: Option<String>,
}

/// A fact extracted from the `<facts>` block
/// (`historian_validate.rs::FactCandidate`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FactCandidate {
    pub category: String,
    pub content: String,
    /// Optional anchor into this output's emitted compartments. Only set when
    /// the model wrote `[origin_compartment=N]`.
    pub origin_compartment_index: Option<i64>,
}

/// A historian-extracted event: the element name is the kind, child element text
/// is keyed by element name (`historian_validate.rs::ParsedEvent`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParsedEvent {
    pub kind: String,
    /// 1-based position of the anchored compartment in this output, after the
    /// out-of-order remap.
    pub at_compartment: Option<i64>,
    pub fields: BTreeMap<String, String>,
}

/// Parsed historian output, before validation heals or discards anything
/// (`historian_validate.rs::ParsedCompartmentOutput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCompartmentOutput {
    pub compartments: Vec<ParsedCompartment>,
    pub facts: Vec<FactCandidate>,
    pub dropped_fact_blocks: usize,
    pub dropped_facts: usize,
    pub events: Vec<ParsedEvent>,
    pub unprocessed_from: Option<i64>,
}

/// A compartment whose range has been mapped onto the chunk and is safe to
/// publish (`historian_validate.rs::ValidatedCompartment`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCompartment {
    pub start_message: i64,
    pub end_message: i64,
    pub title: String,
    pub content: String,
    pub p1: Option<String>,
    pub p2: Option<String>,
    pub p3: Option<String>,
    pub p4: Option<String>,
    pub importance: Option<i64>,
    pub episode_type: Option<String>,
}

/// The side-effect-free publish plan produced by validation
/// (`historian_validate.rs::ValidatedChunk`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidatedChunk {
    pub compartments: Vec<ValidatedCompartment>,
    pub facts: Vec<FactCandidate>,
    pub dropped_fact_blocks: usize,
    pub dropped_facts: usize,
    pub events: Vec<ParsedEvent>,
    /// The next ordinal safe to read after the compartments that persist.
    pub unprocessed_from: i64,
    /// True when the provisional last compartment was withheld so the next run
    /// can re-derive it with real lookahead.
    pub discarded_last: bool,
}

/// Validation failures are plain messages because callers surface them in
/// repair prompts and telemetry (`historian_validate.rs::HistorianValidationError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorianValidationError {
    pub message: String,
}

impl std::fmt::Display for HistorianValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HistorianValidationError {}

fn validation_error(message: impl Into<String>) -> HistorianValidationError {
    HistorianValidationError {
        message: message.into(),
    }
}

/// Require one complete historian output envelope, then use the permissive
/// extraction semantics of the reference's host parser for the structures
/// inside that root: malformed inner XML yields fewer usable structures for
/// validation to assess (`historian_validate.rs::parse_compartment_output`).
pub fn parse_compartment_output(
    text: &str,
) -> Result<ParsedCompartmentOutput, HistorianValidationError> {
    let Some(root) = output_document_regex().captures(text) else {
        return Err(validation_error(
            "Historian output must be one complete <output> root document.",
        ));
    };
    let root_body = root
        .name("body")
        .map(|capture| capture.as_str())
        .unwrap_or_default();
    // A compartment body may mention `<output>` as text; only tags outside the
    // compartment elements can open or close a second root.
    let root_elements = find_compartment_elements(root_body);
    if output_tag_regex().is_match(&text_outside_compartments(root_body, &root_elements)) {
        return Err(validation_error(
            "Historian output must contain exactly one <output> root document.",
        ));
    }

    let mut compartments = Vec::new();

    let elements = find_compartment_elements(text);
    for element in &elements {
        let attrs = element.attrs;
        let inner = element.inner;

        let Some(start_message) = capture_i64(attr_start_regex(), attrs) else {
            continue;
        };
        let Some(end_message) = capture_i64(attr_end_regex(), attrs) else {
            continue;
        };
        let title = match capture_string(attr_title_regex(), attrs) {
            Some(value) if !value.is_empty() => unescape_xml(value),
            _ => continue,
        };
        if title.is_empty() {
            continue;
        }

        let episode_type = capture_string(attr_episode_regex(), attrs).map(unescape_xml);
        let importance = capture_i64(attr_importance_regex(), attrs);

        let mut tiers = extract_tiers(inner);
        if let Some(p1_value) = tiers.remove(&1).filter(|value| !value.is_empty()) {
            let p2 = tiers.remove(&2);
            let p3 = tiers.remove(&3);
            let p4 = tiers.remove(&4);
            // Each missing tier is densified from its parent, so the copy only
            // happens when the tier is actually absent: a present tier moves
            // into its own value and is never cloned.
            let p2_value = p2.unwrap_or_else(|| p1_value.clone());
            let p3_value = p3.unwrap_or_else(|| p2_value.clone());
            let p4_value = p4.unwrap_or_default();
            compartments.push(ParsedCompartment {
                start_message,
                end_message,
                title,
                content: p1_value.clone(),
                p1: Some(p1_value),
                p2: Some(p2_value),
                p3: Some(p3_value),
                p4: Some(p4_value),
                importance,
                episode_type,
            });
            continue;
        }

        let content = unescape_xml(inner.trim());
        if !content.is_empty() {
            compartments.push(ParsedCompartment {
                start_message,
                end_message,
                title,
                content,
                p1: None,
                p2: None,
                p3: None,
                p4: None,
                importance,
                episode_type,
            });
        }
    }

    // Every side channel is read outside the compartment elements, so a body
    // that mentions `<facts>` or `<events>` cannot be mistaken for the real block.
    let outside = text_outside_compartments(text, &elements);
    // A `<facts>` block is read in place; only the no-block fallback has to
    // synthesize a stripped copy of the remaining text.
    let facts_scope: Cow<'_, str> = if let Some(caps) = facts_block_regex().captures(&outside) {
        Cow::Borrowed(caps.get(1).map_or("", |m| m.as_str()))
    } else {
        let without_events = events_block_regex().replace_all(&outside, "");
        let without_side_channels = facts_side_channel_regex().replace_all(&without_events, "");
        Cow::Owned(
            facts_envelope_regex()
                .replace_all(&without_side_channels, "")
                .into_owned(),
        )
    };

    let mut facts = Vec::new();
    let mut dropped_fact_blocks = 0;
    let mut dropped_facts = 0;
    for (category, block) in paired_elements(&facts_scope, category_open_regex()) {
        if !HISTORIAN_CATEGORIES.contains(&category) {
            let count = fact_item_regex()
                .captures_iter(block)
                .filter(|caps| {
                    caps.get(1)
                        .is_some_and(|m| !unescape_xml(m.as_str().trim()).is_empty())
                })
                .count();
            if count == 0 {
                continue;
            }
            dropped_fact_blocks += 1;
            dropped_facts += count;
            tracing::warn!("{}", format_dropped_fact_category(category, count));
            continue;
        }
        for item_caps in fact_item_regex().captures_iter(block) {
            let raw = item_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let unescaped = unescape_xml(raw.trim());
            let (origin_compartment_index, content) = split_anchor_prefix(&unescaped);
            if !content.is_empty() {
                facts.push(FactCandidate {
                    category: category.to_string(),
                    content,
                    origin_compartment_index,
                });
            }
        }
    }

    let unprocessed_from = capture_i64(unprocessed_regex(), &outside);

    let mut events = parse_events(&outside);

    // Compartments are returned sorted by start, but anchors count compartments
    // in emission order. Re-point each in-range anchor at the same compartment's
    // sorted position, because every consumer indexes the sorted list.
    let mut emitted_order: Vec<usize> = (0..compartments.len()).collect();
    emitted_order.sort_by_key(|&emitted| compartments[emitted].start_message);
    let mut sorted_position = vec![0_i64; compartments.len()];
    for (sorted, &emitted) in emitted_order.iter().enumerate() {
        sorted_position[emitted] = sorted as i64 + 1;
    }
    let remap_anchor = |anchor: &mut Option<i64>| {
        if let Some(index) = anchor.as_mut()
            && let Some(&position) = usize::try_from(*index)
                .ok()
                .and_then(|index| index.checked_sub(1))
                .and_then(|slot| sorted_position.get(slot))
        {
            *index = position;
        }
    };
    for event in &mut events {
        remap_anchor(&mut event.at_compartment);
    }
    for fact in &mut facts {
        remap_anchor(&mut fact.origin_compartment_index);
    }
    // Stable, so equal starts keep emission order exactly as `emitted_order` did.
    compartments.sort_by_key(|compartment| compartment.start_message);

    Ok(ParsedCompartmentOutput {
        compartments,
        facts,
        dropped_fact_blocks,
        dropped_facts,
        events,
        unprocessed_from,
    })
}

/// Parse, heal safe gaps, map endpoint ordinals, enforce coverage, apply
/// discard-last boundary healing, and return only data safe to persist
/// (`historian_validate.rs::validate_historian_output`).
pub fn validate_historian_output(
    text: &str,
    chunk: &ChunkView<'_>,
    prior_compartments: &[MessageRange],
    options: ValidateOptions,
) -> Result<ValidatedChunk, HistorianValidationError> {
    let present_ordinals = chunk_present_ordinals(chunk);
    if let Some(error) = validate_chunk_coverage(chunk) {
        return Err(validation_error(format!(
            "Historian chunk coverage invalid: {error}"
        )));
    }

    if let Some(error) = validate_stored_compartments(prior_compartments) {
        return Err(validation_error(format!(
            "Existing compartments are invalid: {error}"
        )));
    }

    if let Some(last) = prior_compartments.last()
        && chunk.start_ordinal <= last.end
    {
        return Err(validation_error(format!(
            "Historian chunk starts at raw message {} but existing compartments end at {}; expected a strictly newer raw message",
            chunk.start_ordinal, last.end
        )));
    }
    if let Some(last) = prior_compartments.last()
        && let Some(expected_start) = next_present_after(present_ordinals, last.end)
        && chunk.start_ordinal != expected_start
    {
        return Err(validation_error(format!(
            "Historian chunk starts at raw message {} but existing compartments end at {}; expected next present raw message {expected_start}",
            chunk.start_ordinal, last.end
        )));
    }

    let mut parsed = parse_compartment_output(text)?;
    if parsed.compartments.is_empty() {
        return Err(validation_error(
            "Historian returned no usable compartments.",
        ));
    }

    heal_compartment_gaps(
        &mut parsed.compartments,
        chunk.tool_only_ranges,
        present_ordinals,
    );
    heal_terminal_completed_tool_arc(
        &mut parsed.compartments,
        &mut parsed.unprocessed_from,
        chunk.completed_tool_arcs,
        present_ordinals,
        chunk.end_ordinal,
    );

    validate_compartment_mapping(&parsed.compartments, chunk).map_err(|error| {
        validation_error(format!(
            "Historian returned invalid compartment output: {error}"
        ))
    })?;

    if let Some(error) = validate_parsed_compartments(
        &parsed.compartments,
        chunk.start_ordinal,
        chunk.end_ordinal,
        present_ordinals,
        parsed.unprocessed_from,
    ) {
        return Err(validation_error(format!(
            "Historian returned invalid compartment output: {error}"
        )));
    }
    if parsed.compartments.last().is_some_and(|compartment| {
        boundary_splits_completed_tool_arc(
            compartment.end_message.saturating_add(1),
            chunk.completed_tool_arcs,
        )
    }) {
        return Err(validation_error(
            "Historian terminal boundary splits a completed tool invocation/result arc",
        ));
    }

    // Every borrow-only rule has passed; the parse tree's strings now move into
    // the publish plan.
    let mut compartments: Vec<ValidatedCompartment> = parsed
        .compartments
        .into_iter()
        .map(ValidatedCompartment::from)
        .collect();
    let mut discarded_last = false;
    if !options.in_emergency && !options.force_keep_last_compartment && compartments.len() >= 2 {
        let last_end = compartments
            .last()
            .map(|compartment| compartment.end_message)
            .unwrap_or(chunk.end_ordinal);
        // Ordinal distance, not present-message count: retired ordinals are part
        // of the distance, so a sparse coordinate gap still counts as lookahead.
        let lookahead_distance = chunk.end_ordinal.saturating_sub(last_end);
        let previous_end = compartments
            .get(compartments.len().saturating_sub(2))
            .map(|compartment| compartment.end_message);
        let pop_would_split_arc = previous_end.is_some_and(|end| {
            boundary_splits_completed_tool_arc(end.saturating_add(1), chunk.completed_tool_arcs)
        });
        if lookahead_distance <= BOUNDARY_HEALING_SLACK && !pop_would_split_arc {
            compartments.pop();
            discarded_last = true;
        }
    }

    let offset = prior_compartments
        .last()
        .map(|compartment| compartment.end.saturating_add(1))
        .unwrap_or(chunk.start_ordinal);
    let last_new_end = compartments
        .last()
        .map(|compartment| compartment.end_message)
        .unwrap_or(0);
    if last_new_end < offset {
        return Err(validation_error(format!(
            "no forward progress beyond raw message {}",
            offset.saturating_sub(1)
        )));
    }

    let persisted_count = compartments.len() as i64;
    let facts = parsed
        .facts
        .into_iter()
        .filter(|fact| {
            !options.force_keep_last_compartment
                && keep_side_channel(
                    fact.origin_compartment_index,
                    persisted_count,
                    discarded_last,
                )
        })
        .collect();
    // A discarded lookahead compartment invalidates the whole output's anchors;
    // keeping any side channel would make a later re-read double-store it.
    let events = parsed
        .events
        .into_iter()
        .filter(|event| {
            if options.force_keep_last_compartment {
                matches!(event.at_compartment, Some(index) if (1..persisted_count).contains(&index))
            } else {
                keep_side_channel(event.at_compartment, persisted_count, discarded_last)
            }
        })
        .collect();

    Ok(ValidatedChunk {
        compartments,
        facts,
        dropped_fact_blocks: parsed.dropped_fact_blocks,
        dropped_facts: parsed.dropped_facts,
        events,
        // A publication floor, not a promise that the next integer exists.
        unprocessed_from: last_new_end.saturating_add(1),
        discarded_last,
    })
}

/// Validate already-persisted ranges before appending new output
/// (`historian_validate.rs::validate_stored_compartments`).
#[must_use]
pub fn validate_stored_compartments(compartments: &[MessageRange]) -> Option<String> {
    let first = compartments.first()?;
    if first.end < first.start {
        return Some(format!("invalid range {}-{}", first.start, first.end));
    }

    let mut previous_end = first.end;
    for compartment in &compartments[1..] {
        if compartment.end < compartment.start {
            return Some(format!(
                "invalid range {}-{}",
                compartment.start, compartment.end
            ));
        }
        if compartment.start <= previous_end {
            return Some(format!(
                "overlap before message {} (saw {}-{})",
                previous_end.saturating_add(1),
                compartment.start,
                compartment.end
            ));
        }
        previous_end = compartment.end;
    }

    None
}

/// Every ordinal the chunk must cover, borrowed from the chunk view: the
/// present set when the caller knows it, otherwise the dense line set.
fn chunk_present_ordinals<'a>(chunk: &ChunkView<'a>) -> &'a [i64] {
    if !chunk.present_ordinals.is_empty() {
        chunk.present_ordinals
    } else {
        chunk.line_ordinals
    }
}

fn validate_strictly_increasing_ordinals(ordinals: &[i64], label: &str) -> Option<String> {
    for pair in ordinals.windows(2) {
        let (previous, current) = (pair[0], pair[1]);
        if current == previous {
            return Some(format!(
                "{label} contain duplicate raw message ordinal {current}"
            ));
        }
        if current < previous {
            return Some(format!(
                "{label} decrease from raw message {previous} to {current}"
            ));
        }
    }
    None
}

fn next_present_after(ordinals: &[i64], after: i64) -> Option<i64> {
    ordinals.iter().copied().find(|ordinal| *ordinal > after)
}

/// Ensure the chunk's ordinal lines cover exactly the present input ordinals in
/// the advertised range. A missing integer is valid when it is absent from the
/// real input set (`historian_validate.rs::validate_chunk_coverage`).
fn validate_chunk_coverage(chunk: &ChunkView<'_>) -> Option<String> {
    if chunk.present_ordinals.is_empty() {
        return validate_dense_chunk_coverage(chunk);
    }
    validate_chunk_coverage_against(chunk, chunk.present_ordinals)
}

fn validate_dense_chunk_coverage(chunk: &ChunkView<'_>) -> Option<String> {
    let line_ordinals = chunk.line_ordinals;
    if let Some(error) = validate_strictly_increasing_ordinals(line_ordinals, "chunk lines") {
        return Some(error);
    }
    if line_ordinals.is_empty() {
        return None;
    }

    let mut expected_ordinal = chunk.start_ordinal;
    for ordinal in line_ordinals {
        if *ordinal != expected_ordinal {
            return Some(format!(
                "chunk omits raw message {expected_ordinal} while still claiming coverage through {}",
                chunk.end_ordinal
            ));
        }
        expected_ordinal = expected_ordinal.saturating_add(1);
    }

    if expected_ordinal.saturating_sub(1) != chunk.end_ordinal {
        return Some(format!(
            "chunk omits raw message {expected_ordinal} while still claiming coverage through {}",
            chunk.end_ordinal
        ));
    }

    None
}

fn validate_chunk_coverage_against(
    chunk: &ChunkView<'_>,
    present_ordinals: &[i64],
) -> Option<String> {
    if let Some(error) = validate_strictly_increasing_ordinals(present_ordinals, "input ordinals") {
        return Some(error);
    }

    let line_ordinals = chunk.line_ordinals;
    if let Some(error) = validate_strictly_increasing_ordinals(line_ordinals, "chunk lines") {
        return Some(error);
    }

    if let Some(outside) = line_ordinals
        .iter()
        .find(|ordinal| **ordinal < chunk.start_ordinal || **ordinal > chunk.end_ordinal)
    {
        return Some(format!(
            "chunk line raw message {outside} is outside claimed coverage {}-{}",
            chunk.start_ordinal, chunk.end_ordinal
        ));
    }

    let expected: Vec<i64> = present_ordinals
        .iter()
        .copied()
        .filter(|ordinal| *ordinal >= chunk.start_ordinal && *ordinal <= chunk.end_ordinal)
        .collect();

    for (line, expected) in line_ordinals.iter().zip(expected.iter()) {
        if line == expected {
            continue;
        }
        if line > expected {
            return Some(format!(
                "chunk omits raw message {expected} while still claiming coverage through {}",
                chunk.end_ordinal
            ));
        }
        return Some(format!(
            "chunk includes raw message {line} that is not present in input range {}-{}",
            chunk.start_ordinal, chunk.end_ordinal
        ));
    }

    if let Some(missing) = expected.get(line_ordinals.len()) {
        return Some(format!(
            "chunk omits raw message {missing} while still claiming coverage through {}",
            chunk.end_ordinal
        ));
    }

    if let Some(extra) = line_ordinals.get(expected.len()) {
        return Some(format!(
            "chunk includes raw message {extra} that is not present in input range {}-{}",
            chunk.start_ordinal, chunk.end_ordinal
        ));
    }

    None
}

fn parse_events(text: &str) -> Vec<ParsedEvent> {
    let Some(block_caps) = events_block_regex().captures(text) else {
        return Vec::new();
    };
    let block = block_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
    let mut events = Vec::new();

    // The Rust regex engine has no backreferences, while the host parser uses one
    // to require `</kind>` to match the opening element. Match only event *open*
    // tags here, then search for the corresponding literal close tag. Event child
    // fields never carry `at_compartment`, so they cannot be mistaken for opens.
    for event_caps in event_open_regex().captures_iter(block) {
        let Some(full_match) = event_caps.get(0) else {
            continue;
        };
        let kind = event_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let close_tag = format!("</{kind}>");
        let body_start = full_match.end();
        let Some(relative_body_end) = block[body_start..].find(&close_tag) else {
            continue;
        };
        let body = &block[body_start..body_start + relative_body_end];

        let mut fields = BTreeMap::new();
        for (name, raw_value) in paired_elements(body, event_field_open_regex()) {
            let value = unescape_xml(raw_value.trim());
            if !value.is_empty() {
                fields.insert(name.to_string(), value);
            }
        }
        events.push(ParsedEvent {
            kind: kind.to_string(),
            at_compartment: event_caps
                .get(2)
                .and_then(|m| m.as_str().parse::<i64>().ok()),
            fields,
        });
    }
    events
}

/// True when a tail beginning at `boundary` would retain a completed result
/// without its call (`boundary.rs::completed_tool_arc_crosses_boundary`).
fn completed_tool_arc_crosses_boundary(inv_ordinal: i64, res_ordinal: i64, boundary: i64) -> bool {
    inv_ordinal < boundary && boundary <= res_ordinal
}

fn boundary_splits_completed_tool_arc(boundary: i64, arcs: &[MessageRange]) -> bool {
    arcs.iter()
        .any(|arc| completed_tool_arc_crosses_boundary(arc.start, arc.end, boundary))
}

/// Extend the terminal compartment to the end of a completed tool arc its
/// boundary would otherwise split (`historian_validate.rs::heal_terminal_completed_tool_arc`).
fn heal_terminal_completed_tool_arc(
    compartments: &mut [ParsedCompartment],
    unprocessed_from: &mut Option<i64>,
    arcs: &[MessageRange],
    present_ordinals: &[i64],
    chunk_end: i64,
) {
    let Some(last) = compartments.last_mut() else {
        return;
    };
    let original_end = last.end_message;
    // Each pass can reveal a further arc, so bound the walk by the arc count.
    for _ in 0..=arcs.len() {
        let boundary = last.end_message.saturating_add(1);
        let next_end = arcs
            .iter()
            .filter(|arc| {
                arc.end <= chunk_end
                    && completed_tool_arc_crosses_boundary(arc.start, arc.end, boundary)
            })
            .map(|arc| arc.end)
            .max()
            .unwrap_or(last.end_message);
        if next_end == last.end_message {
            break;
        }
        last.end_message = next_end;
    }
    if last.end_message != original_end
        && let Some(unprocessed) = unprocessed_from.as_mut()
    {
        *unprocessed = next_present_after(present_ordinals, last.end_message)
            .unwrap_or_else(|| last.end_message.saturating_add(1));
    }
}

/// Absorb a gap only when every omitted present ordinal is tool-only noise
/// (`historian_validate.rs::heal_compartment_gaps`).
fn heal_compartment_gaps(
    compartments: &mut [ParsedCompartment],
    tool_only_ranges: &[MessageRange],
    present_ordinals: &[i64],
) {
    for i in 1..compartments.len() {
        let gap_start = compartments[i - 1].end_message.saturating_add(1);
        let gap_end = compartments[i].start_message.saturating_sub(1);
        if gap_end < gap_start {
            continue;
        }
        // One pass over the present ordinals: the last omitted ordinal and
        // whether every omitted one is tool-only noise, without materializing
        // the filtered set.
        let mut last_omitted: Option<i64> = None;
        let mut fully_inside_tool_only = true;
        for &ordinal in present_ordinals {
            if ordinal < gap_start || ordinal > gap_end {
                continue;
            }
            last_omitted = Some(ordinal);
            if fully_inside_tool_only
                && !tool_only_ranges
                    .iter()
                    .any(|range| range.start <= ordinal && range.end >= ordinal)
            {
                fully_inside_tool_only = false;
            }
        }
        let Some(last) = last_omitted else {
            continue;
        };
        // Production replay showed contiguous narrative coverage; tool-only noise
        // is therefore the sole safe gap to absorb.
        if fully_inside_tool_only {
            compartments[i - 1].end_message = last;
        }
    }
}

/// Map parsed ranges onto the chunk's covered ordinals
/// (`historian_validate.rs::map_parsed_compartments_to_chunk`, minus the
/// message-id resolution ox does not have).
///
/// Split from the conversion so the rule can run — in its original position, and
/// so with its error precedence intact — against a borrow of the parse tree,
/// while [`ValidatedCompartment::from`] later moves the strings out of it.
fn validate_compartment_mapping(
    compartments: &[ParsedCompartment],
    chunk: &ChunkView<'_>,
) -> Result<(), String> {
    for compartment in compartments {
        let both_lines = chunk.line_ordinals.contains(&compartment.start_message)
            && chunk.line_ordinals.contains(&compartment.end_message);
        if !both_lines {
            return Err(format!(
                "Compartment range {}-{} does not map to raw session lines {}-{}",
                compartment.start_message,
                compartment.end_message,
                chunk.start_ordinal,
                chunk.end_ordinal
            ));
        }
    }
    Ok(())
}

/// The publish plan owns the parse tree's strings: conversion moves every field
/// instead of cloning it.
impl From<ParsedCompartment> for ValidatedCompartment {
    fn from(compartment: ParsedCompartment) -> Self {
        Self {
            start_message: compartment.start_message,
            end_message: compartment.end_message,
            title: compartment.title,
            content: compartment.content,
            p1: compartment.p1,
            p2: compartment.p2,
            p3: compartment.p3,
            p4: compartment.p4,
            importance: compartment.importance,
            episode_type: compartment.episode_type,
        }
    }
}

/// Enforce the per-compartment rules: tiered paraphrase present, range inside
/// the chunk, no overlap, no narrative gap, and a consistent
/// `<unprocessed_from>` (`historian_validate.rs::validate_parsed_compartments`).
fn validate_parsed_compartments(
    compartments: &[ParsedCompartment],
    chunk_start: i64,
    chunk_end: i64,
    present_ordinals: &[i64],
    unprocessed_from: Option<i64>,
) -> Option<String> {
    let chunk_ordinals: Vec<i64> = present_ordinals
        .iter()
        .copied()
        .filter(|ordinal| *ordinal >= chunk_start && *ordinal <= chunk_end)
        .collect();
    let mut expected_start = chunk_ordinals.first().copied();

    for (index, compartment) in compartments.iter().enumerate() {
        // P1 is the required v2 boundary. Missing P2-P4 deliberately keep the
        // parser's denser-tier fallbacks; only the flat v1 shape must retry.
        match compartment.p1.as_deref() {
            Some(p1) if !p1.trim().is_empty() => {}
            _ => {
                return Some(format!(
                    "compartment {} is missing the tiered paraphrase structure (p1..p4); re-emit with all four tiers",
                    index + 1
                ));
            }
        }
        if compartment.end_message < compartment.start_message {
            return Some(format!(
                "invalid range {}-{}",
                compartment.start_message, compartment.end_message
            ));
        }
        if compartment.start_message < chunk_start || compartment.end_message > chunk_end {
            return Some(format!(
                "range {}-{} is outside chunk {}-{}",
                compartment.start_message, compartment.end_message, chunk_start, chunk_end
            ));
        }
        if !chunk_ordinals.contains(&compartment.start_message) {
            return Some(format!(
                "range start {} is not a present raw message in chunk {}-{}",
                compartment.start_message, chunk_start, chunk_end
            ));
        }
        if !chunk_ordinals.contains(&compartment.end_message) {
            return Some(format!(
                "range end {} is not a present raw message in chunk {}-{}",
                compartment.end_message, chunk_start, chunk_end
            ));
        }
        let Some(expected) = expected_start else {
            return Some(format!(
                "range {}-{} starts after chunk coverage already ended",
                compartment.start_message, compartment.end_message
            ));
        };
        if compartment.start_message != expected {
            if compartment.start_message < expected {
                return Some(format!(
                    "overlap before message {expected} (saw {}-{})",
                    compartment.start_message, compartment.end_message
                ));
            }
            return Some(format!(
                "gap before present message {} (expected {expected})",
                compartment.start_message
            ));
        }
        expected_start = next_present_after(&chunk_ordinals, compartment.end_message);
    }

    if let Some(unprocessed_from) = unprocessed_from {
        if let Some(expected) = expected_start {
            if unprocessed_from != expected {
                return Some(format!(
                    "<unprocessed_from> {unprocessed_from} does not match next uncovered message {expected}"
                ));
            }
            return None;
        }
        if unprocessed_from == chunk_end.saturating_add(1) {
            return None;
        }
        if unprocessed_from < chunk_start || unprocessed_from > chunk_end {
            return Some(format!(
                "<unprocessed_from> {unprocessed_from} is outside chunk {chunk_start}-{chunk_end}"
            ));
        }
        return Some(format!(
            "<unprocessed_from> {unprocessed_from} does not match completed chunk boundary {}",
            chunk_end.saturating_add(1)
        ));
    }

    if let Some(expected) = expected_start {
        return Some(format!(
            "output left uncovered messages {expected}-{chunk_end} without <unprocessed_from>"
        ));
    }

    None
}

fn keep_side_channel(
    origin_compartment_index: Option<i64>,
    persisted_count: i64,
    discarded_last: bool,
) -> bool {
    if discarded_last {
        return false;
    }
    match origin_compartment_index {
        Some(index) => (1..=persisted_count).contains(&index),
        None => !discarded_last,
    }
}

/// Capture group 1 as a slice of the haystack: the callers either unescape it
/// (which allocates) or copy it into an owned field, so an intermediate
/// `String` here would be a second copy of the same bytes.
fn capture_string<'h>(regex: &Regex, haystack: &'h str) -> Option<&'h str> {
    regex
        .captures(haystack)
        .and_then(|caps| caps.get(1).map(|m| m.as_str()))
}

fn capture_i64(regex: &Regex, haystack: &str) -> Option<i64> {
    regex
        .captures(haystack)
        .and_then(|caps| caps.get(1).and_then(|m| m.as_str().parse::<i64>().ok()))
}

/// True when `inner` opens tier `digit` anywhere at or after `from`.
fn has_tier_opener_from(inner: &str, digit: u32, from: usize) -> bool {
    tier_open_regex()
        .captures_iter(&inner[from..])
        .any(|open| tier_digit(&open) == digit)
}

/// The ASCII digit a tier tag captured. The tier regexes capture exactly one
/// `[0-9]`, so `0` (an out-of-range tier) is a safe stand-in for a failed parse.
fn tier_digit(caps: &regex::Captures<'_>) -> u32 {
    caps.get(1)
        .and_then(|m| m.as_str().parse::<u32>().ok())
        .unwrap_or_default()
}

/// Find the close of tier `digit` whose body starts at `body_start`, when the
/// tier is properly closed: the first `</pN>` with the same digit followed only
/// by another tier tag or the end of the compartment. Mirrors the reference's
/// `find_matching_tier_close`.
fn find_matching_tier_close(
    inner: &str,
    body_start: usize,
    digit: u32,
    found: &BTreeMap<u32, String>,
) -> Option<(usize, usize)> {
    for close in tier_close_regex().captures_iter(&inner[body_start..]) {
        let whole = close.get(0)?;
        let (start, end) = (body_start + whole.start(), body_start + whole.end());
        if tier_digit(&close) != digit {
            continue;
        }
        if !tier_close_follower_regex().is_match(&inner[end..]) {
            continue;
        }
        let otherwise_missing = tier_open_regex()
            .captures_iter(&inner[body_start..start])
            .any(|open| {
                let other = tier_digit(&open);
                (1..=4).contains(&other)
                    && other != digit
                    && !found.contains_key(&other)
                    && !has_tier_opener_from(inner, other, end)
            });
        if otherwise_missing {
            return None;
        }
        return Some((start, end));
    }
    None
}

/// Extract the tier bodies from a compartment inner string, walking the tiers in
/// the order they appear. A properly closed tier keeps any tier tags written
/// inside it as text; otherwise the parser is lenient (an opened `<pN>` is
/// terminated by the next closing tier tag of any digit, or by the next opening
/// tag / the end of the compartment). Mirrors the reference's `extract_tiers`.
fn extract_tiers(inner: &str) -> BTreeMap<u32, String> {
    let mut tiers = BTreeMap::new();
    let mut pos = 0;
    while let Some(open) = tier_open_regex().captures_at(inner, pos) {
        let Some(whole) = open.get(0) else {
            break;
        };
        let digit = tier_digit(&open);
        let body_start = whole.end();
        let mut body_end = body_start;
        pos = body_start;
        // Self-close form (`<p4/>`) → empty tier.
        if open.get(2).map(|m| m.as_str()) != Some("/") {
            if let Some((close_start, close_end)) =
                find_matching_tier_close(inner, body_start, digit, &tiers)
            {
                body_end = close_start;
                pos = close_end;
            } else {
                // Lenient bounds: the next closing tier tag of any digit, or the
                // end of the compartment when there is none.
                let close_at = tier_close_any_regex()
                    .find_at(inner, body_start)
                    .map(|m| m.start());
                body_end = close_at.unwrap_or(inner.len());
                pos = close_at.map_or(inner.len(), |at| at + 1);
                // Over-capture guard: never swallow a later tier's opening tag.
                if let Some(open_inside) =
                    tier_open_any_regex().find_at(&inner[..body_end], body_start)
                {
                    body_end = open_inside.start();
                    pos = body_end;
                }
            }
        }
        tiers
            .entry(digit)
            .or_insert_with(|| unescape_xml(inner[body_start..body_end].trim()));
    }
    tiers
}

/// One `<compartment …>…</compartment>` element; offsets index the parsed text.
struct CompartmentElement<'a> {
    start: usize,
    end: usize,
    attrs: &'a str,
    inner: &'a str,
}

/// Locate every compartment element in emission order. A compartment ends at the
/// first `</compartment>` before the next compartment's open tag that is
/// followed by output structure, so a `</compartment>` written as text inside a
/// body does not end it early. Mirrors the reference's `find_compartment_elements`.
fn find_compartment_elements(text: &str) -> Vec<CompartmentElement<'_>> {
    const CLOSE_TAG: &str = "</compartment>";
    let find_close_from = |from: usize| text[from..].find(CLOSE_TAG).map(|at| from + at);
    let mut elements = Vec::new();
    let mut cursor = 0;
    while let Some(open) = compartment_open_regex().captures_at(text, cursor) {
        let Some(whole) = open.get(0) else {
            break;
        };
        let body_start = whole.end();
        let window_end = compartment_open_regex()
            .find_at(text, body_start)
            .map_or(text.len(), |next| next.start());
        let Some(first_close) = find_close_from(body_start) else {
            break;
        };
        let mut close_at = first_close;
        let mut candidate = Some(first_close);
        while let Some(at) = candidate.filter(|at| *at < window_end) {
            if compartment_close_follower_regex().is_match(&text[at + CLOSE_TAG.len()..]) {
                close_at = at;
                break;
            }
            candidate = find_close_from(at + 1);
        }
        let end = close_at + CLOSE_TAG.len();
        elements.push(CompartmentElement {
            start: whole.start(),
            end,
            attrs: open.get(1).map_or("", |m| m.as_str()),
            inner: &text[body_start..close_at],
        });
        cursor = end;
    }
    elements
}

/// The text with every compartment element removed; side channels are read from it.
fn text_outside_compartments(text: &str, elements: &[CompartmentElement<'_>]) -> String {
    let mut outside = String::with_capacity(text.len());
    let mut last = 0;
    for element in elements {
        outside.push_str(&text[last..element.start]);
        last = element.end;
    }
    outside.push_str(&text[last..]);
    outside
}

/// Split a leading `[origin_compartment=N]` / `(at_compartment=N)` anchor off a
/// side-channel item, returning the anchor and the remaining text.
fn split_anchor_prefix(text: &str) -> (Option<i64>, String) {
    if let Some(caps) = side_channel_anchor_regex().captures(text) {
        let anchor = caps.get(1).and_then(|m| m.as_str().parse::<i64>().ok());
        let content = caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        return (anchor, content);
    }
    (None, text.trim().to_string())
}

/// Decode the five predefined XML entities in a single left-to-right pass, so
/// every entity is decoded exactly once (chained replacements that decode
/// `&amp;` first would turn `&amp;lt;` into `<`).
fn unescape_xml(s: &str) -> String {
    xml_entity_regex()
        .replace_all(s, |caps: &regex::Captures<'_>| {
            match caps.get(1).map(|m| m.as_str()) {
                Some("amp") => "&",
                Some("apos") => "'",
                Some("quot") => "\"",
                Some("lt") => "<",
                Some("gt") => ">",
                // The regex only captures the five predefined entities.
                _ => "",
            }
        })
        .into_owned()
}

/// Scan `text` for `<name …>body</name>` elements the way the host parser's
/// backreference regexes do, which Rust's regex engine cannot express: from each
/// opening tag the body runs to the first closing tag with the same name. An
/// opening tag with no matching close is skipped.
fn paired_elements<'a>(text: &'a str, open_regex: &Regex) -> Vec<(&'a str, &'a str)> {
    let mut elements = Vec::new();
    let mut pos = 0;
    while let Some(open) = open_regex.captures_at(text, pos) {
        let (Some(whole), Some(name)) = (open.get(0), open.get(1)) else {
            break;
        };
        let close_tag = format!("</{}>", name.as_str());
        match text[whole.end()..].find(&close_tag) {
            Some(body_len) => {
                let body_end = whole.end() + body_len;
                elements.push((name.as_str(), &text[whole.end()..body_end]));
                pos = body_end + close_tag.len();
            }
            None => pos = whole.start() + 1,
        }
    }
    elements
}

const HISTORIAN_CATEGORIES: [&str; 5] = crate::mctx::memory::CATEGORIES;

fn format_dropped_fact_category(category: &str, count: usize) -> String {
    format!("[historian] Dropped <facts> category {category} ({count} facts)")
}

// ---------------------------------------------------------------------------
// Regexes (each mirrors the reference's identically named one)
// ---------------------------------------------------------------------------

fn xml_entity_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"&(amp|apos|quot|lt|gt);").expect("xml entity regex"))
}

fn output_document_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)\A\s*<output(?:\s[^>]*)?>(?P<body>.*)</output\s*>\s*\z")
            .expect("output document regex")
    })
}

fn output_tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)</?output(?:\s[^>]*)?>").expect("output tag regex"))
}

/// Compartment open tag; group 1 is the attribute string. Quoted attribute values
/// are consumed whole, so a raw `>` inside one does not end the open tag, and a
/// value may not span a line so a stray quote cannot swallow the body.
fn compartment_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<compartment\s+((?:[^>"]|"[^"\n]*")*?)\s*>"#).expect("compartment open regex")
    })
}

/// What may follow a compartment's real closing tag: the next compartment, the
/// end of the list or document, or one of the output's own blocks.
fn compartment_close_follower_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\A\s*(?:\z|<(?:compartment\s|/compartments\s*>|/output\s*>|(?:facts|events|meta|user_observations|primer_candidates|unprocessed_from)\s*>))",
        )
        .expect("compartment close follower regex")
    })
}

fn attr_start_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bstart="(\d+)""#).expect("attr start regex"))
}

fn attr_end_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bend="(\d+)""#).expect("attr end regex"))
}

fn attr_title_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\btitle="([^"]*)""#).expect("attr title regex"))
}

fn attr_episode_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bepisode_type="([^"]*)""#).expect("attr episode regex"))
}

fn attr_importance_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bimportance="(\d+)""#).expect("attr importance regex"))
}

/// Tier opener: `<p1>` / `<p1 >` (group 2 empty) or the self-close `<p4/>`
/// (group 2 = "/"). Group 1 is the tier digit, ASCII to match JavaScript's `\d`.
fn tier_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<p([0-9])\s*(/?)>").expect("tier open regex"))
}

fn tier_close_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"</p([0-9])\s*>").expect("tier close regex"))
}

/// What may follow a tier's real closing tag: another tier tag or the end of the
/// compartment.
fn tier_close_follower_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\A\s*(?:\z|</?p[0-9]\s*/?>)").expect("tier close follower regex")
    })
}

/// Lenient fallback bound: the start of any tier's closing tag.
fn tier_close_any_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"</p[0-9]").expect("any tier close regex"))
}

/// The over-capture guard: the start of any tier's opening tag.
fn tier_open_any_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<p[0-9]").expect("any tier open regex"))
}

fn facts_side_channel_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)<meta>.*?</meta>|<user_observations>.*?</user_observations>|<primer_candidates>.*?</primer_candidates>")
            .expect("facts side channel regex")
    })
}

fn facts_envelope_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"</?(?:output|compartments)>").expect("facts envelope regex"))
}

fn facts_block_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<facts>(.*?)</facts>").expect("facts block regex"))
}

fn events_block_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<events>(.*?)</events>").expect("events block regex"))
}

/// Opening tag of a fact category block; group 1 is the category name.
fn category_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<([A-Za-z_][A-Za-z0-9_-]*)>").expect("category open regex"))
}

fn fact_item_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*\*\s*(.+)$").expect("fact item regex"))
}

fn unprocessed_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"<unprocessed_from>(\d+)</unprocessed_from>").expect("unprocessed regex")
    })
}

fn event_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<([a-z_]+)\s+at_compartment="(\d+)"\s*>"#).expect("event open regex")
    })
}

/// Opening tag of an event field; group 1 is the field name.
fn event_field_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<([a-z_]+)\s*>").expect("event field open regex"))
}

fn side_channel_anchor_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^\s*[\[(]\s*(?:at_compartment|origin_compartment)\s*=\s*"?(\d+)"?\s*[\])]\s*(.+)$"#,
        )
        .expect("side channel anchor regex")
    })
}
