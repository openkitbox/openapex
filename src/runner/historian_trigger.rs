//! The step-boundary historian trigger: measure the live tail, classify the
//! pressure band, and fold the oldest eligible range into compartments.

use std::sync::Arc;

use anyhow::Result;

use crate::cardinal::chat::Chat;
use crate::config::Config;
use crate::db::Db;
use crate::mctx::compartments::{self, Compartment};
use crate::mctx::historian;
use crate::mctx::memory;
use crate::mctx::render::DEFAULT_HISTORY_BUDGET_TOKENS;
use crate::runner::drain::DrainCtx;
use crate::runner::step::rows_after_with_seq;

// ---------------------------------------------------------------------------
// Historian trigger (step boundary)
// ---------------------------------------------------------------------------

/// Messages kept out of a historian run: the most recent turns stay verbatim
/// in the request, because summarizing what the model is working on right now
/// would lose exactly the detail it needs.
///
/// Deviates from the reference, which derives the fence from a token formula
/// over the context limit, the usage percentage and a reserved headroom
/// (`boundary.rs::derive_protected_tail_token_target`). ox has one fixed
/// history budget, so a message count is the same protection without the
/// formula's five constants.
const PROTECTED_TAIL_MESSAGES: usize = 6;

/// Fire the historian when the live tail has reached a pressure band.
///
/// Detached on purpose: a run is a full hidden completion (seconds), and the
/// tail still has budget headroom when a band trips — the step that notices the
/// pressure is not the one that must wait for it. The pass in the next step
/// picks up whatever compartments the run published.
///
/// Every failure here is logged and swallowed: context assembly must never fail
/// a turn (upstream's transform has the same contract).
pub(crate) fn mctx_historian(ctx: &DrainCtx, floor: i64) {
    if !ctx.config.magic_context.enabled {
        return;
    }
    let db = ctx.db.clone();
    let config = ctx.config.clone();
    let chat = Arc::clone(&ctx.chat);
    let session_id = ctx.session_id.clone();
    let project = ctx.directory.to_string_lossy().into_owned();
    tokio::spawn(async move {
        if let Err(error) = fire_historian(&db, &config, &chat, &session_id, &project, floor).await
        {
            tracing::warn!(session = %session_id, error = %error, "historian run failed");
        }
    });
}

/// The body of one firing decision: measure the tail, classify the pressure,
/// pick the chunk and run it.
async fn fire_historian(
    db: &Db,
    config: &Config,
    chat: &Arc<dyn Chat>,
    session_id: &str,
    project: &str,
    floor: i64,
) -> Result<()> {
    let rows = rows_after_with_seq(db, session_id, floor).await?;
    if rows.is_empty() {
        return Ok(());
    }
    // One pending run per session: a second step in the same burst must not
    // summarize the same range twice. The durable `running` row is the claim
    // (a crash leaves it, and `settle_running_runs` clears it at startup).
    if historian_has_running_run(db, session_id).await? {
        return Ok(());
    }
    // The whole tail's estimate, measured by the chunker itself so the pressure
    // and the budget speak the same units.
    let Some(whole) = historian::chunk(&rows, floor + 1, i64::MAX, usize::MAX) else {
        return Ok(());
    };
    let trigger = historian::trigger(
        whole.token_estimate as f64,
        DEFAULT_HISTORY_BUDGET_TOKENS,
        config.magic_context.soft_threshold,
        config.magic_context.hard_threshold,
    );
    if trigger == historian::Trigger::No {
        return Ok(());
    }

    // The protected fence: the last few ordinals never enter a run.
    let eligible_end = rows
        .len()
        .checked_sub(PROTECTED_TAIL_MESSAGES)
        .map(|cut| rows[cut].0)
        .unwrap_or(floor + 1);
    let Some(chunk) = historian::chunk(
        &rows,
        floor + 1,
        eligible_end,
        DEFAULT_HISTORY_BUDGET_TOKENS as usize,
    ) else {
        return Ok(());
    };

    let compartments = compartments::load(db, session_id).await?;
    let references: Vec<historian::ReferenceCompartment> = compartments
        .iter()
        .map(Compartment::reference_input)
        .collect();
    let blocks = historian::build_reference_blocks(session_id, chunk.start_ordinal, &references);
    let memories = memory::active_memories(db, project).await?;
    let memory_block = historian::render_historian_memory_block(&memories);
    let prompt = historian::CompartmentPromptInputs {
        seed_examples: &blocks.seed_examples,
        session_references: &blocks.session_references,
        project_memory: &memory_block,
        input_source: &chunk.text,
        memory_enabled: true,
        // The dreamer writes `<user_observations>` into the pinboard, so they
        // are only worth asking for when it can run.
        user_observations_requested: config.magic_context.dreamer_enabled,
        extraction_free: false,
    };
    let run_ctx = historian::RunCtx { db, config, chat };
    let plan = historian::RunPlan {
        session_id,
        chunk: &chunk,
        attempt: 1,
        prompt,
    };
    let outcome = historian::run(&run_ctx, &plan).await?;
    tracing::info!(
        session = %session_id,
        trigger = ?trigger,
        run = %outcome.run_id,
        covered = format!("{}..{}", chunk.start_ordinal, chunk.end_ordinal),
        compartments = outcome.compartment_ids.len(),
        "historian published"
    );
    Ok(())
}

