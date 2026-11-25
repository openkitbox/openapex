use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::completion::collect_text;
use crate::mctx::compartments::{self, NewCompartment, Tiers};
use crate::mctx::historian_validate::{self, ValidateOptions};
use crate::mctx::promotion;
use ox_core::config::Config;
use ox_llm::chat_completions::chat::Chat;
use ox_llm::chat_completions::wire::{ChatRequest, ToolDef};
use ox_schema::id::{id, now_millis};
use ox_schema::message::{Compaction, Message};

use super::chunk::Chunk;
use super::prompt::{
    CompartmentPromptInputs, HISTORIAN_SYSTEM_PROMPT, build_compartment_agent_prompt,
    build_historian_repair_prompt, build_prefixed_instruction, historian_request_with_prompt,
    prefixed_request,
};

/// Lifecycle of one `mc_historian_run` row (the `state` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Claimed, completion in flight.
    Running,
    /// Completion returned, validated and published.
    Completed,
    /// The completion failed; the row carries the error text.
    Failed,
    /// Left `running` by a process that died, settled at startup.
    Aborted,
}

impl RunState {
    /// The column spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Completed => "completed",
            RunState::Failed => "failed",
            RunState::Aborted => "aborted",
        }
    }
}

/// What one run needs from its host: the store, the config the model comes
/// from, and the mockable chat seam (the same one the runner drives).
pub struct RunCtx<'a, 'historian> {
    pub db: &'a ox_db::Db,
    pub config: &'a Config,
    pub chat: &'a Arc<dyn Chat>,
    /// Default-model resolution for the hidden completion.
    pub catalog: &'a ox_llm::model_catalog::ModelCatalog,
    /// The session's policy captured at trigger time; detached runs never
    /// re-read a mutable session selection.
    ///
    /// Borrowed, not owned: the host (the pressure event) lives for the whole
    /// run it starts and lends the same policy to every fold it executes, so
    /// a per-fold copy of the string would buy nothing.
    pub reasoning_effort: Option<&'historian str>,
    /// The session's live model, the one the user is actually talking to.
    /// A run whose historian model fails before producing output retries
    /// once through it ([`ModelChain`]); `None` falls back to the default.
    pub(crate) session_model: Option<&'a ox_schema::support::ModelRef>,
}

/// The models a hidden completion may try, in order: the historian's own
/// ([`historian_model`]) and, when it is a different one, the model the
/// session actually runs with (the catalogue default when no session is
/// involved). The fallback exists because the two are usually different
/// deployments: a cheap summarizer behind one gateway going down must not
/// stall every fold while the main model is answering fine.
///
/// Only a failure *before any output* moves down the chain — a transport
/// error, a timeout, an endpoint the router could not reach. A completion that
/// arrived but did not validate is a model answer and is repaired on the same
/// model; switching would mix two models' readings of one chunk.
///
/// Reference: codex `compact_model_fallback.rs` — compaction that failed on
/// the previous model is retried once on the current one when they differ.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelChain<'a> {
    primary: ox_schema::support::ModelRef,
    fallback: Option<std::borrow::Cow<'a, ox_schema::support::ModelRef>>,
}

impl<'a> ModelChain<'a> {
    /// Resolve the chain. `session_model` is the live model when the caller
    /// has a session; otherwise the catalogue default stands in. A fallback
    /// equal to the primary is dropped: the router already tried its
    /// candidates, so the same id again would repeat the same failure.
    pub(crate) fn resolve(
        config: &Config,
        catalog: &ox_llm::model_catalog::ModelCatalog,
        session_model: Option<&'a ox_schema::support::ModelRef>,
    ) -> Result<Self> {
        let primary = historian_model(config, catalog)?;
        let fallback = match session_model {
            Some(model) => Some(std::borrow::Cow::Borrowed(model)),
            None => catalog.default_entry(&config.llm).map(|entry| {
                std::borrow::Cow::Owned(ox_schema::support::ModelRef {
                    id: entry.id,
                    provider_id: entry.provider,
                    variant: None,
                })
            }),
        }
        .filter(|model| model.provider_id != primary.provider_id || model.id != primary.id);
        Ok(Self { primary, fallback })
    }

    /// The model a completion starts with.
    pub(crate) fn primary(&self) -> &ox_schema::support::ModelRef {
        &self.primary
    }

    /// The model to try after `current` failed before producing output:
    /// the fallback when `current` is the primary, nothing further after
    /// that (one step down, never a loop).
    pub(crate) fn next_after(
        &self,
        current: &ox_schema::support::ModelRef,
    ) -> Option<&ox_schema::support::ModelRef> {
        if current.provider_id != self.primary.provider_id || current.id != self.primary.id {
            return None;
        }
        self.fallback.as_deref()
    }
}

