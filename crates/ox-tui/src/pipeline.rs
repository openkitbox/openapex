use std::sync::Arc;

use ox_client::store::{AssistantItem, DataStore, Item, ToolPart};
use ox_schema::support::FinishReason;

use crate::ui::timeline::{Block, Explored, TurnFooter};

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
                push_tools(&mut blocks, &assistant.tools);
            }
            Item::Compaction(compaction) => blocks.push(Block::Compaction {
                id: compaction.id.clone(),
                version: compaction.version,
                status: compaction.status,
                text: Arc::clone(&compaction.text),
                cancelled: compaction.cancelled,
                tokens: compaction.tokens,
            }),
        }
    }
    // Session-level failure/retry notices trail the transcript.
    for notice in &store.notices {
        let text: Arc<str> = Arc::from(notice.text.as_str());
        blocks.push(Block::System { text });
    }
    blocks
}

fn explored_kind(tool: &str) -> Option<fn(&mut Explored)> {
    match tool {
        "read" => Some(|e| e.reads += 1),
        "grep" | "glob" | "web_search" | "websearch" => Some(|e| e.searches += 1),
        "webfetch" => Some(|e| e.fetches += 1),
        _ => None,
    }
}

fn push_tools(blocks: &mut Vec<Block>, tools: &[ToolPart]) {
    let mut group: Option<Explored> = None;
    for tool in tools {
        if let Some(count) = explored_kind(&tool.tool) {
            let explored = group.get_or_insert_with(Explored::default);
            count(explored);
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
        });
    }
    if let Some(explored) = group {
        blocks.push(Block::Explored(explored));
    }
}

/// Upstream `AssistantFooter` appears only on a message that ended its turn
/// (`finish` is terminal, not `tool-calls`/`unknown`). Duration and tok/s span
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
    if !assistant.done || !terminal {
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
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ox_client::store::{AssistantItem, DataStore, Item, ToolPart, ToolState};

    use super::blocks_from_store;
    use crate::ui::timeline::Block;

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
            matches!(&blocks[2], Block::Explored(e) if e.reads == 1 && e.searches == 0 && !e.running)
        );
    }

    #[test]
    fn projects_notices_as_trailing_system_blocks() {
        let mut store = DataStore::default();
        store.items = vec![Item::User {
            text: Arc::from("a"),
            created: None,
        }];
        store.notices = vec![ox_client::store::Notice {
            kind: ox_client::store::NoticeKind::Error,
            text: "rate limited".into(),
        }];

        let blocks = blocks_from_store(&store);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[1], Block::System { text } if text.as_ref() == "rate limited"));
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
}
