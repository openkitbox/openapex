//! Headless data layer: event in, state out. No terminal, no IO — this is the
//! part the TUI tests most heavily.
//!
//! Two invariants keep the UI cheap:
//! 1. `items` is ONE ordered sequence (no parallel user/assistant vectors), so
//!    the block list is a 1:1 projection with no positional pairing heuristics.
//! 2. Every mutation bumps `version` and lowers `changed_from`; the UI compares
//!    `(id, version)` in O(1) and only re-wraps the changed tail. Text lives in
//!    an `Arc` so an unchanged block shares the store's buffer (no deep copy).
//!
//! Session-level rows (upstream `routes/session/rows.ts`) are data here, no
//! display text. Deviations:
//! - Queued compactions are tracked as inbox ids (`queued_compactions`) from
//!   enqueue until delivered or cancelled, not as message rows.
//!   They come from live inbox events only (no REST call lists the inbox),
//!   so a compaction queued before the TUI opened shows no row until
//!   delivered.
//! - [`ShownError`] is not rebuilt on a history reload: after a reload in
//!   mid-turn, a run failure repeating the step's error gets its own row.
//! - A live switch with no `previous` on the wire falls back to the store's
//!   current agent/model; the model fallback is split on the first `/` and
//!   carries no variant.
//! - A history `system` message has no description, so it reads as a bare
//!   `Instructions updated` notice.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ox_schema::event::{EventEnvelope, SessionEventData};
use ox_schema::support::{FinishReason, Record, TokenUsage};

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
    /// A shell / subagent completion notice (`↳ Shell finished · cmd`),
    /// parsed from a `Synthetic` message by [`CompletionItem::from_synthetic`].
    Completion(CompletionItem),
    /// An agent / model / location change (`*-switched` messages, or the
    /// `session.agent.selected` / `model.selected` / `moved` events).
    Switch(Switch),
    /// A system notice (`system` message, `session.instructions.updated`) or
    /// a loaded skill (`skill` message).
    Notice(Notice),
    /// A run failure the turn's last step or compaction did not already show
    /// (`idle.error`, `session.execution.failed`; upstream `reduceSessionRows`
    /// `shownError`).
    Error(ErrorInfo),
}

/// A session error as the wire names it: `type` and `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorInfo {
    pub kind: Arc<str>,
    pub message: Arc<str>,
}

impl From<&ox_schema::support::SessionError> for ErrorInfo {
    fn from(error: &ox_schema::support::SessionError) -> Self {
        Self {
            kind: Arc::from(error.kind.as_str()),
            message: Arc::from(error.message.as_str()),
        }
    }
}

/// The error the turn's last step or settled compaction already shows; a
/// run failure repeating it gets no row of its own. One rule for the live
/// reducer and history parsing (upstream `rows.ts` `reduceSessionRows`
/// `shownError`): every assistant step and settled compaction replaces it,
/// and the turn's end consumes it. User messages do not touch it.
#[derive(Debug, Default)]
pub struct ShownError(Option<ErrorInfo>);

impl ShownError {
    /// An assistant step (its error or none) or a settled compaction (its
    /// failure, or none when completed) is now the latest thing shown.
    pub fn show(&mut self, error: Option<ErrorInfo>) {
        self.0 = error;
    }

    /// The turn ended with `error`: the row to add, if the error differs in
    /// type or message from the one already shown. Resets for the next turn.
    #[must_use]
    pub fn end_turn(&mut self, error: Option<ErrorInfo>) -> Option<ErrorInfo> {
        let shown = self.0.take();
        error.filter(|error| shown.as_ref() != Some(error))
    }
}

/// A scheduled retry of a failed step (`retry` on an assistant message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryInfo {
    pub attempt: i64,
    /// Wall-clock ms the retry fires; the UI derives the countdown from it.
    pub at: i64,
    pub message: Arc<str>,
}

/// A model reference as the wire names it (`providerID`, `id`, `variant`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub provider_id: Arc<str>,
    pub id: Arc<str>,
    pub variant: Option<Arc<str>>,
}