/// The live request a run's messages can begin with (plan A3): the bytes the
/// step request sent for the same rows, so the hidden completion reuses the
/// provider's cached prefix instead of re-reading the conversation cold.
///
/// Every field borrows from the live request's own assembly — the frozen head
/// halves, the static system layer and the session's initial instruction render
/// come from `runner::step::request`, the region from
/// [`build_chat_messages`](crate::runner::step::build_chat_messages) — because a
/// value rebuilt here would be a second renderer and drift from the bytes the
/// provider already holds.
pub struct Prefix<'a> {
    /// The request's first message: the static system layer.
    pub system_static: &'a str,
    /// The request's second message: the session's initial instruction render.
    pub instructions: &'a str,
    /// The frozen head halves (`m[0]`, `m[1]`), as the live request sends them.
    pub head: [Option<&'a str>; 2],
    /// The live transcript rows the prefix covers, decoded and overlaid. It
    /// **must begin at the head boundary** — see [`prefixed_request`].
    pub region: &'a [Message],
    /// The live request's tool definitions, borrowed from the step that fired
    /// this run: providers render the tools array ahead of the messages, so it
    /// is part of the prefix ([`prefixed_request`]).
    pub tools: &'a Arc<Vec<ToolDef>>,
}

/// One run's inputs. The caller (Task 9) resolves the chunk and the context
/// blocks; this keeps [`run`] a pure orchestration.
pub struct RunPlan<'a> {
    /// Owning session.
    pub session_id: &'a str,
    /// The session's project: promoted facts land in its memories.
    pub project: &'a str,
    /// The chunk to summarize, with the ordinals validation checks against.
    pub chunk: &'a Chunk,
    /// The first attempt this run makes; a retry increments the row's attempt.
    pub attempt: i64,
    /// The context blocks assembled for the prompt (the chunk text is taken from
    /// [`Chunk::text`], not from this).
    pub prompt: CompartmentPromptInputs<'a>,
    /// The live request this run's messages continue, when the caller could
    /// produce it. `None` sends the fail-open shape: the chunk rendered as the
    /// prompt's transcript, in an independent two-message request.
    pub prefix: Option<Prefix<'a>>,
    /// Original merged row and token calibration for an oversized checkpoint.
    pub(crate) checkpoint: Option<CheckpointRun<'a>>,
}

pub(crate) struct CheckpointRun<'a> {
    pub row: &'a serde_json::Value,
    pub ratio: f64,
}

/// A settled run: the published compartments and the raw completion that
/// produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// The `mc_historian_run` row id.
    pub run_id: String,
    /// The completion's text, verbatim (the last attempt's).
    pub text: String,
    /// The attempt the run settled with.
    pub attempt: i64,
    /// The `mc_compartments` rows published, in covered order. Shorter than the
    /// model's compartment count when validation withheld the provisional last
    /// one (the default boundary healing), never empty.
    pub compartment_ids: Vec<String>,
    /// The run's memory-promotion side channels, also stored on the run row.
    pub side_channels: SideChannels,
}

/// Total completion attempts one run may make, including the first
/// (`plan.attempt` counts up from there).
///
/// The reference retries an invalid output by re-prompting the *next* model in
/// its fallback chain (`historian.rs::run_historian_firing`), so its bound is the
/// chain's length; ox drives a single model, so it re-prompts the same one a
/// fixed number of times. Three is one original plus two repairs — enough for the
/// common truncation and mismatched-close failures to self-correct. A validation
/// failure is a *model* failure, so the same chunk is retried on the same
/// model; a transport failure is retried only as one step down the
/// [`ModelChain`], and counts against the same bound.
pub const HISTORIAN_MAX_ATTEMPTS: i64 = 3;

/// The longest one historian completion may take before it counts as a
/// transport failure: five minutes. A stream that stalls without closing
/// would otherwise hold the session's run slot for the life of the process.
pub const HISTORIAN_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a `running` row stays authoritative without a heartbeat:
/// [`HISTORIAN_COMPLETION_TIMEOUT`] with a full attempt's margin, so a live
/// run — whose every attempt stamps `updated_at` — is never pre-empted, and
/// one orphaned by a process that lost its connection (not its life, which
/// [`settle_running_runs`] handles) is reclaimed by the next pressure event
/// instead of blocking the session until restart.
pub const HISTORIAN_LEASE: Duration = Duration::from_secs(2 * 5 * 60 + 60);

/// [`collect_text`] under [`HISTORIAN_COMPLETION_TIMEOUT`]. The
/// timeout is an error like any transport failure: the run settles `failed`
/// and the caller decides whether to retry.
async fn collect_text_bounded(
    chat: &Arc<dyn Chat>,
    request: &ChatRequest,
    provider: &str,
) -> Result<String> {
    match tokio::time::timeout(
        HISTORIAN_COMPLETION_TIMEOUT,
        collect_text(chat, request, Some(provider), "historian"),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "historian completion timed out after {}s",
            HISTORIAN_COMPLETION_TIMEOUT.as_secs()
        )),
    }
}

/// Importance a validated compartment gets when the model omitted the attribute.
const DEFAULT_COMPARTMENT_IMPORTANCE: i64 = 50;

