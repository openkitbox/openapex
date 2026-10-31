//! Session generate — ported from upstream
//! `packages/core/src/session/generate.ts` (`SessionGenerate.generate`):
//! "Generates text from current Session context without mutating the Session."
//!
//! This is the server half of the TUI's `/btw` side question
//! (`feature-plugins/prompt/btw.tsx` → `client.session.generate`). It is a
//! one-shot sibling of a step: the request carries the context layers a normal
//! step sends (system[0] static, system[1] keyed instructions, the
//! magic-context head, the wire tail) plus the caller's prompt as a final user
//! message. Nothing is persisted and no event is published — the model answers
//! from what is already there. No tool loop runs (upstream notes that a tool
//! call "would surface as an empty answer"), so a tool-only completion yields
//! `""`.
//!
//! Deliberate deviation from upstream, recorded here: the instruction layer is
//! the baseline render of the *current* sources rather than
//! `SessionHistory.preview`'s initial-plus-update pair. ox keeps no per-session
//! instruction state on disk, and the visible wire window already carries every
//! delta row (`build_chat_messages` folds them in as system messages), so only
//! the frozen initial block needs recomputing; nothing is hashed, diffed or
//! recorded here.

use std::sync::Arc;

use anyhow::Result;

use ox_llm::cardinal::wire::{ChatMessage, ChatRequest};

use super::Inner;
use super::drain::DrainCtx;

/// Answer `prompt` from the session's current context. Read-only: the session
/// transcript, the instruction baseline and the event log are untouched.
pub(super) async fn run(inner: &Inner, session_id: &str, prompt: &str) -> Result<String> {
    let svc = inner.live.current();
    let (_agent, model) =
        super::resolve_selection(&inner.db, session_id, &svc.config, &svc.model_catalog).await?;
    let ctx = DrainCtx {
        session_id: session_id.to_owned(),
        kind: super::services::SessionKind::Main,
        directory: inner.directory.clone(),
        home: inner.home.clone(),
        config: Arc::clone(&svc.config),
        db: inner.db.clone(),
        bus: inner.bus.clone(),
        chat: Arc::clone(&svc.chat),
        tools: Arc::clone(&svc.tools),
        model_catalog: Arc::clone(&svc.model_catalog),
        mcp: Arc::clone(&svc.mcp),
        live: Arc::clone(&inner.live),
        generation: svc.generation,
        instruction_baselines: Arc::clone(&inner.instruction_baselines),
        calibrations: Arc::clone(&inner.calibrations),
        // The session handle's cached git-toplevel anchor (the same slot the
        // busy period's steps use); no live registry entry keeps a fresh,
        // unused cache.
        instruction_root: match inner.sessions.get(session_id) {
            Some(handle) => handle.instruction_root.clone(),
            None => super::InstructionRootCache::default(),
        },
        fold_in_flight: match inner.sessions.get(session_id) {
            Some(handle) => Arc::clone(&handle.fold_in_flight),
            None => Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    };

    let budget = crate::mctx::budget_for(&ctx.config, &ctx.model_catalog, &model.id);
    let (head_m0, head_m1, floor) = super::step::mctx_head(&ctx, budget).await;
    let rows = if ctx.config.magic_context.enabled {
        super::step::load_overlaid_tail(&ctx.db, &ctx.session_id, floor).await?
    } else {
        super::step::load_history_after(&ctx.db, &ctx.session_id, floor).await?
    };

    // Directory and root must share one namespace: the chain's
    // component-wise prefix check degenerates to leaf-only when the
    // directory still carries symlink components the canonical root lost.
    let (directory, project_root) = ctx.instruction_root.get_or_compute(&ctx.directory);
    let observed = crate::prompt::gather(
        &directory,
        &ctx.home,
        project_root.as_deref(),
        ctx.mcp.instructions_block().as_deref(),
    );
    let guidance: Vec<(&str, Option<&'static str>)> = ctx.tools.guidances().collect();
    let system_static =
        crate::prompt::system_static(&guidance, crate::prompt::WorkflowProfile::Parent);
    let instructions = super::instructions::baseline(&observed).initial_render;
    let mut messages = super::step::build_chat_messages(
        &system_static,
        &instructions,
        &[head_m0.as_deref(), head_m1.as_deref()],
        &rows,
    );
    messages.push(ChatMessage::user(prompt));

    let mut request = ChatRequest::new(model.id.clone(), messages);
    request.tools = ctx.tools.definitions_shared();
    request.apply_reasoning_effort(model.variant.as_deref());
    crate::mctx::historian::collect_completion(&ctx.chat, &request).await
}
