use anyhow::Result;
use regex::Regex;

use ox_schema::message::{AssistantContent, Compaction, Message, ToolState};
use ox_schema::support::{Content, Record};

/// Which checkpoint field a borrowed slice belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointField {
    /// Older checkpoint summary.
    Summary,
    /// Verbatim recent context following the summary.
    Recent,
}

/// A contiguous UTF-8 portion of one checkpoint field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointInputSlice<'a> {
    pub field: CheckpointField,
    pub text: &'a str,
    /// Estimated input plus reserved output, excluding the window's 15% reserve.
    pub estimated_tokens: usize,
}

/// Admission inputs for a slice request. `fixed_parts` must include all static
/// system, memory, instruction, field labels, request framing and transport
/// overhead for the actual request, including any per-slice metadata. The caller
/// must use the most restrictive known window across all possible routes, and
/// supply the same calibrated counter used by the request budget walk.
///
/// `ratio` is the finite, positive model calibration used by
/// [`crate::mctx::tokens::counter_fn`]; invalid ratios are rejected rather than
/// silently falling back. The byte-based estimate is monotonic by construction.
pub struct CheckpointSliceBudget<'a> {
    pub window: Option<usize>,
    pub output_tokens: usize,
    pub fixed_parts: &'a [&'a str],
    pub ratio: f64,
}

pub(crate) const MAX_CHECKPOINT_SLICES: usize = 32;

/// Split summary then recent without copying their contents. Each field's slices
/// concatenate to the original bytes. Prefer paragraph and line boundaries; fall
/// back to a UTF-8 character boundary. The result is an estimate, not proof of
/// provider token usage. Each actual request must match the budgeted framing;
/// retries, repair prompts and different routes require fresh admission.
pub fn slice_checkpoint<'a>(
    summary: &'a str,
    recent: &'a str,
    budget: &CheckpointSliceBudget<'_>,
) -> Result<Vec<CheckpointInputSlice<'a>>> {
    let window = budget
        .window
        .filter(|window| *window > 0)
        .ok_or_else(|| anyhow::anyhow!("checkpoint model window is unknown or zero"))?;
    anyhow::ensure!(
        budget.output_tokens > 0,
        "checkpoint output reserve is zero"
    );
    anyhow::ensure!(
        !budget.fixed_parts.is_empty(),
        "checkpoint request framing is missing"
    );
    anyhow::ensure!(
        budget.ratio.is_finite() && budget.ratio > 0.0,
        "invalid checkpoint calibration ratio"
    );
    let count_tokens = crate::mctx::tokens::counter_fn(budget.ratio);
    let limit = (window / 100)
        .checked_mul(85)
        .and_then(|whole| whole.checked_add((window % 100) * 85 / 100))
        .ok_or_else(|| anyhow::anyhow!("checkpoint budget overflow"))?;
    let fixed = budget
        .fixed_parts
        .iter()
        .try_fold(budget.output_tokens, |total, part| {
            total
                .checked_add(count_tokens(part))
                .ok_or_else(|| anyhow::anyhow!("checkpoint budget overflow"))
        })?;
    anyhow::ensure!(
        fixed < limit,
        "checkpoint framing and output exceed window budget"
    );
    let mut slices = Vec::new();
    for (field, text) in [
        (CheckpointField::Summary, summary),
        (CheckpointField::Recent, recent),
    ] {
        let mut remaining = text;
        while !remaining.is_empty() {
            anyhow::ensure!(
                slices.len() < MAX_CHECKPOINT_SLICES,
                "checkpoint requires too many slices for the historian lease"
            );
            let end = checkpoint_slice_end(remaining, limit - fixed, budget.ratio)?;
            let piece = &remaining[..end];
            let cost = checkpoint_text_cost(piece, budget.ratio)?;
            let estimated_tokens = fixed
                .checked_add(cost)
                .ok_or_else(|| anyhow::anyhow!("checkpoint budget overflow"))?;
            slices.push(CheckpointInputSlice {
                field,
                text: piece,
                estimated_tokens,
            });
            remaining = &remaining[end..];
        }
    }
    anyhow::ensure!(!slices.is_empty(), "checkpoint has no content to slice");
    Ok(slices)
}

// Account for JSON escaping in addition to the calibrated text estimate. The
// fixed framing must separately include message envelope and field wrappers.
fn checkpoint_text_cost(text: &str, ratio: f64) -> Result<usize> {
    let escaped_bytes = text.chars().try_fold(0usize, |total, ch| {
        total
            .checked_add(match ch {
                '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 1,
                '\u{0}'..='\u{1f}' => 5,
                _ => 0,
            })
            .ok_or_else(|| anyhow::anyhow!("checkpoint text estimate overflow"))
    })?;
    let bytes = text
        .len()
        .checked_add(escaped_bytes)
        .ok_or_else(|| anyhow::anyhow!("checkpoint text estimate overflow"))?;
    let base = crate::mctx::tokens::heuristic_bytes(bytes) as f64 * ratio;
    if !base.is_finite() || base > usize::MAX as f64 {
        return Err(anyhow::anyhow!("checkpoint text estimate overflow"));
    }
    Ok(base.ceil() as usize)
}

