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

use ox_client::store::{AssistantItem, DataStore, Item, ToolPart};
use ox_schema::support::FinishReason;

use crate::ui::timeline::{Block, Explored, ExploredCall, ExploredCount, TurnFooter};

/// Projection of the store's ordered items into display blocks. Ordering is
/// owned by `DataStore::items`; text is shared via `Arc` so an unchanged block
/// costs a pointer clone. One assistant item becomes up to four kinds of
/// block: its reasoning header, its answer text (with the turn footer once the
/// turn has finished), and its tool calls, where consecutive read/search/fetch
/// calls fold into one `Explored` row (upstream `group-view.tsx`).
pub fn blocks_from_store(store: &DataStore) -> Vec<Block> {
    let mut blocks = Vec::new();
    // First item of the turn currently being projected (the user prompt).
    let mut turn_start = 0;
    let mut turn_created: Option<i64> = None;
    for (index, item) in store.items.iter().enumerate() {
        match item {
            Item::User { text, created } => {
                turn_start = index + 1;
                turn_created = *created;
                blocks.push(Block::User {
                    text: Arc::clone(text),
                });
            }
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
                    footer: turn_footer(store, assistant, turn_start, index, turn_created),
                });
                // Tool rows follow the message text: the wire gives tool
                // parts no ordinal, so the interleaving is not recoverable.
                push_tools(&mut blocks, &assistant.tools, store);
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
    blocks
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

fn push_tools(blocks: &mut Vec<Block>, tools: &[ToolPart], store: &DataStore) {
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
/// the whole turn up to this message (`packages/util/src/session-turn.ts`):
/// duration = completion − prompt creation, tok/s = output tokens over the
/// provider-active time of the turn's steps; tok/s is omitted when either is 0.
fn turn_footer(
    store: &DataStore,
    assistant: &AssistantItem,
    turn_start: usize,
    index: usize,
    turn_created: Option<i64>,
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
    for item in &store.items[turn_start..=index] {
        if let Item::Assistant(step) = item {
            tokens += step.output_tokens;
            active += step.active_ms;
            first_started = first_started.or(step.turn_started_ms);
        }
    }
    let duration_ms = turn_created
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
    use std::sync::Arc;

    use ox_client::store::{AssistantItem, DataStore, Item, ToolPart, ToolState};

    use super::blocks_from_store;
    use crate::ui::timeline::{Block, ExploredCount};

    fn store_with_subagent(child: Option<&str>) -> DataStore {
        let mut store = DataStore::default();
        store.session = Some("ses_parent".into());
        store.items = vec![Item::Assistant(Box::new(AssistantItem {
            id: "m1".into(),
            tools: vec![ToolPart {
                id: "p1".into(),
                tool: "subagent".into(),
                state: ToolState::Success,
                child: child.map(|id| ox_client::store::ChildRef {
                    session_id: id.into(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        }))];
        store
    }

    fn background_of(store: &DataStore) -> bool {
        blocks_from_store(store)
            .iter()
            .find_map(|block| match block {
                Block::Tool { background, .. } => Some(*background),
                _ => None,
            })
            .expect("a tool block")
    }

    #[test]
    fn background_is_true_only_while_the_child_runs() {
        let mut store = store_with_subagent(Some("ses_c"));
        assert!(!background_of(&store), "child known but idle");
        assert!(store.set_child_running("ses_c", true));
        assert!(background_of(&store), "child running");
        assert!(store.set_child_running("ses_c", false));
        assert!(!background_of(&store), "child finished");
    }

    #[test]
    fn a_tool_without_a_child_is_never_background() {
        let mut store = store_with_subagent(None);
        assert!(!store.set_child_running("ses_c", true), "nothing names it");
        assert!(!background_of(&store));
    }

    #[test]
    fn projects_tool_parts_after_the_assistant_text() {
        let mut store = DataStore::default();
        store.items = vec![Item::Assistant(Box::new(AssistantItem {
            id: "m1".into(),
            tools: vec![
                ToolPart {
                    id: "p1".into(),
                    tool: "shell".into(),
                    state: ToolState::Running,
                    summary: "git status".into(),
                    output: Arc::default(),
                    version: 3,
                    ..Default::default()
                },
                ToolPart {
                    id: "p2".into(),
                    tool: "read".into(),
                    state: ToolState::Success,
                    summary: "a.txt".into(),
                    output: Arc::new("ok".into()),
                    version: 1,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }))];

        let blocks = blocks_from_store(&store);
        assert_eq!(blocks.len(), 3, "text, the shell row, one Explored row");
        assert!(
            matches!(&blocks[1], Block::Tool { id, version, .. } if id == "p1" && *version == 3)
        );
        assert!(
            matches!(&blocks[2], Block::Explored(e) if e.counts == [ExploredCount { noun: "read", count: 1 }] && !e.running)
        );
    }

    fn part(id: &str, tool: &str, state: ToolState, version: u64) -> ToolPart {
        ToolPart {
            id: id.into(),
            tool: tool.into(),
            state,
            summary: format!("{tool}-arg"),
            version,
            ..Default::default()
        }
    }

    fn store_with_tools(tools: Vec<ToolPart>) -> DataStore {
        let mut store = DataStore::default();
        store.items = vec![Item::Assistant(Box::new(AssistantItem {
            id: "m1".into(),
            tools,
            ..Default::default()
        }))];
        store
    }

    #[test]
    fn consecutive_exploring_tools_fold_into_one_group_that_keeps_its_calls() {
        let store = store_with_tools(vec![
            part("a", "read", ToolState::Success, 1),
            part("b", "grep", ToolState::Success, 2),
            part("c", "webfetch", ToolState::Success, 3),
            part("d", "shell", ToolState::Success, 4),
            part("e", "read", ToolState::Success, 5),
        ]);

        let blocks = blocks_from_store(&store);
        assert_eq!(blocks.len(), 4, "text, group, shell, second group");
        let Block::Explored(first) = &blocks[1] else {
            panic!("{:?}", blocks[1]);
        };
        assert_eq!(first.group_id, "a", "id of the first call");
        let nouns: Vec<(&str, u32)> = first.counts.iter().map(|c| (c.noun, c.count)).collect();
        assert_eq!(nouns, [("read", 1), ("search", 1), ("fetch", 1)]);
        let calls: Vec<(&str, &str)> = first
            .calls
            .iter()
            .map(|call| (call.id.as_str(), call.tool.as_str()))
            .collect();
        assert_eq!(calls, [("a", "read"), ("b", "grep"), ("c", "webfetch")]);
        let Block::Explored(second) = &blocks[3] else {
            panic!("{:?}", blocks[3]);
        };
        assert_eq!(second.group_id, "e");
        assert_eq!(second.calls.len(), 1);
    }

    #[test]
    fn group_counts_follow_the_first_appearance_of_each_noun() {
        let store = store_with_tools(vec![
            part("a", "webfetch", ToolState::Success, 1),
            part("b", "glob", ToolState::Success, 1),
            part("c", "read", ToolState::Success, 1),
            part("d", "grep", ToolState::Running, 1),
            part("e", "webfetch", ToolState::Success, 1),
        ]);
        let blocks = blocks_from_store(&store);
        let Block::Explored(group) = &blocks[1] else {
            panic!("{:?}", blocks[1]);
        };
        let nouns: Vec<(&str, u32)> = group.counts.iter().map(|c| (c.noun, c.count)).collect();
        assert_eq!(nouns, [("fetch", 2), ("search", 2), ("read", 1)]);
        assert!(group.running, "the grep call is still running");
    }

    #[test]
    fn grouped_calls_share_input_and_metadata_with_the_part() {
        let mut tool = part("a", "read", ToolState::Success, 1);
        tool.metadata = Some(Arc::default());
        let store = store_with_tools(vec![tool]);
        let blocks = blocks_from_store(&store);
        let Block::Explored(group) = &blocks[1] else {
            panic!("{:?}", blocks[1]);
        };
        let Item::Assistant(assistant) = &store.items[0] else {
            panic!("assistant item");
        };
        let part = &assistant.tools[0];
        assert!(
            Arc::ptr_eq(&group.calls[0].input, &part.input),
            "input is shared, not copied"
        );
        assert!(
            matches!((&group.calls[0].metadata, &part.metadata), (Some(a), Some(b)) if Arc::ptr_eq(a, b)),
            "metadata is shared, not copied"
        );
    }

    #[test]
    fn a_call_status_change_makes_the_group_block_unequal() {
        let running = blocks_from_store(&store_with_tools(vec![
            part("a", "read", ToolState::Success, 1),
            part("b", "grep", ToolState::Running, 1),
        ]));
        let done = blocks_from_store(&store_with_tools(vec![
            part("a", "read", ToolState::Success, 1),
            part("b", "grep", ToolState::Success, 2),
        ]));
        let same = blocks_from_store(&store_with_tools(vec![
            part("a", "read", ToolState::Success, 1),
            part("b", "grep", ToolState::Running, 1),
        ]));
        assert_ne!(running[1], done[1]);
        assert_eq!(running[1], same[1]);
    }

    #[test]
    fn session_items_project_by_sharing_their_arcs_and_queued_compactions_trail() {
        let error = ox_client::store::ErrorInfo {
            kind: Arc::from("provider"),
            message: Arc::from("rate limited"),
        };
        let mut store = DataStore::default();
        store.items = vec![
            Item::User {
                text: Arc::from("a"),
                created: None,
            },
            Item::Error(error.clone()),
        ];
        store.queued_compactions = vec!["inb_1".into()];

        let blocks = blocks_from_store(&store);
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        assert!(
            matches!(&blocks[1], Block::Error(e) if Arc::ptr_eq(&e.message, &error.message)),
            "{blocks:?}"
        );
        assert!(matches!(blocks[2], Block::CompactionQueued), "{blocks:?}");
    }

    #[test]
    fn projects_items_one_to_one_in_order() {
        // DataStore has a private field, so struct-update syntax (FRU) cannot
        // be used here; assign the pub fields on a default instance instead.
        let mut store = DataStore::default();
        store.items = vec![
            Item::User {
                text: Arc::from("a"),
                created: None,
            },
            Item::Assistant(Box::new(AssistantItem {
                id: "m1".into(),
                text: Arc::from("reply".to_string()),
                done: true,
                ..Default::default()
            })),
            Item::User {
                text: Arc::from("b"),
                created: None,
            },
        ];

        let blocks = blocks_from_store(&store);
        assert_eq!(blocks.len(), 3, "no pairing heuristics, no dropped items");
        assert!(matches!(blocks[0], Block::User { .. }));
        assert!(matches!(blocks[1], Block::Assistant { .. }));
        assert!(matches!(blocks[2], Block::User { .. }));
    }

    #[test]
    fn todowrite_parts_and_compactions_get_their_own_blocks() {
        use ox_client::store::{CompactionItem, CompactionStatus};
        use ox_client::todo::{Todo, TodoStatus};

        let mut store = DataStore::default();
        store.items = vec![
            Item::Assistant(Box::new(AssistantItem {
                id: "m1".into(),
                tools: vec![
                    ToolPart {
                        id: "t1".into(),
                        tool: "todowrite".into(),
                        state: ToolState::Success,
                        todos: Arc::new(vec![Todo {
                            content: "a".into(),
                            status: TodoStatus::Pending,
                        }]),
                        version: 4,
                        ..Default::default()
                    },
                    ToolPart {
                        id: "t2".into(),
                        tool: "shell".into(),
                        state: ToolState::Success,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })),
            Item::Compaction(Box::new(CompactionItem {
                id: "c1".into(),
                status: CompactionStatus::Completed,
                text: Arc::new("sum".into()),
                cancelled: false,
                tokens: None,
                version: 2,
            })),
        ];

        let blocks = blocks_from_store(&store);
        assert_eq!(blocks.len(), 4, "text, todo panel, shell row, boundary");
        assert!(matches!(
            &blocks[1],
            Block::Todo { id, version: 4, todos, .. } if id == "t1" && todos.len() == 1
        ));
        assert!(matches!(&blocks[2], Block::Tool { tool, .. } if tool == "shell"));
        assert!(matches!(
            &blocks[3],
            Block::Compaction { id, version: 2, status: CompactionStatus::Completed, .. }
                if id == "c1"
        ));
    }

    #[test]
    fn completion_items_project_in_place_as_completion_blocks() {
        use ox_client::store::{CompletionItem, CompletionSource};

        let mut store = DataStore::default();
        store.items = vec![
            Item::User {
                text: Arc::from("a"),
                created: None,
            },
            Item::Completion(CompletionItem {
                id: "n1".into(),
                source: CompletionSource::Subagent,
                state: Some(Arc::from("completed")),
                description: Arc::from("explore"),
                child_id: Some(Arc::from("ses_child")),
            }),
            Item::Assistant(Box::new(AssistantItem {
                id: "m1".into(),
                done: true,
                ..Default::default()
            })),
        ];

        let blocks = blocks_from_store(&store);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[0], Block::User { .. }));
        assert!(matches!(
            &blocks[1],
            Block::Completion { id, source: CompletionSource::Subagent, state: Some(state), description, child_id: Some(child) }
                if id == "n1" && &**state == "completed" && &**description == "explore" && &**child == "ses_child"
        ));
        assert!(matches!(blocks[2], Block::Assistant { .. }));
    }
}