/// A completion that could not be parsed or did not validate, carried with the
/// validator's own message.
///
/// It is its own error type because the distinction is load-bearing: [`run`]
/// re-prompts on this and gives up on a transport failure, and a caller (or a
/// test) can tell the two apart with `Error::downcast_ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationFailure {
    /// The last attempt's validation message.
    pub message: String,
}

impl std::fmt::Display for ValidationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValidationFailure {}

/// Run one historian completion, validate it and publish the result.
///
/// The durable row is written `running` BEFORE the completion starts, so a
/// process that dies mid-completion leaves evidence ([`settle_running_runs`]
/// cleans it up). A completion that cannot be parsed or does not validate is a
/// model failure: the same chunk is re-prompted through
/// [`build_historian_repair_prompt`] until [`HISTORIAN_MAX_ATTEMPTS`] attempts
/// have been made, then the row settles `failed` and the error is returned. A
/// transport failure moves once down the [`ModelChain`] (the session's own
/// model) when there is a step left to take; otherwise it settles the row
/// `failed` at once — retrying it is the caller's decision, not this run's.
///
/// The request shape follows [`RunPlan::prefix`]: with a live prefix the
/// completion continues the conversation the provider already holds and states
/// its task after it (see [`prefixed_request`]); without one it sends the
/// fail-open two-message request. The caller decides and logs which, and the two
/// shapes are otherwise identical — same model, same validation, same
/// publication.
///
/// `None` means the session already has a run in flight and this one did not
/// claim the slot: it has done nothing at all, and the winner's compartments are
/// what the next pass picks up.
pub async fn run(ctx: &RunCtx<'_, '_>, plan: &RunPlan<'_>) -> Result<Option<RunOutcome>> {
    let chain = ModelChain::resolve(ctx.config, ctx.catalog, ctx.session_model)?;
    let mut model = chain.primary();
    // Which task text this run sends. The prefix shape's instruction carries no
    // transcript (its ordinal index names the blocks instead), so the fail-open
    // prompt — which formats the whole chunk — is assembled only when the run
    // actually sends it. Exactly one of the two exists, and the repair re-prompt
    // is built from whichever it was.
    let prefixed_instruction = plan
        .prefix
        .as_ref()
        .map(|_| build_prefixed_instruction(&plan.prompt, &plan.chunk.blocks));
    let fail_open_prompt = plan.prefix.is_none().then(|| {
        if plan.checkpoint.is_some() {
            String::new()
        } else {
            build_compartment_agent_prompt(&plan.prompt)
        }
    });
    let request_prompt = prefixed_instruction
        .as_deref()
        .or(fail_open_prompt.as_deref())
        .expect(
            "one task text is always assembled: the prefix instruction or the fail-open prompt",
        );
    let chunk_ref = format!("{}..{}", plan.chunk.start_ordinal, plan.chunk.end_ordinal);
    let Some((run_id, prior)) =
        claim_run(ctx.db, plan.session_id, &chunk_ref, plan.attempt).await?
    else {
        tracing::debug!(
            session = %plan.session_id,
            chunk = %chunk_ref,
            "historian claim lost to a run already in flight"
        );
        return Ok(None);
    };
    let continuity = check_chunk_continuity(ctx.db, plan.session_id, plan.chunk, &prior).await;
    let continuity = match continuity {
        Ok(continuity) => continuity,
        Err(error) => {
            settle_failed(ctx.db, &run_id, &error).await;
            return Err(error);
        }
    };
    if !continuity {
        let error = "historian chunk no longer starts at the stored coverage boundary";
        settle_run(ctx.db, &run_id, RunState::Aborted, Some(error), None).await?;
        tracing::debug!(
            session = %plan.session_id,
            chunk = %chunk_ref,
            error = %error,
            "historian skipped a stale chunk after claiming"
        );
        return Ok(None);
    }

    if let Some(checkpoint) = &plan.checkpoint {
        let result = run_checkpoint(ctx, plan, checkpoint, model, &run_id, &prior).await;
        if let Err(error) = &result {
            settle_failed(ctx.db, &run_id, error).await;
        }
        return result.map(Some);
    }

    let last_attempt = plan.attempt + HISTORIAN_MAX_ATTEMPTS - 1;
    let mut attempt = plan.attempt;
    // The rejected attempt's raw text and the message that rejected it; both feed
    // the next attempt's repair prompt.
    let mut repair: Option<(String, String)> = None;
    loop {
        // The first attempt reuses the built prompt by reference; only a repair
        // attempt has to synthesize a new one.
        let user_prompt = match &repair {
            Some((previous, error)) => std::borrow::Cow::Owned(build_historian_repair_prompt(
                request_prompt,
                previous,
                error,
            )),
            None => std::borrow::Cow::Borrowed(request_prompt),
        };
        // The prefix shape re-renders the live prefix per attempt: a repair
        // attempt sends the same prefix with a different tail, which is still a
        // cache hit on everything before the instruction.
        let request = match &plan.prefix {
            Some(prefix) => prefixed_request(
                model,
                prefix.system_static,
                prefix.instructions,
                &prefix.head,
                prefix.region,
                prefix.tools,
                HISTORIAN_SYSTEM_PROMPT,
                &user_prompt,
                ctx.reasoning_effort,
            ),
            None => historian_request_with_prompt(model, &user_prompt, ctx.reasoning_effort),
        };

        let text = match collect_text_bounded(ctx.chat, &request, &model.provider_id).await {
            Ok(text) => text,
            Err(error) => {
                // Nothing came back: one step down the chain, as one more
                // attempt of this run, while the attempt budget allows it.
                let Some(next) = chain.next_after(model).filter(|_| attempt < last_attempt) else {
                    settle_failed(ctx.db, &run_id, &error).await;
                    return Err(error);
                };
                tracing::warn!(
                    session = %plan.session_id,
                    failed_model = %format!("{}/{}", model.provider_id, model.id),
                    fallback_model = %format!("{}/{}", next.provider_id, next.id),
                    error = %error,
                    "historian completion failed before output; retrying on the fallback model"
                );
                model = next;
                attempt += 1;
                update_run_attempt(ctx.db, &run_id, attempt).await?;
                continue;
            }
        };

        match validate_text(&text, plan.chunk, &prior) {
            Ok(validated) => {
                // The publish plan is single use: its compartment strings move
                // into the store rows and its side channels into the run
                // outcome, so nothing is cloned out of the validated plan.
                let historian_validate::ValidatedChunk {
                    mut compartments,
                    facts,
                    events,
                    user_observations,
                    ..
                } = validated;
                let user_facts = promotion::user_candidates(user_observations);
                let side_channels = SideChannels { facts, events };
                let promotable = Promotable {
                    facts: &side_channels.facts,
                    user_facts: &user_facts,
                };
                let compartment_ids = match publish_compartments(
                    ctx.db,
                    plan,
                    &run_id,
                    &prior,
                    &mut compartments,
                    promotable,
                    &side_channels,
                )
                .await
                {
                    Ok(ids) => ids,
                    Err(error) => {
                        settle_failed(ctx.db, &run_id, &error).await;
                        return Err(error);
                    }
                };
                return Ok(Some(RunOutcome {
                    run_id,
                    text,
                    attempt,
                    compartment_ids,
                    side_channels,
                }));
            }
            Err(failure) => {
                if attempt >= last_attempt {
                    let error = anyhow::Error::new(failure);
                    settle_failed(ctx.db, &run_id, &error).await;
                    return Err(error);
                }
                attempt += 1;
                update_run_attempt(ctx.db, &run_id, attempt).await?;
                repair = Some((text, failure.message));
            }
        }
    }
}

