use std::sync::Arc;

use anyhow::Result;

use crate::mctx::search;
use crate::prompt;
use ox_core::copied_tags::strip_copied_tag_notation;
use ox_llm::cardinal::wire::{ChatMessage, ChatRequest, ToolChoice, UserContent};
use ox_schema::event::SessionEventData;
use ox_schema::message::Message;

use super::super::drain::DrainCtx;
use super::super::instructions;
use super::attempt::StepParams;
use super::context::{assistant_tokens, build_chat_messages, mctx_head};
use super::history::load_history_after;
use super::load_overlaid_tail;
/// Hits automatic recall may attach to one turn (the hint's line budget).
const RECALL_LIMIT: usize = 3;
/// Shortest typed prompt worth searching. A greeting or a `/clear` retrieves
/// nothing, and every turn that clears this bar pays one indexed query.
const RECALL_MIN_QUERY_CHARS: usize = 8;
/// Upstream `MAX_STEPS_PROMPT` (runner/max-steps.ts): the trailing assistant
/// message of a final step. Never persisted.
const MAX_STEPS_PROMPT: &str = "The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.";

/// Assemble the chat request for one attempt: the transcript is rebuilt from
/// the durable rows (upstream refreshes context on retry and continuation),
/// so a continuation attempt observes the settled partial row plus the
/// continuation prompt.
pub(super) async fn build_request(ctx: &DrainCtx, params: &StepParams<'_>) -> Result<ChatRequest> {
    // The transcript is rebuilt per attempt (upstream refreshes context
    // on retry and continuation): a continuation attempt must observe
    // the settled partial row plus the continuation prompt.
    // The magic-context head replaces everything it covers: the pass mints
    // the frozen bytes and reports the ordinal floor, so the transcript
    // starts after the covered range.
    let system_static = ctx.system_static();
    let observed = ctx.observe_sources().await?;
    {
        let mut baselines = super::super::lock(&ctx.instruction_baselines);
        baselines
            .entry(ctx.session_id.clone())
            .or_insert_with(|| instructions::baseline(&observed));
    }
    let budget = ctx.budget_for(&params.model.id, system_static.len());
    let (head_m0, head_m1, floor) = mctx_head(ctx, budget, &params.model.id).await;
    let rows = if ctx.config.magic_context.enabled {
        // The §N§ overlay (tag prefixes + drop placeholders) is part of the
        // integration: with it off the request must stay byte-identical to
        // the integration-free one.
        load_overlaid_tail(&ctx.db, &ctx.session_id, floor).await?
    } else {
        load_history_after(&ctx.db, &ctx.session_id, floor).await?
    };
    // Step-boundary fold trigger (plan D2): the tail is already decoded
    // in memory, so the usage criterion costs no query — the newest
    // assistant row's provider-reported tokens against this attempt's
    // budget, the same reading `fold_decision` recomputes inside. At or
    // over the threshold a detached silent fold starts; it re-runs the
    // full gate itself, so this site stays cheap and may double-fire
    // harmlessly (single-flight CAS + the gate).
    let usage_pct = rows
        .iter()
        .rev()
        .find_map(assistant_tokens)
        .and_then(|tokens| budget.usage_percentage(tokens));
    if usage_pct.is_some_and(|pct| pct >= super::super::compaction::FOLD_USAGE_THRESHOLD * 100.0) {
        super::super::compaction::spawn_background_fold(ctx);
    }
    // Filesystem work (repository anchor, skills walk, AGENTS.md reads) runs
    // off the executor; see `DrainCtx::observe_sources`.
    let tool_count = ctx.tools.guidances().count();
    // The baseline log needs the static layer's size, so it is built
    // before the sync: assembling it is pure string work over the
    // registry's guidance, unaffected by the keyed sources.
    let (instructions_initial, instruction_update) =
        sync_instructions(ctx, &observed, &rows, system_static.len(), tool_count).await?;
    let mut chat_messages = build_chat_messages(
        &system_static,
        &instructions_initial,
        &[head_m0.as_deref(), head_m1.as_deref()],
        &rows,
    );
    let mut layers = super::super::request_budget::Layers {
        system_static: system_static.len(),
        frozen: instructions_initial.len(),
        m0: head_m0.as_ref().map_or(0, String::len),
        m1: head_m1.as_ref().map_or(0, String::len),
        delta: instruction_update.as_ref().map_or(0, String::len),
        ..Default::default()
    };
    // Pressure is checked after the pass: the floor it reports is what the
    // live tail is measured above. Both windows were resolved once, when the
    // budget was derived. The trigger is called here rather than next to
    // `mctx_head`, because the historian's prefix-reusing request continues
    // *this* request: it reuses the frozen head, the static system layer and
    // the session's initial instruction render, so the bytes it sends are the
    // bytes this step sends. The head halves and the two static layers are
    // handed over by move / `Arc<str>` clone, so a step that fires no fold pays
    // no copy of the prefix it just rendered.
    super::super::historian_trigger::mctx_historian(
        ctx,
        budget,
        floor,
        params.model.variant.clone(),
        super::super::historian_trigger::PrefixContext {
            system_static: Arc::clone(&system_static),
            instructions: Arc::clone(&instructions_initial),
            head_m0,
            head_m1,
            model_id: params.model.id.clone(),
            // The same `Arc` the request below advertises, by refcount: providers
            // render the tools array *before* the system prompt, so a fold that
            // offered none would share no cached prefix at all.
            tools: ctx.tools.definitions_shared(),
        },
    );
    // The delta row is durable for future steps; this step's already-loaded
    // rows predate it, so the text rides this request in memory (upstream
    // `generate.ts:55` places `Message.system(instructionUpdate)` after the
    // transcript and BEFORE the current prompt).
    instructions::insert_update(&mut chat_messages, instruction_update);
    // Automatic recall: the turn's opening request gets the top few
    // memories/notes/compartments matching the prompt's own typed text, as
    // one compact `<recall>` block appended to that user message.
    //
    // The hint rides only the first request of a turn: both the trailing
    // transcript row and the trailing chat message must be that prompt (a
    // later step ends with tool output, and any row pushed after the prompt
    // means this request is not the opening one). So a long tool loop pays
    // at most one extra prefill of the trailing user message — the message
    // the block disappeared from on step 2 — and NO stored row is touched:
    // the bytes exist for this request only, and re-rendering the same rows
    // yields the same content again. Putting it in the tail rather than
    // `system`/`m[0]`/`m[1]` is what keeps the cached prefix intact.
    if ctx.config.magic_context.enabled
        && matches!(chat_messages.last(), Some(ChatMessage::User { .. }))
        && let Some(query) = recall_query(&rows)
    {
        let project = ctx.directory.to_string_lossy();
        // A session with memory switched off recalls no memories; a failed
        // read of the switch is treated as "on" (the hint is best-effort).
        let sources = match crate::mctx::memory_mode::mode_of(&ctx.db, &ctx.session_id).await {
            Ok(mode) if !mode.is_on() => {
                search::SourceFilter::RECALL.without(search::Source::Memory)
            }
            _ => search::SourceFilter::RECALL,
        };
        let request = search::SearchRequest {
            project: project.as_ref(),
            session_id: &ctx.session_id,
            // Recall stays on this session: a wider scope adds noise and
            // puts a project-wide scan on every turn.
            compartment_scope: search::CompartmentScope::Session,
            query: &query,
            sources,
            limit: RECALL_LIMIT,
            from_ms: None,
            to_ms: None,
        };
        // No embedder on this path (it belongs to the `ctx_search` tool), so
        // a hint is lexical. A failed query is silent by design: a turn must
        // never stall or fail because a hint could not be retrieved, and the
        // request is still well-formed without it.
        match search::search(&ctx.db, None, &request).await {
            Ok(hits) => {
                if let Some(block) = search::render_recall_block(&hits)
                    && let Some(ChatMessage::User {
                        content: UserContent::Text(text),
                    }) = chat_messages.last_mut()
                {
                    // Move the text out and put the appended version back:
                    // the only string this path rewrites is that one, and it
                    // is not cloned on the way. A parts-list user turn (prompt
                    // + media) keeps its bytes: the block is plain text.
                    let content = std::mem::take(text);
                    layers.recall = block.len() + if content.is_empty() { 0 } else { 2 };
                    *text = append_recall(content, &block);
                }
            }
            Err(error) => {
                tracing::debug!(session = %ctx.session_id, error = %error, "automatic recall failed");
            }
        }
    }
    let mut request = ChatRequest::new(
        // cardinal routes on the bare model id; the providerID/modelID
        // qualified form is upstream's provider-adapter vocabulary and
        // cardinal answers "no route for model cardinal/<id>" to it.
        params.model.id.clone(),
        chat_messages,
    );
    request.tools = ctx.tools.definitions_shared();
    // The session's selected reasoning level (model variant) rides every
    // chat turn; no selection leaves the field off the wire.
    request.apply_reasoning_effort(params.model.variant.as_deref());
    if params.final_step {
        apply_final_step(&mut request);
        layers.final_step = MAX_STEPS_PROMPT.len();
    }
    super::super::request_budget::apply(ctx, &mut request, layers)?;
    Ok(request)
}

