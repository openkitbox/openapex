//! Headless data layer: event in, state out. No terminal, no IO — this is the
//! part the TUI tests most heavily.
//!
//! Two invariants keep the UI cheap:
//! 1. `items` is ONE ordered sequence (no parallel user/assistant vectors), so
//!    the block list is a 1:1 projection with no positional pairing heuristics.
//! 2. Every mutation bumps `version` and lowers `changed_from`; the UI compares
//!    `(id, version)` in O(1) and only re-wraps the changed tail. Text lives in
//!    an `Arc` so an unchanged block shares the store's buffer (no deep copy).

use std::collections::HashMap;
use std::sync::Arc;

use ox_schema::event::{EventEnvelope, SessionEventData};
use ox_schema::support::{FinishReason, TokenUsage};

use crate::form::{self, FormInfo};
use crate::todo::{Todo, parse_todos};

#[derive(Debug, Clone)]
pub enum Item {
    User {
        text: Arc<str>,
        /// Wire `time.created` (ms). `None` for the optimistic local echo.
        created: Option<i64>,
    },
    Assistant(Box<AssistantItem>),
    /// A context-compaction boundary (`session.compaction.*` live, or a
    /// `compaction` message from history).
    Compaction(Box<CompactionItem>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStatus {
    Running,
    Completed,
    Failed,
}

/// One compaction boundary. Live events carry no message id, so a live item
/// gets a local one; history replaces it wholesale on reload.
#[derive(Debug, Clone)]
pub struct CompactionItem {
    pub id: String,
    pub status: CompactionStatus,
    /// The summary while it streams and once complete; the error message when
    /// the compaction failed (upstream shows both in the same body slot).
    pub text: Arc<String>,
    /// `error.type == "aborted"`: a user cancel, shown without the error.
    pub cancelled: bool,
    /// Usage of the compaction request itself.
    pub tokens: Option<TokenUsage>,
    /// Bumped on every mutation; the UI's cache key.
    pub version: u64,
}

/// Lifecycle of one tool call, driven by the `session.tool.*` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolState {
    /// Input still streaming (`session.tool.input.delta`).
    #[default]
    Input,
    /// `tool.called` seen, not yet resolved.
    Running,
    /// `tool.progress` seen on top of `Running`.
    Progress,
    Success,
    Failed,
}

/// One tool call inside an assistant message. Events are keyed by the tool
/// part `id`, so out-of-order arrival is tolerated and finished parts ignore
/// late events.
#[derive(Debug, Clone, Default)]
pub struct ToolPart {
    pub id: String,
    /// Raw tool name from the wire (`shell`, `read`, `edit`, …).
    pub tool: String,
    pub state: ToolState,
    /// One-line summary (command / path / pattern). Empty when the input had
    /// nothing recognizable — the UI then shows the bare tool name.
    pub summary: String,
    /// Success content text or the failure message.
    pub output: Arc<String>,
    /// Per-part cache version (the UI renders one block per part).
    pub version: u64,
    /// Raw input text, streamed via `input.delta`; fallback for the summary
    /// when `tool.called` never arrives.
    pub input_text: Arc<String>,
    /// The list a `todowrite` call carries, parsed from its input. Empty for
    /// every other tool.
    pub todos: Arc<Vec<Todo>>,
}

#[derive(Debug, Clone, Default)]
pub struct AssistantItem {
    pub id: String,
    /// `Arc<String>` (not `Arc<str>`) so streaming deltas append via
    /// `Arc::make_mut(..).push_str(..)` — amortized O(1) when unique, one
    /// clone when the UI holds a snapshot. `Arc<str>` cannot grow in place.
    pub text: Arc<String>,
    pub reasoning: Arc<String>,
    pub done: bool,
    /// Bumped on every content mutation; the UI's cache key.
    pub version: u64,
    /// Highest `ordinal` applied so far. `None` also skips duplicate ordinal 0.
    pub last_ordinal: Option<i64>,
    /// Byte offset where the current text part started. `text.ended` carries
    /// the final text of ONE part, so it may only rewrite that slice —
    /// earlier parts must survive. `None` (no `text.started` seen) keeps the
    /// old replace-everything behavior.
    pub text_part_start: Option<usize>,
    /// Tool calls in first-seen order. Ordered after the message text — the
    /// wire gives tool parts no ordinal, so interleaving is not recoverable.
    pub tools: Vec<ToolPart>,
    /// Seconds spent reasoning, when `reasoning.started` was seen live.
    /// History-loaded reasoning has no duration on the wire.
    pub reasoning_ms: Option<u64>,
    pub reasoning_started: Option<std::time::Instant>,
    /// Wall-clock start of the turn that produced this message (envelope ms).
    pub turn_started_ms: Option<i64>,
    /// Wall-clock completion of the turn (envelope ms).
    pub turn_done_ms: Option<i64>,
    /// `output + reasoning` tokens across the message's steps.
    pub output_tokens: i64,
    /// Provider-active time: sum of `step.started → step.ended` (ms). Upstream
    /// divides tokens by this, not by the whole turn, so tool time is excluded.
    pub active_ms: i64,
    /// Money spent on this message (USD), summed over its steps.
    pub cost_usd: f64,
    /// Agent that produced the message (`build`), from the wire message or
    /// `step.started`.
    pub agent: Option<String>,
    /// `providerID/id` of the model that produced it.
    pub model: Option<String>,
    /// Why the last step stopped. `None` until a step ends.
    pub finish: Option<FinishReason>,
    /// Tokens the last step put in the context window (input + output +
    /// reasoning + cache). The footer's context figure.
    pub context_tokens: i64,
}

/// Session-level failure line (execution/step failures, scheduled retries).
/// Ephemeral: they live in the SSE stream, not in history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Error,
    Retry,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
}