async fn run_checkpoint(
    ctx: &RunCtx<'_, '_>,
    plan: &RunPlan<'_>,
    checkpoint: &CheckpointRun<'_>,
    model: &ox_schema::support::ModelRef,
    run_id: &str,
    prior: &[historian_validate::MessageRange],
) -> Result<RunOutcome> {
    let message = crate::runner::step::decode_row(checkpoint.row)?;
    let Message::Compaction(message) = message else {
        anyhow::bail!("checkpoint row changed type")
    };
    let Compaction::Completed(completed) = message.compaction else {
        anyhow::bail!("checkpoint is not completed")
    };
    let validated = crate::mctx::checkpoint_slices::run_checkpoint_slices(
        ctx.chat,
        ctx.catalog,
        crate::mctx::checkpoint_slices::CheckpointSliceInput {
            ordinal: plan.chunk.start_ordinal,
            summary: &completed.summary,
            recent: &completed.recent,
            model,
            extra_fields: &[],
            ratio: checkpoint.ratio,
            output_tokens: 4096,
            reasoning_effort: ctx.reasoning_effort,
        },
    )
    .await?;
    let historian_validate::ValidatedChunk {
        mut compartments,
        facts,
        events,
        user_observations,
        ..
    } = validated;
    let user_facts = promotion::user_candidates(user_observations);
    let side_channels = SideChannels { facts, events };
    let promotable = Promotable {
        facts: &side_channels.facts,
        user_facts: &user_facts,
    };
    let compartment_ids = publish_compartments(
        ctx.db,
        plan,
        run_id,
        prior,
        &mut compartments,
        promotable,
        &side_channels,
    )
    .await?;
    Ok(RunOutcome {
        run_id: run_id.to_owned(),
        text: String::new(),
        attempt: plan.attempt,
        compartment_ids,
        side_channels,
    })
}

/// Validate one completion against the chunk it was asked to summarize.
fn validate_text(
    text: &str,
    chunk: &Chunk,
    prior: &[historian_validate::MessageRange],
) -> Result<historian_validate::ValidatedChunk, ValidationFailure> {
    historian_validate::validate_historian_output(
        text,
        &chunk_view(chunk),
        prior,
        ValidateOptions::default(),
    )
    .map_err(|error| ValidationFailure {
        message: error.message,
    })
}

