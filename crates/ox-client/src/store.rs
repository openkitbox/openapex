//! Headless data layer: event in, state out. No terminal, no IO — this is the
//! part the TUI tests most heavily.
//!
//! Two invariants keep the UI cheap:
//! 1. `items` is ONE ordered sequence (no parallel user/assistant vectors), and
//!    the block list is a projection of it. An item index is NOT a block index:
//!    one assistant item may draw several blocks (reasoning, answer, tool
//!    rows), and [`Item::Turn`] draws none. The dirty watermark
//!    ([`DataStore::changed_from`]) is an item index; the projection converts it
//!    (`ox_tui::pipeline::blocks_from_store`) into the block to start re-wrapping.
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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ox_schema::event::{EventEnvelope, SessionEventData};
use ox_schema::support::{FinishReason, Record, TokenUsage};

use crate::form::{self, FormInfo};
use crate::todo::{Todo, parse_todos};

/// Where one attachment's bytes can be fetched from, on demand.
///
/// The store keeps no attachment bytes — a 20 MiB image is ~27 MB of base64 —
/// so a row carries only [`MediaMeta`], and the UI asks the ox-only media
/// endpoint for the bytes of the one image it is about to draw.
#[derive(Debug, Clone)]
pub enum MediaSource {
    /// A user message's `files[index]`.
    Prompt { message: Arc<str>, index: u32 },
    /// A tool part's `state.content[index]`.
    Tool {
        message: Arc<str>,
        part: Arc<str>,
        index: u32,
    },
}

/// One attachment as the UI needs it: no bytes, only what identifies it.
#[derive(Debug, Clone)]
pub struct MediaMeta {
    /// The attachment's file name, when the wire carried one.
    pub name: Option<Arc<str>>,
    pub mime: Arc<str>,
    pub source: MediaSource,
}

impl MediaMeta {
    /// One attachment's identity.
    pub(crate) fn new(name: Option<&str>, mime: &str, source: MediaSource) -> Self {
        Self {
            name: name.map(Arc::from),
            mime: Arc::from(mime),
            source,
        }
    }

    /// Whether this attachment is a directory listing. A directory has no field
    /// of its own on the wire: the server materializes one as a listing with
    /// the [`DIRECTORY_MIME`] type (`ox-server/src/api/attachment.rs`), so this
    /// is derived from `mime` rather than carried beside it.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        &*self.mime == DIRECTORY_MIME
    }
}

/// The mime type a directory attachment is materialized as.
const DIRECTORY_MIME: &str = "application/x-directory";

