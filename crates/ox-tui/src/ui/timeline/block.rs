use ox_client::store::{
    CompactionStatus, CompletionSource, ErrorInfo, MediaMeta, Notice, RetryInfo, Switch, ToolState,
};
use ox_client::todo::Todo;
use ox_schema::support::{Record, TokenUsage};

/// The rows closing a finished (or retrying) turn (upstream `AssistantFooter`,
/// `routes/session/index.tsx:2000`): the step error, the retry countdown and
/// the stats line.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnFooter {
    /// Agent that produced the turn (`apex`); shown title-cased.
    pub agent: Option<String>,
    /// `providerID/id`.
    pub model: Option<String>,
    /// User prompt creation to the message's completion.
    pub duration_ms: Option<i64>,
    /// Output tokens over provider-active time, across the turn's steps.
    pub tokens_per_second: Option<f64>,
    /// Why the message's last step failed. `Step interrupted` is not shown as
    /// an error; it marks the stats line ` · interrupted`.
    pub error: Option<ErrorInfo>,
    /// A scheduled retry; its countdown is derived from `at` when drawn.
    pub retry: Option<RetryInfo>,
}

/// One tool call folded into an `Explored` group; what the expanded row lists
/// (upstream `group-view.tsx:129-157`, `children("tool")`). Fields mirror
/// `Block::Tool`; `input`/`metadata` are shared with the `ToolPart`.
#[derive(Debug, Clone)]
pub struct ExploredCall {
    pub id: String,
    /// `ToolPart::version` — equality is `(id, version)`.
    pub version: u64,
    pub tool: String,
    pub state: ToolState,
    pub input: std::sync::Arc<Record>,
    pub metadata: Option<std::sync::Arc<Record>>,
}

/// One `2 reads` entry of an Explored header: the singular noun upstream
/// counts under (`read`, `search`, `fetch`) and how many calls had it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExploredCount {
    pub noun: &'static str,
    pub count: u32,
}

/// Counts behind one `Explored: 1 read` row (upstream `group-view.tsx`) and
/// the calls they were counted from.
#[derive(Debug, Clone, Default)]
pub struct Explored {
    /// Per-noun counts in order of first appearance in the group.
    pub counts: Vec<ExploredCount>,
    /// Some grouped tool has not finished: `Exploring:` instead of `Explored:`.
    pub running: bool,
    /// Id of the group's first call: the key of its expansion state, stable
    /// while later calls join the group (upstream `groupID`).
    pub group_id: String,
    pub calls: Vec<ExploredCall>,
}

/// Identity comparison, like `Block`: counts, `running` and each call's
/// `(id, version)`; never summary text.
impl PartialEq for Explored {
    fn eq(&self, other: &Self) -> bool {
        self.counts == other.counts
            && self.running == other.running
            && self.group_id == other.group_id
            && self.calls.len() == other.calls.len()
            && self
                .calls
                .iter()
                .zip(&other.calls)
                .all(|(a, b)| a.id == b.id && a.version == b.version)
    }
}
impl Eq for Explored {}

/// Whether a user prompt has reached the agent yet (upstream
/// `pendingDeliveries`, `routes/session/index.tsx:221-224`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Still in the server inbox.
    Pending,
    /// Delivered to the agent, or loaded from history.
    Delivered,
}