/// The validator's view of one chunk.
///
/// ox's chunk carries no tool metadata: [`Chunk`] records ordinals only, so the
/// invocation/result arcs and tool-only ranges the reference derives from its
/// `BoundaryMsg` list are not reconstructible here. Those two rules are
/// therefore inert on the production path (see the module docs) — they stay
/// ported and unit-tested in [`historian_validate`].
fn chunk_view(chunk: &Chunk) -> historian_validate::ChunkView<'_> {
    historian_validate::ChunkView {
        start_ordinal: chunk.start_ordinal,
        end_ordinal: chunk.end_ordinal,
        line_ordinals: &chunk.present_ordinals,
        present_ordinals: &chunk.present_ordinals,
        tool_only_ranges: &[],
        completed_tool_arcs: &[],
    }
}

/// Reject overlap or omitted decodable, non-synthetic messages above the actual floor.
/// Gaps from retired rows and skipped synthetic messages are allowed.
async fn check_chunk_continuity(
    db: &ox_db::Db,
    session_id: &str,
    chunk: &Chunk,
    prior: &[historian_validate::MessageRange],
) -> Result<bool> {
    let floor = prior.iter().map(|range| range.end).max().unwrap_or(0);
    if chunk.start_ordinal <= floor {
        return Ok(false);
    }
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT id, type, data FROM session_message
             WHERE session_id = ?1 AND seq > ?2 AND seq < ?3 ORDER BY seq",
        )
        .await?;
    let mut rows = stmt.query((session_id, floor, chunk.start_ordinal)).await?;
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let data: String = row.get(2)?;
        let mut value: serde_json::Value = serde_json::from_str(&data)?;
        Message::merge_row_columns(&id, &kind, &mut value);
        let Ok(message) = crate::runner::step::decode_row(&value) else {
            continue;
        };
        if !matches!(message, Message::Synthetic { .. }) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Publish validated compartments, promote the run's facts, and return the
/// minted compartment row ids.
///
/// Coverage is derived from these rows, so writing the chunk's range is what
/// advances the floor a later pass reads. Everything goes in one transaction:
/// a partial publish would advance coverage past content that was never
/// summarized, and the run completion and promoted memories commit or roll back
/// with the compartments that produced them. Promotion runs only when the prompt asked
/// for facts (`memory_enabled`) and the model returned some.
///
/// The side channels one publish may promote, borrowed from the validated
/// plan. Project facts go to the project's pinboard; user facts (the
/// historian's `<user_observations>`) go to the user tier.
#[derive(Clone, Copy)]
struct Promotable<'a> {
    facts: &'a [historian_validate::FactCandidate],
    user_facts: &'a [historian_validate::FactCandidate],
}

impl Promotable<'_> {
    fn is_empty(&self) -> bool {
        self.facts.is_empty() && self.user_facts.is_empty()
    }
}

/// Consumes the compartments' strings (`&mut` is ownership transfer here):
/// validation already produced them and the publish plan is not read again.
async fn publish_compartments(
    db: &ox_db::Db,
    plan: &RunPlan<'_>,
    run_id: &str,
    prior: &[historian_validate::MessageRange],
    compartments: &mut [historian_validate::ValidatedCompartment],
    promotable: Promotable<'_>,
    side_channels: &SideChannels,
) -> Result<Vec<String>> {
    let session_id = plan.session_id;
    let mut pending: Vec<(String, Tiers, f64)> = Vec::with_capacity(compartments.len());
    for compartment in compartments {
        pending.push((
            id("mc"),
            Tiers {
                title: std::mem::take(&mut compartment.title),
                start_message: compartment.start_message,
                end_message: compartment.end_message,
                // The historian output carries no dates and ox's chunk tracks
                // none, so the decay renderer gets no span for a live row.
                start_date: None,
                end_date: None,
                p1: compartment.p1.take(),
                p2: compartment.p2.take(),
                p3: compartment.p3.take(),
                p4: compartment.p4.take(),
            },
            importance_of(compartment.importance),
        ));
    }

    let rows: Vec<NewCompartment<'_>> = pending
        .iter()
        .map(|(compartment_id, tiers, importance)| NewCompartment {
            id: compartment_id,
            session_id,
            importance: *importance,
            tiers,
        })
        .collect();
    // Serialized before the transaction opens (see `compartments::insert_all`).
    let tiers = compartments::serialize_tiers(&rows)?;
    let now = now_millis();
    // Promotion is withheld for a session with memory switched off or one
    // carrying external content (see `mctx::memory_mode`): the side channels
    // are still stored on the run row, but nothing becomes project memory.
    // Read before the write transaction opens so the check never extends it.
    let wanted = plan.prompt.memory_enabled && !promotable.is_empty();
    let withheld = if wanted {
        crate::mctx::memory_mode::promotion_withheld(db, session_id).await?
    } else {
        None
    };
    if let Some(reason) = withheld {
        tracing::info!(
            session = %session_id,
            facts = promotable.facts.len(),
            user_facts = promotable.user_facts.len(),
            reason = reason.as_str(),
            "historian side channels not promoted"
        );
    }
    let promote = wanted && withheld.is_none();
    let tx = db.begin().await?;
    verify_claim_and_coverage(&tx, session_id, run_id, prior).await?;
    if let Some(checkpoint) = &plan.checkpoint {
        let row = ox_db::optional(
            &mut tx
                .prepare(
                    "SELECT id, type, data FROM session_message WHERE session_id = ?1 AND seq = ?2",
                )
                .await?,
            (session_id, plan.chunk.start_ordinal),
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("checkpoint row is missing"))?;
        let id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let data: String = row.get(2)?;
        let mut value: serde_json::Value = serde_json::from_str(&data)?;
        Message::merge_row_columns(&id, &kind, &mut value);
        anyhow::ensure!(
            &value == checkpoint.row && id == plan.chunk.end_message_id,
            "checkpoint row changed before publication"
        );
    }
    compartments::insert_all_in(&tx, &rows, &tiers, now)
        .await
        .with_context(|| {
            format!(
                "insert {} mc_compartments rows for session {session_id}",
                rows.len()
            )
        })?;
    if promote {
        promote_side_channels(&tx, plan, promotable, now).await?;
    }
    let facts =
        serde_json::to_string(&side_channels.facts).context("serialize run facts side channel")?;
    let events = serde_json::to_string(&side_channels.events)
        .context("serialize run events side channel")?;
    settle_run_in(
        &tx,
        run_id,
        RunState::Completed,
        None,
        Some(&facts),
        Some(&events),
    )
    .await?;
    tx.commit()
        .await
        .with_context(|| format!("commit historian publish for session {session_id}"))?;
    Ok(pending.into_iter().map(|(id, _, _)| id).collect())
}

