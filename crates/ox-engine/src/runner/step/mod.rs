//! One streaming step, ported from `packages/core/src/session/runner/step.ts`.
//!
//! Event choreography (matching upstream):
//!
//! - `session.step.started` is published lazily: on the first provider event
//!   of the step, or before a retry is scheduled — never for a stream that
//!   yields nothing (an empty connect-failure sequence retries cleanly and
//!   only the final failure starts the assistant row).
//! - Text/reasoning deltas stream as ephemeral events between
//!   `*.started`/`*.ended` pairs, each block with its own zero-based
//!   ordinal.
//! - Tool calls accumulate per stream index from
//!   `StreamEvent::ToolCallDelta` fragments: `session.tool.input.started`
//!   fires once the call is identified (id + name known), `*.input.delta`
//!   per fragment (ephemeral), `*.input.ended` when the arguments complete.
//!   Cardinal completes a call's arguments at `finish_reason`, so the
//!   settlement — `session.tool.called` plus the spawned execution — runs
//!   at `Finish` while the stream may still be open (upstream: tool
//!   execution is concurrent with the stream, llm.ts + step.ts).
//! - Each execution settles its tool part durably: `session.tool.success`
//!   or `session.tool.failed` (status `completed` / `error`). An
//!   interrupted in-flight execution settles as failed/`aborted`.
//! - On a clean finish: `session.step.streamed`, tool settlement, then
//!   `session.step.ended` with upstream's token shape (`input` = prompt
//!   minus cached, `output` = completion, `reasoning`, `cache.read` =
//!   cached, `cache.write` = 0). When the stream carries no usage, `input`
//!   is an estimate from the sent request's size (bytes/4 x the session
//!   calibration ratio) and the other fields are 0. Deviation: the estimate
//!   lives in the same field; the row carries `tokens.estimated = true` (ox
//!   extension, absent for reported usage); bytes/4 under-counts CJK,
//!   so folds may trigger slightly late; pre-existing zero rows are not
//!   rewritten. The estimate also lands on interrupted and incomplete-stream
//!   rows (their usage chunk never arrives, even for models that do report
//!   it), so those turns now add a prompt-size estimate (no cache split) to
//!   `session_v2.tokens_input` and the stats sums; a failed attempt that
//!   never opened a stream keeps `tokens: None`.
//! - An interruption after the step started settles the step with
//!   `session.step.failed` carrying the upstream `aborted` error, after
//!   failing unsettled tool parts.
//! - Failures retry per [`retry::classify`](super::retry::classify) / [`retry::Backoff`](super::retry::Backoff)
//!   (upstream's SessionRunnerRetry publishes `session.retry.scheduled`):
//!   network-class errors retry until recovery or interrupt, the bounded
//!   class up to `MAX_BOUNDED_RETRIES` consecutive failures, the never
//!   class fails immediately. The delay is computed once and used for both
//!   the event's `at` and the sleep. A 400 context-length overflow before
//!   any output surfaces as [`StepOutcome::Overflow`] so the drain can
//!   compact and retry once (upstream `Outcome.Compacted`).
//! - A mid-stream failure after published output never transparently
//!   retries: only an *interrupted stream* continues (upstream
//!   `Outcome.Continue`) — a transport read failure, or a stream that ends
//!   without a recognized finish reason. The partial assistant settles
//!   `session.step.failed` with the stream error (upstream `failAssistant`:
//!   no `step.ended`, finish `error`, open tool inputs fail with the same
//!   error), the retry announcement follows, and the continuation prompt
//!   (llm.ts `CONTINUE_AFTER_INCOMPLETE_STREAM`) is published as a
//!   synthetic user message before the attempt re-runs against a fresh
//!   assistant row.
//!
//! The context assembly lives in one named function
//! ([`build_chat_messages`]) so the e2e suite can trust the mapping.

mod attempt;
mod content;
mod context;
mod error;
mod history;
mod request;
mod stream;
mod tool;
pub(crate) mod usage;

pub(crate) use attempt::{StepOutcome, StepParams, fail_step, run};
pub(crate) use content::user_prompt_text;
pub(crate) use context::{build_chat_messages, compaction_boundary, mctx_head, tool_input};
pub(crate) use history::{
    decode_row, last_assistant_text, load_history, load_history_after, load_overlaid_tail,
    load_overlaid_tail_with_ordinals, recent_turns, rows_after_with_seq, settled_tail,
};