#[derive(Debug, Clone)]
pub enum Block {
    User {
        text: std::sync::Arc<str>,
        /// Every attachment on the prompt as metadata: images are drawn as
        /// thumbnails, anything else as a `file`/`dir` label row. Empty for the
        /// optimistic local echo, which carries no attachment.
        files: std::sync::Arc<[ox_client::store::MediaMeta]>,
        delivery: Delivery,
    },
    /// The reasoning header of one assistant message, collapsed by default.
    Reasoning {
        id: String,
        /// Store-owned version, as for `Assistant`.
        version: u64,
        text: std::sync::Arc<String>,
        done: bool,
        /// Reasoning duration when seen live (`reasoning.started` → `ended`).
        ms: Option<u64>,
    },
    /// The answer text of one assistant message and, once the turn has
    /// finished, its footer.
    Assistant {
        id: String,
        /// Store-owned version. Equality is `(id, version)`, never text.
        version: u64,
        /// `Arc<String>` mirrors `AssistantItem` so the store→block projection
        /// is a pointer clone — no per-delta deep copy of the growing text.
        text: std::sync::Arc<String>,
        footer: Option<TurnFooter>,
    },
    /// An agent / model / location change (upstream `SessionSwitchMessageV2`).
    Switch(Switch),
    /// A system notice or a loaded skill (upstream `SessionNoticeMessageV2`,
    /// `SessionSkillMessage`), or a described synthetic row such as a
    /// historian fold (`◈ historian folded 59..59 · 1 compartment`).
    Notice(Notice),
    /// A run failure no step or compaction already shows: `Error: message`.
    Error(ErrorInfo),
    /// A compaction waiting in the inbox (upstream `CompactionQueued`).
    CompactionQueued,
    /// Upstream `BackgroundToolHint`: the transcript's last row while a
    /// running shell/subagent call can be moved to the background.
    BackgroundHint,
    /// Consecutive read/search/fetch tool calls, folded into one row.
    Explored(Explored),
    /// One inline tool call row (and its collapsed output).
    Tool {
        id: String,
        /// `ToolPart::version` — equality is `(id, version)`.
        version: u64,
        tool: String,
        state: ToolState,
        summary: String,
        output: std::sync::Arc<String>,
        /// A subagent call whose child session is still running. Not part of
        /// the part, so a child starting or ending does not bump `version`:
        /// equality compares it too.
        background: bool,
        /// The parsed call input, shared with the `ToolPart` (per-tool
        /// texts read their fields from it).
        input: std::sync::Arc<Record>,
        /// The result metadata, shared with the `ToolPart`.
        metadata: Option<std::sync::Arc<Record>>,
        /// The images in this call's result, shared with the `ToolPart`: drawn
        /// as a strip below the block, the bytes fetched by
        /// [`ox_client::store::MediaSource::Tool`]. Empty for every other call.
        media: std::sync::Arc<[MediaMeta]>,
    },
    /// A `shell` call, drawn as a block (`super::shell_block`, upstream
    /// `Shell`). `input`/`output` are shared with the `ToolPart`.
    Shell {
        id: String,
        /// `ToolPart::version` — equality is `(id, version)` plus the two
        /// store-derived flags below.
        version: u64,
        state: ToolState,
        /// The parsed call input (`command`, `workdir`).
        input: std::sync::Arc<Record>,
        /// The text shown: the tool result (output or failure message), or
        /// the bound shell's fetched output (`crate::shell_captures`).
        /// Equality compares the pointer: a new capture is a new `Arc`.
        output: std::sync::Arc<String>,
        /// `output` lacks the shell's earliest bytes.
        omitted: bool,
        /// The result names a `shellID` (upstream `Boolean(shellID)`).
        shell: bool,
        /// The call returned while its shell kept running
        /// (`metadata.status == "running"`).
        background: bool,
        /// The bound shell is alive (`DataStore::is_shell_running`). Not part
        /// of the part, so equality compares it.
        shell_running: bool,
    },
    /// A `todowrite` call: the `# Todos` panel once it completed with a
    /// non-empty list, an inline row otherwise (upstream `TodoWrite`).
    Todo {
        id: String,
        /// `ToolPart::version` — equality is `(id, version)`.
        version: u64,
        state: ToolState,
        todos: std::sync::Arc<Vec<Todo>>,
        /// The failure message of a failed call, shared with the `ToolPart`.
        output: std::sync::Arc<String>,
    },
    /// A context-compaction boundary rule with its summary below.
    Compaction {
        id: String,
        /// `CompactionItem::version` — equality is `(id, version)`.
        version: u64,
        status: CompactionStatus,
        text: std::sync::Arc<String>,
        cancelled: bool,
        tokens: Option<TokenUsage>,
    },
    /// A shell / subagent completion notice (`↳ Shell finished · cmd`).
    /// Mirrors `ox_client::store::CompletionItem`; equality is by `id` alone
    /// because the item is immutable once built.
    Completion {
        id: String,
        source: CompletionSource,
        state: Option<std::sync::Arc<str>>,
        description: std::sync::Arc<str>,
        child_id: Option<std::sync::Arc<str>>,
    },
}