fn checkpoint_slice_end(text: &str, available: usize, ratio: f64) -> Result<usize> {
    // Exponential probing bounds each search to roughly twice the slice size;
    // binary search is valid because this estimate depends only on byte length.
    let mut high = 1usize;
    loop {
        let end = floor_char_boundary(text, high.min(text.len()));
        if high >= text.len() || checkpoint_text_cost(&text[..end], ratio)? > available {
            break;
        }
        high = high.saturating_mul(2).min(text.len());
    }
    let mut low = 0;
    let mut best = 0;
    while low <= high {
        let midpoint = low + (high - low) / 2;
        let end = floor_char_boundary(text, midpoint);
        if checkpoint_text_cost(&text[..end], ratio)? <= available {
            best = end;
            if midpoint == text.len() {
                break;
            }
            low = midpoint + 1;
        } else if midpoint == 0 {
            break;
        } else {
            high = midpoint - 1;
        }
    }
    anyhow::ensure!(
        best > 0,
        "checkpoint single character exceeds request budget"
    );
    if best == text.len() {
        return Ok(best);
    }
    let prefix = &text[..best];
    if let Some(pos) = prefix.rfind("\n\n").filter(|pos| *pos >= best / 2) {
        return Ok(pos + 2);
    }
    if let Some(pos) = prefix.rfind('\n').filter(|pos| *pos >= best / 2) {
        return Ok(pos + 1);
    }
    Ok(best)
}

fn floor_char_boundary(text: &str, mut end: usize) -> usize {
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// The chunk one historian run covers: a contiguous ordinal range of the tail,
/// formatted into the reference's `[range] R: text` transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// First covered ordinal (inclusive).
    pub start_ordinal: i64,
    /// Last covered ordinal (inclusive).
    pub end_ordinal: i64,
    /// Message id of the last covered message — the run's anchor. ox's
    /// publication writes the end *ordinal* (see the module docs), so this is
    /// carried for logging and for the reference's anchor shape, not for
    /// validation.
    pub end_message_id: String,
    /// The ordinals the fold actually covers, ascending — `start_ordinal`
    /// joined to `end_ordinal`.
    ///
    /// This is the range a caller that rebuilds real wire messages must
    /// re-render to reproduce exactly the conversation span that was folded: it
    /// ends where publication writes the compartment's `coverage_end`, so a
    /// re-rendered prefix and the next pass's floor cannot disagree. The
    /// requested bounds are not it: a row the walk skips (synthetic, or one that
    /// does not decode) and a block that does not fit both move an endpoint
    /// inward, and a trailing row that renders nothing is left for the next
    /// pass rather than claimed here.
    ///
    /// Never empty — [`chunk`] answers `None` when nothing was admitted, so
    /// "covered nothing" is the absence of a `Chunk`, not an empty range.
    pub covered: std::ops::RangeInclusive<i64>,
    /// The formatted transcript, used verbatim as the `<new_messages>` body.
    pub text: String,
    /// Token estimate of [`Chunk::text`], measured the same way as `budget`.
    pub token_estimate: usize,
    /// What this region cost the conversation, in the provider's units: the
    /// rendered text plus the tool-result bodies the transcript collapses into
    /// `TC:` lines. Pressure and the protected tail are measured in these
    /// tokens; the chunker's `budget` is measured in [`Chunk::token_estimate`].
    pub context_tokens: usize,
    /// Every ordinal the transcript shows, ascending — the ordinals validation
    /// chunks against. It is the union of the emitted blocks' `[start-end]`
    /// ranges, because a block's range is what the model reads; an ordinal that
    /// rendered no line (a system note, a skipped row) is part of a block's
    /// range and may therefore be a compartment boundary.
    pub present_ordinals: Vec<i64>,
    /// One entry per emitted block, in emission order — what the prefix-shaped
    /// instruction states as its ordinal index. The flat transcript carries the
    /// same information in its `[N-M] R:` headers; the prefix shape has no
    /// headers, so the tail has to say it
    /// ([`super::prompt::build_prefixed_instruction`]).
    pub blocks: Vec<ChunkBlock>,
}

