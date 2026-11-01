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
    #[must_use]
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

/// The background shell a `shell` tool call is bound to, read from its result
/// metadata (`ox-tools/src/shell/mod.rs`: `{status, shellID, ..}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRef {
    pub id: String,
    /// `metadata.status == "running"`: the call returned while the shell kept
    /// running (upstream `background`).
    pub background: bool,
}

/// One outstanding `GET /api/shell` rebuild: the shell frames seen since it
/// was issued are newer than its answer, so they are replayed over it (the
/// `probing` rule for children, per shell id).
#[derive(Debug, Default)]
struct ShellSync {
    generation: u64,
    /// Last running state each frame since the request left the shell in.
    /// Bounded by the shells that changed while one request was in flight.
    seen: HashMap<String, bool>,
}

impl ShellRef {
    /// The tool name the server registers the shell tool under
    /// (`ox-tools/src/shell/mod.rs`, `Tool::name`).
    pub const SHELL_TOOL: &'static str = "shell";

    /// Owned [`ShellView::from_metadata`].
    pub fn from_metadata(
        tool: &str,
        metadata: Option<&ox_schema::support::Record>,
    ) -> Option<Self> {
        ShellView::from_metadata(tool, metadata).map(ShellView::into_owned)
    }
}

/// [`ShellRef`] borrowed from the call's metadata, for per-frame readers
/// that only look the id up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellView<'a> {
    pub id: &'a str,
    pub background: bool,
}

impl<'a> ShellView<'a> {
    /// The single parse point shared by history and the live path. `None`
    /// unless `tool` is the shell tool and the metadata names a string
    /// `shellID`. Other tools' metadata is never read.
    #[must_use]
    pub fn from_metadata(
        tool: &str,
        metadata: Option<&'a ox_schema::support::Record>,
    ) -> Option<Self> {
        if tool != ShellRef::SHELL_TOOL {
            return None;
        }
        let metadata = metadata?;
        let id = metadata.get("shellID")?.as_str()?;
        let status = metadata.get("status").and_then(serde_json::Value::as_str);
        Some(Self {
            id,
            background: status == Some("running"),
        })
    }

