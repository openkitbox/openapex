//! Whole-request estimates, separate from provider-reported usage and mctx policy.
//! Pure measurement borrows the final paired request. `apply` owns logging and
//! wiring; unsupported media disables input admission, never output limits.

use ox_llm::cardinal::wire::{ChatMessage, ChatRequest, UserContent, UserPart};
use ox_llm::model_catalog::RuntimeLimits;

/// Assembler-owned content attribution; history is the remainder after pairing.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Layers {
    pub system_static: usize,
    pub frozen: usize,
    pub m0: usize,
    pub m1: usize,
    pub delta: usize,
    pub recall: usize,
    pub final_step: usize,
}

#[derive(Debug)]
pub(crate) struct RequestBill {
    pub serialized_bytes: usize,
    pub content_bytes: usize,
    pub schema_bytes: usize,
    pub framing_bytes: usize,
    pub local_tokens: usize,
    pub estimated_input: u64,
    pub guarded_input: u64,
    pub unsupported_media: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("request needs compaction: {0}")]
pub(crate) struct NeedsCompaction(pub &'static str);

pub(crate) fn measure(request: &ChatRequest, ratio: f64) -> Result<RequestBill, serde_json::Error> {
    let mut content_bytes = 0usize;
    let mut unsupported_media = false;
    for message in &request.messages {
        match message {
            ChatMessage::System { content } | ChatMessage::Tool { content, .. } => {
                content_bytes += content.len()
            }
            ChatMessage::User { content } => match content {
                UserContent::Text(text) => content_bytes += text.len(),
                UserContent::Parts(parts) => {
                    for part in parts {
                        match part {
                            UserPart::Text { text } => content_bytes += text.len(),
                            UserPart::Image { .. } => unsupported_media = true,
                        }
                    }
                }
            },
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                content_bytes += content.as_ref().map_or(0, String::len);
                content_bytes += reasoning_content.as_ref().map_or(0, String::len);
                for call in tool_calls.iter().flatten() {
                    content_bytes += call.function.name.len() + call.function.arguments.len();
                }
            }
        }
    }
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut schema = Counter(0);
    if !request.tools.is_empty() {
        serde_json::to_writer(&mut schema, request.tools.as_slice())?;
    }
    let mut serialized = Counter(0);
    serde_json::to_writer(&mut serialized, request)?;
    let serialized_bytes = serialized.0;
    let local_tokens = crate::mctx::tokens::heuristic_bytes(serialized_bytes);
    let ratio = if ratio.is_finite() && ratio > 0.0 {
        ratio
    } else {
        1.0
    };
    let estimated_input = (local_tokens as f64 * ratio).ceil() as u64;
    Ok(RequestBill {
        serialized_bytes,
        content_bytes,
        schema_bytes: schema.0,
        framing_bytes: serialized_bytes.saturating_sub(content_bytes + schema.0),
        local_tokens,
        estimated_input,
        guarded_input: (estimated_input as f64 * 1.15).ceil() as u64,
        unsupported_media,
    })
}

pub(crate) fn output(
    bill: &RequestBill,
    limits: RuntimeLimits,
    requested: Option<u64>,
) -> Result<Option<u64>, NeedsCompaction> {
    let mut cap = limits.output.tokens();
    if !bill.unsupported_media {
        if limits
            .input
            .tokens()
            .is_some_and(|input| bill.guarded_input > input)
        {
            return Err(NeedsCompaction("input limit exceeded"));
        }
        if let Some(window) = limits.shared_context.tokens() {
            let usable = window / 100 * 99 + window % 100 * 99 / 100;
            let room = usable
                .checked_sub(bill.guarded_input)
                .filter(|room| *room > 0)
                .ok_or(NeedsCompaction("no positive output room"))?;
            cap = Some(cap.map_or(room, |output| output.min(room)));
        }
    }
    let selected = match (cap, requested) {
        (Some(cap), Some(requested)) => Some(cap.min(requested)),
        (cap, requested) => cap.or(requested),
    };
    if selected == Some(0) {
        return Err(NeedsCompaction("zero output cap"));
    }
    Ok(selected)
}

pub(crate) fn apply(
    ctx: &super::drain::DrainCtx,
    request: &mut ChatRequest,
    provider: &str,
    layers: Layers,
) -> anyhow::Result<()> {
    let limits = ctx
        .model_catalog
        .runtime_limits_for(Some(provider), &request.model)?;
    let ratio = crate::mctx::calibration::ratio_for(
        &ctx.calibrations,
        &ctx.session_id,
        provider,
        &request.model,
    );
    let bill = measure(request, ratio)?;
    let selected = output(&bill, limits, request.max_tokens);
    let attributed = layers.system_static
        + layers.frozen
        + layers.m0
        + layers.m1
        + layers.delta
        + layers.recall
        + layers.final_step;
    tracing::debug!(model = %request.model, ?limits, ratio, ?layers,
        history_bytes = bill.content_bytes.saturating_sub(attributed),
        serialized_bytes = bill.serialized_bytes, content_bytes = bill.content_bytes, schema_bytes = bill.schema_bytes,
        escaping_framing_control_bytes = bill.framing_bytes,
        local_tokens = bill.local_tokens, estimated_input = bill.estimated_input,
        guarded_input = bill.guarded_input, unsupported_media = bill.unsupported_media,
        selected_output = ?selected, "request geometry estimate (not reported usage)");
    request.max_tokens = selected?;
    Ok(())
}