/// One emitted block of a [`Chunk`], as the prefix shape's ordinal index states
/// it: the range the model may cite and the role the block renders under.
///
/// Ranges and roles only, never transcript text: the tail is the cache-miss
/// portion of the request and the transcript is already in the request's
/// prefix, so a copy here would be paid for twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkBlock {
    /// First ordinal of the block (inclusive).
    pub start_ordinal: i64,
    /// Last ordinal of the block (inclusive).
    pub end_ordinal: i64,
    /// The block's role as [`role_of`] names it (`user`, `assistant`, or the
    /// message kind). Spelled out rather than the flat transcript's one letter
    /// ([`compact_role`]) because the prefix shape never shows the letters.
    pub role: &'static str,
}

/// The two ceilings a chunk is cut against.
///
/// They are deliberately different units — [`Self::request`] counts the rendered
/// transcript the historian is sent, [`Self::context`] counts what the region
/// cost the conversation (a tool result is one `TC:` line but was read in full) —
/// so they are named at the boundary instead of passed as two bare `usize`s a
/// caller could swap without the compiler noticing.
#[derive(Debug, Clone, Copy)]
pub struct ChunkLimits {
    /// Rendered tokens the request may hold: [`Chunk::token_estimate`].
    pub request: usize,
    /// Tokens the region may cost the conversation: [`Chunk::context_tokens`].
    /// Absolute — a block that would cross it is never admitted.
    pub context: usize,
}

impl ChunkLimits {
    /// No ceiling at all: for measuring a whole tail, and for tests.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            request: usize::MAX,
            context: usize::MAX,
        }
    }
}

/// Pick the chunk a run would cover.
///
/// `rows` are the tail rows in ordinal order (as
/// [`rows_after_with_seq`](crate::runner::step::rows_after_with_seq) returns
/// them); `eligible_end_ordinal` is the exclusive end of the eligible head, so
/// the protected tail at and beyond it is never covered.
///
/// Ported from `historian_chunk.rs::Builder`: messages are grouped into blocks
/// of consecutive same-role messages, a block is emitted whole or not at all,
/// and the run stops at the first block that would cross the request budget
/// (unless nothing has been emitted yet, in which case a single oversize block
/// is admitted, except checkpoints, which must fit the request budget). Returning
/// `None` means the eligible range held nothing worth a run, its first message
/// crosses the context ceiling, or a checkpoint crosses the request ceiling.
///
/// See [`ChunkLimits`] for the two ceilings. The context one is absolute: a
/// block that would cross it is never admitted, not even as the first one.
///
/// A same-role run is cut at a message boundary before it would cross either
/// ceiling (the reference keeps the run whole; ox cuts it so an agent's long
/// autonomous stretch cannot stall the fold behind one unadmittable block).
/// Under [`ChunkLimits::unbounded`] nothing is ever cut.
///
/// `count_tokens` is the session's calibrated counter
/// ([`crate::mctx::tokens::counter_fn`]). Both ceilings come from the model
/// window — provider tokens — so the rendered text and the collapsed tool
/// bodies must be measured in the same unit; counting them with the raw
/// `bytes / 4` estimate would compare two different scales.
pub fn chunk(
    rows: &[(i64, serde_json::Value)],
    start_ordinal: i64,
    eligible_end_ordinal: i64,
    limits: ChunkLimits,
    count_tokens: &dyn Fn(&str) -> usize,
) -> Option<Chunk> {
    // The reference sorts before building (`ordered.sort_by_key`); the query
    // already orders by `seq`, so this only guards against a caller that did
    // not.
    let mut ordered: Vec<&(i64, serde_json::Value)> = rows.iter().collect();
    ordered.sort_by_key(|(ordinal, _)| *ordinal);

    let mut builder = Builder::new(limits, count_tokens);
    for (ordinal, row) in ordered {
        if *ordinal < start_ordinal {
            continue;
        }
        if *ordinal >= eligible_end_ordinal {
            break;
        }
        let message = match crate::runner::step::decode_row(row) {
            Ok(message) => message,
            // An undecodable row is skipped rather than failing the whole
            // chunk: it contributes no renderable content, and the next
            // decodable row still advances the covered range.
            Err(error) => {
                tracing::warn!(ordinal, error = %error, "historian chunk skipped an undecodable tail row");
                continue;
            }
        };
        // The reference drops every synthetic message before chunking
        // (`historian_chunk.rs` filters on `meta.synthetic`): those rows are
        // host bookkeeping, not conversation, and summarizing them as user
        // history would put words in the model's mouth.
        if matches!(message, Message::Synthetic { .. }) {
            continue;
        }
        if !builder.push(*ordinal, &message) {
            break;
        }
    }
    builder.finish()
}

