//! Session title generation, ported from upstream
//! `packages/core/src/session/title.ts` (`SessionTitle.generate`).
//!
//! Upstream runs this as a dedicated hidden agent ("title") with a
//! model-selection chain (agent model → provider's small model → primary) and
//! an exact publish-sequence guard. Per the recorded decision (see the module
//! doc in `.ox/plans`, session note #6), ox's port is deliberately
//! smaller:
//!
//! - **No dedicated title agent.** ox has no agent registry; the title
//!   prompt rides as a plain system message, exactly as the compaction
//!   request does with `COMPACTION_SYSTEM`. Tools are never attached, so the
//!   agent's `permissions: all-deny` posture holds by construction.
//! - **The session's primary model generates the title.** No small-model
//!   family preference and no minimal-reasoning variant lookup — the catalog
//!   has no family concept, and the variant list is an upstream model
//!   registry feature.
//! - **The rename guard is a re-read, not a sequence assertion.** Before
//!   publishing, the stored title is compared to the title snapshot taken
//!   when generation started; a changed value means a manual rename (or a
//!   completed generation) won the race and the result is dropped — the same
//!   outcome as upstream's `titleChanged` defect, without touching the event
//!   bus's commit API.
//!
//! Triggers live outside this module (mirroring upstream):
//! - automatic: the drain spawns one generation when a root session's first
//!   steer is promoted with no title yet (`drain.rs`);
//! - manual: `session.update` with an empty `title` spawns one
//!   (`api/session.rs`).

use anyhow::Result;

use crate::runner::Chat;
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_llm::cardinal::wire::{ChatMessage, ChatRequest};
use ox_schema::event::SessionEventData;
use ox_schema::message::{AssistantContent, Message};
use ox_schema::support::ModelRef;
use std::sync::Arc;

/// Upstream `PROMPT_TITLE` (plugin/agent.ts:19) verbatim: the title agent's
/// system prompt.
pub(crate) const PROMPT_TITLE: &str = "You are a title generator. You output ONLY a thread title. Nothing else.

<task>
Generate a brief title that would help the user find this conversation later.

Follow all rules in <rules>
Use the <examples> so you know what a good title looks like.
Your output must be:
- A single line
- <=50 characters
- No explanations
</task>