#[derive(Debug, Clone)]
pub enum Item {
    User {
        text: Arc<str>,
        /// Metadata for every attachment on the prompt, images and non-images
        /// alike — the UI labels the latter `file`/`dir`. Empty for the local
        /// echo ([`DataStore::push_user`]), which carries no attachment and no
        /// message id to fetch one by.
        files: Arc<[MediaMeta]>,
        /// Wire `time.created` (ms). `None` for the optimistic local echo.
        created: Option<i64>,
        /// Whether this row is the input that begins a turn. Upstream
        /// `packages/util/src/session-turn.ts` `inputIndex` dates a turn from
        /// the first input after the previous `idle` marker, so a prompt typed
        /// while a turn runs — persisted mid-turn as a `user` message, a steer —
        /// is a row INSIDE the turn and must not restart its clock.
        ///
        /// Live: computed at echo time from the store's busy state
        /// ([`DataStore::push_user`]); the turn row that follows a prompt typed
        /// while idle carries the execution start, so `created` stays `None`.
        /// History: true for a `user` message that opens a turn (no turn open
        /// yet), false inside one.
        starts_turn: bool,
    },
    /// A turn started here: the session opened one execution. Carries the
    /// turn's first input `time.created` when the wire has it (a history input
    /// row), and the execution's own `created` live.
    ///
    /// It renders nothing; it exists so the projector can date a turn from
    /// where the turn started rather than from a rendered row (upstream
    /// `packages/util/src/session-turn.ts` `inputIndex`), which is what keeps a
    /// synthetic-started turn — described or not — measurable and a steered
    /// turn whole.
    Turn {
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
    /// A system notice (`system` message, `session.instructions.updated`), a
    /// described `synthetic` message (a historian fold: `◈ historian folded
    /// 59..59 · 1 compartment`), or a loaded skill (`skill` message).
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

impl Item {
    /// The single classification point for a `synthetic` session message:
    /// history (`crate::history`), the live `session.synthetic` event and an
    /// inbox-delivered synthetic all go through it.
    ///
    /// A shell / subagent completion keeps its [`Item::Completion`] row; every
    /// other synthetic becomes a notice row (`◈ description`) — a historian
    /// fold, a todo reminder. The message `text` is its model-facing body (a
    /// repeat reminder, a continuation prompt) and never a row: a synthetic
    /// describes itself, so one without a description makes no row at all
    /// (upstream `rows.ts` drops `!description?.trim()`). That is the only
    /// synthetic allowed to be dropped.
    #[must_use]
    pub fn from_synthetic(
        id: &str,
        metadata: Option<&Record>,
        description: Option<&str>,
    ) -> Option<Self> {
        if let Some(completion) = CompletionItem::from_synthetic(id, metadata, description) {
            return Some(Self::Completion(completion));
        }
        let description = description.filter(|text| !text.trim().is_empty())?;
        Some(Self::Notice(Notice::System {
            description: Some(Arc::from(description)),
        }))
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
    /// Ids some frame touched since the request left. Only the ids: their
    /// state (and owner) is read back from `running_shells`, which the frame
    /// already updated. Bounded by the shells that changed while one request
    /// was in flight.
    seen: HashSet<String>,
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

/// One running shell with the session that owns it, as the `GET /api/shell`
/// rebuild reports them (the running subset of that list). The owner is
/// `metadata.sessionID` (`ox-tools/src/shell/sessions.rs`: `info()`), the same
/// field the `shell.created` frame's `info` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningShell {
    pub id: String,
    /// `None` when the server recorded no owner for the shell.
    pub session: Option<String>,
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
    /// The images in this call's result the UI can draw: `image/*` entries with
    /// inline `data:` bytes, as metadata only — [`MediaSource::Tool`] fetches
    /// the bytes on demand. Empty for every other result, which is most of them.
    pub media: Arc<[MediaMeta]>,
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
    /// Milliseconds the provider spent thinking, summed over the message's
    /// thinking windows: the live `reasoning.started → reasoning.ended` spans, or
    /// a history-loaded message's reasoning-part `time` blocks. `None` when
    /// neither recorded a duration, in which case the row shows no suffix.
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
    /// Agent that produced the message (`apex`), from the wire message or
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
    /// Running shells by id, each with the session that owns it (`None` when
    /// the server recorded no `metadata.sessionID`). From the `shell.*` frames
    /// and the `GET /api/shell` rebuild; the cap is the number of live shells
    /// on the server.
    ///
    /// [`DataStore::apply_shell_frame`] is the only writer, and a rebuild
    /// replays through its result: clearing this map outside a frame would let
    /// an in-flight answer restore the sessions it just dropped.
    running_shells: HashMap<String, Option<String>>,
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
    /// Rows classified at enqueue time by [`Item::from_synthetic`] for
    /// inbox synthetics not yet delivered, by inbox id. Moved into `items` on
    /// delivery; dropped on cancel and on history reload. Holding the `Item`
    /// (not the narrower [`CompletionItem`]) is what keeps the delivered row
    /// identical to the one history builds for the same message.
    pending_synthetics: HashMap<String, Item>,
    /// Forms awaiting an answer (`form.created` not yet replied/cancelled),
    /// oldest first. The `question` tool is the only producer today.
    pub pending_forms: Vec<FormInfo>,
    /// Last durable seq **of the focused session only**.
    pub last_seq: Option<i64>,
    /// First **item** index whose content changed since the last UI sync. Item
    /// indices are not block indices (see the module doc): a caller deriving
    /// blocks must project this through the same walk that builds them.
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
        self.running_shells.contains_key(id)
    }

    /// Running shells owned by `session`: the prompt footer's shell count.
    /// Shells the server recorded no owner for, and other sessions' shells,
    /// are not counted.
    ///
    /// Deviation: upstream's footer counts every shell its store holds for the
    /// session, settled ones included (its `Shell` tab has to filter
    /// `status === "running"` again). `running_shells` holds only running
    /// shells: a frame inserts one while it runs and removes it on exit, and
    /// the rebuild lists only unsettled sessions
    /// (`ox-tools/src/shell/sessions.rs`, `ShellSessions::list`). So this is
    /// upstream's Shell-tab count, not its footer count.
    #[must_use]
    pub fn running_shells_of(&self, session: &str) -> usize {
        self.running_shells
            .values()
            .filter(|owner| owner.as_deref() == Some(session))
            .count()
    }

    /// Children named by a tool part that are running right now: the prompt
    /// footer's subagent count. Derived, so it cannot drift from the rows.
    ///
    /// Deviation: upstream walks the session's family and keeps the members
    /// whose status is `running` (`feature-plugins/prompt/footer.tsx:24-29`),
    /// so a child the transcript has not named yet still counts. ox's family
    /// is only fetched while the panel is open, so `running_children` (same
    /// source as the subagent rows) is the resident count: a still-unnamed
    /// child is the one case it misses.
    #[must_use]
    pub fn running_subagents(&self) -> usize {
        self.running_children.len()
    }

    /// Feed one `shell.*` frame (`ox-tools/src/shell/events.rs:frame`):
    /// `created` with status `running` adds `info.id` with the owner from
    /// `info.metadata.sessionID`; `exited` / `deleted` remove `id`. Frames
    /// carry no top-level session id (only a `created` frame's `info` names
    /// one), so the caller routes them here before any session filter.
    /// Returns whether the map changed. While a rebuild is outstanding the
    /// frame's id is also recorded, so the older answer cannot undo it.
    pub fn apply_shell_frame(&mut self, kind: &str, data: &serde_json::Value) -> bool {
        fn id_of(value: &serde_json::Value) -> Option<&str> {
            value.get("id").and_then(serde_json::Value::as_str)
        }
        let (id, running, owner) = match kind {
            "shell.created" => {
                let Some(info) = data.get("info") else {
                    return false;
                };
                let Some(id) = id_of(info) else {
                    return false;
                };
                let running =
                    info.get("status").and_then(serde_json::Value::as_str) == Some("running");
                let owner = info
                    .get("metadata")
                    .and_then(|metadata| metadata.get("sessionID"))
                    .and_then(serde_json::Value::as_str);
                (id, running, owner)
            }
            "shell.exited" | "shell.deleted" => {
                let Some(id) = id_of(data) else {
                    return false;
                };
                (id, false, None)
            }
            _ => return false,
        };
        if let Some(sync) = &mut self.shell_sync {
            sync.seen.insert(id.to_owned());
        }
        if running {
            match self.running_shells.entry(id.to_owned()) {
                Entry::Occupied(mut slot) => {
                    let owner = owner.map(str::to_owned);
                    if *slot.get() == owner {
                        false
                    } else {
                        slot.insert(owner);
                        true
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert(owner.map(str::to_owned));
                    true
                }
            }
        } else {
            self.running_shells.remove(id).is_some()
        }
    }

    /// A `GET /api/shell` rebuild numbered `generation` is about to be sent.
    /// It supersedes any earlier one still in flight.
    pub fn begin_shell_sync(&mut self, generation: u64) {
        self.shell_sync = Some(ShellSync {
            generation,
            seen: HashSet::new(),
        });
    }

    /// Answer to rebuild `generation` (the shells whose status is `running`,
    /// each with its owner): replaces the map, then replays the frames seen
    /// since the request was issued, which are newer — for those ids the
    /// frame's state and owner win. An answer to any other request
    /// (superseded, or already applied) is dropped. Returns whether the map
    /// changed.
    pub fn set_running_shells(&mut self, generation: u64, shells: Vec<RunningShell>) -> bool {
        let Some(sync) = self
            .shell_sync
            .take_if(|sync| sync.generation == generation)
        else {
            return false;
        };
        let mut next: HashMap<String, Option<String>> = shells
            .into_iter()
            .map(|shell| (shell.id, shell.session))
            .collect();
        for id in sync.seen {
            match self.running_shells.get(&id) {
                Some(owner) => {
                    next.insert(id, owner.clone());
                }
                None => {
                    next.remove(&id);
                }
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
        self.pending_synthetics.clear();
        self.shown_error = ShownError::default();
        for (index, item) in self.items.iter().enumerate() {
            if let Item::Assistant(assistant) = item {
                self.assistant_index.insert(assistant.id.clone(), index);
            }
        }
        self.changed_from = 0;
    }

    /// Optimistic local echo of a submitted prompt. It is a steer when a turn is
    /// already running: the row lands inside that turn (the server delivers it
    /// at the next step boundary), so it must not open a new one. A prompt typed
    /// while idle opens the turn the following [`Item::Turn`] dates.
    pub fn push_user(&mut self, text: &str) {
        self.items.push(Item::User {
            text: Arc::from(text),
            files: Arc::default(),
            created: None,
            starts_turn: self.running_since.is_none(),
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
                // A message may think more than once: the parts add up, the way
                // history sums its persisted reasoning-part windows, so a
                // reloaded row and a live one report the same duration.
                if let Some(started) = item.reasoning_started.take() {
                    let elapsed = started.elapsed().as_millis() as u64;
                    item.reasoning_ms = Some(item.reasoning_ms.unwrap_or(0) + elapsed);
                }
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
                    media: Arc::default(),
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
                part.media = tool_media(content, assistant_message_id, id);
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
            content,
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
                if let Some(content) = content {
                    part.media = tool_media(content, assistant_message_id, id);
                }
                part.metadata = metadata.clone().map(Arc::new);
                part.version += 1;
            }
            store.touch(index);
        }
        SessionEventData::ExecutionStarted { .. } => {
            // One turn item per busy period: the actor publishes
            // `execution.started` exactly once per it, and a steer or synthetic
            // delivered mid-turn arrives as an inbox row and starts nothing
            // here, so it cannot split the turn. The boundary is pushed even
            // when `running_since` looks set, because that flag can be stale —
            // a stream that dropped while a turn finished loses the terminal
            // frame, so the next turn would otherwise get no boundary at all
            // and its footer would span the previous turn's steps. Only a repeat
            // of the frame that already opened the turn (a replay) adds nothing:
            // a second boundary would draw no row, but it would cut the tok/s
            // window of the later steps short.
            if store.running_since != Some(event.created) {
                store.push_row(Item::Turn {
                    created: Some(event.created),
                });
            }
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
            // No chip: the row only appears once delivered, but it is
            // classified here so an inbox synthetic and the same message
            // reloaded from history agree on their row.
            if let Some(row) = Item::from_synthetic(
                inbox_id,
                payload.metadata.as_ref(),
                payload.description.as_deref(),
            ) {
                store.pending_synthetics.insert(inbox_id.clone(), row);
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
            if let Some(row) = store.pending_synthetics.remove(inbox_id) {
                store.items.push(row);
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
            store.pending_synthetics.remove(inbox_id);
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
        SessionEventData::SessionSynthetic {
            description,
            metadata,
            ..
        } => {
            // `text` is the model-facing body (a repeat reminder, a
            // continuation prompt), never a row; a described synthetic gets
            // its `◈` row here, the same one history builds.
            if let Some(item) =
                Item::from_synthetic(&event.id, metadata.as_ref(), description.as_deref())
            {
                store.push_row(item);
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

/// The images of one tool result, as metadata only ([`ToolPart::media`]).
///
/// Only `image/*` bytes can be drawn, and only an inline `data:` URI can be
/// fetched from the media endpoint, so everything else is dropped here rather
/// than at draw time. `index` is the entry's own position in `content` — that
/// is what the endpoint reads — so filtering never renumbers it. The base64
/// payload is never touched.
#[must_use]
pub(crate) fn tool_media(
    content: &[ox_schema::support::Content],
    message_id: &str,
    part_id: &str,
) -> Arc<[MediaMeta]> {
    let mut media = Vec::new();
    // The source ids are allocated only when the result actually carries an
    // image: most tool results draw nothing, and a shell call can finish
    // hundreds of times in one turn.
    let mut source_ids: Option<(Arc<str>, Arc<str>)> = None;
    for (index, entry) in content.iter().enumerate() {
        let ox_schema::support::Content::File { uri, mime, name } = entry else {
            continue;
        };
        if !mime.starts_with("image/") || !uri.starts_with("data:") {
            continue;
        }
        let Ok(index) = u32::try_from(index) else {
            continue;
        };
        let (message, part) =
            source_ids.get_or_insert_with(|| (Arc::from(message_id), Arc::from(part_id)));
        media.push(MediaMeta::new(
            name.as_deref(),
            mime,
            MediaSource::Tool {
                message: Arc::clone(message),
                part: Arc::clone(part),
                index,
            },
        ));
    }
    media.into()
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
    use super::*;

    /// One `session.synthetic` frame through the real path: wire JSON →
    /// `parse_typed` → the reducer the socket feed uses.
    fn reduced(data: serde_json::Value) -> Vec<Item> {
        let frame = serde_json::json!({
            "id": "evt_synthetic",
            "created": 1,
            "type": "session.synthetic",
            "data": data,
        });
        let envelope: EventEnvelope<serde_json::Value> =
            serde_json::from_value(frame).expect("a synthetic frame deserializes");
        let mut store = DataStore::default();
        reduce(&mut store, &envelope.parse_typed());
        store.items
    }

    #[test]
    fn a_synthetic_makes_one_row_or_none() {
        // (a) A shell completion keeps its completion row.
        let completion = reduced(serde_json::json!({
            "sessionID": "ses_1",
            "text": "Run echo done",
            "description": "Done",
            "metadata": { "source": "shell", "state": "completed" },
        }));
        assert!(
            matches!(completion.as_slice(), [Item::Completion(_)]),
            "a shell completion rides the completion row, got {completion:?}"
        );

        // (b) A described synthetic is a notice row (`◈ historian folded …`).
        let folded = "historian folded 59..59 · 1 compartment";
        let notice = reduced(serde_json::json!({
            "sessionID": "ses_1",
            "text": "",
            "description": folded,
        }));
        match notice.as_slice() {
            [
                Item::Notice(Notice::System {
                    description: Some(text),
                }),
            ] => assert_eq!(text.as_ref(), folded, "the row keeps the wire description"),
            other => panic!("a described synthetic must be one notice row, got {other:?}"),
        }

        // (c) No description = no row: the body of such a synthetic is a
        // model-facing prompt (a repeat reminder), and upstream drops it
        // (`rows.ts` `!description?.trim()`).
        for data in [
            serde_json::json!({ "sessionID": "ses_1", "text": "" }),
            serde_json::json!({ "sessionID": "ses_1", "text": "repeat reminder", "description": "  " }),
        ] {
            let items = reduced(data.clone());
            assert!(
                items.is_empty(),
                "a synthetic with no description makes no row, got {items:?} for {data}"
            );
        }
    }

    /// One raw frame through `parse_typed` into `store` — the socket feed path.
    fn apply(store: &mut DataStore, frame: serde_json::Value) {
        let envelope: EventEnvelope<serde_json::Value> =
            serde_json::from_value(frame).expect("a frame deserializes");
        reduce(store, &envelope.parse_typed());
    }

    /// The `session.inbox.delivered` frame for one inbox id.
    fn delivered(inbox_id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "evt_delivered",
            "created": 2,
            "type": "session.inbox.delivered",
            "data": { "sessionID": "ses_1", "inboxID": inbox_id },
        })
    }

    /// The `session.inbox.enqueued` frame for one synthetic inbox item.
    fn enqueued(inbox_id: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "evt_enqueued",
            "created": 1,
            "type": "session.inbox.enqueued",
            "data": {
                "sessionID": "ses_1",
                "inboxID": inbox_id,
                "item": { "type": "synthetic", "delivery": "queue", "payload": payload },
            },
        })
    }

    #[test]
    fn an_inbox_synthetic_delivers_the_row_the_classifier_built() {
        // `POST /api/session/{id}/synthetic` reaches the timeline through the
        // inbox, never through `session.synthetic`. Enqueue + delivery must
        // therefore build the same row `Item::from_synthetic` classifies, or
        // the live row and the reloaded one disagree.
        let folded = "historian folded 59..59 · 1 compartment";
        let mut store = DataStore::default();
        apply(
            &mut store,
            enqueued(
                "msg_notice",
                serde_json::json!({ "text": "", "description": folded }),
            ),
        );
        assert!(
            store.items.is_empty(),
            "an enqueued synthetic makes no row before delivery, got {:?}",
            store.items
        );
        apply(&mut store, delivered("msg_notice"));
        match store.items.as_slice() {
            [
                Item::Notice(Notice::System {
                    description: Some(text),
                }),
            ] => assert_eq!(
                text.as_ref(),
                folded,
                "a described inbox synthetic delivers its notice row"
            ),
            other => panic!("a described inbox synthetic must be one notice row, got {other:?}"),
        }

        // A shell-shaped inbox completion keeps the completion row it had
        // before this path went through the classifier (text collapsed).
        let mut store = DataStore::default();
        apply(
            &mut store,
            enqueued(
                "msg_completion",
                serde_json::json!({
                    "text": "Run echo done",
                    "description": "Done   with  spaces",
                    "metadata": { "source": "shell", "state": "completed" },
                }),
            ),
        );
        assert!(
            store.items.is_empty(),
            "an enqueued completion makes no row before delivery, got {:?}",
            store.items
        );
        apply(&mut store, delivered("msg_completion"));
        match store.items.as_slice() {
            [Item::Completion(completion)] => {
                assert_eq!(
                    completion.id, "msg_completion",
                    "the row is keyed by inbox id"
                );
                assert_eq!(
                    completion.source,
                    CompletionSource::Shell,
                    "a shell-sourced inbox item stays a shell completion"
                );
                assert_eq!(
                    completion.description.as_ref(),
                    "Done with spaces",
                    "a shell description is still collapsed"
                );
            }
            other => panic!("a shell inbox item must be one completion row, got {other:?}"),
        }
    }

    /// The `session.execution.started` frame.
    fn execution_started(created: i64) -> serde_json::Value {
        serde_json::json!({
            "id": "evt_started",
            "created": created,
            "type": "session.execution.started",
            "data": { "sessionID": "ses_1" },
        })
    }

    /// A `session.step.started` frame for one assistant message.
    fn step_started(created: i64, message_id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "evt_step",
            "created": created,
            "type": "session.step.started",
            "data": {
                "sessionID": "ses_1",
                "assistantMessageID": message_id,
                "agent": "apex",
                "model": { "providerID": "cardinal", "id": "test" },
                "started": created,
            },
        })
    }

    #[test]
    fn an_execution_makes_exactly_one_turn_item() {
        // A turn is one busy period: its second step is the same turn, and a
        // synthetic delivered mid-turn is an input that starts nothing. Both
        // would otherwise split the turn the footer measures.
        let mut store = DataStore::default();
        apply(&mut store, execution_started(1_000));
        apply(&mut store, step_started(1_100, "msg_a"));
        apply(&mut store, step_started(2_000, "msg_a"));
        apply(
            &mut store,
            enqueued(
                "msg_notice",
                serde_json::json!({ "text": "", "description": "historian folded 1..1" }),
            ),
        );
        apply(&mut store, delivered("msg_notice"));
        apply(
            &mut store,
            serde_json::json!({
                "id": "evt_succeeded",
                "created": 3_000,
                "type": "session.execution.succeeded",
                "data": { "sessionID": "ses_1" },
            }),
        );
        apply(&mut store, execution_started(4_000));
        let turns: Vec<Option<i64>> = store
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Turn { created } => Some(*created),
                _ => None,
            })
            .collect();
        assert_eq!(
            turns,
            vec![Some(1_000), Some(4_000)],
            "one turn item per execution: not one per step, and none for a synthetic delivered mid-turn"
        );
    }

    #[test]
    fn a_replayed_execution_start_adds_no_second_boundary() {
        // The same frame can reach the reducer twice (a reconnect replays from
        // the client's last seq). A second boundary draws no row, but it would
        // restart the projection's tok/s window, dropping the turn's earlier
        // steps from it.
        let mut store = DataStore::default();
        apply(&mut store, execution_started(1_000));
        apply(&mut store, step_started(1_100, "msg_a"));
        apply(&mut store, execution_started(1_000));
        let turns: Vec<Option<i64>> = store
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Turn { created } => Some(*created),
                _ => None,
            })
            .collect();
        assert_eq!(
            turns,
            vec![Some(1_000)],
            "the frame that already opened the turn adds no boundary"
        );
        assert_eq!(store.running_since, Some(1_000), "the flag is unchanged");
    }

    #[test]
    fn a_lost_terminal_frame_still_opens_the_next_turn() {
        // The stream can drop while a turn is finishing, so its
        // `execution.succeeded` never arrives and `running_since` stays set;
        // the reload that follows shows the turn closed. The store cannot tell,
        // so the next `execution.started` must open a turn anyway: a duplicate
        // boundary carries the same `created` and draws no row, while a missing
        // one leaves the turn undated and its footer spanning the previous one.
        let mut store = DataStore::default();
        apply(&mut store, execution_started(1_000));
        apply(&mut store, step_started(1_100, "msg_a"));
        let history: Vec<ox_schema::message::Message> = serde_json::from_value(serde_json::json!([
            { "type": "user", "id": "u1", "time": { "created": 900 }, "text": "go" },
            {
                "type": "assistant",
                "id": "msg_a",
                "time": { "created": 1_100, "completed": 2_000 },
                "agent": "apex",
                "model": { "id": "test", "providerID": "cardinal" },
                "content": [{ "type": "text", "text": "done" }],
            },
            {
                "type": "idle",
                "id": "i1",
                "time": { "created": 2_000 },
                "outcome": "succeeded",
            },
        ]))
        .expect("the reloaded history deserializes");
        store.replace_items(crate::history::parse_messages(&history));
        assert_eq!(
            store.running_since,
            Some(1_000),
            "the lost terminal frame leaves the store thinking the turn runs"
        );
        store.push_user("next");
        let echoed: Vec<bool> = store
            .items
            .iter()
            .filter_map(|item| match item {
                Item::User { starts_turn, .. } => Some(*starts_turn),
                _ => None,
            })
            .collect();
        assert_eq!(
            echoed,
            vec![true, false],
            "the echo keeps reading as a steer: a prompt typed while a run is \
             genuinely active must stay inside that turn"
        );

        apply(&mut store, execution_started(5_000));
        let turns: Vec<Option<i64>> = store
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Turn { created } => Some(*created),
                _ => None,
            })
            .collect();
        assert_eq!(
            turns,
            vec![Some(5_000)],
            "the new execution opens a turn where it started, so the projection \
             dates the turn from 5000 and not from the stale 1000"
        );
        assert!(
            matches!(
                store.items.last(),
                Some(Item::Turn {
                    created: Some(5_000)
                })
            ),
            "the boundary lands after the echoed prompt, got {:?}",
            store.items.last()
        );
        assert_eq!(
            store.running_since,
            Some(5_000),
            "the run flag follows the boundary it just opened"
        );
    }