/// Select only an oversized completed checkpoint that fits the context fence.
/// Earlier rows are not skipped: continuity must never advance over them.
pub(crate) fn oversized_checkpoint<'a>(
    rows: &'a [(i64, serde_json::Value)],
    start: i64,
    end: i64,
    limits: ChunkLimits,
    count: &dyn Fn(&str) -> usize,
) -> Option<(Chunk, &'a serde_json::Value)> {
    let mut row = None;
    for candidate in rows
        .iter()
        .filter(|(ordinal, _)| *ordinal >= start && *ordinal < end)
    {
        let Ok(message) = crate::runner::step::decode_row(&candidate.1) else {
            continue;
        };
        if matches!(message, Message::Synthetic { .. }) {
            continue;
        }
        if matches!(message, Message::Compaction(item) if matches!(item.compaction, Compaction::Completed(_)))
        {
            row = Some(candidate);
        }
        break;
    }
    let row = row?;
    let chunk = chunk(
        std::slice::from_ref(row),
        row.0,
        end,
        ChunkLimits::unbounded(),
        count,
    )?;
    if chunk.context_tokens > limits.context || chunk.token_estimate <= limits.request {
        return None;
    }
    Some((chunk, &row.1))
}

/// One block of the chunk: consecutive same-role messages collapsed into one
/// `[range] R: text` line. Ported from `historian_chunk.rs::ChunkBlock`.
#[derive(Debug)]
struct Block {
    role: String,
    /// The role spelled out (see [`ChunkBlock::role`]), carried so the ordinal
    /// index can name it without re-deriving the message it came from.
    label: &'static str,
    start_ordinal: i64,
    end_ordinal: i64,
    /// One rendered string per message, joined with ` / ` at format time.
    parts: Vec<String>,
    /// Commit hashes harvested from this block's assistant text.
    commit_hashes: Vec<String>,
    /// Tool-result body tokens the block's messages carry (see
    /// [`tool_result_tokens`]): not rendered, but spent.
    result_tokens: usize,
    /// Running upper bound on what [`format_block`] renders (without the
    /// commit suffix, which [`Builder::absorbs`] bounds separately), kept so
    /// deciding whether a message still fits does not re-render the block
    /// (which would make a long same-role run quadratic). An upper bound,
    /// not an estimate: an under-count would let a first block grow past a
    /// ceiling that [`Builder::flush`] then refuses, stalling the chunk.
    rendered_tokens: usize,
    /// Message id of the last message absorbed into the block.
    last_message_id: String,
}

/// Failure-tolerant chunk builder. Ported from `historian_chunk.rs::Builder`,
/// without the reference-only bookkeeping (tool-result boundaries, snapshot
/// identity, and the `tool_expansions` map — which has no producer anywhere in
/// the tree this was ported from, so ox always renders the call summary).
struct Builder<'a> {
    limits: ChunkLimits,
    /// The session's calibrated counter, so the rendered text and the collapsed
    /// tool bodies are measured in the same unit as [`ChunkLimits`].
    count_tokens: &'a dyn Fn(&str) -> usize,
    /// [`commit_suffix_cap_tokens`] under `count_tokens`, measured once per
    /// chunk rather than per message.
    commit_suffix_tokens: usize,
    total_tokens: usize,
    /// Tool-result body tokens over the emitted blocks, kept apart from
    /// `total_tokens` because only that one is what the request costs.
    total_result_tokens: usize,
    lines: Vec<String>,
    current: Option<Block>,
    /// Ordinals of messages that render nothing (system/noise) or have not been
    /// absorbed into a block yet, held so the next block's range starts where
    /// the noise did.
    pending_noise: Vec<(i64, String)>,
    start_ordinal: Option<i64>,
    end_ordinal: i64,
    end_message_id: String,
    /// Ordinals of the blocks actually emitted, ascending. A block that does not
    /// fit the budget is dropped whole, so only emitted ranges may contribute.
    present_ordinals: Vec<i64>,
    /// The emitted blocks' ranges and roles, for the prefix shape's ordinal
    /// index — the same emission set `present_ordinals` is derived from.
    blocks: Vec<ChunkBlock>,
}

impl<'a> Builder<'a> {
    fn new(limits: ChunkLimits, count_tokens: &'a dyn Fn(&str) -> usize) -> Self {
        Self {
            limits,
            count_tokens,
            commit_suffix_tokens: commit_suffix_cap_tokens(count_tokens),
            total_tokens: 0,
            total_result_tokens: 0,
            lines: Vec::new(),
            current: None,
            pending_noise: Vec::new(),
            start_ordinal: None,
            end_ordinal: 0,
            end_message_id: String::new(),
            present_ordinals: Vec::new(),
            blocks: Vec::new(),
        }
    }