async fn verify_claim_and_coverage(
    tx: &ox_db::WriteTx<'_>,
    session_id: &str,
    run_id: &str,
    prior: &[historian_validate::MessageRange],
) -> Result<()> {
    let (state, updated_at) = ox_db::optional(
        &mut tx
            .prepare(
                "SELECT state, updated_at FROM mc_historian_run WHERE id = ?1 AND session_id = ?2",
            )
            .await?,
        (run_id, session_id),
    )
    .await?
    .map(|row| -> Result<(String, i64)> { Ok((row.get(0)?, row.get(1)?)) })
    .transpose()?
    .ok_or_else(|| anyhow::anyhow!("historian claim {run_id} is missing"))?;
    if state != RunState::Running.as_str() {
        anyhow::bail!("historian claim {run_id} is no longer running");
    }
    if updated_at < lease_floor(now_millis()) {
        anyhow::bail!("historian claim {run_id} lease expired");
    }
    let current = stored_ranges_in(tx, session_id).await?;
    if current != prior {
        anyhow::bail!("stored historian coverage changed after claim");
    }
    Ok(())
}

async fn stored_ranges_in(
    conn: &ox_db::WriteTx<'_>,
    session_id: &str,
) -> Result<Vec<historian_validate::MessageRange>> {
    let mut stmt = conn
        .prepare(
            "SELECT tiers FROM mc_compartments
             WHERE session_id = ?1
             ORDER BY created_at, id",
        )
        .await?;
    let mut rows = stmt.query([session_id]).await?;
    let mut ranges = Vec::new();
    while let Some(row) = rows.next().await? {
        let tiers: String = row.get(0)?;
        let tiers: Tiers = serde_json::from_str(&tiers)
            .with_context(|| format!("decode stored historian tiers for session {session_id}"))?;
        ranges.push(historian_validate::MessageRange {
            start: tiers.start_message,
            end: tiers.end_message,
        });
    }
    Ok(ranges)
}

/// Promote the project facts into the project and the user observations into
/// the user tier, inside the publish transaction. Each tier is one promotion
/// pass over its own owner; the ids they mint are sequential because the
/// passes run back to back on one write transaction.
async fn promote_side_channels(
    tx: &ox_db::WriteTx<'_>,
    plan: &RunPlan<'_>,
    promotable: Promotable<'_>,
    now: i64,
) -> Result<()> {
    let tiers = [
        (plan.project, promotable.facts),
        (crate::mctx::memory::USER_SCOPE, promotable.user_facts),
    ];
    for (owner, facts) in tiers {
        if facts.is_empty() {
            continue;
        }
        let summary = promotion::apply(tx, owner, facts, now).await?;
        tracing::info!(
            session = %plan.session_id,
            owner,
            facts = facts.len(),
            inserted = summary.inserted,
            bumped = summary.bumped,
            "historian side channel promoted"
        );
    }
    Ok(())
}

/// A validated importance clamped into the store's 1..100 decay band.
fn importance_of(importance: Option<i64>) -> f64 {
    importance
        .unwrap_or(DEFAULT_COMPARTMENT_IMPORTANCE)
        .clamp(1, 100) as f64
}

