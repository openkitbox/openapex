use ox_client::store::ToolState;

use super::block::Block;
use crate::ui::spinner;
use crate::ui::tool_text;

/// Everything the renderer needs beyond the `Block` itself. `Copy`, so `sync`
/// can compute it per block without allocating.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ViewState {
    /// Assistant blocks: the reasoning body is expanded (`r`).
    pub(super) reasoning_expanded: bool,
    /// Tool blocks: the output body is expanded (`o`).
    pub(super) tool_expanded: bool,
    /// Tool blocks: the row is the current focus (`[` / `]`).
    pub(super) tool_focused: bool,
    /// Explored blocks: the per-call tool rows are listed under the title.
    pub(super) explored_expanded: bool,
    /// The pointer is over this Explored title, Reasoning header, User block
    /// or a tool/Completion title row that links to a child session.
    pub(super) hovered: bool,
    /// The spinner frame of a block that spins (see `spins`).
    pub(super) spin: usize,
    /// Assistant blocks with a scheduled retry: whole seconds until it fires,
    /// from the transcript's wall clock. Part of the view so a tick rebuilds
    /// the block only when the displayed second changes.
    pub(super) retry_secs: Option<i64>,
}

/// Whether `tool` in `state` is drawn by `render::own_block` (when its data
/// allows). Those calls, and their inline fallbacks in the same state, have
/// nothing to open: the block shows everything, the fallback has no body.
pub(super) fn draws_block(tool: &str, state: ToolState) -> bool {
    match tool {
        "edit" | "write" | "question" => matches!(state, ToolState::Success),
        "patch" | "apply_patch" => matches!(state, ToolState::Success | ToolState::Failed),
        _ => false,
    }
}

/// Upstream `braille` spinner, 80ms a frame (`ui/spinner.rs` owns them).
pub(super) const SPINNER_FRAMES: [char; 10] = spinner::BRAILLE_FRAMES;
pub(super) const SPINNER_INTERVAL_MS: u128 = spinner::BRAILLE_INTERVAL_MS;

/// Blocks whose spinner moves with the clock: streaming reasoning, running
/// tool and todo rows (including calls whose child is currently running).
pub(super) fn spins(block: &Block) -> bool {
    let running = |state: &ToolState| {
        matches!(
            state,
            ToolState::Input | ToolState::Running | ToolState::Progress
        )
    };
    match block {
        Block::Reasoning { done, .. } => !done,
        Block::Tool { state, .. } => running(state),
        Block::Todo { state, .. } => running(state),
        Block::Shell {
            state,
            shell_running,
            ..
        } => *shell_running || running(state),
        Block::Explored(explored) => explored.running,
        _ => false,
    }
}

/// A failed tool or todo row with a message to show: the click target that
/// opens the error.
fn failed_with_error(block: &Block) -> bool {
    match block {
        Block::Tool { state, output, .. } | Block::Todo { state, output, .. } => {
            matches!(state, ToolState::Failed) && !output.is_empty()
        }
        _ => false,
    }
}

/// A tool or todo row a click opens: a failed row with an error, or a
/// generic row with something to list. Legacy rows (`shell`, …) are not
/// clickable unless failed, as before.
pub(super) fn clickable(block: &Block) -> bool {
    match block {
        Block::Tool { tool, state, .. } if draws_block(tool, *state) => false,
        Block::Tool { tool, .. } if !failed_with_error(block) => {
            tool_text::is_generic(tool) && tool_expandable(block)
        }
        _ => failed_with_error(block),
    }
}

/// The id of a tool block, for focus/expansion bookkeeping.
pub(super) fn entry_tool_id(block: &Block) -> Option<&str> {
    match block {
        Block::Tool { id, .. } | Block::Todo { id, .. } | Block::Shell { id, .. } => {
            Some(id.as_str())
        }
        _ => None,
    }
}

/// Whether a tool or todo row has something behind its header: a failed
/// row's error, a generic row's input and output (upstream `GenericTool`
/// `expandable`), or a finished legacy row's output.
pub(super) fn tool_expandable(block: &Block) -> bool {
    match block {
        Block::Tool {
            tool,
            state,
            output,
            input,
            ..
        } => {
            if draws_block(tool, *state) {
                false
            } else if failed_with_error(block) {
                true
            } else if tool_text::own_row(tool) {
                !output.is_empty() && matches!(state, ToolState::Success)
            } else {
                tool_text::is_generic(tool) && tool_text::generic_expandable(input, output)
            }
        }
        // A failed todo row opens its error with `o` like a tool's output.
        Block::Todo { .. } => failed_with_error(block),
        _ => false,
    }
}