    /// Offer one message. `false` means the chunk is full: stop, because the
    /// pending block did not fit.
    fn push(&mut self, ordinal: i64, message: &Message) -> bool {
        let role = role_of(message);
        let (parts, commit_hashes) = render_parts(role, message);
        let result_tokens = tool_result_tokens(message, self.count_tokens);
        let compact_role = compact_role(role);
        // A message with nothing to render (a system note, a switch marker)
        // carries no content for the model but still occupies an ordinal, so it
        // joins the soonest block instead of ending the chunk.
        if compact_role == "S" || parts.is_empty() {
            self.pending_noise.push((ordinal, message.id().to_owned()));
            return true;
        }
        let count = self.count_tokens;
        let parts_tokens = rendered_parts_tokens(&parts, count);
        // Joining an open block adds this message's parts after a ` / `.
        let joined_tokens = parts_tokens + piece_tokens(" / ", count);
        let brings_hashes = !commit_hashes.is_empty();
        if self.current.as_ref().is_some_and(|block| {
            block.role == compact_role
                && block.label != "compaction"
                && role != "compaction"
                && self.absorbs(block, joined_tokens, result_tokens, brings_hashes)
        }) {
            let block = self.current.as_mut().expect("block checked above");
            block.end_ordinal = ordinal;
            block.parts.extend(parts);
            block.commit_hashes = merge_commit_hashes(&block.commit_hashes, commit_hashes);
            block.result_tokens += result_tokens;
            block.rendered_tokens += joined_tokens;
            block.last_message_id = message.id().to_owned();
            // Any noise between the two messages falls inside the block's
            // range, so it is absorbed rather than held for the next block.
            self.pending_noise.clear();
            return true;
        }
        // A role switch, or a same-role message that would carry the open
        // block across a ceiling: the open block ends here, at a message
        // boundary, and this message opens the next one.
        if !self.flush() {
            return false;
        }
        let start = self
            .pending_noise
            .first()
            .map(|(ordinal, _)| *ordinal)
            .unwrap_or(ordinal);
        self.pending_noise.clear();
        // Measured once, at the widest range the block could grow to, so the
        // end ordinal gaining digits never outgrows the bound.
        let header_tokens =
            piece_tokens(&format!("[{start}-{}] {compact_role}: ", i64::MAX), count);
        self.current = Some(Block {
            role: compact_role,
            label: role,
            start_ordinal: start,
            end_ordinal: ordinal,
            parts,
            commit_hashes,
            result_tokens,
            rendered_tokens: header_tokens + parts_tokens,
            last_message_id: message.id().to_owned(),
        });
        true
    }

    /// Whether `block` can absorb one more message of `joined_tokens` rendered
    /// and `result_tokens` collapsed tokens without the chunk crossing either
    /// ceiling. Decided on [`Block::rendered_tokens`] plus, when the block
    /// carries commit hashes, the widest suffix they can render as;
    /// [`Self::flush`] re-measures the real text and has the final word.
    fn absorbs(
        &self,
        block: &Block,
        joined_tokens: usize,
        result_tokens: usize,
        brings_hashes: bool,
    ) -> bool {
        let count = self.count_tokens;
        let separator_tokens = if self.lines.is_empty() {
            0
        } else {
            piece_tokens("\n", count)
        };
        let suffix_tokens = if brings_hashes || !block.commit_hashes.is_empty() {
            self.commit_suffix_tokens
        } else {
            0
        };
        let rendered = separator_tokens + block.rendered_tokens + suffix_tokens + joined_tokens;
        let context = self.context_tokens() + rendered + block.result_tokens + result_tokens;
        context <= self.limits.context && self.total_tokens + rendered <= self.limits.request
    }

    /// Emit the open block if it fits. Ported from
    /// `historian_chunk.rs::Builder::flush_current_block`.
    fn flush(&mut self) -> bool {
        let Some(block) = self.current.take() else {
            return true;
        };
        let text = format_block(&block);
        let count = self.count_tokens;
        let separator_tokens = if self.lines.is_empty() {
            0
        } else {
            count("\n")
        };
        let block_tokens = count(&text) + separator_tokens;
        let block_context = block_tokens + block.result_tokens;
        // The fence is absolute — it is what the model working on the tail keeps
        // verbatim — so it is tested before the oversize-first-block allowance
        // below, which exists only so one huge message cannot stall the fold.
        if self.context_tokens() + block_context > self.limits.context {
            if block.label == "compaction" {
                tracing::warn!(
                    ordinal = block.end_ordinal,
                    block_context,
                    context_limit = self.limits.context,
                    "historian checkpoint exceeds remaining context budget; coverage not advanced"
                );
            }
            self.current = Some(block);
            return false;
        }
        if self.total_tokens + block_tokens > self.limits.request
            && (self.total_tokens > 0 || block.label == "compaction")
        {
            if block.label == "compaction" {
                tracing::warn!(
                    ordinal = block.end_ordinal,
                    block_tokens,
                    request_limit = self.limits.request,
                    "historian checkpoint exceeds remaining request budget; coverage not advanced"
                );
            }
            self.current = Some(block);
            return false;
        }
        self.start_ordinal.get_or_insert(block.start_ordinal);
        self.end_ordinal = block.end_ordinal;
        self.end_message_id.clone_from(&block.last_message_id);
        self.total_tokens += block_tokens;
        self.total_result_tokens += block.result_tokens;
        self.present_ordinals
            .extend(block.start_ordinal..=block.end_ordinal);
        self.blocks.push(ChunkBlock {
            start_ordinal: block.start_ordinal,
            end_ordinal: block.end_ordinal,
            role: block.label,
        });
        self.lines.push(text);
        true
    }