impl From<&ox_schema::support::ModelRef> for ModelChoice {
    fn from(model: &ox_schema::support::ModelRef) -> Self {
        Self {
            provider_id: Arc::from(model.provider_id.as_str()),
            id: Arc::from(model.id.as_str()),
            variant: model.variant.as_deref().map(Arc::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Switch {
    Agent {
        agent: Arc<str>,
        previous: Option<Arc<str>>,
    },
    Model {
        model: ModelChoice,
        previous: Option<ModelChoice>,
    },
    Location {
        directory: Arc<str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// `description` is the wire's (`Instructions updated: AGENTS.md`).
    System {
        description: Option<Arc<str>>,
    },
    Skill {
        name: Arc<str>,
    },
}

/// Who produced a completion notice (`metadata.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSource {
    Shell,
    Subagent,
}

/// One completion notice. Immutable once built, so it carries no version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// Inbox / message id.
    pub id: String,
    pub source: CompletionSource,
    /// The wire `state` word verbatim (`completed`/`error`/`cancelled`, or
    /// anything else); `None` when absent. Never normalised here.
    pub state: Option<Arc<str>>,
    /// Shell: whitespace runs collapsed to one space and trimmed. Subagent:
    /// verbatim. Empty means no description.
    pub description: Arc<str>,
    /// `metadata.childID`, subagent notices only.
    pub child_id: Option<Arc<str>>,
}

impl CompletionItem {
    /// The single parse point shared by history and the live path. `None`
    /// unless `metadata.source` is `"shell"` or `"subagent"`.
    pub fn from_synthetic(
        id: &str,
        metadata: Option<&ox_schema::support::Record>,
        description: Option<&str>,
    ) -> Option<Self> {
        let metadata = metadata?;
        let text = |key: &str| metadata.get(key).and_then(serde_json::Value::as_str);
        let source = match text("source")? {
            "shell" => CompletionSource::Shell,
            "subagent" => CompletionSource::Subagent,
            _ => return None,
        };
        let description = description.unwrap_or("");
        let (description, child_id): (Arc<str>, Option<Arc<str>>) = match source {
            CompletionSource::Shell => {
                let mut collapsed = String::with_capacity(description.len());
                for word in description.split_whitespace() {
                    if !collapsed.is_empty() {
                        collapsed.push(' ');
                    }
                    collapsed.push_str(word);
                }
                (Arc::from(collapsed), None)
            }
            CompletionSource::Subagent => (Arc::from(description), text("childID").map(Arc::from)),
        };
        Some(Self {
            id: id.to_owned(),
            source,
            state: text("state").map(Arc::from),
            description,
            child_id,
        })
    }
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

/// The child (subagent) session a `subagent` tool call started or resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRef {
    pub session_id: String,
}

impl ChildRef {
    /// The tool name the server registers the subagent tool under
    /// (`ox-tools/src/subagent.rs`, `Tool::name`).
    pub const SUBAGENT_TOOL: &'static str = "subagent";

    /// The single parse point shared by history and the live path. The
    /// subagent tool puts `{sessionID, status, description}` in its result
    /// metadata, both for a background launch and a foreground completion.
    /// Other tools' metadata is never read.
    pub fn from_metadata(
        tool: &str,
        metadata: Option<&ox_schema::support::Record>,
    ) -> Option<Self> {
        if tool != Self::SUBAGENT_TOOL {
            return None;
        }
        let session_id = metadata?.get("sessionID")?.as_str()?;
        Some(Self {
            session_id: session_id.to_owned(),
        })
    }

    /// Whether some tool part in `items` names `session_id` as its child.
    fn referenced_in(items: &[Item], session_id: &str) -> bool {
        items.iter().any(|item| match item {
            Item::Assistant(assistant) => assistant.tools.iter().any(|part| {
                part.child
                    .as_ref()
                    .is_some_and(|c| c.session_id == session_id)
            }),
            _ => false,
        })
    }
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
    /// The child session of a `subagent` call, once its result is known.
    pub child: Option<ChildRef>,
    /// The parsed tool input (`tool.called` live, the wire state's `input` in
    /// history), kept whole so renderers can read any tool-specific field.
    /// Empty until the call is known, and stays empty when only streamed
    /// input text arrived.
    pub input: Arc<Record>,
    /// The result metadata of a finished call (`tool.success`/`tool.failed`
    /// live, `completed`/`error` in history), e.g. glob `count` or shell
    /// `shellID`. `None` while running or when the result carried none.
    pub metadata: Option<Arc<Record>>,
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
    /// Why the message's last step failed (`step.failed`, wire `error`).
    pub error: Option<ErrorInfo>,
    /// The retry scheduled after a failed attempt (`session.retry.scheduled`,
    /// wire `retry`); cleared when the next attempt starts or the run ends.
    pub retry: Option<RetryInfo>,
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
    /// Known child sessions (see [`ChildRef`]) whose turn is in flight.
    /// Derived from `session.execution.*` events alone; see
    /// [`DataStore::observe_foreign`].
    running_children: HashSet<String>,
    /// Children whose REST probe is in flight. An execution event for a child
    /// removes it from here, so the (older) probe result is then discarded:
    /// the event stream is newer than the answer to a request sent before it.
    probing: HashSet<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    /// Inbox ids of compactions enqueued but not yet delivered or cancelled,
    /// oldest first (upstream `CompactionQueued`).
    pub queued_compactions: Vec<String>,
    /// Prompts accepted but not yet delivered.
    pub queued: Vec<Queued>,
    /// Completion notices enqueued but not yet delivered, by inbox id. Moved
    /// into `items` on delivery; dropped on cancel and on history reload.
    pending_completions: HashMap<String, CompletionItem>,
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
    /// See [`ShownError`]; derived from live events only, so a reload in
    /// mid-turn starts it empty.
    shown_error: ShownError,
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

    /// End the turn: record its failure unless the turn's last step or
    /// settled compaction already shows it ([`ShownError`]).
    fn end_turn(&mut self, error: Option<ErrorInfo>) {
        if let Some(error) = self.shown_error.end_turn(error) {
            self.items.push(Item::Error(error));
            let index = self.items.len() - 1;
            self.touch(index);
        }
    }

    /// Push a row-only item (switch, notice) at the end of the transcript.
    fn push_row(&mut self, item: Item) {
        self.items.push(item);
        let index = self.items.len() - 1;
        self.touch(index);
    }

    /// Authoritative replacement after a reconnect/reload.
    /// The form the UI should show: the oldest still pending.
    pub fn pending_form(&self) -> Option<&FormInfo> {
        self.pending_forms.first()
    }

    /// Something is running for this session: its own turn, or a child
    /// session it delegated to. Display only; interrupts keep using
    /// `running_since`, which child events never touch.
    pub fn busy(&self) -> bool {
        self.running_since.is_some() || !self.running_children.is_empty()
    }

    /// Whether `session_id` is a known child whose turn is in flight.
    pub fn is_child_running(&self, session_id: &str) -> bool {
        self.running_children.contains(session_id)
    }

    /// Feed an event that belongs to a session OTHER than the focused one.
    /// The caller (the TUI event loop) must route every such event here
    /// instead of dropping it, and must NOT pass it to [`reduce`]. Only
    /// `session.execution.*` of a child already named by a tool part of the
    /// current items has an effect; everything else is ignored, so other
    /// clients' sessions on the same server cannot make this one look busy.
    ///
    /// Limitation: a child's `execution.started` that arrives before the
    /// parent's tool result names the child is ignored (the id is not known
    /// yet), and so is a `succeeded` seen in that window. The REST rebuild
    /// after history load is responsible for that window.
    ///
    /// Returns whether the event named a known child (so the caller needs to
    /// repaint); `false` for every event that changed nothing.
    pub fn observe_foreign(&mut self, event: &EventEnvelope<SessionEventData>) -> bool {
        let (session_id, running) = match &event.data {
            SessionEventData::ExecutionStarted { session_id } => (session_id, true),
            SessionEventData::ExecutionSucceeded { session_id }
            | SessionEventData::ExecutionFailed { session_id, .. }
            | SessionEventData::ExecutionInterrupted { session_id, .. } => (session_id, false),
            _ => return false,
        };
        // Fail closed: the focused session is `reduce`'s business.
        if !self.is_foreign_child(session_id) {
            return false;
        }
        self.probing.remove(session_id);
        self.set_running(session_id, running);
        true
    }

    fn set_running(&mut self, session_id: &str, running: bool) {
        if running {
            self.running_children.insert(session_id.to_owned());
        } else {
            self.running_children.remove(session_id);
        }
    }

    /// Whether `session_id` is a child the current items name and not the
    /// focused session itself (`reduce`'s business).
    fn is_foreign_child(&self, session_id: &str) -> bool {
        self.session.as_deref() != Some(session_id)
            && ChildRef::referenced_in(&self.items, session_id)
    }

    /// Set a child's running state from a source other than the event stream
    /// (the REST probe). Same fail-closed rule as [`observe_foreign`]: only a
    /// child some tool part names is touched. Returns whether it acted.
    pub fn set_child_running(&mut self, session_id: &str, running: bool) -> bool {
        if !self.is_foreign_child(session_id) {
            return false;
        }
        self.set_running(session_id, running);
        true
    }

    /// Every child session the items name, once each, in first-seen order.
    pub fn referenced_children(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut children = Vec::new();
        for item in &self.items {
            let Item::Assistant(assistant) = item else {
                continue;
            };
            for child in assistant
                .tools
                .iter()
                .filter_map(|part| part.child.as_ref())
            {
                if seen.insert(child.session_id.as_str()) {
                    children.push(child.session_id.clone());
                }
            }
        }
        children
    }

    /// The child of tool part `part_id` inside assistant message `message_id`.
    pub fn tool_child(&self, message_id: &str, part_id: &str) -> Option<&ChildRef> {
        let index = *self.assistant_index.get(message_id)?;
        let Item::Assistant(assistant) = self.items.get(index)? else {
            return None;
        };
        assistant
            .tools
            .iter()
            .find(|part| part.id == part_id)?
            .child
            .as_ref()
    }

    /// Note that a REST probe for each referenced child in `ids` is about to
    /// be sent. Must be called before the request goes out.
    pub fn mark_probing(&mut self, ids: &[String]) {
        for id in ids {
            if self.is_foreign_child(id) {
                self.probing.insert(id.clone());
            }
        }
    }

    /// Apply a probe answer unless an execution event for the child arrived
    /// after [`mark_probing`] (or the child is no longer referenced). Each
    /// `mark_probing` is consumed by one answer. Returns whether it applied.
    pub fn apply_probe(&mut self, session_id: &str, running: bool) -> bool {
        self.probing.remove(session_id) && self.set_child_running(session_id, running)
    }

    pub fn replace_items(&mut self, items: Vec<Item>) {
        self.items = items;
        let items = &self.items;
        self.running_children
            .retain(|id| ChildRef::referenced_in(items, id));
        self.probing.retain(|id| ChildRef::referenced_in(items, id));
        self.assistant_index.clear();
        self.pending_completions.clear();
        self.shown_error = ShownError::default();
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
                    child: None,
                    input: Arc::default(),
                    metadata: None,
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
                part.input = Arc::new(input.clone());
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
            metadata,
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
                part.child = ChildRef::from_metadata(&part.tool, metadata.as_ref());
                part.metadata = metadata.clone().map(Arc::new);
                part.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ToolFailed {
            assistant_message_id,
            id,
            error,
            metadata,
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
                part.metadata = metadata.clone().map(Arc::new);
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
            store.end_turn(None);
        }
        SessionEventData::ExecutionFailed { error, .. } => {
            store.running_since = None;
            finish_last_assistant(store, event.created);
            store.end_turn(Some(ErrorInfo::from(error)));
        }
        SessionEventData::StepStarted {
            assistant_message_id,
            agent,
            model,
            started,
            ..
        } => {
            store.step_started_ms = Some(*started);
            // A new attempt supersedes the previous message's pending retry
            // (the projector closes the stale assistant in place).
            if !store.assistant_index.contains_key(assistant_message_id)
                && let Some(index) = last_assistant(store)
                && let Item::Assistant(item) = &mut store.items[index]
                && item.retry.take().is_some()
            {
                item.version += 1;
                store.touch(index);
            }
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.agent = Some(agent.clone());
                item.model = Some(format!("{}/{}", model.provider_id, model.id));
                // A retried step restarts the message (message_updater
                // StepStarted on an existing row).
                item.error = None;
                item.retry = None;
                item.finish = None;
                item.version += 1;
            }
            store.shown_error.show(None);
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
        SessionEventData::SessionRetryScheduled {
            assistant_message_id,
            attempt,
            at,
            error,
            ..
        } => {
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.retry = Some(RetryInfo {
                    attempt: *attempt,
                    at: *at,
                    message: Arc::from(error.message.as_str()),
                });
                item.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::StepFailed {
            assistant_message_id,
            error,
            finish,
            ..
        } => {
            let error = ErrorInfo::from(error);
            store.shown_error.show(Some(error.clone()));
            let index = store.ensure_assistant(assistant_message_id);
            if let Item::Assistant(item) = &mut store.items[index] {
                item.error = Some(error);
                item.retry = None;
                item.finish = Some(match finish {
                    Some(ox_schema::event::StepFailedFinish::ContentFilter) => {
                        FinishReason::ContentFilter
                    }
                    None => FinishReason::Error,
                });
                item.version += 1;
            }
            store.touch(index);
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
        SessionEventData::InboxEnqueued {
            inbox_id,
            item: ox_schema::inbox::Item::Synthetic { payload, .. },
            ..
        } => {
            // No chip: a completion notice only appears once delivered.
            if let Some(notice) = CompletionItem::from_synthetic(
                inbox_id,
                payload.metadata.as_ref(),
                payload.description.as_deref(),
            ) {
                store.pending_completions.insert(inbox_id.clone(), notice);
            }
        }
        SessionEventData::InboxEnqueued {
            inbox_id,
            item: ox_schema::inbox::Item::Compaction { .. },
            ..
        } => {
            if !store.queued_compactions.contains(inbox_id) {
                store.queued_compactions.push(inbox_id.clone());
                store.changed_from = 0;
            }
        }
        SessionEventData::InboxDelivered { inbox_id, .. } => {
            store.queued.retain(|queued| queued.inbox_id != *inbox_id);
            store.queued_compactions.retain(|id| id != inbox_id);
            // Timeline position = delivery time; a repeat finds nothing to move.
            if let Some(notice) = store.pending_completions.remove(inbox_id) {
                store.items.push(Item::Completion(notice));
            }
            store.changed_from = 0;
        }
        SessionEventData::InboxCancelled { inbox_id, .. } => {
            store.queued.retain(|queued| queued.inbox_id != *inbox_id);
            store.queued_compactions.retain(|id| id != inbox_id);
            store.pending_completions.remove(inbox_id);
            store.changed_from = 0;
        }
        SessionEventData::SessionModelSelected {
            model, previous, ..
        } => {
            // `previous` falls back to the session's current model, as the
            // projector falls back to the session row.
            let previous = previous.as_ref().map(ModelChoice::from).or_else(|| {
                let current = store.model.as_deref()?;
                let (provider_id, id) = current.split_once('/')?;
                Some(ModelChoice {
                    provider_id: Arc::from(provider_id),
                    id: Arc::from(id),
                    variant: None,
                })
            });
            store.push_row(Item::Switch(Switch::Model {
                model: ModelChoice::from(model),
                previous,
            }));
            store.model = Some(format!("{}/{}", model.provider_id, model.id));
        }
        SessionEventData::SessionAgentSelected {
            agent, previous, ..
        } => {
            let previous = previous
                .as_deref()
                .or(store.agent.as_deref())
                .map(Arc::from);
            store.push_row(Item::Switch(Switch::Agent {
                agent: Arc::from(agent.as_str()),
                previous,
            }));
            store.agent = Some(agent.clone());
        }
        SessionEventData::SessionMoved { payload, .. } => {
            store.push_row(Item::Switch(Switch::Location {
                directory: Arc::from(payload.location.directory.as_str()),
            }));
        }
        SessionEventData::SessionInstructionsUpdated { delta, text, .. } => {
            // Upstream message-updater: no text = bookkeeping, no row. The
            // description is the projector's (message_updater.rs).
            if text.as_deref().is_some_and(|text| !text.is_empty()) {
                let keys: Vec<&str> = delta.keys().map(String::as_str).collect();
                let description = format!("Instructions updated: {}", keys.join(", "));
                store.push_row(Item::Notice(Notice::System {
                    description: Some(Arc::from(description)),
                }));
            }
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
            store.shown_error.show(None);
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
            store.shown_error.show(Some(ErrorInfo::from(error)));
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

/// `MoneyUsd` is the wire's `Finite` (int or float); cost math needs f64.
pub(crate) fn money_usd(value: &ox_schema::support::Finite) -> f64 {
    match value {
        ox_schema::support::Finite::Int(n) => *n as f64,
        ox_schema::support::Finite::Float(n) => *n,
    }
}

fn last_assistant(store: &DataStore) -> Option<usize> {
    store
        .items
        .iter()
        .rposition(|item| matches!(item, Item::Assistant(_)))
}

/// On run end the last assistant message becomes immutable: replayed deltas
/// must not append afterwards, and no retry is pending any more.
fn finish_last_assistant(store: &mut DataStore, done_ms: i64) {
    if let Some(index) = last_assistant(store) {
        if let Item::Assistant(item) = &mut store.items[index] {
            item.retry = None;
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

    use super::{
        ChildRef, CompletionItem, CompletionSource, DataStore, ErrorInfo, Item, ToolState, reduce,
    };

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
    fn step_failure_and_retry_live_on_the_assistant_and_a_new_error_is_an_item() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.retry.scheduled",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","attempt":2,"at":123,
                   "error":{"type":"provider","message":"timeout"}}),
        );
        let retry = assistant(&store).retry.clone().expect("retry recorded");
        assert_eq!(
            (retry.attempt, retry.at, &*retry.message),
            (2, 123, "timeout")
        );

        apply(
            &mut store,
            "session.step.failed",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1",
                   "error":{"type":"provider","message":"bad request"}}),
        );
        let item = assistant(&store);
        assert!(item.retry.is_none(), "a failed step ends the retry");
        assert_eq!(
            item.error.as_ref().map(|e| &*e.message),
            Some("bad request")
        );
        assert_eq!(item.finish, Some(ox_schema::support::FinishReason::Error));

        // The same error again is already on screen; a different one is not.
        for message in ["bad request", "rate limited"] {
            apply(
                &mut store,
                "session.execution.failed",
                json!({"sessionID":"ses_1","error":{"type":"provider","message":message}}),
            );
        }
        let errors: Vec<&ErrorInfo> = store
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Error(error) => Some(error),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(&*errors[0].message, "rate limited");
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

    fn enqueue_synthetic(store: &mut DataStore, inbox_id: &str, metadata: serde_json::Value) {
        apply(
            store,
            "session.inbox.enqueued",
            json!({"sessionID":"ses_1","inboxID":inbox_id,
                   "item":{"type":"synthetic","delivery":"steer",
                           "payload":{"text":"body","description":"ls  -la","metadata":metadata}}}),
        );
    }

    fn deliver(store: &mut DataStore, inbox_id: &str) {
        apply(
            store,
            "session.inbox.delivered",
            json!({"sessionID":"ses_1","inboxID":inbox_id}),
        );
    }

    fn completions(store: &DataStore) -> Vec<&CompletionItem> {
        store
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Completion(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn shell_meta() -> serde_json::Value {
        json!({"source":"shell","shellID":"sh_1","state":"completed"})
    }

    #[test]
    fn synthetic_shell_notice_lands_on_delivery_after_existing_items() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.step.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","agent":"build",
                   "model":{"id":"m","providerID":"p"},"started":1}),
        );
        let before = store.items.len();

        enqueue_synthetic(&mut store, "inb_9", shell_meta());
        assert_eq!(store.items.len(), before, "enqueue adds no item");
        assert!(store.queued.is_empty(), "synthetic creates no chip");

        store.claim_changed_from();
        deliver(&mut store, "inb_9");
        assert_eq!(store.items.len(), before + 1);
        let Some(Item::Completion(c)) = store.items.last() else {
            panic!("trailing completion expected: {:?}", store.items.last());
        };
        assert_eq!(c.id, "inb_9");
        assert_eq!(c.source, CompletionSource::Shell);
        assert_eq!(&*c.description, "ls -la");
        assert!(
            store.changed_from <= before,
            "subscribers must redraw the tail"
        );
    }

    #[test]
    fn cancelled_synthetic_never_becomes_a_notice() {
        let mut store = DataStore::default();
        enqueue_synthetic(&mut store, "inb_9", shell_meta());
        apply(
            &mut store,
            "session.inbox.cancelled",
            json!({"sessionID":"ses_1","inboxID":"inb_9"}),
        );
        deliver(&mut store, "inb_9");
        assert!(completions(&store).is_empty());
    }

    #[test]
    fn duplicate_delivery_yields_one_notice() {
        let mut store = DataStore::default();
        enqueue_synthetic(&mut store, "inb_9", shell_meta());
        deliver(&mut store, "inb_9");
        deliver(&mut store, "inb_9");
        assert_eq!(completions(&store).len(), 1);
    }

    #[test]
    fn non_completion_synthetics_never_surface() {
        let mut store = DataStore::default();
        enqueue_synthetic(&mut store, "inb_1", json!({"source":"goal"}));
        enqueue_synthetic(&mut store, "inb_2", json!({}));
        apply(
            &mut store,
            "session.inbox.enqueued",
            json!({"sessionID":"ses_1","inboxID":"inb_3",
                   "item":{"type":"synthetic","payload":{"text":"sys"},"delivery":"steer"}}),
        );
        for id in ["inb_1", "inb_2", "inb_3"] {
            deliver(&mut store, id);
        }
        assert!(store.items.is_empty(), "{:?}", store.items);
    }

    #[test]
    fn replace_items_drops_pending_completions() {
        let mut store = DataStore::default();
        enqueue_synthetic(&mut store, "inb_9", shell_meta());
        store.replace_items(Vec::new());
        deliver(&mut store, "inb_9");
        assert!(completions(&store).is_empty());
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

    fn meta(value: serde_json::Value) -> ox_schema::support::Record {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn from_synthetic_parses_shell_states_verbatim() {
        for state in ["completed", "error", "cancelled", "weird"] {
            let metadata = meta(json!({"source": "shell", "shellID": "sh_1", "state": state}));
            let item = CompletionItem::from_synthetic("inb_1", Some(&metadata), Some("ls"))
                .expect("shell notice parses");
            assert_eq!(item.id, "inb_1");
            assert_eq!(item.source, CompletionSource::Shell);
            assert_eq!(item.state.as_deref(), Some(state));
            assert_eq!(&*item.description, "ls");
            assert_eq!(item.child_id, None);
        }
    }

    #[test]
    fn from_synthetic_subagent_keeps_child_id_and_description_verbatim() {
        let metadata =
            meta(json!({"source": "subagent", "childID": "ses_c", "state": "completed"}));
        let item = CompletionItem::from_synthetic("inb_2", Some(&metadata), Some("  a\n\tb   c "))
            .expect("subagent notice parses");
        assert_eq!(item.source, CompletionSource::Subagent);
        assert_eq!(item.child_id.as_deref(), Some("ses_c"));
        assert_eq!(&*item.description, "  a\n\tb   c ");
    }

    #[test]
    fn from_synthetic_shell_description_collapses_whitespace() {
        let metadata = meta(json!({"source": "shell", "state": "completed"}));
        let item = CompletionItem::from_synthetic(
            "inb_3",
            Some(&metadata),
            Some("  echo   hi\n\tthere \n"),
        )
        .unwrap();
        assert_eq!(&*item.description, "echo hi there");
    }

    #[test]
    fn from_synthetic_absent_description_is_empty() {
        let metadata = meta(json!({"source": "shell", "state": "completed"}));
        let item = CompletionItem::from_synthetic("inb_4", Some(&metadata), None).unwrap();
        assert_eq!(&*item.description, "");
    }

    #[test]
    fn from_synthetic_missing_or_non_string_state_is_none() {
        let missing = meta(json!({"source": "shell"}));
        let item = CompletionItem::from_synthetic("i", Some(&missing), None).unwrap();
        assert_eq!(item.state, None);
        let numeric = meta(json!({"source": "shell", "state": 3}));
        let item = CompletionItem::from_synthetic("i", Some(&numeric), None).unwrap();
        assert_eq!(item.state, None);
    }

    #[test]
    fn from_synthetic_rejects_other_sources() {
        assert!(CompletionItem::from_synthetic("i", None, Some("x")).is_none());
        let no_source = meta(json!({"state": "completed"}));
        assert!(CompletionItem::from_synthetic("i", Some(&no_source), None).is_none());
        let goal = meta(json!({"source": "goal", "state": "completed"}));
        assert!(CompletionItem::from_synthetic("i", Some(&goal), None).is_none());
        let non_string = meta(json!({"source": 7}));
        assert!(CompletionItem::from_synthetic("i", Some(&non_string), None).is_none());
    }

    #[test]
    fn from_synthetic_shell_ignores_child_id() {
        let metadata = meta(json!({"source": "shell", "state": "completed", "childID": "ses_x"}));
        let item = CompletionItem::from_synthetic("i", Some(&metadata), None).unwrap();
        assert_eq!(item.child_id, None);
    }

    // -- Child (subagent) sessions -------------------------------------------

    fn record(value: serde_json::Value) -> ox_schema::support::Record {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn child_ref_from_metadata_table() {
        let with_id = record(json!({"sessionID":"ses_c","status":"running"}));
        let no_id = record(json!({"status":"running"}));
        let numeric = record(json!({"sessionID": 7}));
        let cases: [(&str, Option<&ox_schema::support::Record>, Option<&str>); 6] = [
            ("subagent", Some(&with_id), Some("ses_c")),
            ("shell", Some(&with_id), None),
            ("subagent", None, None),
            ("subagent", Some(&no_id), None),
            ("subagent", Some(&numeric), None),
            ("", Some(&with_id), None),
        ];
        for (tool, metadata, expected) in cases {
            let got = ChildRef::from_metadata(tool, metadata);
            assert_eq!(
                got.as_ref().map(|c| c.session_id.as_str()),
                expected,
                "tool={tool:?} metadata={metadata:?}"
            );
        }
    }

    fn subagent_call(store: &mut DataStore, metadata: serde_json::Value) {
        apply(
            store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"subagent"}),
        );
        apply(
            store,
            "session.tool.success",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "content":[{"type":"text","text":"working"}],
                   "metadata": metadata, "executed":true}),
        );
    }

    #[test]
    fn live_tool_success_records_the_child_and_bumps_the_version() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"subagent"}),
        );
        let before = assistant(&store).tools[0].version;
        apply(
            &mut store,
            "session.tool.success",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "content":[{"type":"text","text":"working"}],
                   "metadata":{"sessionID":"ses_c","status":"running","description":"d"},
                   "executed":true}),
        );
        let part = &assistant(&store).tools[0];
        assert_eq!(part.child.as_ref().unwrap().session_id, "ses_c");
        assert!(part.version > before, "the UI line cache keys on version");
    }

    #[test]
    fn live_tool_success_without_metadata_has_no_child() {
        let mut store = DataStore::default();
        subagent_call(&mut store, json!(null));
        assert!(assistant(&store).tools[0].child.is_none());
    }

    fn child_event(store: &mut DataStore, kind: &str, data: serde_json::Value) {
        store.observe_foreign(&env(kind, data).parse_typed());
    }

    fn store_with_child() -> DataStore {
        let mut store = DataStore {
            session: Some("ses_1".to_owned()),
            ..DataStore::default()
        };
        subagent_call(
            &mut store,
            json!({"sessionID":"ses_c","status":"running","description":"d"}),
        );
        store
    }

    #[test]
    fn child_execution_events_toggle_running_children_and_busy() {
        let interrupted = json!({"sessionID":"ses_c","reason":"user"});
        let failed = json!({"sessionID":"ses_c","error":{"type":"x","message":"boom"}});
        let ended = [
            ("session.execution.succeeded", json!({"sessionID":"ses_c"})),
            ("session.execution.failed", failed),
            ("session.execution.interrupted", interrupted),
        ];
        for (kind, data) in ended {
            let mut store = store_with_child();
            assert!(!store.busy());
            child_event(
                &mut store,
                "session.execution.started",
                json!({"sessionID":"ses_c"}),
            );
            assert!(store.is_child_running("ses_c"), "{kind}");
            assert!(store.busy(), "{kind}");
            child_event(&mut store, kind, data);
            assert!(!store.is_child_running("ses_c"), "{kind}");
            assert!(!store.busy(), "{kind}");
        }
    }

    #[test]
    fn unknown_foreign_session_is_ignored() {
        let mut store = store_with_child();
        child_event(
            &mut store,
            "session.execution.started",
            json!({"sessionID":"ses_other"}),
        );
        assert!(!store.busy());
        assert!(!store.is_child_running("ses_other"));
    }

    #[test]
    fn non_execution_foreign_events_are_ignored() {
        let mut store = store_with_child();
        child_event(
            &mut store,
            "session.text.delta",
            json!({"sessionID":"ses_c","assistantMessageID":"m","ordinal":0,"delta":"x"}),
        );
        assert!(!store.busy());
        assert_eq!(store.items.len(), 1, "foreign events never reach items");
    }

    #[test]
    fn child_events_never_touch_running_since() {
        let mut store = store_with_child();
        child_event(
            &mut store,
            "session.execution.started",
            json!({"sessionID":"ses_c"}),
        );
        assert_eq!(store.running_since, None);
        apply(
            &mut store,
            "session.execution.started",
            json!({"sessionID":"ses_1"}),
        );
        assert_eq!(store.running_since, Some(1000));
        child_event(
            &mut store,
            "session.execution.succeeded",
            json!({"sessionID":"ses_c"}),
        );
        assert_eq!(store.running_since, Some(1000), "parent turn unaffected");
        apply(
            &mut store,
            "session.execution.succeeded",
            json!({"sessionID":"ses_1"}),
        );
        assert_eq!(store.running_since, None);
    }

    #[test]
    fn parent_turn_end_does_not_clear_running_children() {
        let mut store = store_with_child();
        child_event(
            &mut store,
            "session.execution.started",
            json!({"sessionID":"ses_c"}),
        );
        apply(
            &mut store,
            "session.execution.started",
            json!({"sessionID":"ses_1"}),
        );
        apply(
            &mut store,
            "session.execution.succeeded",
            json!({"sessionID":"ses_1"}),
        );
        assert_eq!(store.running_since, None);
        assert!(store.busy(), "the child is still running");
    }

    #[test]
    fn replace_items_keeps_referenced_children_and_drops_the_rest() {
        let mut store = store_with_child();
        child_event(
            &mut store,
            "session.execution.started",
            json!({"sessionID":"ses_c"}),
        );
        let kept = store.items.clone();
        store.replace_items(kept);
        assert!(store.is_child_running("ses_c"), "still referenced");
        store.replace_items(Vec::new());
        assert!(!store.is_child_running("ses_c"), "no longer referenced");
        assert!(!store.busy());
    }

    // -- Probe feed: referenced children and REST-derived state --------------

    #[test]
    fn referenced_children_lists_each_child_once_in_order() {
        let mut store = store_with_child();
        // A second call resumes the same child; a third names another one.
        for (message, part, child) in [("msg_2", "part_2", "ses_c"), ("msg_3", "part_3", "ses_d")] {
            apply(
                &mut store,
                "session.tool.input.started",
                json!({"sessionID":"ses_1","assistantMessageID":message,"id":part,"name":"subagent"}),
            );
            apply(
                &mut store,
                "session.tool.success",
                json!({"sessionID":"ses_1","assistantMessageID":message,"id":part,
                       "content":[],"metadata":{"sessionID":child},"executed":true}),
            );
        }
        assert_eq!(store.referenced_children(), ["ses_c", "ses_d"]);
        assert!(DataStore::default().referenced_children().is_empty());
    }

    #[test]
    fn tool_child_finds_the_child_of_one_part() {
        let store = store_with_child();
        let child = store.tool_child("msg_1", "part_1").unwrap();
        assert_eq!(child.session_id, "ses_c");
        assert!(store.tool_child("msg_1", "nope").is_none());
        assert!(store.tool_child("nope", "part_1").is_none());
    }

    #[test]
    fn set_child_running_table() {
        // (child, running, expected is_child_running afterwards)
        let cases = [
            ("ses_c", true, true),
            ("ses_c", false, false),
            ("ses_other", true, false), // not referenced: fail closed
            ("ses_1", true, false),     // the focused session is reduce's
        ];
        for (child, running, expected) in cases {
            let mut store = store_with_child();
            let acted = store.set_child_running(child, running);
            assert_eq!(acted, child == "ses_c", "{child} {running}");
            assert_eq!(store.is_child_running(child), expected, "{child} {running}");
            assert_eq!(store.busy(), expected, "{child} {running}");
        }
    }

    #[test]
    fn set_child_running_false_clears_a_running_child() {
        let mut store = store_with_child();
        assert!(store.set_child_running("ses_c", true));
        assert!(store.busy());
        store.set_child_running("ses_c", false);
        assert!(!store.busy());
    }

    fn started(store: &mut DataStore) {
        child_event(
            store,
            "session.execution.started",
            json!({"sessionID":"ses_c"}),
        );
    }

    fn ended(store: &mut DataStore) {
        child_event(
            store,
            "session.execution.succeeded",
            json!({"sessionID":"ses_c"}),
        );
    }

    #[test]
    fn a_probe_result_applies_when_no_event_arrived_since_it_started() {
        for running in [true, false] {
            let mut store = store_with_child();
            store.mark_probing(&["ses_c".to_owned()]);
            assert!(store.apply_probe("ses_c", running));
            assert_eq!(store.is_child_running("ses_c"), running);
        }
    }

    #[test]
    fn an_event_after_the_probe_started_wins_over_its_result() {
        // The event says finished, the (older) probe says running.
        let mut store = store_with_child();
        store.mark_probing(&["ses_c".to_owned()]);
        ended(&mut store);
        assert!(!store.apply_probe("ses_c", true));
        assert!(!store.is_child_running("ses_c"));

        // The event says started, the (older) probe says idle.
        let mut store = store_with_child();
        store.mark_probing(&["ses_c".to_owned()]);
        started(&mut store);
        assert!(!store.apply_probe("ses_c", false));
        assert!(store.is_child_running("ses_c"));
    }

    #[test]
    fn an_event_before_the_probe_started_loses_to_its_result() {
        let mut store = store_with_child();
        started(&mut store);
        store.mark_probing(&["ses_c".to_owned()]);
        assert!(store.apply_probe("ses_c", false));
        assert!(!store.is_child_running("ses_c"));
    }

    #[test]
    fn a_probe_result_is_used_once_and_only_if_it_was_started() {
        let mut store = store_with_child();
        assert!(!store.apply_probe("ses_c", true), "never started");
        store.mark_probing(&["ses_c".to_owned()]);
        assert!(store.apply_probe("ses_c", true));
        assert!(!store.apply_probe("ses_c", false), "already consumed");
        assert!(store.is_child_running("ses_c"));
    }

    #[test]
    fn mark_probing_ignores_unreferenced_ids_and_replace_items_forgets_them() {
        let mut store = store_with_child();
        store.mark_probing(&["ses_other".to_owned(), "ses_c".to_owned()]);
        assert!(!store.apply_probe("ses_other", true));
        store.replace_items(Vec::new());
        assert!(!store.apply_probe("ses_c", true), "no longer referenced");
    }

    /// Drive one glob call through the live events: started, called with
    /// `input`, then success (with `metadata` unless null) or failure.
    fn live_glob_call(
        input: serde_json::Value,
        metadata: serde_json::Value,
        outcome: &str,
    ) -> super::ToolPart {
        let mut store = DataStore::default();
        let base = json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1"});
        let with = |extra: serde_json::Value| {
            let mut data = base.clone();
            for (key, value) in extra.as_object().expect("extra is an object") {
                data[key] = value.clone();
            }
            data
        };
        apply(
            &mut store,
            "session.tool.input.started",
            with(json!({"name":"glob"})),
        );
        apply(
            &mut store,
            "session.tool.called",
            with(json!({"input": input, "executed": true})),
        );
        if outcome == "success" {
            apply(
                &mut store,
                "session.tool.success",
                with(json!({"content":[{"type":"text","text":"a.rs"}],
                            "metadata": metadata, "executed": true})),
            );
        } else {
            apply(
                &mut store,
                "session.tool.failed",
                with(json!({"error":{"type":"x","message":"boom"},
                            "metadata": metadata, "executed": true})),
            );
        }
        assistant(&store).tools[0].clone()
    }

    /// The same glob call as recorded in history.
    fn history_glob_call(state: serde_json::Value) -> super::ToolPart {
        let messages: Vec<ox_schema::message::Message> = serde_json::from_value(json!([
            {"type":"assistant","id":"msg_1","time":{"created":1},"agent":"build",
             "model":{"providerID":"p","id":"m"},
             "content":[{"type":"tool","id":"part_1","name":"glob","state": state,
                         "time":{"created":1}}]}
        ]))
        .unwrap();
        let items = crate::history::parse_messages(&messages);
        match &items[0] {
            Item::Assistant(assistant) => assistant.tools[0].clone(),
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    /// Error rows only, as `(type, message)`.
    fn error_rows(items: &[Item]) -> Vec<(String, String)> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::Error(error) => Some((error.kind.to_string(), error.message.to_string())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_steer_between_step_error_and_run_failure_hides_the_repeat_in_both_paths() {
        // Live: the step fails, the user steers, the run fails with the same error.
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.step.failed",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1",
                   "error":{"type":"provider","message":"bad request"}}),
        );
        store.push_user("try again");
        apply(
            &mut store,
            "session.execution.failed",
            json!({"sessionID":"ses_1","error":{"type":"provider","message":"bad request"}}),
        );

        let messages: Vec<ox_schema::message::Message> = serde_json::from_value(json!([
            {"type":"assistant","id":"msg_1","time":{"created":1},"agent":"build",
             "model":{"providerID":"p","id":"m"},"content":[],
             "error":{"type":"provider","message":"bad request"}},
            {"type":"user","id":"msg_u1","time":{"created":2},"text":"try again"},
            {"type":"idle","id":"msg_i1","time":{"created":3},"outcome":"failed",
             "error":{"type":"provider","message":"bad request"}}
        ]))
        .unwrap();
        let history = crate::history::parse_messages(&messages);

        assert_eq!(
            error_rows(&store.items),
            error_rows(&history),
            "live and reload must agree"
        );
        assert!(
            error_rows(&history).is_empty(),
            "the step already shows it: {history:?}"
        );
    }

    #[test]
    fn a_failed_compaction_hides_only_an_identical_error() {
        // Same message, different type: upstream compares both fields.
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.compaction.failed",
            json!({"sessionID":"ses_1","reason":"auto",
                   "error":{"type":"provider","message":"too long"}}),
        );
        apply(
            &mut store,
            "session.execution.failed",
            json!({"sessionID":"ses_1","error":{"type":"unknown","message":"too long"}}),
        );
        assert_eq!(
            error_rows(&store.items),
            vec![("unknown".to_owned(), "too long".to_owned())]
        );
    }

    #[test]
    fn live_and_history_keep_the_same_input_and_metadata() {
        let input = json!({"pattern":"**/*.rs"});
        let metadata = json!({"count": 3, "truncated": false});
        let live = live_glob_call(input.clone(), metadata.clone(), "success");
        let history = history_glob_call(json!({
            "status":"completed","input": input,
            "content":[{"type":"text","text":"a.rs"}],"metadata": metadata
        }));
        assert_eq!(
            live.input, history.input,
            "input must not depend on the path"
        );
        assert_eq!(live.metadata, history.metadata, "metadata likewise");
        assert_eq!(live.input.get("pattern"), Some(&json!("**/*.rs")));
        let count = live.metadata.as_ref().and_then(|m| m.get("count"));
        assert_eq!(count, Some(&json!(3)), "got {:?}", live.metadata);
    }

    #[test]
    fn failed_calls_keep_metadata_in_both_paths() {
        let input = json!({"pattern":"x"});
        let metadata = json!({"count": 0});
        let live = live_glob_call(input.clone(), metadata.clone(), "failed");
        let history = history_glob_call(json!({
            "status":"error","input": input,
            "error":{"type":"x","message":"boom"},"metadata": metadata
        }));
        assert!(live.metadata.is_some(), "tool.failed carries metadata");
        assert_eq!(live.metadata, history.metadata, "same metadata on reload");
        assert_eq!(live.input, history.input, "same input on reload");
    }

    #[test]
    fn metadata_is_none_when_the_result_had_none() {
        let input = json!({"pattern":"x"});
        let live = live_glob_call(input.clone(), json!(null), "success");
        let history = history_glob_call(json!({
            "status":"completed","input": input,
            "content":[{"type":"text","text":"a.rs"}]
        }));
        assert!(live.metadata.is_none(), "got {:?}", live.metadata);
        assert!(history.metadata.is_none(), "got {:?}", history.metadata);
    }

    #[test]
    fn input_is_empty_without_tool_called() {
        let mut store = DataStore::default();
        apply(
            &mut store,
            "session.tool.input.started",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1","name":"read"}),
        );
        apply(
            &mut store,
            "session.tool.input.delta",
            json!({"sessionID":"ses_1","assistantMessageID":"msg_1","id":"part_1",
                   "delta":"{\"path\":\"a.txt\"}"}),
        );
        let part = &assistant(&store).tools[0];
        assert!(part.input.is_empty(), "got {:?}", part.input);
        assert!(part.metadata.is_none(), "got {:?}", part.metadata);
        let history = history_glob_call(json!({"status":"streaming","input":"{\"pattern\":"}));
        assert!(history.input.is_empty(), "got {:?}", history.input);
    }
}