/// Final-step request shape (upstream llm.ts): keep `tools` so the provider
/// cache prefix survives, forbid calling them, and end the history with
/// [`MAX_STEPS_PROMPT`] as an assistant message. Runs after
/// `ChatRequest::new` (which pairs tool results), so the appended text-only
/// message cannot disturb pairing.
fn apply_final_step(request: &mut ChatRequest) {
    request.tool_choice = ToolChoice::None;
    request.messages.push(ChatMessage::Assistant {
        content: Some(MAX_STEPS_PROMPT.to_owned()),
        tool_calls: None,
        reasoning_content: None,
    });
}

/// The typed text that opens the current turn, when this request is that
/// turn's opening request: `None` unless the transcript's trailing row is the
/// user prompt (a later step trails tool output) and it carries enough text to
/// retrieve anything.
///
/// The rows are the request *view*, so their text carries the overlay's `§N§`
/// prefix. That prefix is not something the user typed: leaving it in would
/// make its number a query token (the tagger's `§1§` becomes the word `1`) and
/// every turn would match whatever the corpus says about that number. The same
/// strip the persistence boundary applies removes it.
fn recall_query(rows: &[Message]) -> Option<String> {
    let Some(Message::User { prompt, .. }) = rows.last() else {
        return None;
    };
    let typed = strip_copied_tag_notation(&prompt.text);
    let text = typed.trim();
    (text.chars().count() >= RECALL_MIN_QUERY_CHARS).then(|| text.to_owned())
}