    /// What the emitted blocks cost the conversation: the rendered text plus the
    /// tool bodies it collapsed.
    fn context_tokens(&self) -> usize {
        self.total_tokens + self.total_result_tokens
    }

    fn finish(mut self) -> Option<Chunk> {
        self.flush();
        if self.lines.is_empty() {
            return None;
        }
        let context_tokens = self.context_tokens();
        let start_ordinal = self.start_ordinal.unwrap_or(self.end_ordinal);
        let end_ordinal = self.end_ordinal;
        Some(Chunk {
            start_ordinal,
            end_ordinal,
            end_message_id: self.end_message_id,
            covered: start_ordinal..=end_ordinal,
            text: self.lines.join("\n"),
            token_estimate: self.total_tokens,
            context_tokens,
            present_ordinals: self.present_ordinals,
            blocks: self.blocks,
        })
    }
}

/// The reference's role vocabulary, mapped onto ox's message variants. ox's
/// `user`-family covers what the reference splits into `user`, `synthetic` and
/// `skill`; everything else keeps its own kind so `compact_role` can reduce it
/// to a single letter.
fn role_of(message: &Message) -> &'static str {
    match message {
        Message::User { .. } | Message::Synthetic { .. } | Message::Skill { .. } => "user",
        Message::System { .. } => "system",
        Message::Assistant(_) => "assistant",
        other => other.kind(),
    }
}

/// The reference's `text_parts`: user text is cleaned (reminders/markers
/// stripped), every part is whitespace-normalized, and empty parts drop out.
/// Assistant prose additionally goes through [`compact_text_for_summary`],
/// which returns the block's commit hashes alongside the compacted text.
///
/// An assistant message with no text renders its tool calls instead
/// (`format_tool_summary`), which is where ox's in-message tool results land.
fn render_parts(role: &str, message: &Message) -> (Vec<String>, Vec<String>) {
    match message {
        Message::User { prompt, .. } => {
            let text = normalize_text(&clean_user_text(&crate::runner::step::user_prompt_text(
                prompt,
            )));
            if text.is_empty() {
                (Vec::new(), Vec::new())
            } else {
                (vec![text], Vec::new())
            }
        }
        Message::Synthetic { text, .. } | Message::Skill { text, .. } => {
            let text = normalize_text(text);
            if text.is_empty() {
                (Vec::new(), Vec::new())
            } else {
                (vec![text], Vec::new())
            }
        }
        Message::Compaction(message) => {
            let Compaction::Completed(completed) = &message.compaction else {
                return (Vec::new(), Vec::new());
            };
            let mut parts = Vec::new();
            if !completed.summary.is_empty() {
                parts.push(format!("Checkpoint summary: {}", completed.summary));
            }
            if !completed.recent.is_empty() {
                parts.push(format!("Checkpoint recent context: {}", completed.recent));
            }
            (parts, Vec::new())
        }
        Message::Assistant(message) => {
            let mut parts = Vec::new();
            let mut commit_hashes = Vec::new();
            for part in &message.content {
                let AssistantContent::Text { text, .. } = part else {
                    continue;
                };
                let text = normalize_text(text.trim());
                if text.is_empty() {
                    continue;
                }
                let (compacted, hashes) = compact_text_for_summary(&text, role);
                parts.push(compacted);
                commit_hashes = merge_commit_hashes(&commit_hashes, hashes);
            }
            if !parts.is_empty() {
                return (parts, commit_hashes);
            }
            let tools = message
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantContent::Tool { name, state, .. } => Some(format_tool_summary(
                        name,
                        &crate::runner::step::tool_input(state),
                    )),
                    _ => None,
                })
                .collect();
            (tools, Vec::new())
        }
        _ => (Vec::new(), Vec::new()),
    }
}

/// The tool-result body tokens one message carries.
///
/// The transcript renders a settled call as a single `TC:` line, but the
/// conversation that produced the tail read the body in full — so a tail of
/// tool-heavy turns is far larger than its rendering claims. This is the
/// missing half of [`Chunk::context_tokens`]: the historian request pays only
/// [`Chunk::token_estimate`], the conversation paid this too. Ported from the
/// reference's `tool_result_body_tokens` (its consumer there is the result
/// boundary; ox's is the pressure reading).
fn tool_result_tokens(message: &Message, count_tokens: &dyn Fn(&str) -> usize) -> usize {
    let Message::Assistant(message) = message else {
        return 0;
    };
    message
        .content
        .iter()
        .map(|part| match part {
            AssistantContent::Tool { state, .. } => tool_state_tokens(state, count_tokens),
            _ => 0,
        })
        .sum()
}

