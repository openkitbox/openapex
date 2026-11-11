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
//! call "would surface as an empty answer"). Deliberate safety deviation:
//! tool calls and incomplete completions return an error rather than an empty
//! or truncated answer.
//!
//! Instruction decisions share the live path's pure preview: frozen initial
//! bytes plus any currently needed delta, without publishing or applying it.
//! Equality is defined only for the same sources/history/head snapshot; the
//! caller's final prompt and live automatic recall intentionally differ.

use std::sync::Arc;

use anyhow::Result;

use ox_llm::cardinal::wire::{ChatMessage, ChatRequest};

use super::Inner;
use super::drain::DrainCtx;
use super::instructions;

/// Answer `prompt` from the session's current context. Read-only: the session
/// transcript, the instruction baseline and the event log are untouched.
pub(super) async fn run(inner: &Inner, session_id: &str, prompt: &str) -> Result<String> {
    let svc = inner.live.current();
    let (_agent, model) =
        super::resolve_selection(&inner.db, session_id, &svc.config, &svc.model_catalog).await?;
    let agent = super::session_agent(&inner.db, session_id).await;
    let ctx = DrainCtx {
        tasks: inner.tasks.clone(),
        session_id: session_id.to_owned(),
        agent,
        directory: super::session_directory(&inner.db, session_id, &inner.directory).await,
        home: inner.home.clone(),
        config: Arc::clone(&svc.config),
        db: inner.db.clone(),
        bus: inner.bus.clone(),
        chat: Arc::clone(&svc.chat),
        tools: svc.tools_for(agent),
        model_catalog: Arc::clone(&svc.model_catalog),
        mcp: Arc::clone(&svc.mcp),
        live: Arc::clone(&inner.live),
        generation: svc.generation,
        instruction_baselines: Arc::clone(&inner.instruction_baselines),
        calibrations: Arc::clone(&inner.calibrations),
        cache_ledger: Arc::clone(&inner.cache_ledger),
        grants: Arc::clone(&inner.grants),
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

    let system_static = ctx.system_static();
    let observed = ctx.observe_sources().await?;
    let budget = ctx.budget_for(&model, system_static.len());
    let (head_m0, head_m1, rows) = ctx.preview_context(budget, &model).await?;

    // Shared admission rejects BudgetExceeded before constructing a preview;
    // unlike transient Unavailable it cannot fall back to prior instructions.
    let preview = {
        let baselines = super::lock(&ctx.instruction_baselines);
        instructions::preview(&observed, &rows, baselines.get(session_id))?
    };
    let mut messages = super::step::build_chat_messages(
        &system_static,
        &preview.initial,
        &[head_m0.as_deref(), head_m1.as_deref()],
        &rows,
    );
    let update =
        (!preview.deltas.is_empty()).then(|| instructions::render_updates(&preview.deltas));
    let layers = super::request_budget::Layers {
        system_static: system_static.len(),
        frozen: preview.initial.len(),
        m0: head_m0.as_ref().map_or(0, String::len),
        m1: head_m1.as_ref().map_or(0, String::len),
        delta: update.as_ref().map_or(0, String::len),
        ..Default::default()
    };
    instructions::insert_update(&mut messages, update);
    messages.push(ChatMessage::user(prompt));

    let mut request = ChatRequest::new(model.id.clone(), messages);
    request.tools = ctx.tools.definitions_shared();
    request.apply_reasoning_effort(model.variant.as_deref());
    super::request_budget::apply(&ctx, &mut request, &model.provider_id, layers)?;
    crate::completion::collect_text(&ctx.chat, &request, Some(&model.provider_id), "generate").await
}