    #[test]
    fn a_prompt_echoed_while_a_turn_runs_is_a_steer() {
        // A typed prompt is echoed immediately. While a turn is in flight the
        // server delivers it as a steer at the next step boundary, so the row
        // belongs to the running turn and must not open another one; the next
        // prompt typed while idle does.
        fn echoed_turns(store: &DataStore) -> Vec<bool> {
            store
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::User { starts_turn, .. } => Some(*starts_turn),
                    _ => None,
                })
                .collect()
        }

        let mut store = DataStore::default();
        store.push_user("go");
        apply(&mut store, execution_started(1_000));
        store.push_user("use grep");
        assert_eq!(
            echoed_turns(&store),
            vec![true, false],
            "the prompt that opened the turn starts it, the steer typed while it ran does not"
        );

        apply(
            &mut store,
            serde_json::json!({
                "id": "evt_succeeded",
                "created": 3_000,
                "type": "session.execution.succeeded",
                "data": { "sessionID": "ses_1" },
            }),
        );
        store.push_user("next");
        assert_eq!(
            echoed_turns(&store),
            vec![true, false, true],
            "typed while idle, the prompt starts the next turn"
        );
    }

    /// The `shell.created` frame for one shell (`ox-tools/src/shell/events.rs`:
    /// `{ info }`), carrying the owner the server puts in `metadata.sessionID`.
    fn shell_created(id: &str, session: Option<&str>) -> serde_json::Value {
        let metadata = match session {
            Some(session) => serde_json::json!({ "sessionID": session }),
            None => serde_json::json!({}),
        };
        serde_json::json!({
            "info": { "id": id, "status": "running", "metadata": metadata },
        })
    }

    /// The `shell.exited` frame: its id sits at the top level, not under `info`.
    fn shell_exited(id: &str) -> serde_json::Value {
        serde_json::json!({ "id": id, "status": "completed" })
    }

    #[test]
    fn the_shell_count_is_per_session() {
        // The footer counts the focused session's own shells, so another
        // session's shell and one the server recorded no owner for must not
        // inflate it.
        let mut store = DataStore::default();
        store.begin_shell_sync(1);
        assert!(
            store.set_running_shells(
                1,
                vec![
                    RunningShell {
                        id: "sh_a".to_owned(),
                        session: Some("ses_1".to_owned()),
                    },
                    RunningShell {
                        id: "sh_b".to_owned(),
                        session: Some("ses_1".to_owned()),
                    },
                    RunningShell {
                        id: "sh_c".to_owned(),
                        session: Some("ses_2".to_owned()),
                    },
                    RunningShell {
                        id: "sh_d".to_owned(),
                        session: None,
                    },
                ],
            ),
            "the first answer fills a map that was empty, got {:?}",
            store.running_shells
        );
        assert_eq!(
            store.running_shells_of("ses_1"),
            2,
            "only the focused session's shells count; another session's and the \
             ownerless one are not its, map: {:?}",
            store.running_shells
        );
        assert_eq!(
            store.running_shells_of("ses_2"),
            1,
            "another session's shell belongs to that session, map: {:?}",
            store.running_shells
        );
        assert_eq!(
            store.running_shells_of("ses_3"),
            0,
            "a session that owns no shell counts none, map: {:?}",
            store.running_shells
        );
    }

    #[test]
    fn a_created_shell_frame_records_its_owner() {
        let mut store = DataStore::default();
        assert!(
            store.apply_shell_frame("shell.created", &shell_created("sh_a", Some("ses_1"))),
            "a created frame adds a shell the map did not have"
        );
        assert!(
            store.is_shell_running("sh_a"),
            "a created running shell reads as running, map: {:?}",
            store.running_shells
        );
        assert_eq!(
            store.running_shells_of("ses_1"),
            1,
            "the frame's metadata names the owner, map: {:?}",
            store.running_shells
        );
        assert!(
            store.apply_shell_frame("shell.exited", &shell_exited("sh_a")),
            "an exited frame removes a shell the map had"
        );
        assert!(
            !store.is_shell_running("sh_a"),
            "an exited shell no longer reads as running, map: {:?}",
            store.running_shells
        );
        assert_eq!(
            store.running_shells_of("ses_1"),
            0,
            "an exited shell leaves its owner's count, map: {:?}",
            store.running_shells
        );
    }

    #[test]
    fn a_frame_seen_during_a_rebuild_beats_the_answer() {
        // The answer was built before the exit, so it still lists the shell;
        // the frame is newer and must win, or the footer keeps a dead shell.
        let mut store = DataStore::default();
        assert!(
            store.apply_shell_frame("shell.created", &shell_created("sh_a", Some("ses_1"))),
            "the shell starts out running"
        );
        store.begin_shell_sync(7);
        assert!(
            store.apply_shell_frame("shell.exited", &shell_exited("sh_a")),
            "the exit changes the map"
        );
        store.set_running_shells(
            7,
            vec![RunningShell {
                id: "sh_a".to_owned(),
                session: Some("ses_1".to_owned()),
            }],
        );
        assert!(
            !store.is_shell_running("sh_a"),
            "the newer exit frame outlives the older answer, map: {:?}",
            store.running_shells
        );
        assert_eq!(
            store.running_shells_of("ses_1"),
            0,
            "and the dead shell leaves the footer's count, map: {:?}",
            store.running_shells
        );
    }

    #[test]
    fn only_the_latest_rebuild_is_applied() {
        // A superseded answer describes a moment the newer request has already
        // moved past; applying it would resurrect shells the newer answer is
        // about to correct. An answer that changes nothing must also report so,
        // or the caller repaints the footer on every sync.
        let mut store = DataStore::default();
        store.begin_shell_sync(1);
        store.begin_shell_sync(2);
        let stale = vec![RunningShell {
            id: "sh_a".to_owned(),
            session: Some("ses_1".to_owned()),
        }];
        assert!(
            !store.set_running_shells(1, stale),
            "an answer to a superseded request is dropped, map: {:?}",
            store.running_shells
        );
        assert!(
            !store.is_shell_running("sh_a"),
            "the dropped answer left no shell behind, map: {:?}",
            store.running_shells
        );
        let same = vec![RunningShell {
            id: "sh_a".to_owned(),
            session: Some("ses_1".to_owned()),
        }];
        assert!(
            store.set_running_shells(2, same.clone()),
            "the first applied answer changes the map, map: {:?}",
            store.running_shells
        );
        store.begin_shell_sync(3);
        assert!(
            !store.set_running_shells(3, same),
            "an answer matching the current map changes nothing, map: {:?}",
            store.running_shells
        );
    }

    #[test]
    fn a_rebuild_keeps_the_owner_a_frame_supplied() {
        // The answer predates the frame, so it carries no owner for that id;
        // replaying the frame must keep the owner it did supply.
        let mut store = DataStore::default();
        store.begin_shell_sync(8);
        assert!(
            store.apply_shell_frame("shell.created", &shell_created("sh_a", Some("ses_1"))),
            "the frame adds the shell"
        );
        store.set_running_shells(
            8,
            vec![RunningShell {
                id: "sh_a".to_owned(),
                session: None,
            }],
        );
        assert_eq!(
            store.running_shells_of("ses_1"),
            1,
            "the frame's owner survives a rebuild that could not know it, map: {:?}",
            store.running_shells
        );
    }

    /// A store whose transcript names `child` in one subagent tool part — the
    /// precondition both child paths check (`is_foreign_child`).
    fn store_naming_child(child: &str) -> DataStore {
        let mut store = DataStore {
            session: Some("ses_1".to_owned()),
            ..Default::default()
        };
        store.replace_items(vec![Item::Assistant(Box::new(AssistantItem {
            id: "msg_a".to_owned(),
            tools: vec![ToolPart {
                id: "prt_a".to_owned(),
                tool: ChildRef::SUBAGENT_TOOL.to_owned(),
                child: Some(ChildRef {
                    session_id: child.to_owned(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        }))]);
        store
    }

    #[test]
    fn the_subagent_count_follows_the_children_that_run() {
        let child = "ses_child";
        let mut store = store_naming_child(child);
        assert_eq!(
            store.running_subagents(),
            0,
            "a named child that has not started does not count, children: {:?}",
            store.running_children
        );
        assert!(
            !store.set_child_running("ses_unknown", true),
            "a session no tool part names is not this session's child"
        );
        assert_eq!(
            store.running_subagents(),
            0,
            "so an unnamed session cannot enter the count, children: {:?}",
            store.running_children
        );
        store.mark_probing(&[child.to_owned()]);
        assert!(
            store.apply_probe(child, true),
            "the probe applies while the child is named"
        );
        assert_eq!(
            store.running_subagents(),
            1,
            "a running child counts, children: {:?}",
            store.running_children
        );
        assert!(
            store.set_child_running(child, false),
            "the stop applies while the child is still named"
        );
        assert_eq!(
            store.running_subagents(),
            0,
            "a settled child leaves the count, children: {:?}",
            store.running_children
        );
    }
}