/// [`ToolState`]'s settled bodies, in tokens. A call that is still streaming or
/// running has no body yet (its result lands in a later row).
fn tool_state_tokens(state: &ToolState, count_tokens: &dyn Fn(&str) -> usize) -> usize {
    let content = match state {
        ToolState::Completed { content, .. } => Some(content.as_slice()),
        ToolState::Error { content, .. } => content.as_deref(),
        ToolState::Streaming { .. } | ToolState::Running { .. } => None,
    };
    content.map_or(0, |content| {
        content
            .iter()
            .map(|part| match part {
                Content::Text { text } => count_tokens(text),
                // A file part rides the next request as a user message, not as
                // this message's text: counting its uri would be noise.
                Content::File { .. } => 0,
            })
            .sum()
    })
}

/// `historian_chunk.rs::compact_text_for_summary`: assistant prose that mentions
/// a commit collapses its verbatim hashes into the block's `commits:` list, so
/// the model never re-reads a hash as prose. Any other role keeps its text and
/// reports no hashes.
fn compact_text_for_summary(text: &str, role: &str) -> (String, Vec<String>) {
    let commit_hashes = if role == "assistant" {
        extract_commit_hashes(text)
    } else {
        Vec::new()
    };
    if commit_hashes.is_empty() || !commit_verb_regex().is_match(text) {
        return (text.to_string(), commit_hashes);
    }
    let stripped = commit_hash_extract_regex().replace_all(text, "");
    let stripped = empty_parens_regex().replace_all(&stripped, "");
    let stripped = space_before_comma_regex().replace_all(&stripped, ",");
    let stripped = repeated_comma_regex().replace_all(&stripped, ", ");
    let stripped = repeated_space_regex().replace_all(&stripped, " ");
    let stripped = space_before_punct_regex().replace_all(&stripped, "$1");
    let trimmed = stripped.trim();
    (
        if trimmed.is_empty() {
            text.to_string()
        } else {
            trimmed.to_string()
        },
        commit_hashes,
    )
}

/// `historian_chunk.rs::extract_commit_hashes`.
fn extract_commit_hashes(text: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    for capture in commit_hash_extract_regex().captures_iter(text) {
        let Some(hash) = capture.get(1).map(|value| value.as_str().to_lowercase()) else {
            continue;
        };
        if hashes.contains(&hash) {
            continue;
        }
        hashes.push(hash);
        if hashes.len() >= MAX_COMMITS_PER_BLOCK {
            break;
        }
    }
    hashes
}

/// `historian_chunk.rs::compact_role`.
fn compact_role(role: &str) -> String {
    match role {
        "assistant" => "A".to_string(),
        "user" => "U".to_string(),
        _ => role
            .chars()
            .next()
            .map(|ch| ch.to_uppercase().collect::<String>())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "M".to_string()),
    }
}

/// `historian_chunk.rs::format_block` — `[N] U: text` for a one-message block,
/// `[N-M] U: text / text` for a merged one.
fn format_block(block: &Block) -> String {
    let range = if block.start_ordinal == block.end_ordinal {
        format!("[{}]", block.start_ordinal)
    } else {
        format!("[{}-{}]", block.start_ordinal, block.end_ordinal)
    };
    let commit_suffix = if block.commit_hashes.is_empty() {
        String::new()
    } else {
        format!(" commits: {}", block.commit_hashes.join(", "))
    };
    format!(
        "{} {}:{} {}",
        range,
        block.role,
        commit_suffix,
        block.parts.join(" / ")
    )
}

/// Upper bound on what one message's `parts` add to a [`format_block`] line:
/// each part plus the ` / ` joining it to the next.
fn rendered_parts_tokens(parts: &[String], count_tokens: &dyn Fn(&str) -> usize) -> usize {
    let separators = parts.len().saturating_sub(1) * piece_tokens(" / ", count_tokens);
    let texts: usize = parts
        .iter()
        .map(|part| piece_tokens(part, count_tokens))
        .sum();
    texts + separators
}

/// One piece of a line, counted alone, plus one token. The production counter
/// rounds up, so pieces already sum to an upper bound; the slack covers any
/// counter that rounds down (`len / 4`), which undercounts a sum of pieces by
/// under one token per piece against the joined text.
fn piece_tokens(piece: &str, count_tokens: &dyn Fn(&str) -> usize) -> usize {
    count_tokens(piece) + 1
}

