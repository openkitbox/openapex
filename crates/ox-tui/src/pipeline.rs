//! Store → display blocks. Owns the Explored grouping: consecutive
//! read/glob/grep/webfetch/websearch calls of one assistant message fold into
//! one `Explored` block (upstream `routes/session/grouping/session.ts`
//! `partPath`/`append`, drawn as in `group-view.tsx:110-157`). The header
//! counts each noun (`read`, `search` for grep/glob/websearch, `fetch`) in
//! order of first appearance, as upstream's `Object.entries(counts)` does.
//!
//! Deviations:
//! - A group is "running" while any of its calls has not finished. Upstream
//!   also marks a group completed once another row is appended after it
//!   (`completePrevious`), so a call still running behind a newer row reads
//!   `Explored:` there and `Exploring:` here.
//! - Groups stay within one assistant message, and tool rows follow the
//!   message text (the wire gives tool parts no ordinal). Upstream's
//!   `append` can merge adjacent exploration parts across messages.
//! - Permission-blocked calls are not moved to the group's end
//!   (`partitionPending`).
//! - `web_search` (ox's tool name) counts as a search alongside upstream's
//!   `websearch`; names are matched as sent, not lower-cased.
//! - Expanded children are plain `tool_row` rows: not hover or click
//!   targets, so a failed child's error cannot be opened from the group, and
//!   `websearch` children use the generic text (`tool_text` has none).
//! - The `verbosity = high` default-open and the exploration-grouping toggle
//!   are not ported; groups start collapsed.
//!
//! Session-level rows (upstream `routes/session/index.tsx:2000-2240`) map one
//! item to one block: `Switch`, `Notice` and `Error` items move their `Arc`
//! payloads across, the step error and retry ride on the assistant footer,
//! and each compaction waiting in the inbox appends a trailing
//! `CompactionQueued` row (upstream `rows.ts:70-79` splices them in at the
//! end, ahead of pending prompts, which ox shows in the dock instead).
//!
//! Session-row deviations:
//! - The footer shows for a terminal finish only once the message is `done`
//!   (upstream `rows.ts:411` needs only the terminal finish), or whenever the
//!   message carries an error or a retry.
//! - Model switch labels are `provider/id[/variant]`: ox has no model
//!   display-name catalogue in the TUI.
//! - A `Step interrupted` error draws no error row; the stats line gains
//!   ` · interrupted` instead.
//! - The subagent row's `Background` badge uses the raised background
//!   (upstream's inline subagent row uses the plain badge).

use std::sync::Arc;

use ox_client::store::{AssistantItem, DataStore, Item, ShellRef, ToolPart};
use ox_schema::support::FinishReason;

use crate::shell_captures::ShellCaptures;
use crate::ui::timeline::{Block, Explored, ExploredCall, ExploredCount, TurnFooter};

/// Projection of the store's ordered items into display blocks. Ordering is
/// owned by `DataStore::items`; text is shared via `Arc` so an unchanged block
/// costs a pointer clone. One assistant item becomes up to four kinds of
/// block: its reasoning header, its answer text (with the turn footer once the
/// turn has finished), and its tool calls, where consecutive read/search/fetch
/// calls fold into one `Explored` row (upstream `group-view.tsx`). A shell
/// call shows its tool result or its shell's fetched output in `captures`.
#[must_use]
pub struct Projection {
    pub blocks: Vec<Block>,
    /// The block index of the store item the dirty watermark
    /// (`DataStore::changed_from`) points at; every block before it is
    /// untouched. `blocks.len()` when the watermark is past the last item, so a
    /// caller can diff `blocks[first_block..]`.
    ///
    /// It is the item's index only when every earlier item drew exactly one
    /// block: a turn item draws none, and one assistant item may draw several,
    /// so the projection has to convert the index rather than pass it on.
    pub first_block: usize,
}

