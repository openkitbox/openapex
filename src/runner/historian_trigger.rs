//! The step-boundary historian trigger: measure the live tail, classify the
//! pressure band, and fold the oldest eligible range into compartments.
//!
//! Two properties are worth knowing when reading the logs. First, the chunker
//! admits an oversize block (a single message bigger than the request budget)
//! only as the first block of a run, so a huge message that follows small ones
//! costs one cheap run to reach it and the next run folds it. Second, the
//! context fence is absolute: when the *only* foldable block is bigger than
//! what the fence leaves room for, no run starts — folding it would leave the
//! model almost no verbatim history, which is worse than waiting. Such a tail
//! usually becomes foldable as it grows (the fence keeps a fixed share of the
//! end, and the fence shrinks as usage rises); a message that dominates the
//! tail can outlast that, and then the overflow path is the fallback. That case
//! logs a warning, because it is the one shape where mctx cannot make progress.
//!
//! Every number here comes from the session's [`Budget`], derived at the step
//! boundary from the model catalogue — never a module constant. A 1M-window
//! model folds in 8.5k-token tail steps and one historian run reads at most a
//! quarter of *its own* window; a 32k model folds much sooner.

use std::sync::Arc;

use anyhow::Result;

use crate::cardinal::chat::Chat;
use crate::config::Config;
use crate::db::Db;
use crate::mctx::budget::Budget;
use crate::mctx::compartments::{self, Compartment};
use crate::mctx::historian::{self, ChunkLimits};
use crate::mctx::memory;
use crate::runner::drain::DrainCtx;
use crate::runner::step::rows_after_with_seq;

// ---------------------------------------------------------------------------
// Historian trigger (step boundary)
// ---------------------------------------------------------------------------