/// A prompt queued while a run is active (server-side inbox, `steer`).
#[derive(Debug, Clone)]
pub struct Queued {
    pub inbox_id: String,
    pub text: Arc<str>,
}

#[derive(Debug, Default)]
pub struct DataStore {
    pub session: Option<String>,
    pub items: Vec<Item>,
    assistant_index: HashMap<String, usize>,
    pub running_since: Option<i64>,
    pub model: Option<String>,
    pub agent: Option<String>,
    /// Session-level failure/retry notices, newest last.
    pub notices: Vec<Notice>,
    /// Prompts accepted but not yet delivered.
    pub queued: Vec<Queued>,
    /// Forms awaiting an answer (`form.created` not yet replied/cancelled),
    /// oldest first. The `question` tool is the only producer today.
    pub pending_forms: Vec<FormInfo>,
    /// Last durable seq **of the focused session only**.
    pub last_seq: Option<i64>,
    /// First item index whose content changed since the last UI sync.
    pub changed_from: usize,
    /// Dispatch timestamp of the step in flight; steps never overlap, so one
    /// slot (not per-item state) is enough to accumulate `active_ms`.
    step_started_ms: Option<i64>,
}

impl DataStore {
    fn ensure_assistant(&mut self, message_id: &str) -> usize {
        if let Some(index) = self.assistant_index.get(message_id) {
            return *index;
        }
        let index = self.items.len();
        // The turn clock starts at `execution.started`; an assistant message is
        // created after it, so this is the first chance to stamp the start.
        let turn_started_ms = self.running_since;
        self.items.push(Item::Assistant(Box::new(AssistantItem {
            id: message_id.to_owned(),
            turn_started_ms,
            ..Default::default()
        })));
        self.assistant_index.insert(message_id.to_owned(), index);
        self.changed_from = self.changed_from.min(index);
        index
    }

    fn touch(&mut self, index: usize) {
        self.changed_from = self.changed_from.min(index);
    }

    /// The compaction still in flight, if any. Compactions never overlap, so
    /// the last running one is the one the live events refer to.
    fn running_compaction(&self) -> Option<usize> {
        self.items.iter().rposition(
            |item| matches!(item, Item::Compaction(c) if c.status == CompactionStatus::Running),
        )
    }

    /// The item an ended/failed event settles: the running one, or a fresh
    /// one when `started` was missed (reconnect between the two).
    fn settle_compaction(&mut self, event_id: &str) -> usize {
        if let Some(index) = self.running_compaction() {
            return index;
        }
        self.items.push(Item::Compaction(Box::new(CompactionItem {
            id: format!("live:{event_id}"),
            status: CompactionStatus::Running,
            text: Arc::default(),
            cancelled: false,
            tokens: None,
            version: 0,
        })));
        self.items.len() - 1
    }

    /// Authoritative replacement after a reconnect/reload.
    /// The form the UI should show: the oldest still pending.
    pub fn pending_form(&self) -> Option<&FormInfo> {
        self.pending_forms.first()
    }

    pub fn replace_items(&mut self, items: Vec<Item>) {
        self.items = items;
        self.assistant_index.clear();
        for (index, item) in self.items.iter().enumerate() {
            if let Item::Assistant(assistant) = item {
                self.assistant_index.insert(assistant.id.clone(), index);
            }
        }
        self.changed_from = 0;
    }

    /// Optimistic local echo of a submitted prompt.
    pub fn push_user(&mut self, text: &str) {
        self.items.push(Item::User {
            text: Arc::from(text),
            created: None,
        });
        let index = self.items.len() - 1;
        self.touch(index);
    }

    /// Take the dirty watermark so the next mutation starts a new one.
    pub fn claim_changed_from(&mut self) -> usize {
        std::mem::replace(&mut self.changed_from, usize::MAX)
    }

    /// Context-window size after the latest step that reported one.
    pub fn context_tokens(&self) -> Option<i64> {
        self.context_usage().map(|(tokens, _)| tokens)
    }

    /// Like [`Self::context_tokens`], plus the `providerID/id` of the model
    /// that produced the figure (upstream looks the window up by that model).
    pub fn context_usage(&self) -> Option<(i64, Option<&str>)> {
        self.items.iter().rev().find_map(|item| match item {
            Item::Assistant(assistant) if assistant.context_tokens > 0 => {
                Some((assistant.context_tokens, assistant.model.as_deref()))
            }
            _ => None,
        })
    }

    /// Session spend (USD): derived from the messages rather than kept as
    /// separate state, so history reloads cannot make it drift.
    pub fn total_cost(&self) -> f64 {
        self.items
            .iter()
            .map(|item| match item {
                Item::Assistant(assistant) => assistant.cost_usd,
                _ => 0.0,
            })
            .sum()
    }
}