/// Settle a row `failed` without masking the error that caused it: a failed
/// settle only warns, so the caller still sees the original cause.
async fn settle_failed(db: &ox_db::Db, run_id: &str, error: &anyhow::Error) {
    if let Err(settle) =
        settle_run(db, run_id, RunState::Failed, Some(&error.to_string()), None).await
    {
        tracing::warn!(run = %run_id, error = %settle, "failed to settle historian run as failed");
    }
}

/// Record the attempt a retry is about to make, so the row's `attempt` column
/// always names the try that produced its terminal state.
async fn update_run_attempt(db: &ox_db::Db, run_id: &str, attempt: i64) -> Result<()> {
    let now = now_millis();
    let conn = db.acquire().await?;
    let changed = conn.execute(
        "UPDATE mc_historian_run SET attempt = ?2, updated_at = ?3 WHERE id = ?1 AND state = 'running'",
        (run_id, attempt, now),
    )
    .await
    .with_context(|| format!("record mc_historian_run attempt {attempt}"))?;
    if changed == 0 {
        anyhow::bail!("historian claim {run_id} is no longer running");
    }
    Ok(())
}

/// Settle every `running` row `aborted`.
///
/// A run cannot outlive the process that owns its connection, so any `running`
/// row found at startup is a leftover from a killed process. Call this once per
/// process, before accepting work (Task 9 wires it at startup). Returns how many
/// rows were settled.
pub async fn settle_running_runs(db: &ox_db::Db) -> Result<usize> {
    let now = now_millis();
    // The age is reported per run so an operator can see how long the
    // killed process had a run in flight (`updated_at` is the terminal
    // transition's stamp; nothing else records a run's duration).
    let conn = db.acquire().await?;
    let settled = async {
        let mut stmt = conn
            .prepare("SELECT created_at FROM mc_historian_run WHERE state = 'running'")
            .await?;
        let mut rows = stmt.query(()).await?;
        let mut ages = Vec::new();
        while let Some(row) = rows.next().await? {
            ages.push(row.get::<i64>(0)?);
        }
        drop(rows);
        drop(stmt);
        for created_at in &ages {
            tracing::warn!(
                age_ms = now - created_at,
                "settled a stale running historian run as aborted: its process exited"
            );
        }
        conn.execute(
            "UPDATE mc_historian_run
                 SET state = 'aborted', updated_at = ?1,
                     error = COALESCE(error, 'aborted at startup: the owning process exited')
                 WHERE state = 'running'",
            [now],
        )
        .await
    }
    .await
    .context("settle stale running historian runs")?;
    Ok(settled as usize)
}

/// The one predicate saying a session's run slot is taken: a `running` row
/// whose lease (`updated_at`, stamped at claim and on every attempt) is
/// younger than `?2`. Shared by the pre-check and the claim so the two cannot
/// drift apart.
const RUNNING_RUN_SQL: &str = "SELECT count(*) FROM mc_historian_run
     WHERE session_id = ?1 AND state = 'running' AND updated_at >= ?2";

/// The oldest `updated_at` a `running` row may carry at `now` and still hold
/// its slot.
fn lease_floor(now: i64) -> i64 {
    now.saturating_sub(HISTORIAN_LEASE.as_millis() as i64)
}

/// Whether `session_id` already has a run in flight, without touching the store
/// otherwise: the trigger's cheap early exit before it loads the tail. A row is
/// `running` from before its completion starts until it settles — or until
/// its lease lapses, after which it no longer counts (and the next claim
/// settles it).
pub async fn has_running_run(db: &ox_db::Db, session_id: &str) -> Result<bool> {
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(RUNNING_RUN_SQL).await?;
    let mut rows = stmt.query((session_id, lease_floor(now_millis()))).await?;
    Ok(match rows.next().await? {
        Some(row) => row.get::<i64>(0)? > 0,
        None => false,
    })
}

/// Claim the session's run slot: write the `running` row and hand back its id,
/// or `None` when another run already holds it.
///
/// The claim is one insert, because the rule lives in the schema:
/// `mc_historian_run_running_idx` is unique over the running rows, so the
/// losing insert matches that conflict, inserts nothing and returns no row.
/// Before it, inside the same write transaction, a `running` row whose lease
/// has lapsed is settled `aborted` so the slot can be taken: with the write
/// gate held there is no window in which two claimants both see the slot
/// free, in this process or in another one over the same file.
async fn claim_run(
    db: &ox_db::Db,
    session_id: &str,
    chunk_ref: &str,
    attempt: i64,
) -> Result<Option<(String, Vec<historian_validate::MessageRange>)>> {
    let run_id = id("hrun");
    let now = now_millis();
    let tx = db.begin().await?;
    let reclaimed = tx
        .execute(
            "UPDATE mc_historian_run
                SET state = 'aborted', updated_at = ?3,
                    error = 'lease expired: no heartbeat within the historian lease'
              WHERE session_id = ?1 AND state = 'running' AND updated_at < ?2",
            (session_id, lease_floor(now), now),
        )
        .await?;
    if reclaimed > 0 {
        tracing::warn!(session = %session_id, "historian run slot reclaimed from an expired lease");
    }
    let claimed = {
        let mut stmt = tx
            .prepare(
                "INSERT INTO mc_historian_run
                     (id, session_id, state, chunk_ref, attempt, created_at, updated_at)
                 VALUES (?1, ?2, 'running', ?3, ?4, ?5, ?5)
                 ON CONFLICT (session_id) WHERE state = 'running' DO NOTHING
                 RETURNING id",
            )
            .await?;
        let mut rows = stmt
            .query((run_id.as_str(), session_id, chunk_ref, attempt, now))
            .await?;
        match rows.next().await? {
            Some(row) => Some(row.get::<String>(0)?),
            None => None,
        }
    };
    let Some(run_id) = claimed else {
        tx.commit().await?;
        return Ok(None);
    };
    let prior = stored_ranges_in(&tx, session_id).await?;
    tx.commit().await?;
    Ok(Some((run_id, prior)))
}