pub fn blocks_from_store(
    store: &DataStore,
    captures: &ShellCaptures,
    changed_from: usize,
) -> Projection {
    let mut blocks = Vec::new();
    // Where the turn being projected started; see [`turn_start`].
    let mut turn = TurnStart::default();
    // The block index of the item at `changed_from`, recorded by the walk that
    // already visits every item: no second pass and no per-item index table.
    let mut first_block = None;
    for (index, item) in store.items.iter().enumerate() {
        if index == changed_from {
            first_block = Some(blocks.len());
        }
        if let Some(start) = turn_start(index, item) {
            turn = start;
        }
        match item {
            Item::User { text, .. } => {
                blocks.push(Block::User {
                    text: Arc::clone(text),
                });
            }
            // Nothing to draw: the turn item only moved the boundary above.
            Item::Turn { .. } => {}
            Item::Assistant(assistant) => {
                if !assistant.reasoning.trim().is_empty() {
                    blocks.push(Block::Reasoning {
                        id: assistant.id.clone(),
                        version: assistant.version,
                        text: Arc::clone(&assistant.reasoning),
                        done: assistant.done
                            || !assistant.text.is_empty()
                            || !assistant.tools.is_empty(),
                        ms: assistant.reasoning_ms,
                    });
                }
                blocks.push(Block::Assistant {
                    id: assistant.id.clone(),
                    version: assistant.version,
                    text: Arc::clone(&assistant.text),
                    footer: turn_footer(store, assistant, turn, index),
                });
                // Tool rows follow the message text: the wire gives tool
                // parts no ordinal, so the interleaving is not recoverable.
                push_tools(&mut blocks, &assistant.tools, store, captures);
            }
            Item::Compaction(compaction) => blocks.push(Block::Compaction {
                id: compaction.id.clone(),
                version: compaction.version,
                status: compaction.status,
                text: Arc::clone(&compaction.text),
                cancelled: compaction.cancelled,
                tokens: compaction.tokens,
            }),
            Item::Completion(notice) => blocks.push(Block::Completion {
                id: notice.id.clone(),
                source: notice.source,
                state: notice.state.clone(),
                description: Arc::clone(&notice.description),
                child_id: notice.child_id.clone(),
            }),
            Item::Switch(switch) => blocks.push(Block::Switch(switch.clone())),
            Item::Notice(notice) => blocks.push(Block::Notice(notice.clone())),
            Item::Error(error) => blocks.push(Block::Error(error.clone())),
        }
    }
    blocks.extend(
        store
            .queued_compactions
            .iter()
            .map(|_| Block::CompactionQueued),
    );
    let first_block = first_block.unwrap_or(blocks.len());
    Projection {
        blocks,
        // A watermark past the last item (nothing dirty) compares nothing, but
        // still lets the caller pick up appended and dropped trailing blocks.
        first_block,
    }
}

/// Where the turn being projected started: `after` is the index just past the
/// item that opened it, `created` that item's `time.created` when the wire has
/// one.
#[derive(Debug, Clone, Copy, Default)]
struct TurnStart {
    after: usize,
    created: Option<i64>,
}

/// The turn start an item opens, or `None` for an item inside the turn.
///
/// Upstream `packages/util/src/session-turn.ts` `inputIndex` dates a turn from
/// its first input (a `user` or `synthetic` message, the walk stopping at an
/// `idle` marker). That file sits outside the sparse reference checkout; reach
/// it with `git show HEAD:packages/util/src/session-turn.ts` there. Two items
/// carry the rule here: a prompt row that begins a turn
/// (its `starts_turn` flag), and the [`Item::Turn`] the store and history
/// emit at a turn that began without one. Neither is a rendered row, so the
/// boundary holds for a description-less synthetic prompt (a repeat or todo
/// reminder, a continuation) and a steered turn stays whole: the row of a
/// prompt typed while the turn runs is inside it, never a new start.
///
/// A history prompt row keeps its own `time.created` — history writes turn
/// items only at synthetics — so a typed prompt's exact time is never replaced
/// by an execution start. A live prompt echo has no time, and the turn item
/// that follows it carries the execution start, which is the same value the
/// footer used to fall back to.
fn turn_start(index: usize, item: &Item) -> Option<TurnStart> {
    let created = match item {
        Item::User {
            starts_turn: false, ..
        } => return None,
        Item::User { created, .. } | Item::Turn { created } => *created,
        _ => return None,
    };
    Some(TurnStart {
        after: index + 1,
        created,
    })
}

/// The noun upstream counts an exploring tool under (`group-view.tsx`
/// `label`), or `None` for tools that do not join an Explored group.
fn explored_noun(tool: &str) -> Option<&'static str> {
    match tool {
        "read" => Some("read"),
        "grep" | "glob" | "web_search" | "websearch" => Some("search"),
        "webfetch" => Some("fetch"),
        _ => None,
    }
}

