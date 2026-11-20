//! Historian — Tasks 6a/6b: trigger, chunk, prompt, cardinal execution, the
//! durable run half, validation and publication.
//!
//! 1. [`trigger`] — may a run start at all, judged by tail pressure.
//! 2. [`chunk`] — which tail messages a run would cover, and the transcript
//!    text the model sees for them.
//! 3. the prompt — [`HISTORIAN_SYSTEM_PROMPT`] plus
//!    [`build_compartment_agent_prompt`], the production-tuned instruction
//!    surface, [`build_prefixed_instruction`], the tail the prefix-reusing
//!    request carries instead of the flat transcript, and
//!    [`build_historian_repair_prompt`], the re-prompt used after
//!    a completion that does not validate.
//! 4. [`run`] — the orchestration: a durable `mc_historian_run` row is written
//!    `running` *before* the completion starts, a hidden (session-less,
//!    tool-less) completion is driven through [`ox_llm::cardinal::chat::Chat`], the raw text is
//!    validated and published as compartments
//!    ([`crate::mctx::historian_validate`]), and the row settles
//!    `completed`/`failed` with the attempt recorded.
//! 5. [`settle_running_runs`] — the crash-recovery half: a `running` row left
//!    by a killed process is settled `aborted` instead of staying in flight.
//!
//! Publication is what advances coverage: [`crate::mctx::compartments::coverage_end`]
//! derives the floor from the `mc_compartments` rows, so writing a row with the
//! chunk's range is all a later pass needs to pick the new floor up.
//!
//! ## Port map (reference = `reference/magic-context/crates/`)
//!
//! - [`trigger`] ← `mc-module/src/boundary.rs` `check_compartment_trigger_with_index`
//!   (lines ~778-925: the force band, `TailSize`, `BelowProactiveFloor`,
//!   `ProjectedHeadroom` order) and `mc-module/src/scheduler.rs` `escalation_bands`
//!   (lines ~202-212, `force_materialize_percentage = max(85, threshold+2)`).
//! - [`chunk`] and its `Builder` ← `mc-module/src/historian_chunk.rs`
//!   `Builder::push_message` / `Builder::absorb_tool_only` /
//!   `Builder::flush_current_block` and the formatting helpers (`compact_role`
//!   line ~1940, `format_block` ~1914, `compact_text_for_summary` ~1951,
//!   `clean_user_text` ~1876, `extract_key_arg`, `truncate_utf16`).
//! - the prompt ← `mc-module/src/historian_prompt.rs` (whole file): the vendored
//!   `HISTORIAN_SYSTEM_PROMPT`, the seed/reference calibration blocks, the
//!   historian memory block and `build_compartment_agent_prompt` (line ~569);
//!   [`build_historian_repair_prompt`] ← `build_historian_repair_prompt`
//!   (line ~159).
//! - [`run`] / [`settle_running_runs`] ← `mc-store/src/historian_claim.rs` (the
//!   durable `mc_historian_pending_run` lifecycle: the row exists before the
//!   completion is handed to a claimant, and a run whose queuing process
//!   restarted is settled rather than left in flight — its `parked` phase) and
//!   `mc-module/src/historian_producer.rs` (the completion call itself).
//! - validation and publication ← `mc-module/src/historian_validate.rs`
//!   (see [`crate::mctx::historian_validate`] for the port map) plus the
//!   invalid-output retry in `mc-module/src/historian.rs::run_historian_firing`
//!   (line ~2610, which swaps in the repair prompt and re-runs the completion).
//!
//! ## Documented deviations from the reference
//!
//! - **The bands are derived from one knob, the reference's way.** The
//!   proactive floor (`execute − 2`) and the force band (`max(85, execute + 2)`)
//!   come from [`MctxConfig::execute_threshold_percentage`](crate::mctx::MctxConfig)
//!   through [`Budget`](crate::mctx::budget::Budget), so an operator tunes one
//!   number and every budget follows. A `soft_threshold`/`hard_threshold` pair
//!   from an older `ox.toml` is warned about and ignored.
//! - **The run row is a state, not a phase machine.** The reference's
//!   `mc_historian_pending_run` is a queue for a *separate* claimant process:
//!   `pending/claimed/reported/parked` plus lease, heartbeat and deadline
//!   columns. ox runs the historian in-process, so there is no claimant to lease
//!   to; the only durability question is "did this process die mid-completion?",
//!   which one `state` column (`running/completed/failed/aborted`) answers.
//! - **The chunk reads ox's message-granularity rows.** The reference chunks CK
//!   *blocks* (a tool result is its own `role: "tool"` message). In ox an
//!   assistant message already carries its tool calls *and* their results, so a
//!   completed tool arc can never straddle a message boundary: the "never split
//!   an arc" rule holds by construction and the reference's
//!   `absorb_tool_only` / `extract_tool_result_summaries` collapse into the
//!   assistant message's own rendering.
//! - **`clean_user_text` keeps no steering wrapper.** The reference preserves
//!   the `[SYSTEM DIRECTIVE: MAGIC-CONTEXT …]` wrapper verbatim; ox has no
//!   steering-injection transform to twin, so the port strips only
//!   `<system-reminder>` blocks and the OMO initiator marker.
//! - **The system prompt is sent bare.** The reference appends a
//!   content-language directive to it through a separate wrapper
//!   (`with_content_language_directive`, line ~142); ox has no language
//!   resolver, so there is nothing to append. The reference's repair prompt
//!   (`build_historian_repair_prompt`) is validation, i.e. Task 6b.
//! - **The trigger implements the bands and the tail budget.** The reference's
//!   `TriggerReason` also carries a commit-cluster reason and reclaim-ride
//!   suppression when tail reducers would already satisfy the target. ox fires
//!   on the derived usage bands *or* on the tail alone exceeding
//!   [`Budget::trigger`](crate::mctx::budget::Budget) (the `TailSize` floor),
//!   and the suppression gates have nothing to suppress until ox has a reclaim
//!   ride.
//! - **Synthetic rows are skipped, as the reference skips `meta.synthetic`.**
//! - **No completion timeout.** The reference leases a run to a separate
//!   claimant with a lease deadline, so a hung claimant expires. ox runs the
//!   completion in-process and has no chat-request deadline of its own (the
//!   session runner does not impose one either), so a hung transport leaves the
//!   row `running` until the process restarts and [`settle_running_runs`]
//!   sweeps it.
//! - **The historian memory block is category-grouped without ids**, exactly as
//!   the reference documents: it feeds content-based fact deduplication, never
//!   a by-id correction. ox's memory store only admits the five canonical
//!   categories, so the priority list is that closed set rather than the
//!   reference's twelve (which carry legacy categories ox cannot store).
//! - **Coverage is ordinal-derived, not anchored to a provider message id.**
//!   Publication writes the compartment's end *ordinal* into
//!   `mc_compartments.coverage_end`, which is what
//!   [`crate::mctx::compartments::coverage_end`] and the next pass read; the reference
//!   instead resolves the boundary to a message id and validates that stored
//!   ids stay ordered. [`Chunk::end_message_id`] is still carried as the run's
//!   anchor (and as the transcript's last covered message), but validation
//!   needs no id, so ox has no id-ordering rule to port.
//! - **The repair retry is bounded here, and re-prompts the same model.** The
//!   reference's invalid-output retry lives in its producer loop
//!   (`run_historian_firing`) and walks its *fallback model chain*, re-prompting
//!   the next available model with the repair prompt. ox's [`run`] drives one
//!   model, so the bound is the local [`HISTORIAN_MAX_ATTEMPTS`] and every
//!   attempt goes to the same model.
//! - **Tool-arc metadata is not reconstructed.** The reference's chunk carries
//!   `tool_only_ranges` and `completed_tool_arcs`, derived from the raw
//!   invocation/result pairs in its coverage window; ox's [`Chunk`] (Task 6a)
//!   records ordinals only, so [`run`] passes empty slices and the three rules
//!   that read them (terminal-arc healing, tool-only gap healing and the
//!   terminal-boundary arc check) are inert in production. They are still
//!   ported and unit-tested in [`crate::mctx::historian_validate`], and become
//!   live the moment the chunker exposes invocation/result pairs.
//! - **The side channels live on the run row, and facts are promoted at
//!   publish.** [`historian_validate`] parses `<facts>`/`<events>` and returns
//!   them on `ValidatedChunk`. `mc_compartments` has no session-level column
//!   for them (the reference keeps them on its assembly snapshot), so
//!   publication stores both on the producing `mc_historian_run` row. The
//!   facts are also promoted into `mc_memory` by [`crate::mctx::promotion`];
//!   completion, promotion and compartments commit together. Events have no
//!   consumer yet.

mod chunk;
mod prompt;
mod run;
mod trigger;

pub use self::chunk::*;
pub use self::prompt::*;
pub use self::run::*;
pub use self::trigger::*;