    #[must_use]
    pub fn into_owned(self) -> ShellRef {
        ShellRef {
            id: self.id.to_owned(),
            background: self.background,
        }
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

impl ToolPart {
    /// The shell this call is bound to; derived from [`Self::metadata`] so it
    /// cannot drift from it.
    #[must_use]
    pub fn shell(&self) -> Option<ShellRef> {
        self.shell_view().map(ShellView::into_owned)
    }

    /// [`Self::shell`] borrowed: no allocation.
    #[must_use]
    pub fn shell_view(&self) -> Option<ShellView<'_>> {
        ShellView::from_metadata(&self.tool, self.metadata.as_deref())
    }

    /// The `workdir` a shell call ran in, from its parsed input. `None` for
    /// other tools, when absent or not a string, and while only streamed input
    /// text exists (see [`Self::input`]).
    #[must_use]
    pub fn shell_workdir(&self) -> Option<&str> {
        if self.tool != ShellRef::SHELL_TOOL {
            return None;
        }
        self.input.get("workdir")?.as_str()
    }
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
    /// Ids of the server's running shells (upstream `data.shell`), from the
    /// `shell.*` frames and the `GET /api/shell` rebuild. Ids only: the cap
    /// is the number of live shells on the server.
    running_shells: HashSet<String>,
    /// The `GET /api/shell` request in flight, if any; see
    /// [`DataStore::begin_shell_sync`].
    shell_sync: Option<ShellSync>,
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
    #[must_use]
    pub fn pending_form(&self) -> Option<&FormInfo> {
        self.pending_forms.first()
    }

    /// The session's current todo list (upstream `util/todo.ts`
    /// `sessionTodos`): the list of the last `todowrite` call that
    /// succeeded, across every assistant message. Running and failed calls
    /// do not count. Empty when there is none. Borrowed, never copied.
    ///
    /// Deviation: upstream stops at the session's revert boundary; the TUI
    /// tracks no revert state, so every message is considered.
    #[must_use]
    pub fn session_todos(&self) -> &[Todo] {
        let assistants = self.items.iter().rev().filter_map(|item| match item {
            Item::Assistant(assistant) => Some(assistant),
            _ => None,
        });
        for assistant in assistants {
            let last = assistant
                .tools
                .iter()
                .rev()
                .find(|part| part.tool == "todowrite" && part.state == ToolState::Success);
            if let Some(part) = last {
                return &part.todos;
            }
        }
        &[]
    }

    /// Whether `session_id` is a known child whose turn is in flight.
    #[must_use]
    pub fn is_child_running(&self, session_id: &str) -> bool {
        self.running_children.contains(session_id)
    }

    /// Whether shell `id` is running (upstream: present in `data.shell`).
    #[must_use]
    pub fn is_shell_running(&self, id: &str) -> bool {
        self.running_shells.contains(id)
    }

    /// Feed one `shell.*` frame (`ox-tools/src/shell/events.rs:frame`):
    /// `created` with status `running` adds `info.id`; `exited` / `deleted`
    /// remove `id`. Shell frames carry no session id, so the caller routes
    /// them here before any session filter. Returns whether the set changed.
    /// While a rebuild is outstanding the frame is also recorded, so its
    /// older answer cannot undo it.
    pub fn apply_shell_frame(&mut self, kind: &str, data: &serde_json::Value) -> bool {
        fn id_of(value: &serde_json::Value) -> Option<&str> {
            value.get("id").and_then(serde_json::Value::as_str)
        }
        let (id, running) = match kind {
            "shell.created" => {
                let Some(info) = data.get("info") else {
                    return false;
                };
                let Some(id) = id_of(info) else {
                    return false;
                };
                let running =
                    info.get("status").and_then(serde_json::Value::as_str) == Some("running");
                (id, running)
            }
            "shell.exited" | "shell.deleted" => {
                let Some(id) = id_of(data) else {
                    return false;
                };
                (id, false)
            }
            _ => return false,
        };
        if let Some(sync) = &mut self.shell_sync {
            match sync.seen.get_mut(id) {
                Some(state) => *state = running,
                None => {
                    sync.seen.insert(id.to_owned(), running);
                }
            }
        }
        if running {
            !self.running_shells.contains(id) && self.running_shells.insert(id.to_owned())
        } else {
            self.running_shells.remove(id)
        }
    }

    /// A `GET /api/shell` rebuild numbered `generation` is about to be sent.
    /// It supersedes any earlier one still in flight.
    pub fn begin_shell_sync(&mut self, generation: u64) {
        self.shell_sync = Some(ShellSync {
            generation,
            seen: HashMap::new(),
        });
    }

    /// Answer to rebuild `generation` (the ids whose status is `running`):
    /// replaces the set, then replays the frames seen since the request was
    /// issued, which are newer. An answer to any other request (superseded,
    /// or already applied) is dropped. Returns whether the set changed.
    pub fn set_running_shells(&mut self, generation: u64, ids: Vec<String>) -> bool {
        let Some(sync) = self
            .shell_sync
            .take_if(|sync| sync.generation == generation)
        else {
            return false;
        };
        let mut next: HashSet<String> = ids.into_iter().collect();
        for (id, running) in sync.seen {
            if running {
                next.insert(id);
            } else {
                next.remove(&id);
            }
        }
        if next == self.running_shells {
            return false;
        }
        self.running_shells = next;
        true
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub fn context_tokens(&self) -> Option<i64> {
        self.context_usage().map(|(tokens, _)| tokens)
    }

    /// Like [`Self::context_tokens`], plus the `providerID/id` of the model
    /// that produced the figure (upstream looks the window up by that model).
    #[must_use]
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
    #[must_use]
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
        // Only queue-delivery prompts wait for the turn to end; steering ones
        // land at the next step boundary and get no dock entry (upstream
        // `queuedPrompts`, `routes/session/index.tsx:221-229`).
        SessionEventData::InboxEnqueued {
            inbox_id,
            item:
                ox_schema::inbox::Item::User {
                    payload,
                    delivery: ox_schema::inbox::Delivery::Queue,
                },
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
        // Steered: it is no longer waiting for the turn. A change back to
        // `queue` carries no text, so it does not bring an entry back.
        SessionEventData::InboxDeliveryChanged {
            inbox_id,
            delivery: ox_schema::inbox::Delivery::Steer,
            ..
        } => {
            store.queued.retain(|queued| queued.inbox_id != *inbox_id);
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
#[must_use]
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