fn push_tools(
    blocks: &mut Vec<Block>,
    tools: &[ToolPart],
    store: &DataStore,
    captures: &ShellCaptures,
) {
    let mut group: Option<Explored> = None;
    for tool in tools {
        if let Some(noun) = explored_noun(&tool.tool) {
            let explored = group.get_or_insert_with(|| Explored {
                group_id: tool.id.clone(),
                ..Explored::default()
            });
            match explored.counts.iter_mut().find(|c| c.noun == noun) {
                Some(entry) => entry.count += 1,
                None => explored.counts.push(ExploredCount { noun, count: 1 }),
            }
            explored.calls.push(ExploredCall {
                id: tool.id.clone(),
                version: tool.version,
                tool: tool.tool.clone(),
                state: tool.state,
                input: Arc::clone(&tool.input),
                metadata: tool.metadata.clone(),
            });
            explored.running |= !matches!(
                tool.state,
                ox_client::store::ToolState::Success | ox_client::store::ToolState::Failed
            );
            continue;
        }
        if let Some(explored) = group.take() {
            blocks.push(Block::Explored(explored));
        }
        if tool.tool == ShellRef::SHELL_TOOL {
            let bound = tool.shell();
            let shown = captures.shown(tool, bound.as_ref());
            blocks.push(Block::Shell {
                id: tool.id.clone(),
                version: tool.version,
                state: tool.state,
                input: Arc::clone(&tool.input),
                output: shown.text,
                omitted: shown.omitted,
                shell: bound.is_some(),
                background: bound.as_ref().is_some_and(|shell| shell.background),
                shell_running: bound
                    .as_ref()
                    .is_some_and(|shell| store.is_shell_running(&shell.id)),
            });
            continue;
        }
        if tool.tool == "todowrite" {
            blocks.push(Block::Todo {
                id: tool.id.clone(),
                version: tool.version,
                state: tool.state,
                todos: Arc::clone(&tool.todos),
                output: Arc::clone(&tool.output),
            });
            continue;
        }
        blocks.push(Block::Tool {
            id: tool.id.clone(),
            version: tool.version,
            tool: tool.tool.clone(),
            state: tool.state,
            summary: tool.summary.clone(),
            output: Arc::clone(&tool.output),
            background: tool
                .child
                .as_ref()
                .is_some_and(|child| store.is_child_running(&child.session_id)),
            input: Arc::clone(&tool.input),
            metadata: tool.metadata.clone(),
        });
    }
    if let Some(explored) = group {
        blocks.push(Block::Explored(explored));
    }
}