impl Block {
    #[must_use]
    pub fn user(text: &str) -> Self {
        Block::User {
            text: std::sync::Arc::from(text),
            files: std::sync::Arc::from(Vec::new()),
            delivery: Delivery::Delivered,
        }
    }
    #[must_use]
    pub fn assistant(id: &str, version: u64, text: &str) -> Self {
        Block::Assistant {
            id: id.to_owned(),
            version,
            text: std::sync::Arc::new(text.to_owned()),
            footer: None,
        }
    }
    #[must_use]
    pub fn reasoning(id: &str, version: u64, text: &str, done: bool) -> Self {
        Block::Reasoning {
            id: id.to_owned(),
            version,
            text: std::sync::Arc::new(text.to_owned()),
            done,
            ms: None,
        }
    }
}

/// O(1) identity comparison: text is shared via `Arc`, store-backed blocks
/// compare `(id, version)`. Deep string comparison here would defeat the cache
/// it feeds.
impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Block::User {
                    text: a,
                    files: af,
                    delivery: ad,
                },
                Block::User {
                    text: b,
                    files: bf,
                    delivery: bd,
                },
            ) => std::sync::Arc::ptr_eq(a, b) && std::sync::Arc::ptr_eq(af, bf) && ad == bd,
            (
                Block::Reasoning {
                    id: aid,
                    version: aversion,
                    done: adone,
                    ..
                },
                Block::Reasoning {
                    id: bid,
                    version: bversion,
                    done: bdone,
                    ..
                },
            ) => aid == bid && aversion == bversion && adone == bdone,
            (
                Block::Assistant {
                    id: aid,
                    version: aversion,
                    footer: afooter,
                    ..
                },
                Block::Assistant {
                    id: bid,
                    version: bversion,
                    footer: bfooter,
                    ..
                },
            ) => aid == bid && aversion == bversion && afooter == bfooter,
            // Short `Arc<str>` fields; immutable once built.
            (Block::Switch(a), Block::Switch(b)) => a == b,
            (Block::Notice(a), Block::Notice(b)) => a == b,
            (Block::Error(a), Block::Error(b)) => {
                std::sync::Arc::ptr_eq(&a.message, &b.message) || a == b
            }
            (Block::CompactionQueued, Block::CompactionQueued) => true,
            (Block::BackgroundHint, Block::BackgroundHint) => true,
            (Block::Explored(a), Block::Explored(b)) => a == b,
            (
                Block::Tool {
                    id: aid,
                    version: aversion,
                    background: abackground,
                    media: amedia,
                    ..
                },
                Block::Tool {
                    id: bid,
                    version: bversion,
                    background: bbackground,
                    media: bmedia,
                    ..
                },
            ) => {
                // The strip is laid out from the pointer, so a new attachment
                // list (a new `Arc`) is a new block even at the same version.
                aid == bid
                    && aversion == bversion
                    && abackground == bbackground
                    && std::sync::Arc::ptr_eq(amedia, bmedia)
            }
            (
                Block::Shell {
                    id: aid,
                    version: aversion,
                    output: aoutput,
                    omitted: aomitted,
                    background: abackground,
                    shell_running: arunning,
                    ..
                },
                Block::Shell {
                    id: bid,
                    version: bversion,
                    output: boutput,
                    omitted: bomitted,
                    background: bbackground,
                    shell_running: brunning,
                    ..
                },
            ) => {
                aid == bid
                    && aversion == bversion
                    && std::sync::Arc::ptr_eq(aoutput, boutput)
                    && aomitted == bomitted
                    && abackground == bbackground
                    && arunning == brunning
            }
            (
                Block::Todo {
                    id: aid,
                    version: aversion,
                    ..
                },
                Block::Todo {
                    id: bid,
                    version: bversion,
                    ..
                },
            )
            | (
                Block::Compaction {
                    id: aid,
                    version: aversion,
                    ..
                },
                Block::Compaction {
                    id: bid,
                    version: bversion,
                    ..
                },
            ) => aid == bid && aversion == bversion,
            (Block::Completion { id: a, .. }, Block::Completion { id: b, .. }) => a == b,
            _ => false,
        }
    }
}
impl Eq for Block {}