/// Upper bound on the ` commits: …` suffix [`format_block`] appends: the most
/// hashes a block keeps, each at the longest length the extractor accepts.
fn commit_suffix_cap_tokens(count_tokens: &dyn Fn(&str) -> usize) -> usize {
    let widest = vec!["f".repeat(12); MAX_COMMITS_PER_BLOCK].join(", ");
    piece_tokens(&format!(" commits: {widest}"), count_tokens)
}

/// `historian_chunk.rs::format_tool_summary`.
fn format_tool_summary(name: &str, input: &Record) -> String {
    if let Some(description) = input
        .get("description")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
    {
        format!("TC: {description}")
    } else if let Some(key_arg) = extract_key_arg(input) {
        format!("TC: {name}({key_arg})")
    } else {
        format!("TC: {name}")
    }
}

/// `historian_chunk.rs::extract_key_arg`.
fn extract_key_arg(input: &Record) -> Option<String> {
    for key in ["filePath", "path", "pattern", "query"] {
        if let Some(value) = input.get(key).and_then(serde_json::Value::as_str) {
            return Some(truncate_utf16(value, 60));
        }
    }
    for key in ["symbol", "module", "action"] {
        if let Some(value) = input.get(key).and_then(serde_json::Value::as_str) {
            return Some(value.to_string());
        }
    }
    None
}

/// `boundary.rs::truncate_utf16` (60 UTF-16 units is `extract_key_arg`'s cap).
fn truncate_utf16(value: &str, max_units: usize) -> String {
    if value.encode_utf16().count() <= max_units {
        return value.to_string();
    }
    let mut units = 0;
    let mut out = String::new();
    for ch in value.chars() {
        units += ch.len_utf16();
        if units > max_units {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

/// `historian_chunk.rs::clean_user_text`, minus the steering-wrapper
/// preservation (see the module docs).
fn clean_user_text(text: &str) -> String {
    let strip = |segment: &str| {
        system_reminder_regex()
            .replace_all(segment, "")
            .replace(OMO_INTERNAL_INITIATOR_MARKER, "")
    };
    strip(text).trim().to_string()
}

/// `historian_chunk.rs::normalize_text`.
fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Most commit hashes one block carries (`historian_chunk.rs::MAX_COMMITS_PER_BLOCK`).
const MAX_COMMITS_PER_BLOCK: usize = 5;
/// `historian_chunk.rs::SYSTEM_DIRECTIVE_PREFIX` consumers only need the text.
const OMO_INTERNAL_INITIATOR_MARKER: &str = "<!-- OMO_INTERNAL_INITIATOR -->";

/// `historian_chunk.rs::merge_commit_hashes`.
fn merge_commit_hashes(existing: &[String], next: Vec<String>) -> Vec<String> {
    let mut merged = existing.to_vec();
    for hash in next {
        if merged.contains(&hash) {
            continue;
        }
        merged.push(hash);
        if merged.len() >= MAX_COMMITS_PER_BLOCK {
            break;
        }
    }
    merged
}

// Regex accessors return a clone so the lazy-DFA cache is dropped after the
// call instead of living on per worker thread (see `historian_validate.rs`).
/// `historian_chunk.rs::system_reminder_regex`.
fn system_reminder_regex() -> Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<system-reminder>[\s\S]*?</system-reminder>").expect("reminder regex")
    })
    .clone()
}

/// `historian_chunk.rs::commit_hash_extract_regex`. ASCII boundaries and ASCII
/// case folding, as its TypeScript twin (a hash right after a letter such as
/// `é` is not a commit reference).
fn commit_hash_extract_regex() -> Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i-u)`?\b([0-9a-f]{7,12})\b`?").expect("commit hash regex"))
        .clone()
}

/// `historian_chunk.rs::commit_verb_regex` — the gate for hash stripping: only
/// prose that talks about committing collapses its hashes.
fn commit_verb_regex() -> Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i-u)\b(?:commit(?:ted|ting|s)?|cherry-?pick(?:ed|ing|s)?|merge[ds]?|merging|rebas(?:e|ed|es|ing))\b")
            .expect("commit verb regex")
    }).clone()
}

/// `historian_chunk.rs::empty_parens_regex`.
fn empty_parens_regex() -> Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\(\s*\)").expect("empty parens regex"))
        .clone()
}

/// `historian_chunk.rs::space_before_comma_regex`.
fn space_before_comma_regex() -> Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+,").expect("space before comma regex"))
        .clone()
}

/// `historian_chunk.rs::repeated_comma_regex`.
fn repeated_comma_regex() -> Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r",\s*,+").expect("repeated comma regex"))
        .clone()
}

/// `historian_chunk.rs::repeated_space_regex`.
fn repeated_space_regex() -> Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s{2,}").expect("repeated space regex"))
        .clone()
}

/// `historian_chunk.rs::space_before_punct_regex`.
fn space_before_punct_regex() -> Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+([,.;:])").expect("space before punct regex"))
        .clone()
}