/// Upstream `AssistantFooter` appears on a message that ended its turn
/// (`finish` is terminal, not `tool-calls`/`unknown`) or failed, and on one
/// waiting for a retry (`rows.ts` `assistant-footer`). Duration and tok/s span
/// the current turn up to this message (`packages/util/src/session-turn.ts`):
/// duration = completion − the turn's start, tok/s = output tokens over the
/// provider-active time of the turn's steps; tok/s is omitted when either is 0.
///
/// The start comes from [`TurnStart`]: the turn's first input `time.created`
/// when the wire has one, otherwise the turn's first step
/// ([`AssistantItem::turn_started_ms`] — the execution start live, the
/// message's own creation in history), otherwise the duration is omitted.
fn turn_footer(
    store: &DataStore,
    assistant: &AssistantItem,
    turn: TurnStart,
    index: usize,
) -> Option<TurnFooter> {
    let terminal = matches!(
        assistant.finish,
        Some(
            FinishReason::Stop
                | FinishReason::Length
                | FinishReason::ContentFilter
                | FinishReason::Error
        )
    );
    // Upstream `terminal || message.retry` (`rows.ts:411`). ox also waits for
    // `done` on a terminal finish: `finish` is set per step mid-turn.
    let ended = (terminal && assistant.done) || assistant.error.is_some();
    if !ended && assistant.retry.is_none() {
        return None;
    }
    let (mut tokens, mut active) = (0i64, 0i64);
    let mut first_started = None;
    for item in &store.items[turn.after..=index] {
        if let Item::Assistant(step) = item {
            tokens += step.output_tokens;
            active += step.active_ms;
            first_started = first_started.or(step.turn_started_ms);
        }
    }
    let duration_ms = turn
        .created
        .or(first_started)
        .zip(assistant.turn_done_ms)
        .map(|(start, done)| (done - start).max(0));
    let tokens_per_second =
        (tokens > 0 && active > 0).then(|| tokens as f64 / (active as f64 / 1000.0));
    Some(TurnFooter {
        agent: assistant.agent.clone().or_else(|| store.agent.clone()),
        model: assistant.model.clone().or_else(|| store.model.clone()),
        duration_ms,
        tokens_per_second,
        error: assistant.error.clone(),
        retry: assistant.retry.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_client::store::{CompletionItem, CompletionSource};

    /// One finished assistant step of the turn that started at
    /// `turn_started_ms` and stopped at `done_ms`.
    fn step(id: &str, turn_started_ms: i64, done_ms: i64, tokens: i64, active_ms: i64) -> Item {
        Item::Assistant(Box::new(AssistantItem {
            id: id.to_owned(),
            done: true,
            finish: Some(FinishReason::Stop),
            turn_started_ms: Some(turn_started_ms),
            turn_done_ms: Some(done_ms),
            output_tokens: tokens,
            active_ms,
            ..Default::default()
        }))
    }

    /// A typed prompt: history dates it with the wire's `time.created`.
    fn prompt(created: i64, text: &str) -> Item {
        Item::User {
            text: Arc::from(text),
            created: Some(created),
            starts_turn: true,
        }
    }

    /// The optimistic echo of a prompt typed while idle: no wire time, and the
    /// turn row that follows carries the execution start.
    fn echo(text: &str) -> Item {
        Item::User {
            text: Arc::from(text),
            created: None,
            starts_turn: true,
        }
    }

    /// A turn that started without a dated prompt row.
    fn turn(created: i64) -> Item {
        Item::Turn {
            created: Some(created),
        }
    }

    /// A prompt typed while the turn was already running: the server persists
    /// it mid-turn as a user message, so it is a row inside the turn, not a new
    /// turn's start (upstream `session-turn.ts` `inputIndex` takes the first
    /// input after the previous idle marker).
    fn steer(created: Option<i64>, text: &str) -> Item {
        Item::User {
            text: Arc::from(text),
            created,
            starts_turn: false,
        }
    }

    /// A described completion row: a subagent finishing with nobody typing.
    fn completion() -> Item {
        Item::Completion(CompletionItem {
            id: "notice".to_owned(),
            source: CompletionSource::Subagent,
            state: Some(Arc::from("completed")),
            description: Arc::from("Explore finished"),
            child_id: Some(Arc::from("child")),
        })
    }

    /// The assistant footers in transcript order, through the real projection.
    fn footers(items: Vec<Item>) -> Vec<Option<TurnFooter>> {
        let mut store = DataStore::default();
        store.items.extend(items);
        blocks_from_store(&store, &ShellCaptures::default(), usize::MAX)
            .blocks
            .into_iter()
            .filter_map(|block| match block {
                Block::Assistant { footer, .. } => Some(footer),
                _ => None,
            })
            .collect()
    }

    fn durations(footers: &[Option<TurnFooter>]) -> Vec<Option<i64>> {
        footers
            .iter()
            .map(|footer| footer.as_ref().and_then(|footer| footer.duration_ms))
            .collect()
    }

    fn duration(footer: &Option<TurnFooter>) -> Option<i64> {
        footer.as_ref().and_then(|footer| footer.duration_ms)
    }

    fn tokens_per_second(footer: &Option<TurnFooter>) -> Option<f64> {
        footer.as_ref().and_then(|footer| footer.tokens_per_second)
    }

    #[test]
    fn footer_duration_spans_only_the_current_turn() {
        // Turns 1 and 2 are typed; turn 3 starts from a described synthetic (a
        // subagent completion). The session has been running 240s by the last
        // completion, the previous prompt is 180s back, and the turn is 60s.
        let footers = footers(vec![
            prompt(0, "one"),
            step("a1", 0, 60_000, 400, 60_000),
            prompt(60_000, "two"),
            step("a2", 60_000, 120_000, 400, 60_000),
            turn(180_000),
            completion(),
            step("a3", 180_000, 240_000, 300, 60_000),
        ]);
        assert_eq!(
            durations(&footers),
            vec![Some(60_000), Some(60_000), Some(60_000)],
            "each footer spans its own turn, not the session or the previous turn"
        );
        assert_eq!(
            tokens_per_second(&footers[2]),
            Some(5.0),
            "tok/s counts only the current turn's steps (300 tokens over 60s)"
        );
    }

    #[test]
    fn a_turn_that_starts_from_a_description_less_synthetic_is_dated() {
        // A repeat reminder, a todo reminder, an output-limit or
        // incomplete-stream continuation renders no row at all (`rows.ts` drops
        // a synthetic without a description) and is still a turn input: without
        // the turn item the footer would span the session.
        let footers = footers(vec![
            prompt(0, "one"),
            step("a1", 0, 60_000, 400, 60_000),
            turn(120_000),
            step("a2", 120_000, 180_000, 300, 60_000),
        ]);
        assert_eq!(
            durations(&footers),
            vec![Some(60_000), Some(60_000)],
            "a description-less synthetic still starts the turn it prompts"
        );
    }

    #[test]
    fn a_steered_turn_is_not_split() {
        // The live shape: the prompt echo carries no wire time, the turn row
        // that follows it carries `execution.started`, and the steer echoed
        // while the turn runs is a row inside the turn — so the footer spans
        // the whole turn and tok/s counts both of its steps.
        let footers = footers(vec![
            echo("go"),
            turn(0),
            step("a1", 0, 30_000, 100, 10_000),
            steer(None, "use grep"),
            step("a2", 0, 60_000, 900, 10_000),
        ]);
        assert_eq!(
            durations(&footers),
            vec![Some(30_000), Some(60_000)],
            "the steer's row does not restart the turn: the last footer still spans the whole turn"
        );
        assert_eq!(
            tokens_per_second(&footers[1]),
            Some(50.0),
            "both steps count (1000 tokens over their 20s of provider time)"
        );
    }

    #[test]
    fn a_steer_row_does_not_restart_the_turn() {
        // A prompt typed while the turn runs is persisted mid-turn as a `user`
        // message. The turn is the first input after the previous idle marker
        // (upstream `session-turn.ts` `inputIndex`), so the steer's row must
        // not restart either clock.
        let footers = footers(vec![
            prompt(0, "go"),
            step("a1", 0, 30_000, 100, 10_000),
            steer(Some(20_000), "actually, use grep"),
            step("a2", 0, 60_000, 900, 10_000),
        ]);
        assert_eq!(
            durations(&footers),
            vec![Some(30_000), Some(60_000)],
            "the turn still starts at its first input (0), not at the steer (20s)"
        );
        assert_eq!(
            tokens_per_second(&footers[1]),
            Some(50.0),
            "both steps of the turn count (1000 tokens over their 20s of provider time)"
        );
    }

    #[test]
    fn footer_of_a_single_turn_is_unchanged() {
        // A history turn: the prompt's creation (1s) wins over the step's own
        // start (0.5s), so the footer is 180s − 1s, and 2136 tokens over 15s.
        let history = footers(vec![
            prompt(1_000, "only"),
            step("a1", 500, 180_000, 2_136, 15_000),
        ]);
        assert_eq!(history.len(), 1, "one assistant message yields one footer");
        assert_eq!(duration(&history[0]), Some(179_000));
        assert_eq!(tokens_per_second(&history[0]), Some(142.4));

        // The live shape: the prompt echo carries no time, so the turn item's
        // execution start (0.5s) dates the turn — the same fallback as before.
        let live = footers(vec![
            echo("only"),
            turn(500),
            step("a1", 500, 180_000, 2_136, 15_000),
        ]);
        assert_eq!(duration(&live[0]), Some(179_500));
        assert_eq!(tokens_per_second(&live[0]), Some(142.4));
    }

    /// Every rendered item draws exactly one block: the turn row is the only
    /// item that draws none, which is why the store's item-index watermark has
    /// to be converted rather than handed to the transcript as a block index.
    #[test]
    fn a_rich_transcript_draws_one_block_per_item_but_the_turn_rows() {
        let mut store = DataStore::default();
        store.items.extend(vec![
            prompt(0, "one"),
            turn(60_000),
            step("a1", 0, 60_000, 100, 10_000),
            turn(90_000),
            completion(),
            step("a2", 90_000, 120_000, 100, 10_000),
        ]);
        let turns = store
            .items
            .iter()
            .filter(|item| matches!(item, Item::Turn { .. }))
            .count();
        assert_eq!(
            turns, 2,
            "the fixture must contain turn rows to prove anything"
        );
        let projection = blocks_from_store(&store, &ShellCaptures::default(), usize::MAX);
        assert_eq!(
            projection.blocks.len(),
            store.items.len() - turns,
            "each item draws one block except the {} turn rows",
            turns
        );
    }

    /// The block a changed item starts at: the turn row before the answer shifts
    /// it down one, so an item index passed on unchanged would skip the answer.
    #[test]
    fn the_changed_block_index_skips_the_unrendered_turn_row() {
        let mut store = DataStore::default();
        store.items.extend(vec![
            prompt(0, "go"),
            turn(1_000),
            step("a1", 1_000, 2_000, 1, 1_000),
        ]);

        let changed = blocks_from_store(&store, &ShellCaptures::default(), 2);
        assert_eq!(
            changed.first_block, 1,
            "the answer is block 1 of [prompt, answer] although it is item 2"
        );
        assert!(
            matches!(changed.blocks[changed.first_block], Block::Assistant { .. }),
            "the block at the watermark is the answer"
        );

        let all = blocks_from_store(&store, &ShellCaptures::default(), 0);
        assert_eq!(all.first_block, 0, "the first item is the first block");

        let clean = blocks_from_store(&store, &ShellCaptures::default(), usize::MAX);
        assert_eq!(
            clean.first_block,
            clean.blocks.len(),
            "a watermark past the last item compares nothing"
        );
    }
}