/// Append the recall block to a user message's content: one blank line between
/// the prompt and the block, no trailing newline.
fn append_recall(mut content: String, block: &str) -> String {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(block);
    content
}

/// Per-step keyed-instruction sync: diff current sources against what the
/// wire shows, publish durable delta rows, and return the update text for
/// this request's tail. The first step of a session builds its baseline and
/// logs the injection-surface accounting once (`injection baseline`).
async fn sync_instructions(
    ctx: &DrainCtx,
    observed: &[prompt::Observed],
    rows: &[Message],
    system0_bytes: usize,
    tool_count: usize,
) -> Result<(Arc<str>, Option<String>)> {
    let (preview, baseline_sizes) = {
        let mut baselines = super::super::lock(&ctx.instruction_baselines);
        // The sizes are pure from `observed`, so they can be reported after
        // the lock is released: tracing must not hold the baseline mutex.
        let fresh = !baselines.contains_key(&ctx.session_id);
        let baseline = baselines
            .entry(ctx.session_id.clone())
            .or_insert_with(|| instructions::baseline(observed));
        let preview = instructions::preview(observed, rows, Some(baseline))?;
        let sizes = fresh.then(|| prompt::injection_sizes(observed, system0_bytes, tool_count));
        (preview, sizes)
    };
    if let Some(sizes) = baseline_sizes {
        tracing::info!(
            session = %ctx.session_id,
            system0_bytes = sizes.system0_bytes,
            mcp_bytes = sizes.mcp_bytes,
            skills_bytes = sizes.skills_bytes,
            skills_catalog = ?sizes.skills_catalog,
            instructions_bytes = sizes.instructions_bytes,
            date_bytes = sizes.date_bytes,
            env_bytes = sizes.env_bytes,
            tools = sizes.tools,
            total_bytes = sizes.total_bytes,
            estimated_tokens = sizes.estimated_tokens,
            "injection baseline"
        );
    }
    let instructions::Preview { initial, deltas } = preview;
    if deltas.is_empty() {
        return Ok((initial, None));
    }
    let text = instructions::render_updates(&deltas);
    let delta = instructions::delta_record(&deltas);
    if let Err(error) = ctx
        .bus
        .publish(SessionEventData::SessionInstructionsUpdated {
            session_id: ctx.session_id.clone(),
            delta,
            sources: instructions::source_records(&deltas),
            text: Some(text.clone()),
        })
        .await
    {
        tracing::warn!(session = %ctx.session_id, %error, "instruction update publish failed");
        return Ok((initial, None));
    }
    let mut baselines = super::super::lock(&ctx.instruction_baselines);
    if let Some(baseline) = baselines.get_mut(&ctx.session_id) {
        instructions::apply(deltas, baseline);
    }
    Ok((initial, Some(text)))
}