pub fn reduce(store: &mut DataStore, event: &EventEnvelope<SessionEventData>) {
    if let Some(durable) = &event.durable
        && (store.session.is_none()
            || Some(durable.aggregate_id.as_str()) == store.session.as_deref())
    {
        store.last_seq = Some(durable.seq);
    }

    match &event.data {
        SessionEventData::TextStarted {
            assistant_message_id,
            ordinal,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                // A second text part starts a new paragraph; the flat text
                // model keeps parts joined with a blank line.
                if item.last_ordinal.is_none_or(|last| *ordinal > last) {
                    if !item.text.is_empty() {
                        Arc::make_mut(&mut item.text).push_str("\n\n");
                    }
                    item.text_part_start = Some(item.text.len());
                    item.last_ordinal = Some(*ordinal);
                }
            }
        }
        SessionEventData::TextDelta {
            assistant_message_id,
            ordinal,
            delta,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            let Item::Assistant(item) = &mut store.items[index] else {
                return;
            };
            if item.done {
                return; // history is authoritative; replayed deltas must not append
            }
            // `ordinal` numbers the part, not the delta: every delta of the
            // current part shares it. Only a delta of an older part is stale.
            if item.last_ordinal.is_some_and(|last| *ordinal < last) {
                return;
            }
            if item.last_ordinal.is_none() {
                item.text_part_start.get_or_insert(item.text.len());
            }
            Arc::make_mut(&mut item.text).push_str(delta);
            item.last_ordinal = Some(*ordinal);
            item.version += 1;
            store.touch(index);
        }
        SessionEventData::TextEnded {
            assistant_message_id,
            text,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                // The wire's final text covers only THIS part: rewrite the
                // part's slice, never the whole body. `done` stays with the
                // execution events — a message may have more parts coming.
                if let Some(start) = item.text_part_start.take()
                    && start <= item.text.len()
                {
                    Arc::make_mut(&mut item.text).truncate(start);
                } else {
                    item.text = Arc::default();
                }
                Arc::make_mut(&mut item.text).push_str(text);
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ReasoningStarted {
            assistant_message_id,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.reasoning_started = Some(std::time::Instant::now());
            }
        }
        SessionEventData::ReasoningDelta {
            assistant_message_id,
            delta,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                if item.done {
                    return; // history is authoritative; replayed deltas must not append
                }
                Arc::make_mut(&mut item.reasoning).push_str(delta);
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ReasoningEnded {
            assistant_message_id,
            text,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.reasoning = Arc::new(text.clone());
                item.reasoning_ms = item
                    .reasoning_started
                    .take()
                    .map(|started| started.elapsed().as_millis() as u64);
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ToolInputStarted {
            assistant_message_id,
            id,
            name,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.tools.push(ToolPart {
                    id: id.clone(),
                    tool: name.clone(),
                    state: ToolState::Input,
                    summary: String::new(),
                    output: Arc::default(),
                    version: 0,
                    input_text: Arc::default(),
                    todos: Arc::default(),
                });
            }
            store.touch(index);
        }
        SessionEventData::ToolInputDelta {
            assistant_message_id,
            id,
            delta,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index]
                && let Some(part) = item.tools.iter_mut().find(|part| part.id == *id)
            {
                Arc::make_mut(&mut part.input_text).push_str(delta);
                part.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ToolInputEnded {
            assistant_message_id,
            id,
            text,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index]
                && let Some(part) = item.tools.iter_mut().find(|part| part.id == *id)
            {
                part.input_text = Arc::new(text.clone());
                part.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ToolCalled {
            assistant_message_id,
            id,
            input,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index]
                && let Some(part) = live_part(item, id)
            {
                part.state = ToolState::Running;
                part.summary = tool_summary(&part.tool, input);
                part.todos = todos_of(&part.tool, input);
                part.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ToolProgress {
            assistant_message_id,
            id,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index]
                && let Some(part) = live_part(item, id)
            {
                part.state = ToolState::Progress;
                part.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ToolSuccess {
            assistant_message_id,
            id,
            content,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index]
                && let Some(part) = live_part(item, id)
            {
                part.state = ToolState::Success;
                if part.summary.is_empty() {
                    part.summary = summary_from_input_text(&part.tool, &part.input_text);
                }
                if part.todos.is_empty() {
                    part.todos = todos_from_input_text(&part.tool, &part.input_text);
                }
                part.output = Arc::new(join_text(content));
                part.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ToolFailed {
            assistant_message_id,
            id,
            error,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index]
                && let Some(part) = live_part(item, id)
            {
                part.state = ToolState::Failed;
                if part.summary.is_empty() {
                    part.summary = summary_from_input_text(&part.tool, &part.input_text);
                }
                part.output = Arc::new(error.message.clone());
                part.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ExecutionStarted { .. } => {
            store.running_since = Some(event.created);
        }
        SessionEventData::ExecutionSucceeded { .. }
        | SessionEventData::ExecutionInterrupted { .. } => {
            store.running_since = None;
            finish_last_assistant(store, event.created);
        }
        SessionEventData::ExecutionFailed { error, .. } => {
            store.running_since = None;
            finish_last_assistant(store, event.created);
            store.notices.push(Notice {
                kind: NoticeKind::Error,
                text: error.message.clone(),
            });
            store.changed_from = 0; // notices live outside `items`
        }
        SessionEventData::StepStarted {
            assistant_message_id,
            agent,
            model,
            started,
            ..
        } => {
            store.step_started_ms = Some(*started);
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.agent = Some(agent.clone());
                item.model = Some(format!("{}/{}", model.provider_id, model.id));
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::StepEnded {
            assistant_message_id,
            finish,
            cost,
            tokens,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            let active_ms = store
                .step_started_ms
                .take()
                .map(|started| (event.created - started).max(0))
                .unwrap_or(0);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.output_tokens += tokens.output + tokens.reasoning;
                item.active_ms += active_ms;
                item.cost_usd += money_usd(cost);
                item.finish = Some(*finish);
                item.context_tokens = total_tokens(tokens);
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::SessionRetryScheduled { attempt, error, .. } => {
            store.notices.push(Notice {
                kind: NoticeKind::Retry,
                text: format!("retry {attempt} · {}", error.message),
            });
            store.changed_from = 0;
        }
        SessionEventData::StepFailed { error, .. } => {
            store.notices.push(Notice {
                kind: NoticeKind::Error,
                text: error.message.clone(),
            });
            store.changed_from = 0;
        }
        SessionEventData::InboxEnqueued {
            inbox_id,
            item: ox_schema::inbox::Item::User { payload, .. },
            ..
        } => {
            store.queued.push(Queued {
                inbox_id: inbox_id.clone(),
                text: Arc::from(payload.prompt.text.as_str()),
            });
            store.changed_from = 0;
        }
        SessionEventData::InboxDelivered { inbox_id, .. }
        | SessionEventData::InboxCancelled { inbox_id, .. } => {
            store.queued.retain(|queued| queued.inbox_id != *inbox_id);
            store.changed_from = 0;
        }
        SessionEventData::SessionModelSelected { model, .. } => {
            store.model = Some(format!("{}/{}", model.provider_id, model.id));
        }
        SessionEventData::SessionAgentSelected { agent, .. } => {
            store.agent = Some(agent.clone());
        }
        SessionEventData::CompactionStarted { .. } => {
            store.items.push(Item::Compaction(Box::new(CompactionItem {
                id: format!("live:{}", event.id),
                status: CompactionStatus::Running,
                text: Arc::default(),
                cancelled: false,
                tokens: None,
                version: 0,
            })));
            let index = store.items.len() - 1;
            store.touch(index);
        }
        SessionEventData::CompactionDelta { text, .. } => {
            if let Some(index) = store.running_compaction()
                && let Item::Compaction(item) = &mut store.items[index]
            {
                Arc::make_mut(&mut item.text).push_str(text);
                item.version += 1;
                store.touch(index);
            }
        }
        SessionEventData::CompactionEnded { text, tokens, .. } => {
            let index = store.settle_compaction(&event.id);
            if let Item::Compaction(item) = &mut store.items[index] {
                item.status = CompactionStatus::Completed;
                item.text = Arc::new(text.clone());
                item.tokens = *tokens;
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::CompactionFailed { error, tokens, .. } => {
            let index = store.settle_compaction(&event.id);
            if let Item::Compaction(item) = &mut store.items[index] {
                item.status = CompactionStatus::Failed;
                item.cancelled = error.kind == "aborted";
                item.text = Arc::new(error.message.clone());
                item.tokens = *tokens;
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::Unknown { r#type, data } => {
            form::apply_event(
                &mut store.pending_forms,
                store.session.as_deref(),
                r#type,
                data,
            );
        }
        _ => {}
    }
}

/// On run end the last assistant message becomes immutable: replayed deltas
/// must not append afterwards.
/// `MoneyUsd` is the wire's `Finite` (int or float); cost math needs f64.
pub(crate) fn money_usd(value: &ox_schema::support::Finite) -> f64 {
    match value {
        ox_schema::support::Finite::Int(n) => *n as f64,
        ox_schema::support::Finite::Float(n) => *n,
    }
}

fn finish_last_assistant(store: &mut DataStore, done_ms: i64) {
    if let Some(index) = store
        .items
        .iter()
        .rposition(|item| matches!(item, Item::Assistant(_)))
    {
        if let Item::Assistant(item) = &mut store.items[index] {
            item.done = true;
            item.turn_done_ms = Some(done_ms);
            if item.turn_started_ms.is_none() {
                item.turn_started_ms = Some(done_ms);
            }
            item.version += 1;
        }
        store.touch(index);
    }
}

/// Everything one step put in the context window.
pub fn total_tokens(tokens: &TokenUsage) -> i64 {
    tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write
}

/// The tool part still accepting events (`Success`/`Failed` are terminal).
fn live_part<'a>(item: &'a mut AssistantItem, id: &str) -> Option<&'a mut ToolPart> {
    item.tools
        .iter_mut()
        .find(|part| part.id == id && !matches!(part.state, ToolState::Success | ToolState::Failed))
}

fn join_text(content: &[ox_schema::support::Content]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            ox_schema::support::Content::Text { text } => Some(text.as_str()),
            ox_schema::support::Content::File { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One-line summary from the parsed `tool.called` input. Recognizes the input
/// keys ox's own tools use; unknown tools get an empty summary and the UI
/// falls back to the bare name. Single line, hard-bounded.
pub(crate) fn tool_summary(tool: &str, input: &ox_schema::support::Record) -> String {
    let key = match tool {
        "shell" => "command",
        "read" | "write" | "edit" | "patch" => "path",
        "grep" | "glob" => "pattern",
        "web_search" => "query",
        "webfetch" => "url",
        "skill" => "name",
        _ => return String::new(),
    };
    bound_summary(
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    )
}

/// The todo list a `todowrite` call carries; empty for any other tool.
pub(crate) fn todos_of(tool: &str, input: &ox_schema::support::Record) -> Arc<Vec<Todo>> {
    if tool != "todowrite" {
        return Arc::default();
    }
    Arc::new(parse_todos(input.get("todos")))
}

/// Fallback when `tool.called` never arrived: the streamed input text.
pub(crate) fn todos_from_input_text(tool: &str, input_text: &str) -> Arc<Vec<Todo>> {
    if tool != "todowrite" {
        return Arc::default();
    }
    let value = serde_json::from_str::<serde_json::Value>(input_text).ok();
    Arc::new(parse_todos(value.as_ref().and_then(|v| v.get("todos"))))
}

/// Fallback when `tool.called` never arrived: parse the streamed input text.
pub(crate) fn summary_from_input_text(tool: &str, input_text: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input_text) else {
        return String::new();
    };
    let Some(map) = value.as_object() else {
        return String::new();
    };
    let record: ox_schema::support::Record = map
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    tool_summary(tool, &record)
}

/// Collapse to one line and bound the length, keeping whole words where the
/// whitespace allows.
fn bound_summary(text: &str) -> String {
    const MAX: usize = 72;
    let single_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= MAX {
        return single_line;
    }
    let mut cut: String = single_line.chars().take(MAX).collect();
    if let Some(space) = cut.rfind(' ') {
        cut.truncate(space);
    }
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use ox_schema::event::EventEnvelope;
    use serde_json::json;

    use super::{DataStore, Item, NoticeKind, ToolState, reduce};

    fn env(kind: &str, data: serde_json::Value) -> EventEnvelope<serde_json::Value> {
        serde_json::from_value(json!({
            "id": "evt_1", "created": 1000, "type": kind, "data": data
        }))
        .unwrap()
    }

    fn apply(store: &mut DataStore, kind: &str, data: serde_json::Value) {
        reduce(store, &env(kind, data).parse_typed());
    }

    fn assistant(store: &DataStore) -> &super::AssistantItem {
        store
            .items
            .iter()
            .find_map(|item| match item {
                Item::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .unwrap()
    }

    /// `form.created` exactly as the server publishes it
    /// (`ox-tools/src/form.rs:publish`): `{form: Form.Info}`.
    fn form_created(id: &str, session: &str) -> serde_json::Value {
        json!({"form": {
            "id": id, "sessionID": session, "title": "Pick",
            "fields": [{
                "type": "string", "key": "q0", "title": "Stack",
                "description": "Which stack?", "required": true,
                "options": [{"value": "a", "label": "A", "description": "first"}],
                "custom": true
            }]
        }})
    }

    #[test]
    fn form_created_becomes_the_pending_form() {
        let mut store = DataStore::default();
        apply(&mut store, "form.created", form_created("frm_1", "ses_1"));
        let form = store.pending_form().expect("form pending");
        assert_eq!(form.id, "frm_1");
        assert_eq!(form.session_id, "ses_1");
        assert_eq!(form.title, "Pick");
        assert_eq!(form.fields.len(), 1);
    }

    #[test]
    fn duplicate_form_created_is_idempotent() {
        let mut store = DataStore::default();
        apply(&mut store, "form.created", form_created("frm_1", "ses_1"));
        apply(&mut store, "form.created", form_created("frm_1", "ses_1"));
        assert_eq!(store.pending_forms.len(), 1);
    }

    #[test]
    fn form_replied_and_cancelled_clear_only_their_form() {
        let mut store = DataStore::default();
        apply(&mut store, "form.created", form_created("frm_1", "ses_1"));
        apply(&mut store, "form.created", form_created("frm_2", "ses_1"));
        assert_eq!(store.pending_form().unwrap().id, "frm_1");

        apply(
            &mut store,
            "form.replied",
            json!({"id": "frm_1", "sessionID": "ses_1", "answer": {"q0": "a"}}),
        );
        assert_eq!(store.pending_form().unwrap().id, "frm_2");

        apply(
            &mut store,
            "form.cancelled",
            json!({"id": "frm_9", "sessionID": "ses_1"}),
        );
        assert_eq!(store.pending_forms.len(), 1, "unknown id is a no-op");

        apply(
            &mut store,
            "form.cancelled",
            json!({"id": "frm_2", "sessionID": "ses_1"}),
        );
        assert!(store.pending_form().is_none());
    }

    #[test]
    fn form_for_another_session_is_ignored() {
        let mut store = DataStore {
            session: Some("ses_1".into()),
            ..Default::default()
        };
        apply(
            &mut store,
            "form.created",
            form_created("frm_1", "ses_other"),
        );
        apply(&mut store, "form.created", form_created("frm_2", "global"));
        assert!(store.pending_forms.is_empty());
        apply(&mut store, "form.created", form_created("frm_3", "ses_1"));
        assert_eq!(store.pending_form().unwrap().id, "frm_3");
    }

    #[test]
    fn malformed_form_created_is_dropped_and_unknown_field_kinds_survive() {
        let mut store = DataStore::default();
        apply(&mut store, "form.created", json!({"form": {"id": "frm_1"}}));
        assert!(store.pending_forms.is_empty());

        // Upstream also has number/integer/external fields; the server does
        // not emit them yet, but the form must still be listed.
        apply(
            &mut store,
            "form.created",
            json!({"form": {"id": "frm_2", "sessionID": "ses_1", "title": "t",
                "fields": [{"type": "number", "key": "n"}]}}),
        );
        assert_eq!(store.pending_form().unwrap().fields.len(), 1);
    }

    #[test]
    fn tool_parts_stream_call_and_finish() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"shell"}),
        );
        assert_eq!(assistant(&store).tools.len(), 1);
        assert_eq!(assistant(&store).tools[0].tool, "shell");
        assert_eq!(assistant(&store).tools[0].state, ToolState::Input);

        apply(
            &mut store,
            "session.tool.called",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "input":{"command":"git status"},"executed":true}),
        );
        assert_eq!(assistant(&store).tools[0].state, ToolState::Running);
        assert_eq!(assistant(&store).tools[0].summary, "git status");

        apply(
            &mut store,
            "session.tool.success",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "content":[{"type":"text","text":"On main"}],"executed":true}),
        );
        assert_eq!(assistant(&store).tools[0].state, ToolState::Success);
        assert_eq!(&*assistant(&store).tools[0].output, "On main");
    }

    #[test]
    fn tool_failure_carries_the_error_message() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"shell"}),
        );
        apply(
            &mut store,
            "session.tool.failed",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "error":{"type":"execution","message":"boom"},"executed":true}),
        );
        let tool = &assistant(&store).tools[0];
        assert_eq!(tool.state, ToolState::Failed);
        assert_eq!(&*tool.output, "boom");
    }

    #[test]
    fn finished_tools_ignore_late_events() {
        let mut store = DataStore::default();
        for (kind, extra) in [
            (
                "session.tool.input.started",
                json!({"id":"part_1","name":"shell"}),
            ),
            (
                "session.tool.called",
                json!({"id":"part_1","input":{},"executed":true}),
            ),
            (
                "session.tool.success",
                json!({"id":"part_1","content":[{"type":"text","text":"done"}],"executed":true}),
            ),
            (
                "session.tool.failed",
                json!({"id":"part_1","error":{"type":"x","message":"late"},"executed":true}),
            ),
        ] {
            let mut data = extra;
            data["sessionID"] = json!("ses_1");
            data["assistantMessageID"] = json!("msg_1");
            apply(&mut store, kind, data);
        }
        let tool = &assistant(&store).tools[0];
        assert_eq!(tool.state, ToolState::Success);
        assert_eq!(&*tool.output, "done");
    }

    #[test]
    fn tool_input_stream_is_a_summary_fallback_without_called() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"read"}),
        );
        apply(
            &mut store,
            "session.tool.input.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","delta":"{\"path\":\"a"}),
        );
        apply(
            &mut store,
            "session.tool.input.ended",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "text":"{\"path\":\"a.txt\"}"}),
        );
        // No `tool.called` arrived: the summary is derived when the outcome is.
        apply(
            &mut store,
            "session.tool.success",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "content":[{"type":"text","text":"ok"}],"executed":true}),
        );
        assert_eq!(assistant(&store).tools[0].summary, "a.txt");
    }

    #[test]
    fn unknown_tool_yields_no_summary() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"mcp_thing"}),
        );
        apply(
            &mut store,
            "session.tool.called",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "input":{"foo":"bar"},"executed":true}),
        );
        assert_eq!(assistant(&store).tools[0].summary, "");
    }

    #[test]
    fn summaries_are_single_line_and_bounded() {
        let long = format!("{}-tail", "x".repeat(120));
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"shell"}),
        );
        apply(
            &mut store,
            "session.tool.called",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "input":{"command":format!("head\n{long}")},"executed":true}),
        );
        let summary = &assistant(&store).tools[0].summary;
        assert!(!summary.contains('\n'), "{summary:?}");
        assert!(summary.chars().count() <= 80, "{summary:?}");
        assert!(summary.starts_with("head"), "{summary:?}");
        assert!(summary.ends_with('…'), "{summary:?}");
    }

    #[test]
    fn execution_failure_retry_and_step_failure_become_notices() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.execution.failed",
            json!({"sessionID":"ses_1","error":{"type":"provider","message":"rate limited"}}),
        );
        assert_eq!(store.notices.len(), 1);
        assert_eq!(store.notices[0].kind, NoticeKind::Error);
        assert_eq!(store.notices[0].text, "rate limited");

        apply(
            &mut store,
            "session.retry.scheduled",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","attempt":2,"at":123,
                   "error":{"type":"provider","message":"timeout"}}),
        );
        assert_eq!(store.notices[1].kind, NoticeKind::Retry);
        assert!(
            store.notices[1].text.contains("2"),
            "{:?}",
            store.notices[1].text
        );
        assert!(
            store.notices[1].text.contains("timeout"),
            "{:?}",
            store.notices[1].text
        );

        apply(
            &mut store,
            "session.step.failed",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1",
                   "error":{"type":"provider","message":"bad request"}}),
        );
        assert_eq!(store.notices[2].text, "bad request");
    }

    #[test]
    fn inbox_events_track_the_queue() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.inbox.enqueued",
            json!({"sessionID":"ses_1","inboxID":"inb_1",
                   "item":{"type":"user","payload":{"text":"hi again"},"delivery":"steer"}}),
        );
        assert_eq!(store.queued.len(), 1);
        assert_eq!(&*store.queued[0].text, "hi again");

        // A non-user item must not create a chip.
        apply(
            &mut store,
            "session.inbox.enqueued",
            json!({"sessionID":"ses_1","inboxID":"inb_2",
                   "item":{"type":"synthetic","payload":{"text":"sys"},"delivery":"steer"}}),
        );
        assert_eq!(store.queued.len(), 1);

        apply(
            &mut store,
            "session.inbox.delivered",
            json!({"sessionID":"ses_1","inboxID":"inb_1"}),
        );
        assert!(store.queued.is_empty());
    }

    #[test]
    fn reasoning_duration_is_recorded_when_seen_live() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.reasoning.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0}),
        );
        apply(
            &mut store,
            "session.reasoning.ended",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1,"text":"thought"}),
        );
        assert_eq!(&*assistant(&store).reasoning, "thought");
        assert!(
            assistant(&store).reasoning_ms.is_some(),
            "live reasoning records a duration"
        );
    }

    #[test]
    fn multi_part_text_messages_join_with_paragraph_breaks() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0}),
        );
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0,"delta":"part one"}),
        );
        apply(
            &mut store,
            "session.text.ended",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0,"text":"part one"}),
        );
        apply(
            &mut store,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1}),
        );
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1,"delta":"part"}),
        );
        apply(
            &mut store,
            "session.text.ended",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1,"text":"part two"}),
        );
        // The run end — not the part end — closes the message.
        apply(
            &mut store,
            "session.execution.succeeded",
            json!({"sessionID":"ses_1"}),
        );
        let item = assistant(&store);
        assert_eq!(item.text.as_str(), "part one\n\npart two");
        assert!(item.done);
    }

    #[test]
    fn every_delta_of_one_part_is_applied() {
        // The wire's `ordinal` numbers the PART, not the delta: all deltas of
        // a part share it (ox-engine runner/step.rs). Dropping repeats would
        // freeze the text at its first chunk until `text.ended`.
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0}),
        );
        for delta in ["Hel", "lo ", "world"] {
            apply(
                &mut store,
                "session.text.delta",
                json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0,"delta":delta}),
            );
        }
        assert_eq!(assistant(&store).text.as_str(), "Hello world");
        apply(
            &mut store,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1}),
        );
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1,"delta":"a"}),
        );
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1,"delta":"b"}),
        );
        assert_eq!(assistant(&store).text.as_str(), "Hello world\n\nab");
    }

    #[test]
    fn text_deltas_assemble_and_stale_parts_are_skipped() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1,"delta":"你"}),
        );
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1,"delta":"好"}),
        );
        // A delta of an earlier part is stale.
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0,"delta":"x"}),
        );
        assert_eq!(assistant(&store).text.as_ref(), "你好");
        assert_eq!(
            assistant(&store).version,
            2,
            "only real appends bump version"
        );
    }

    #[test]
    fn text_ended_is_authoritative() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0}),
        );
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0,"delta":"partial"}),
        );
        apply(
            &mut store,
            "session.text.ended",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0,"text":"final"}),
        );
        assert_eq!(assistant(&store).text.as_ref(), "final");
        assert!(
            !assistant(&store).done,
            "a part end must not close the message — execution events do"
        );
    }

    #[test]
    fn execution_events_toggle_running_state() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.execution.started",
            json!({"sessionID":"ses_1"}),
        );
        assert_eq!(store.running_since, Some(1000));
        apply(
            &mut store,
            "session.execution.succeeded",
            json!({"sessionID":"ses_1"}),
        );
        assert_eq!(store.running_since, None);
    }

    #[test]
    fn model_and_agent_selection_are_tracked() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.model.selected",
            json!({"sessionID":"ses_1","model":{"providerID":"p","id":"m"}}),
        );
        apply(
            &mut store,
            "session.agent.selected",
            json!({"sessionID":"ses_1","agent":"build"}),
        );
        assert_eq!(store.model.as_deref(), Some("p/m"));
        assert_eq!(store.agent.as_deref(), Some("build"));
    }

    #[test]
    fn changed_from_marks_only_the_mutated_item() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0}),
        );
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0,"delta":"a"}),
        );
        store.changed_from = usize::MAX; // simulate "already synced"
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":1,"delta":"b"}),
        );
        assert_eq!(store.changed_from, 0);
    }

    #[test]
    fn durable_seq_is_per_session() {
        let mut store = DataStore::default();
        let envelope: EventEnvelope<serde_json::Value> = serde_json::from_value(json!({
            "id":"evt_9","created":1,"type":"session.text.delta",
            "data":{"sessionID":"ses_1","assistantMessageID":"m","ordinal":0,"delta":"x"},
            "durable":{"aggregateID":"ses_1","seq":41,"version":1}
        }))
        .unwrap();
        reduce(&mut store, &envelope.parse_typed());
        assert_eq!(store.last_seq, Some(41));
    }

    #[test]
    fn done_messages_ignore_replayed_deltas() {
        let mut store = DataStore::default();
        store.replace_items(vec![Item::Assistant(Box::new(super::AssistantItem {
            id: "msg_1".into(),
            text: std::sync::Arc::new("final".to_string()),
            reasoning: std::sync::Arc::new(String::new()),
            done: true,
            version: 0,
            last_ordinal: None,
            ..Default::default()
        }))]);
        apply(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0,"delta":"dup"}),
        );
        apply(
            &mut store,
            "session.reasoning.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","delta":"dup"}),
        );
        let Item::Assistant(item) = &store.items[0] else {
            panic!()
        };
        assert_eq!(item.text.as_str(), "final");
        assert_eq!(item.version, 0, "skipped mutations do not bump version");
    }

    /// Same as `apply`, but with an explicit envelope timestamp: the usage
    /// assertions below are about the clock, so a frozen `created` cannot work.
    fn apply_at(store: &mut DataStore, created: i64, kind: &str, data: serde_json::Value) {
        let envelope: EventEnvelope<serde_json::Value> = serde_json::from_value(json!({
            "id": "evt_1", "created": created, "type": kind, "data": data
        }))
        .unwrap();
        reduce(store, &envelope.parse_typed());
    }

    fn step_tokens() -> serde_json::Value {
        json!({
            "input": 10,
            "output": 60,
            "reasoning": 10,
            "cache": {"read": 0, "write": 0},
        })
    }

    #[test]
    fn turn_usage_tracks_step_tokens_and_active_time() {
        let mut store = DataStore::default();
        apply_at(
            &mut store,
            1_000,
            "session.execution.started",
            json!({"sessionID":"ses_1"}),
        );
        apply_at(
            &mut store,
            1_100,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0}),
        );
        apply_at(
            &mut store,
            1_200,
            "session.step.started",
            json!({
                "sessionID":"ses_1",
                "assistantMessageID":"msg_1",
                "agent":"build",
                "model":{"id":"m","providerID":"p"},
                "started":1_200,
            }),
        );
        apply_at(
            &mut store,
            1_500,
            "session.step.ended",
            json!({
                "sessionID":"ses_1",
                "assistantMessageID":"msg_1",
                "finish":"stop",
                "cost":0.0,
                "tokens": step_tokens(),
            }),
        );
        apply_at(
            &mut store,
            2_400,
            "session.execution.succeeded",
            json!({"sessionID":"ses_1"}),
        );

        let item = assistant(&store);
        assert_eq!(item.output_tokens, 70, "output + reasoning tokens");
        assert_eq!(item.active_ms, 300, "step started 1200 → ended 1500");
        assert_eq!(item.turn_started_ms, Some(1_000));
        assert_eq!(item.turn_done_ms, Some(2_400));
    }

    #[test]
    fn step_cost_accumulates_into_the_session_total() {
        let mut store = DataStore::default();
        apply_at(
            &mut store,
            1_000,
            "session.execution.started",
            json!({"sessionID":"ses_1"}),
        );
        apply_at(
            &mut store,
            1_100,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0}),
        );
        for (started, ended, cost) in [(1_200, 1_500, 0.0125), (1_600, 1_800, 0.01)] {
            apply_at(
                &mut store,
                started,
                "session.step.started",
                json!({
                    "sessionID":"ses_1",
                    "assistantMessageID":"msg_1",
                    "agent":"build",
                    "model":{"id":"m","providerID":"p"},
                    "started":started,
                }),
            );
            apply_at(
                &mut store,
                ended,
                "session.step.ended",
                json!({
                    "sessionID":"ses_1",
                    "assistantMessageID":"msg_1",
                    "finish":"stop",
                    "cost": cost,
                    "tokens": step_tokens(),
                }),
            );
        }
        assert!((store.total_cost() - 0.0225).abs() < 1e-9);
    }

    #[test]
    fn a_failed_execution_still_closes_the_turn_clock() {
        let mut store = DataStore::default();
        apply_at(
            &mut store,
            5_000,
            "session.execution.started",
            json!({"sessionID":"ses_1"}),
        );
        apply_at(
            &mut store,
            5_100,
            "session.text.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","ordinal":0}),
        );
        apply_at(
            &mut store,
            6_500,
            "session.execution.failed",
            json!({
                "sessionID":"ses_1",
                "error":{"type":"provider","message":"boom"},
            }),
        );

        let item = assistant(&store);
        assert!(item.done);
        assert_eq!(item.turn_started_ms, Some(5_000));
        assert_eq!(item.turn_done_ms, Some(6_500));
    }

    fn compaction(store: &DataStore) -> Vec<&super::CompactionItem> {
        store
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Compaction(c) => Some(&**c),
                _ => None,
            })
            .collect()
    }

    fn todowrite_events(store: &mut DataStore) {
        apply(
            store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"todowrite"}),
        );
        apply(
            store,
            "session.tool.called",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "input":{"todos":[
                       {"content":"write tests","status":"completed"},
                       {"content":"ship it","status":"in_progress"}]},
                   "executed":true}),
        );
    }

    #[test]
    fn todowrite_input_becomes_the_parts_todo_list() {
        let mut store = DataStore::default();
        todowrite_events(&mut store);
        apply(
            &mut store,
            "session.tool.success",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "content":[{"type":"text","text":"ok"}],"executed":true}),
        );
        let part = &assistant(&store).tools[0];
        assert_eq!(part.state, ToolState::Success);
        assert_eq!(part.todos.len(), 2);
        assert_eq!(part.todos[0].content, "write tests");
        assert_eq!(part.todos[1].status, crate::todo::TodoStatus::InProgress);
    }

    #[test]
    fn todos_fall_back_to_the_streamed_input_when_called_never_arrived() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"todowrite"}),
        );
        apply(
            &mut store,
            "session.tool.input.ended",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "text":"{\"todos\":[{\"content\":\"a\",\"status\":\"pending\"}]}"}),
        );
        apply(
            &mut store,
            "session.tool.success",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "content":[],"executed":true}),
        );
        assert_eq!(assistant(&store).tools[0].todos.len(), 1);
    }

    #[test]
    fn other_tools_carry_no_todos() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"read"}),
        );
        apply(
            &mut store,
            "session.tool.called",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "input":{"path":"a.txt","todos":[{"content":"x"}]},"executed":true}),
        );
        assert!(assistant(&store).tools[0].todos.is_empty());
    }

    #[test]
    fn compaction_events_drive_one_boundary_item() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.compaction.started",
            json!({"sessionID":"ses_1","reason":"auto","recent":""}),
        );
        assert_eq!(
            compaction(&store)[0].status,
            super::CompactionStatus::Running
        );

        apply(
            &mut store,
            "session.compaction.delta",
            json!({"sessionID":"ses_1","text":"## Goal\n"}),
        );
        apply(
            &mut store,
            "session.compaction.delta",
            json!({"sessionID":"ses_1","text":"ship"}),
        );
        assert_eq!(compaction(&store)[0].text.as_str(), "## Goal\nship");

        apply(
            &mut store,
            "session.compaction.ended",
            json!({"sessionID":"ses_1","reason":"auto","recent":"","text":"## Goal\nshipped",
                   "tokens":{"input":1200,"output":300,"reasoning":0,"cache":{"read":10,"write":5}}}),
        );
        let items = compaction(&store);
        assert_eq!(items.len(), 1, "ended settles the running item");
        assert_eq!(items[0].status, super::CompactionStatus::Completed);
        assert_eq!(items[0].text.as_str(), "## Goal\nshipped");
        assert_eq!(items[0].tokens.unwrap().input, 1200);
        assert!(items[0].version >= 3);
    }

    #[test]
    fn failed_compaction_keeps_the_error_and_flags_a_cancel() {
        let mut store = DataStore::default();
        for (kind, message, cancelled) in [("provider", "boom", false), ("aborted", "stop", true)] {
            apply(
                &mut store,
                "session.compaction.started",
                json!({"sessionID":"ses_1","reason":"manual","recent":""}),
            );
            apply(
                &mut store,
                "session.compaction.failed",
                json!({"sessionID":"ses_1","reason":"manual",
                       "error":{"type":kind,"message":message}}),
            );
            let last = *compaction(&store).last().unwrap();
            assert_eq!(last.status, super::CompactionStatus::Failed);
            assert_eq!(last.text.as_str(), message);
            assert_eq!(last.cancelled, cancelled);
        }
        assert_eq!(compaction(&store).len(), 2);
    }

    #[test]
    fn ended_without_started_still_leaves_a_boundary() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.compaction.ended",
            json!({"sessionID":"ses_1","reason":"auto","recent":"","text":"s"}),
        );
        let items = compaction(&store);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, super::CompactionStatus::Completed);
    }
}