/// Move a run row to its terminal state.
async fn settle_run(
    db: &ox_db::Db,
    run_id: &str,
    state: RunState,
    error: Option<&str>,
    side_channels: Option<&SideChannels>,
) -> Result<()> {
    // The run row owns the side channels: the historian emits `<facts>` and
    // `<events>` once per run (a compartment has no such fields); the facts
    // were already promoted at publish, and this row keeps the record.
    // Serialized before the write, so a serialization failure never leaves a
    // half-settled row.
    let facts = match side_channels {
        Some(channels) => Some(
            serde_json::to_string(&channels.facts).context("serialize run facts side channel")?,
        ),
        None => None,
    };
    let events = match side_channels {
        Some(channels) => Some(
            serde_json::to_string(&channels.events).context("serialize run events side channel")?,
        ),
        None => None,
    };
    let tx = db.begin().await?;
    settle_run_in(
        &tx,
        run_id,
        state,
        error,
        facts.as_deref(),
        events.as_deref(),
    )
    .await?;
    tx.commit().await
}

async fn settle_run_in(
    tx: &ox_db::WriteTx<'_>,
    run_id: &str,
    state: RunState,
    error: Option<&str>,
    facts: Option<&str>,
    events: Option<&str>,
) -> Result<()> {
    let now = now_millis();
    let settled = tx
        .execute(
            "UPDATE mc_historian_run
             SET state = ?2, updated_at = ?3, error = ?4,
                 facts = COALESCE(?5, facts), events = COALESCE(?6, events)
             WHERE id = ?1 AND state = 'running'
               AND (?2 != 'completed' OR updated_at >= ?7)",
            (
                run_id,
                state.as_str(),
                now,
                error,
                facts,
                events,
                lease_floor(now),
            ),
        )
        .await
        .with_context(|| format!("settle mc_historian_run row as {}", state.as_str()))?;
    if settled == 0 {
        anyhow::bail!("no mc_historian_run row settled as {}", state.as_str());
    }
    Ok(())
}

/// The memory-promotion side channels one historian completion carries: the
/// reference parses both out of the model's output and hands them to the
/// memory path rather than to the compartment store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideChannels {
    /// `<facts>` candidates, as parsed (each keeps its anchor and text).
    pub facts: Vec<historian_validate::FactCandidate>,
    /// `<events>` entries, as parsed.
    pub events: Vec<historian_validate::ParsedEvent>,
}

/// The model the historian completion routes to. A bare `historian_model`
/// reference resolves its provider from the model catalogue; a qualified
/// `provider/id` must name the same catalogue entry. An unset reference falls
/// back to ox's default model resolution. A reference that matches no catalogue
/// entry is an error, not a placeholder identity.
///
/// `pub(crate)` because the dreamer runs its hidden completions through the
/// same resolution and the same transport ([`collect_text`]): one route
/// to the model, not two.
pub(crate) fn historian_model(
    config: &Config,
    catalog: &ox_llm::model_catalog::ModelCatalog,
) -> Result<ox_schema::support::ModelRef> {
    if let Some(qualified) = &config.magic_context.historian_model {
        let (provider, id) = qualified
            .split_once('/')
            .map_or((None, qualified.as_str()), |(provider, id)| {
                (Some(provider), id)
            });
        let entries = catalog.cached();
        let Some(entry) = entries.iter().find(|entry| {
            entry.id == id && provider.is_none_or(|selected| entry.provider == selected)
        }) else {
            anyhow::bail!(
                "magic_context.historian_model `{qualified}` does not match a model in the catalogue"
            );
        };
        return Ok(ox_schema::support::ModelRef {
            id: entry.id.clone(),
            provider_id: entry.provider.clone(),
            variant: None,
        });
    }
    match catalog.default_entry(&config.llm) {
        Some(entry) => Ok(ox_schema::support::ModelRef {
            id: entry.id,
            provider_id: entry.provider,
            variant: None,
        }),
        None => anyhow::bail!(
            "no model configured for the historian completion: set magic_context.historian_model or a default model"
        ),
    }
}