/// Fire the historian when the live tail has reached a pressure band.
///
/// Detached on purpose: a run is a full hidden completion (seconds), and the
/// tail still has budget headroom when a band trips — the step that notices the
/// pressure is not the one that must wait for it. The pass in the next step
/// picks up whatever compartments the run published.
///
/// Every failure here is logged and swallowed: context assembly must never fail
/// a turn (upstream's transform has the same contract).
pub(crate) fn mctx_historian(ctx: &DrainCtx, budget: Budget, floor: i64) {
    if !ctx.config.magic_context.enabled {
        return;
    }
    let db = ctx.db.clone();
    let config = ctx.config.clone();
    let chat = Arc::clone(&ctx.chat);
    let bus = ctx.bus.clone();
    let catalog = Arc::clone(&ctx.model_catalog);
    let session_id = ctx.session_id.clone();
    let project = ctx.directory.to_string_lossy().into_owned();
    tokio::spawn(async move {
        let firing = Firing {
            db: &db,
            config: &config,
            chat: &chat,
            catalog: &catalog,
            budget,
            session_id: &session_id,
            project: &project,
        };
        match firing.run(floor).await {
            Ok(Some(published)) => {
                // A transcript notice: a fold is background work with no tool
                // call of its own, so without this the only trace is the head
                // changing under the conversation. `text` is empty — the model
                // already has the compartments — so only the human-facing
                // `description` is set (the TUI renders it as a `◈` row).
                let notice = crate::schema::event::SessionEventData::SessionSynthetic {
                    session_id: session_id.clone(),
                    text: String::new(),
                    description: Some(format!(
                        "historian folded {} · {} compartment{}",
                        published.covered,
                        published.compartments,
                        if published.compartments == 1 { "" } else { "s" }
                    )),
                    metadata: None,
                };
                if let Err(error) = bus.publish(notice).await {
                    tracing::warn!(
                        session = %session_id,
                        error = %error,
                        "publish the fold notice failed"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(session = %session_id, error = %error, "historian run failed");
            }
        }
    });
}

/// The context the assistant asked the provider for, as the provider reported
/// it: `input + cache.read` — the same sum the client's footer divides by the
/// model window. `None` when the tail holds no assistant turn (a fresh session)
/// or the row carries no token counts.
fn prompt_tokens(row: &serde_json::Value) -> Option<i64> {
    if row.get("type").and_then(|kind| kind.as_str()) != Some("assistant") {
        return None;
    }
    let tokens = row.get("tokens")?;
    let input = tokens.get("input")?.as_i64()?;
    let cached = tokens
        .get("cache")
        .and_then(|cache| cache.get("read"))
        .and_then(|read| read.as_i64())
        .unwrap_or(0);
    Some(input + cached)
}

/// One firing decision's context: the session, its derived budgets, and the
/// collaborators a run needs. Grouped because they are invariantly the same for
/// the whole decision — only the floor varies — and eight positional arguments
/// invite a swap that still compiles.
struct Firing<'a> {
    db: &'a Db,
    config: &'a Config,
    chat: &'a Arc<dyn Chat>,
    catalog: &'a crate::model_catalog::ModelCatalog,
    budget: Budget,
    session_id: &'a str,
    project: &'a str,
}

impl Firing<'_> {
    /// Measure the tail, classify the pressure, pick the chunk and run it.
    async fn run(self, floor: i64) -> Result<Option<Published>> {
        let Firing {
            db,
            config,
            chat,
            catalog,
            budget,
            session_id,
            project,
        } = self;
        // One pending run per session: a second step in the same burst must not
        // summarize the same range twice. This pre-check is only a shortcut —
        // it avoids loading the whole tail while a run is in flight; the
        // authoritative claim is `historian::run`'s, which checks and inserts in
        // one write transaction. `settle_running_runs` clears the row a crash
        // leaves behind.
        if historian::has_running_run(db, session_id).await? {
            return Ok(None);
        }
        let rows = rows_after_with_seq(db, session_id, floor).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        // The tail's usage as the provider last reported it — `None` before the
        // session's first completion, when the tail size is the only signal.
        let usage = rows.iter().rev().find_map(|(_, row)| prompt_tokens(row));
        let usage_pct = usage.and_then(|tokens| budget.usage_percentage(tokens));
        // The whole tail's estimate, measured by the chunker itself so the pressure
        // and the budget speak the same units.
        let Some(whole) = historian::chunk(&rows, floor + 1, i64::MAX, ChunkLimits::unbounded())
        else {
            return Ok(None);
        };
        // What a run could actually fold: the tail's real size minus the fence.
        // Both numbers are context tokens (`Chunk::context_tokens`), the unit the
        // provider's usage speaks — a tool result counts by the body it cost, not
        // by the `TC:` line that renders it. Decided before the trigger so a tail
        // that is large but wholly protected (a short session, a huge newest
        // turn) is skipped with a visible reason instead of firing a run that
        // cannot start.
        let protected = budget.protected_tail(usage_pct.unwrap_or(0.0));
        let foldable = whole.context_tokens.saturating_sub(protected as usize);
        if foldable == 0 {
            tracing::debug!(
                session = %session_id,
                tail_tokens = whole.context_tokens,
                protected_tail = protected,
                "historian skipped: nothing outside the protected tail"
            );
            return Ok(None);
        }
        let trigger = historian::trigger(
            usage_pct,
            whole.context_tokens as f64,
            budget.trigger,
            &budget,
        );
        tracing::debug!(
            session = %session_id,
            tail_tokens = whole.context_tokens,
            rendered_tokens = whole.token_estimate,
            // `None` = no completion has reported usage yet; it is not 0%.
            usage_pct = ?usage_pct,
            trigger_budget = budget.trigger,
            trigger = ?trigger,
            messages = rows.len(),
            bytes = whole.text.len(),
            first_ordinal = rows.first().map(|(ordinal, _)| *ordinal),
            last_ordinal = rows.last().map(|(ordinal, _)| *ordinal),
            floor,
            "historian pressure"
        );
        if trigger == historian::Trigger::No {
            return Ok(None);
        }

        // The newest row is never covered: it is the turn in flight. Everything
        // older competes for the chunk budget, which stops the run at the fence.
        let Some((last_ordinal, _)) = rows.last() else {
            return Ok(None);
        };
        let eligible_end = *last_ordinal;
        // The chunk is cut against two limits: the historian's read budget (the
        // rendered text it sends) and the fence (what the region may cost the
        // conversation). Nothing fitting means the oldest eligible block alone
        // would leave less than the fence's worth of verbatim tail; a fold that
        // did that would hand the model a summarized old block plus almost no
        // recent history, so the run waits instead.
        let Some(chunk) = historian::chunk(
            &rows,
            floor + 1,
            eligible_end,
            ChunkLimits {
                request: budget.historian_chunk as usize,
                context: foldable,
            },
        ) else {
            // Under real pressure this is the one shape mctx cannot resolve by
            // itself, so it is an operator signal, not just a trace: a tail
            // dominated by a single block stays unfolded until it grows past the
            // fence. Below the pressure band the same skip is routine (a short
            // session whose whole tail is protected), hence debug.
            let message =
                "historian skipped: the oldest eligible block alone crosses the protected tail";
            if usage_pct.unwrap_or(0.0) >= budget.soft_pct {
                tracing::warn!(
                    session = %session_id,
                    foldable,
                    protected_tail = protected,
                    usage_pct = ?usage_pct,
                    "{message}"
                );
            } else {
                tracing::debug!(
                    session = %session_id,
                    foldable,
                    protected_tail = protected,
                    usage_pct = ?usage_pct,
                    "{message}"
                );
            }
            return Ok(None);
        };

        let compartments = compartments::load(db, session_id).await?;
        let references: Vec<historian::ReferenceCompartment> = compartments
            .iter()
            .map(Compartment::reference_input)
            .collect();
        let blocks =
            historian::build_reference_blocks(session_id, chunk.start_ordinal, &references);
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
        let run_ctx = historian::RunCtx {
            db,
            config,
            chat,
            catalog,
        };
        let plan = historian::RunPlan {
            session_id,
            chunk: &chunk,
            attempt: 1,
            prompt,
        };
        tracing::debug!(
            session = %session_id,
            chunk_start = chunk.start_ordinal,
            chunk_end = chunk.end_ordinal,
            chunk_tokens = chunk.token_estimate,
            present = chunk.present_ordinals.len(),
            "historian chunk"
        );
        let Some(outcome) = historian::run(&run_ctx, &plan).await? else {
            // Another run holds the claim; it will publish, and the next pass
            // picks the compartments up.
            return Ok(None);
        };
        tracing::info!(
            session = %session_id,
            trigger = ?trigger,
            run = %outcome.run_id,
            covered = format!("{}..{}", chunk.start_ordinal, chunk.end_ordinal),
            compartments = outcome.compartment_ids.len(),
            "historian published"
        );
        Ok(Some(Published {
            covered: format!("{}..{}", chunk.start_ordinal, chunk.end_ordinal),
            compartments: outcome.compartment_ids.len(),
        }))
    }
}