<rules>
- you MUST use the same language as the user message you are summarizing
- Title must be grammatically correct and read naturally - no word salad
- Never include tool names in the title (e.g. \"read tool\", \"bash tool\", \"edit tool\")
- Focus on the main topic or question the user needs to retrieve
- Vary your phrasing - avoid repetitive patterns like always starting with \"Analyzing\"
- When a file is mentioned, focus on WHAT the user wants to do WITH the file, not just that they shared it
- Keep exact: technical terms, numbers, filenames, HTTP codes
- Remove: the, this, my, a, an
- Never assume tech stack
- Never use tools
- NEVER respond to questions, just generate a title for the conversation
- The title should NEVER include \"summarizing\" or \"generating\" when generating a title
- DO NOT SAY YOU CANNOT GENERATE A TITLE OR COMPLAIN ABOUT THE INPUT
- Always output something meaningful, even if the input is minimal.
- If the user message is short or conversational (e.g. \"hello\", \"lol\", \"what's up\", \"hey\"):
  -> create a title that reflects the user's tone or intent (such as Greeting, Quick check-in, Light chat, Intro message, etc.)
</rules>

<examples>
\"debug 500 errors in production\" -> Debugging production 500 errors
\"refactor user service\" -> Refactoring user service
\"why is app.js failing\" -> app.js failure investigation
\"implement rate limiting\" -> Rate limiting implementation
\"how do I connect postgres to my API\" -> Postgres API connection
\"best practices for React hooks\" -> React hooks best practices
\"@src/credential.ts can you add refresh token support\" -> Credential refresh token support
\"@utils/parser.ts this is broken\" -> Parser bug fix
\"look at @config.json\" -> Config review
\"@App.tsx add dark mode toggle\" -> Dark mode toggle in App
</examples>";

/// Upstream `MAX_LENGTH`: the generated title is truncated to 100 characters.
const MAX_LENGTH: usize = 100;
/// Upstream `MAX_CONTEXT_LENGTH`: the whole title input stays within 8,000
/// characters.
const MAX_CONTEXT_LENGTH: usize = 8_000;
/// Upstream `MAX_FIRST_MESSAGE_LENGTH`: the first user message alone is
/// capped at 2,000 characters.
const MAX_FIRST_MESSAGE_LENGTH: usize = 2_000;

/// Upstream `truncate`: 100 chars max, `...` on overflow.
fn truncate(value: &str) -> String {
    if value.chars().count() <= MAX_LENGTH {
        return value.to_owned();
    }
    // Rust slices must land on char boundaries; `chars()` keeps the cut at
    // exactly MAX_LENGTH - 3 characters regardless of the byte width.
    let mut out: String = value.chars().take(MAX_LENGTH - 3).collect();
    out.push_str("...");
    out
}

/// Upstream `isUntitled`: ox sessions start untitled (`title IS NULL`) — the
/// created event carries no timestamp fallback, so the upstream regex branch
/// can never match. A title the user set by hand is NOT untitled.
pub(crate) async fn is_untitled(db: &Db, session_id: &str) -> Result<bool> {
    Ok(load_title(db, session_id).await?.is_none())
}

/// The session's stored title (None = untitled).
async fn load_title(db: &Db, session_id: &str) -> Result<Option<String>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT title FROM session_v2 WHERE id = ?1")
        .await?;
    let row = ox_db::optional(&mut stmt, [session_id]).await?;
    Ok(row
        .map(|row| row.get::<Option<String>>(0))
        .transpose()?
        .flatten())
}

/// The session's root status: title generation is root-only upstream
/// (`llm.ts:177` gates on `!selected.session.parentID`).
pub(crate) async fn is_root_session(db: &Db, session_id: &str) -> Result<bool> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT parent_id FROM session_v2 WHERE id = ?1")
        .await?;
    let row = ox_db::optional(&mut stmt, [session_id]).await?;
    Ok(row
        .map(|row| row.get::<Option<String>>(0))
        .transpose()?
        .flatten()
        .is_none())
}

/// The model that generates the title: the session's selection, exactly as
/// `drain.rs` resolves it for the turn (no small-model preference — see the
/// module doc).
async fn title_model(
    db: &Db,
    session_id: &str,
    config: &ox_core::config::Config,
    catalog: &ox_llm::model_catalog::ModelCatalog,
) -> Result<ModelRef> {
    let (_, model) = super::resolve_selection(db, session_id, config, catalog).await?;
    Ok(model)
}

/// The title input: upstream `generate`'s text construction.
///
/// Untitled session → the first user message alone (≤ 2,000 chars). Titled
/// session → `Original request:` + the first message, then `Recent
/// conversation:` + the remaining user/assistant text (≤ 8,000 chars total).
/// When the recent slice is empty, the input is the original request alone.
async fn title_input(db: &Db, session_id: &str, untitled: bool) -> Result<Option<String>> {
    let first = first_user_message(db, session_id).await?;
    let Some((first_id, first_text)) = first else {
        return Ok(None);
    };
    if untitled {
        return Ok(Some(
            first_text.chars().take(MAX_FIRST_MESSAGE_LENGTH).collect(),
        ));
    }
    let original: String = format!(
        "Original request:\n{}",
        first_text
            .chars()
            .take(MAX_FIRST_MESSAGE_LENGTH)
            .collect::<String>()
    );
    // `load_history_after` reads the tail above the magic-context floor; the
    // title input wants the whole visible conversation, so the floor is 0.
    let messages = super::step::load_history_after(db, session_id, 0).await?;
    let mut recent = String::new();
    for message in messages {
        match message {
            Message::User {
                ref id, ref prompt, ..
            } if *id != first_id => {
                let text = prompt.text.trim();
                if !text.is_empty() {
                    push_line(&mut recent, &format!("User: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let text = assistant
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContent::Text { text, .. } => {
                            let trimmed = text.trim();
                            (!trimmed.is_empty()).then(|| trimmed.to_owned())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    push_line(&mut recent, &format!("Assistant: {text}"));
                }
            }
            _ => {}
        }
    }
    if recent.is_empty() {
        return Ok(Some(original));
    }
    let prefix = format!("{original}\n\nRecent conversation:\n");
    // Tail-keep: the newest exchanges matter most to a re-generation.
    let keep = MAX_CONTEXT_LENGTH.saturating_sub(prefix.chars().count());
    let recent: String = recent
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Ok(Some(format!("{prefix}{recent}")))
}

/// Append one `Label: text` line to the recent-conversation block, separated
/// by a blank line (upstream joins with `\n\n`).
fn push_line(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(line);
}

/// The earliest user message in the session (`SessionHistory.firstUserMessage`).
/// The row's `data` column omits `id`/`type` (they live in their own
/// columns), so both are reattached before decoding.
async fn first_user_message(db: &Db, session_id: &str) -> Result<Option<(String, String)>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT id, data FROM session_message WHERE session_id = ?1 AND type = 'user'
                 ORDER BY seq ASC LIMIT 1",
        )
        .await?;
    let row = ox_db::optional(&mut stmt, [session_id]).await?;
    let Some(row) = row else { return Ok(None) };
    let id: String = row.get(0)?;
    let data: String = row.get(1)?;
    let mut value: serde_json::Value = serde_json::from_str(&data)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("id".to_owned(), serde_json::Value::String(id.clone()));
        map.insert(
            "type".to_owned(),
            serde_json::Value::String("user".to_owned()),
        );
    }
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(Some((id, text)))
}