/// Whether this session already has a run in flight. A row is `running` from
/// before its completion starts until it settles.
async fn historian_has_running_run(db: &Db, session_id: &str) -> Result<bool> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT count(*) FROM mc_historian_run
             WHERE session_id = ?1 AND state = 'running'",
        )
        .await?;
    let mut rows = stmt.query([session_id]).await?;
    Ok(match rows.next().await? {
        Some(row) => row.get::<i64>(0)? > 0,
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::task::{Context, Poll};

    use futures_core::Stream;

    use super::*;
    use crate::cardinal::ChatRequest;
    use crate::cardinal::chat::{BoxChatStream, ChatFuture};
    use crate::cardinal::stream::StreamEvent;
    use crate::config::ModelEntry;
    use crate::mctx::MctxConfig;

    const SESSION: &str = "ses_historian";

    /// One scripted completion: the text streamed, then a clean stop. A second
    /// call would need a second script, so an unexpected extra run fails loudly
    /// by returning an empty stream instead of repeating.
    struct TextChat {
        text: StdMutex<Option<String>>,
        requests: StdMutex<Vec<ChatRequest>>,
    }

    impl TextChat {
        fn new(text: &str) -> Self {
            Self {
                text: StdMutex::new(Some(text.to_owned())),
                requests: StdMutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    impl Chat for TextChat {
        fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
            self.requests
                .lock()
                .expect("requests lock")
                .push(request.clone());
            let text = self.text.lock().expect("text lock").take();
            Box::pin(async move {
                let events = match text {
                    Some(text) => {
                        VecDeque::from([Ok(StreamEvent::TextDelta(text)), Ok(StreamEvent::Done)])
                    }
                    None => VecDeque::new(),
                };
                Ok(Box::pin(EventsStream { events }) as BoxChatStream)
            })
        }
    }

    struct EventsStream {
        events: VecDeque<Result<StreamEvent, crate::cardinal::CardinalError>>,
    }

    impl Stream for EventsStream {
        type Item = Result<StreamEvent, crate::cardinal::CardinalError>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.events.pop_front())
        }
    }

    fn config() -> Config {
        Config {
            models: vec![ModelEntry {
                provider: "cardinal".to_owned(),
                id: "m".to_owned(),
                name: Some("m".to_owned()),
            }],
            default_model: Some("cardinal/m".to_owned()),
            magic_context: MctxConfig {
                enabled: true,
                dreamer_enabled: false,
                ..MctxConfig::default()
            },
            ..Config::default()
        }
    }

    /// A session row plus `count` user messages, `bulk_chars` of text in each.
    async fn seed_session(db: &Db, count: i64, bulk_chars: usize) {
        let conn = db.acquire().await.unwrap();
        conn.execute(
            "INSERT INTO session_v2
             (id, project_id, slug, directory, version, time_created, time_updated)
             VALUES (?1, '/p', 's', '/p', '2', 0, 0)",
            [SESSION],
        )
        .await
        .unwrap();
        for seq in 1..=count {
            let text = "x".repeat(bulk_chars);
            let data = serde_json::json!({
                "id": format!("msg_{seq}"),
                "type": "user",
                "time": { "created": 0 },
                "text": text,
            })
            .to_string();
            conn.execute(
                "INSERT INTO session_message
                 (id, session_id, type, seq, time_created, time_updated, data)
                 VALUES (?1, ?2, 'user', ?3, 0, 0, ?4)",
                (format!("msg_{seq}"), SESSION, seq, data),
            )
            .await
            .unwrap();
        }
    }

    fn valid_output(start: i64, end: i64) -> String {
        format!(
            "<output><compartments>\
<compartment start=\"{start}\" end=\"{end}\" title=\"Bulk\" episode_type=\"feature\" importance=\"60\">\
<p1>P1 body</p1><p2>P2 body</p2><p3>P3 body</p3><p4>P4 body</p4>\
</compartment></compartments>\
<facts></facts><events></events></output>"
        )
    }

    /// A tail past the soft band fires one run and publishes a compartment.
    #[tokio::test]
    async fn pressure_above_the_soft_band_fires_one_run() {
        let db = Db::open_in_memory().await.unwrap();
        // 8 rows of 40k characters ≈ 80k estimated tokens: past the 70% band of
        // the 60k budget, and enough rows that the fence (6) leaves two.
        seed_session(&db, 8, 40_000).await;
        let config = config();
        let chat: Arc<dyn Chat> = Arc::new(TextChat::new(&valid_output(1, 2)));

        fire_historian(&db, &config, &chat, SESSION, "/p", 0)
            .await
            .expect("a published run");

        let compartments = compartments::load(&db, SESSION).await.unwrap();
        assert_eq!(compartments.len(), 1, "one compartment published");
        assert_eq!(compartments[0].coverage_end, "2", "the fence held");
    }

    /// Below the soft band nothing runs and no completion is issued.
    #[tokio::test]
    async fn a_quiet_tail_issues_no_completion() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, 3, 10).await;
        let config = config();
        let mock = Arc::new(TextChat::new(&valid_output(1, 2)));
        let chat: Arc<dyn Chat> = mock.clone();

        fire_historian(&db, &config, &chat, SESSION, "/p", 0)
            .await
            .expect("no work is not an error");

        assert!(mock.requests().is_empty(), "no completion for a quiet tail");
        assert!(compartments::load(&db, SESSION).await.unwrap().is_empty());
    }

    /// A run already in flight is the claim: a second firing does nothing.
    #[tokio::test]
    async fn a_running_row_blocks_a_second_firing() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, 8, 40_000).await;
        let conn = db.acquire().await.unwrap();
        conn.execute(
            "INSERT INTO mc_historian_run (id, session_id, chunk_ref, attempt, state, created_at)
             VALUES ('run_1', ?1, '1..2', 1, 'running', 0)",
            [SESSION],
        )
        .await
        .unwrap();
        drop(conn);

        let config = config();
        let mock = Arc::new(TextChat::new(&valid_output(1, 2)));
        let chat: Arc<dyn Chat> = mock.clone();
        fire_historian(&db, &config, &chat, SESSION, "/p", 0)
            .await
            .expect("a skipped firing is not an error");

        assert!(mock.requests().is_empty(), "one run at a time");
        assert!(compartments::load(&db, SESSION).await.unwrap().is_empty());
    }
}