/// What one published run folded, for the transcript notice.
struct Published {
    /// Ordinal range the run covered (`a..b`).
    covered: String,
    /// Compartments the run published.
    compartments: usize,
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

    /// A catalogue reporting one model — what a gateway would serve.
    fn catalog() -> crate::model_catalog::ModelCatalog {
        crate::model_catalog::ModelCatalog::from_entries(vec![ModelEntry {
            provider: "cardinal".to_owned(),
            id: "m".to_owned(),
            name: Some("m".to_owned()),
            context: None,
            output: None,
        }])
    }

    fn config() -> Config {
        Config {
            default_model: Some("cardinal/m".to_owned()),
            magic_context: MctxConfig {
                enabled: true,
                dreamer_enabled: false,
                ..MctxConfig::default()
            },
            ..Config::default()
        }
    }

    /// The session budget the tests run with: a 256k model, everything else at
    /// its default — trigger 8.5k tokens, chunk 50k, protected tail 51.1k.
    fn budget() -> Budget {
        Budget::derive(&MctxConfig::default(), Some(262_144), None)
    }

    /// A session row plus `count` messages, alternating user and assistant (a
    /// block boundary per row, so the chunker's budget applies per message),
    /// each carrying `bulk_chars` of text.
    async fn seed_session(db: &Db, count: i64, bulk_chars: usize) {
        let rows: Vec<(bool, usize)> = (1..=count).map(|seq| (seq % 2 == 0, bulk_chars)).collect();
        seed_messages(db, &rows).await;
    }

