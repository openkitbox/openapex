// ---------------------------------------------------------------------------
// Task, failure and report types
// ---------------------------------------------------------------------------

/// The tasks the MVP schedules, in the order a tick runs them. The order is
/// the reference's: `task-registry.ts::CANONICAL_DREAM_TASKS` lists `verify`
/// before `curate`, and `compareTaskOrder` orders due tasks that way; the tasks
/// added for context management follow them, cheapest first. It is fixed so a
/// tick is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DreamTask {
    /// Re-check memories whose referenced files are newer (or missing, if never verified).
    Verify,
    /// Duplicate/stale consolidation of one memory category.
    Curate,
    /// Score unclassified memories for the pinboard's injection budget.
    Classify,
    /// Record at most one durable lesson from the project's recent user turns.
    Retrospective,
    /// Move parked smart notes whose condition holds to `ready`.
    SurfaceNotes,
}

impl DreamTask {
    /// Every task, in tick order.
    pub const ALL: [DreamTask; 5] = [
        DreamTask::Verify,
        DreamTask::Curate,
        DreamTask::Classify,
        DreamTask::Retrospective,
        DreamTask::SurfaceNotes,
    ];

    /// The `task` column spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DreamTask::Verify => "verify",
            DreamTask::Curate => "curate",
            DreamTask::Classify => "classify",
            DreamTask::Retrospective => "retrospective",
            DreamTask::SurfaceNotes => "surface_notes",
        }
    }
}

/// Why a dream run failed, in the reference's vocabulary
/// (`provider-output-failure.ts`, `dreamer.md` "Failure records"). Recorded on
/// the state row so a stuck task is diagnosable from the database alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The output had no complete expected root element (truncation or drift).
    ParseFailed,
    /// The completion succeeded but carried no text.
    EmptyCompletion,
    /// The transport failed, or the stream ended in an error.
    ProviderError,
    /// No model could be resolved for the task.
    NoModels,
    /// A host-side failure (the store, or applying a valid manifest).
    Unknown,
}

impl FailureClass {
    /// The spelling recorded in `mc_dream_state.last_error`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FailureClass::ParseFailed => "parse_failed",
            FailureClass::EmptyCompletion => "empty_completion",
            FailureClass::ProviderError => "provider_error",
            FailureClass::NoModels => "no_models",
            FailureClass::Unknown => "unknown",
        }
    }
}

/// A failed task run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskFailure {
    pub(super) class: FailureClass,
    pub(super) message: String,
}

impl TaskFailure {
    /// A host-side failure: the class is [`FailureClass::Unknown`] (the
    /// reference's catch-all) and the message keeps the root cause.
    pub(super) fn host(what: &str, error: &anyhow::Error) -> Self {
        Self {
            class: FailureClass::Unknown,
            message: format!("{what}: {}", error.root_cause()),
        }
    }

    /// The `last_error` text: the class first, so the column is greppable.
    pub(super) fn recorded(&self) -> String {
        format!("{}: {}", self.class.as_str(), self.message)
    }
}

/// What one task run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TaskOutcome {
    /// The task ran to completion. `cursor` patches the rotation cursor when
    /// the task owns one.
    Completed { detail: String, cursor: Option<u32> },
    /// The moment was due, but the cheap gate found no work. Not a failure:
    /// the reference advances a skipped slot without touching `last_run_at`.
    Skipped { detail: String },
    /// The run failed; see [`TaskFailure`].
    Failed(TaskFailure),
}

/// What one `(project, task)` did in this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEntry {
    /// The project the task ran for.
    pub project: String,
    /// The task that ran.
    pub task: DreamTask,
    /// How it ended.
    pub status: TickStatus,
}

/// The terminal status of one task in one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickStatus {
    /// Ran and applied its result.
    Completed { detail: String },
    /// Due, but the gate found nothing to do.
    Skipped { detail: String },
    /// Failed; the failure is on the state row and the tick continues.
    Failed {
        class: FailureClass,
        message: String,
    },
    /// Another task of the same `(project, task)` is still running: the
    /// re-entrancy guard refused this one.
    Busy,
}

/// What one tick did. Tasks that were not due are not listed — the report is
/// "what the tick acted on", and a task that was neither due nor busy was not
/// acted on at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// One entry per task the tick considered and skipped (busy) or ran.
    pub entries: Vec<TaskEntry>,
}
