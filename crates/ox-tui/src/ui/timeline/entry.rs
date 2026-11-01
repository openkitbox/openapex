use ratatui::text::Line;

use super::block::Block;
use super::render::render_entry;
use super::view::{SPINNER_FRAMES, ViewState, tool_expandable};

pub(super) struct Entry {
    pub(super) block: Block,
    pub(super) generation: u64,
    pub(super) lines: Vec<Line<'static>>,
    pub(super) width: u16,
    pub(super) view: ViewState,
    /// A click or `o` toggles the block (see `entry_expandable`). Computed
    /// with `lines`, because a shell block's answer depends on the width its
    /// collapse ran at; pointer moves read it instead of re-rendering.
    pub(super) expandable: bool,
    /// The row of `lines` holding a shell block's spinner lead (see
    /// `shell_block::Rendered::spinner_row`), rendered with `lines`.
    spinner_row: Option<usize>,
}

impl Entry {
    pub(super) fn new(block: Block, generation: u64, width: u16, view: ViewState) -> Self {
        let (lines, expandable, spinner_row) = render_entry(&block, width, view);
        Self {
            block,
            generation,
            lines,
            width,
            view,
            expandable,
            spinner_row,
        }
    }

    pub(super) fn lines_for(&mut self, width: u16) -> &[Line<'static>] {
        if self.width != width {
            (self.lines, self.expandable, self.spinner_row) =
                render_entry(&self.block, width, self.view);
            self.width = width;
        }
        &self.lines
    }

    /// Move a shell block to spinner frame `view.spin` by rewriting its one
    /// spinner span, when that is the only change: the expanded output of a
    /// running shell (up to the capture cap) is not cleaned and re-wrapped
    /// 12 times a second. The rows stay exactly what `render_entry` would
    /// draw, since the glyph is the only thing the frame index selects.
    pub(super) fn respin(&mut self, view: ViewState) -> bool {
        let Some(row) = self.spinner_row else {
            return false;
        };
        let unchanged = ViewState {
            spin: view.spin,
            ..self.view
        };
        if unchanged != view {
            return false;
        }
        let Some(span) = self
            .lines
            .get_mut(row)
            .and_then(|line| line.spans.get_mut(crate::ui::shell_block::SPINNER_SPAN))
        else {
            return false;
        };
        let glyph = SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()];
        span.content = crate::ui::shell_block::spinner_lead(glyph).into();
        self.view = view;
        true
    }
}

/// Whether a click or `o` toggles the entry: cached at render for a shell
/// block, derived from the block for tool and todo rows.
pub(super) fn entry_expandable(entry: &Entry) -> bool {
    match entry.block {
        Block::Shell { .. } => entry.expandable,
        _ => tool_expandable(&entry.block),
    }
}