    /// One message per `(assistant, chars)` entry, in order.
    async fn seed_messages(db: &Db, rows: &[(bool, usize)]) {
        let conn = db.acquire().await.unwrap();
        conn.execute(
            "INSERT INTO session_v2
             (id, project_id, slug, directory, version, time_created, time_updated)
             VALUES (?1, '/p', 's', '/p', '2', 0, 0)",
            [SESSION],
        )
        .await
        .unwrap();
        for (index, (assistant, chars)) in rows.iter().enumerate() {
            let seq = index as i64 + 1;
            let text = "x".repeat(*chars);
            // Alternating roles, so every row is its own chunk block and the
            // budget applies per message.
            let (kind, data) = if *assistant {
                (
                    "assistant",
                    serde_json::json!({
                        "id": format!("msg_{seq}"),
                        "type": "assistant",
                        "time": { "created": 0 },
                        "agent": "build",
                        "model": { "id": "m", "providerID": "p" },
                        "content": [{ "type": "text", "text": text }],
                    }),
                )
            } else {
                (
                    "user",
                    serde_json::json!({
                        "id": format!("msg_{seq}"),
                        "type": "user",
                        "time": { "created": 0 },
                        "text": text,
                    }),
                )
            };
            conn.execute(
                "INSERT INTO session_message
                 (id, session_id, type, seq, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
                (format!("msg_{seq}"), SESSION, kind, seq, data.to_string()),
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

    /// A tail past the trigger budget fires one run, and the run stops at the
    /// protected fence: the newest ~51k tokens (30% of the usable window) stay
    /// verbatim for the model that is working on them.
    #[tokio::test]
    async fn a_tail_past_the_trigger_budget_fires_one_run() {
        let db = Db::open_in_memory().await.unwrap();
        // 8 rows of 40k chars ≈ 10k tokens each: an 80k tail, of which the
        // 51.1k protected tail leaves 28.9k foldable.
        seed_session(&db, 8, 40_000).await;
        let config = config();
        // The chunker applies the fence, so pin its arithmetic directly:
        // 80_000 rendered tokens of tail against the 51_118-token fence leaves
        // 28_882 tokens a run may cost the conversation.
        let rows = rows_after_with_seq(&db, SESSION, 0).await.unwrap();
        let foldable = 80_000 - budget().protected_tail(0.0) as usize;
        assert_eq!(foldable, 28_882);
        let chunk = historian::chunk(
            &rows,
            1,
            rows.last().unwrap().0,
            ChunkLimits {
                request: budget().historian_chunk as usize,
                context: foldable,
            },
        )
        .expect("chunk");
        assert_eq!(
            (chunk.start_ordinal, chunk.end_ordinal),
            (1, 2),
            "the budget, not the row count, decides what a run covers"
        );

        let chat: Arc<dyn Chat> = Arc::new(TextChat::new(&valid_output(1, 2)));
        let published = Firing {
            db: &db,
            config: &config,
            chat: &chat,
            catalog: &catalog(),
            budget: budget(),
            session_id: SESSION,
            project: "/p",
        }
        .run(0)
        .await
        .expect("a published run")
        .expect("the run folded something");
        assert_eq!(published.covered, "1..2");
        assert_eq!(published.compartments, 1);

        let compartments = compartments::load(&db, SESSION).await.unwrap();
        assert_eq!(compartments.len(), 1, "one compartment published");
        assert_eq!(
            compartments[0].coverage_end, "2",
            "the run covers what the chunker admitted and nothing newer"
        );
    }

    /// A tail that is *entirely* inside the protected window is not foldable:
    /// 60k tokens of tail against a 51.1k fence leaves less than one message's
    /// worth, and the chunker's oversized-first-block rule must not spend the
    /// fence's own content to make progress.
    #[tokio::test]
    async fn a_tail_inside_the_protected_window_does_not_fold() {
        let db = Db::open_in_memory().await.unwrap();
        // Three rows of 80k chars ≈ 20k tokens each.
        seed_session(&db, 3, 80_000).await;
        let config = config();
        let chat: Arc<dyn Chat> = Arc::new(TextChat::new(&valid_output(1, 1)));

        assert!(
            Firing {
                db: &db,
                config: &config,
                chat: &chat,
                catalog: &catalog(),
                budget: budget(),
                session_id: SESSION,
                project: "/p",
            }
            .run(0)
            .await
            .expect("a decision, not an error")
            .is_none(),
            "the oldest row straddles the protected tail, so nothing is foldable yet"
        );
        assert!(
            compartments::load(&db, SESSION).await.unwrap().is_empty(),
            "no compartment was published"
        );
    }

    /// A tail dominated by one block does not fold: that block alone is bigger
    /// than everything the fence leaves foldable, so covering it would hand the
    /// model a summary plus almost no verbatim history. The run waits and the
    /// skip is visible in the log; no completion is issued.
    #[tokio::test]
    async fn a_block_that_dominates_the_tail_does_not_fold() {
        let db = Db::open_in_memory().await.unwrap();
        // One 200k-token turn, then a 20-token reply: 200_020 tokens of tail
        // against the 51_118 fence leaves 148_902 foldable, and the oldest block
        // is 200_000 — more than that, so it is not admissible.
        seed_messages(&db, &[(false, 800_000), (true, 80)]).await;
        let config = config();
        let mock = Arc::new(TextChat::new(&valid_output(1, 1)));
        let chat: Arc<dyn Chat> = mock.clone();

        assert!(
            Firing {
                db: &db,
                config: &config,
                chat: &chat,
                catalog: &catalog(),
                budget: budget(),
                session_id: SESSION,
                project: "/p",
            }
            .run(0)
            .await
            .expect("a decision, not an error")
            .is_none(),
            "the only foldable block crosses the fence, so the run waits"
        );
        assert!(
            mock.requests().is_empty(),
            "no completion for a refused fold"
        );
        assert!(compartments::load(&db, SESSION).await.unwrap().is_empty());
    }

    /// Below the trigger budget nothing runs and no completion is issued.
    #[tokio::test]
    async fn a_quiet_tail_issues_no_completion() {
        let db = Db::open_in_memory().await.unwrap();
        seed_session(&db, 3, 10).await;
        let config = config();
        let mock = Arc::new(TextChat::new(&valid_output(1, 2)));
        let chat: Arc<dyn Chat> = mock.clone();

        assert!(
            Firing {
                db: &db,
                config: &config,
                chat: &chat,
                catalog: &catalog(),
                budget: budget(),
                session_id: SESSION,
                project: "/p",
            }
            .run(0)
            .await
            .expect("no work is not an error")
            .is_none()
        );

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
        assert!(
            Firing {
                db: &db,
                config: &config,
                chat: &chat,
                catalog: &catalog(),
                budget: budget(),
                session_id: SESSION,
                project: "/p",
            }
            .run(0)
            .await
            .expect("a skipped firing is not an error")
            .is_none()
        );

        assert!(mock.requests().is_empty(), "one run at a time");
        assert!(compartments::load(&db, SESSION).await.unwrap().is_empty());
    }
}