/// Upstream `attempt`'s extraction: the first non-blank trimmed line of the
/// completion.
fn extract_title(text: &str) -> Option<String> {
    text.split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Generate (or regenerate) the session title. All failures are silent: title
/// generation is advisory background work, so a transport error or a vanished
/// session logs at most and never surfaces to the turn or the HTTP request
/// that spawned it (upstream's `Effect.orElseSucceed` posture on
/// `store.context`).
pub async fn generate(
    db: Db,
    bus: EventBus,
    chat: Arc<dyn Chat>,
    config: Arc<ox_core::config::Config>,
    catalog: Arc<ox_llm::model_catalog::ModelCatalog>,
    session_id: String,
) {
    if let Err(error) = generate_inner(&db, &bus, chat, &config, &catalog, &session_id).await {
        tracing::debug!(session = %session_id, error = %error, "title generation failed");
    }
}

async fn generate_inner(
    db: &Db,
    bus: &EventBus,
    chat: Arc<dyn Chat>,
    config: &ox_core::config::Config,
    catalog: &Arc<ox_llm::model_catalog::ModelCatalog>,
    session_id: &str,
) -> Result<()> {
    let snapshot = load_title(db, session_id).await?;
    let untitled = snapshot.is_none();
    let Some(text) = title_input(db, session_id, untitled).await? else {
        return Ok(());
    };
    let model = title_model(db, session_id, config, catalog).await?;
    // Upstream's request is tools-free and single-shot (the agent's deny-all
    // permissions); `ChatRequest::new` already advertises no tools.
    // The session's primary model generates the title (the same selection
    // `drain.rs` resolves), so the variant rides the hidden completion too.
    let mut request = ChatRequest::new(
        model.id.clone(),
        vec![ChatMessage::system(PROMPT_TITLE), ChatMessage::user(text)],
    );
    request.apply_reasoning_effort(model.variant.as_deref());
    let response = crate::mctx::historian::collect_completion(&chat, &request).await?;
    let Some(title) = extract_title(&response) else {
        return Ok(());
    };
    let title = truncate(&title);
    // The race guard: a concurrent rename (manual or another generation) that
    // landed while the completion was in flight wins; the stale result is
    // dropped. `current.title !== session.title` upstream, simplified here to
    // the snapshot comparison (ox has no extra `titleChanged` defect).
    let current = load_title(db, session_id).await?;
    if current != snapshot || current.as_deref() == Some(title.as_str()) {
        return Ok(());
    }
    bus.publish(SessionEventData::SessionRenamed {
        session_id: session_id.to_owned(),
        title,
    })
    .await?;
    Ok(())
}
