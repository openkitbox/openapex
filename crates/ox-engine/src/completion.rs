//! Tool-free hidden completions. Semantic completion is independent of SSE
//! framing; callers retain their deadlines, publication policy and usage owner.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use ox_llm::cardinal::chat::Chat;
use ox_llm::cardinal::stream::StreamEvent;
use ox_llm::cardinal::wire::{ChatRequest, Usage};
use ox_schema::id::now_millis;

/// Successful text-only finishes supported by the current two wire protocols.
/// In particular, Messages `pause_turn` requires continuation, not publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextFinish {
    Stop,
    EndTurn,
    StopSequence,
}

impl TextFinish {
    fn parse(reason: &str) -> Result<Self> {
        match reason {
            "stop" => Ok(Self::Stop),
            "end_turn" => Ok(Self::EndTurn),
            "stop_sequence" => Ok(Self::StopSequence),
            _ => anyhow::bail!("hidden completion has unsuccessful finish: {reason}"),
        }
    }
}

pub(crate) struct TextCompletion {
    pub text: String,
    pub finish: TextFinish,
    pub usage: Option<Usage>,
}

/// Collect a semantic text completion, moving each delta to its optional sink.
/// The sink may publish ephemeral progress, never a completed checkpoint.
/// Drain past Finish for trailing usage and conflicting terminal events; Done
/// alone never establishes success. No timeout is reset or installed here.
pub(crate) async fn collect_with_deltas<Delta, Pending>(
    chat: &Arc<dyn Chat>,
    request: &ChatRequest,
    mut on_delta: Delta,
) -> Result<TextCompletion>
where
    Delta: FnMut(String) -> Pending,
    Pending: Future<Output = Result<()>>,
{
    let mut stream = chat
        .stream_chat(request)
        .await
        .map_err(observe_error)
        .context("hidden completion request failed")?;
    let mut text = String::new();
    let mut finish = None;
    let mut usage = None;
    while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        match event
            .map_err(observe_error)
            .context("hidden completion stream failed")?
        {
            StreamEvent::TextDelta(delta) => {
                text.push_str(&delta);
                on_delta(delta).await?;
            }
            StreamEvent::Finish { reason } => {
                let seen = TextFinish::parse(&reason)?;
                if finish.is_some_and(|previous| previous != seen) {
                    anyhow::bail!("hidden completion has conflicting finishes");
                }
                finish = Some(seen);
            }
            StreamEvent::ToolCallDelta { .. } => {
                anyhow::bail!("hidden completion returned a tool call");
            }
            StreamEvent::Usage(reported) => usage = Some(reported),
            StreamEvent::ReasoningDelta(_) | StreamEvent::Done => {}
        }
    }
    let finish = finish.context("hidden completion ended without a successful finish")?;
    Ok(TextCompletion {
        text,
        finish,
        usage,
    })
}

fn observe_error(error: ox_llm::error::ChatError) -> ox_llm::error::ChatError {
    crate::mctx::quota::observe(&error, now_millis());
    error
}

/// Hidden callers without a session usage ledger keep their cache-usage log at
/// this boundary; compaction uses its own delta/publication owner instead.
pub(crate) async fn collect_text(
    chat: &Arc<dyn Chat>,
    request: &ChatRequest,
    purpose: &'static str,
) -> Result<String> {
    let completion = collect_with_deltas(chat, request, |_| std::future::ready(Ok(()))).await?;
    if let Some(usage) = completion.usage {
        let read = usage.cached_tokens.unwrap_or(0);
        tracing::info!(
            purpose,
            prompt_tokens = usage.prompt_tokens,
            cache_read = read,
            cache_uncached = usage.prompt_tokens.saturating_sub(read),
            "OX_COMPLETION_USAGE"
        );
    }
    tracing::debug!(purpose, finish = ?completion.finish, "hidden completion finished");
    Ok(completion.text)
}
