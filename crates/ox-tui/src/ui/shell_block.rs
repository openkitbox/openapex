//! A shell call drawn as a block: upstream `Shell` / `ShellDisplay`
//! (`routes/session/index.tsx:2967-3141`) inside the shared `BlockTool`
//! frame (`block_tool`). Pure: the call's fields and view flags borrowed in,
//! rows and expandability out.
//!
//! Layout (upstream `gap={1}` between parts): the command (`$ cd wd && cmd`,
//! a spinner in place of `$ ` while running, or `Writing command…` before the
//! command is known), the muted output, the `Background` badge; then the
//! frame's own gap and the failure text. Collapsed, the command is
//! character-wrapped to at most 2 rows and command + output share the
//! `collapse_shell_output` budget (`max_lines = 10`,
//! `max_chars = 10 × max(20, width − 6 − (running ? 2 : 0))`); expanded, the
//! command word-wraps and the output is shown whole.
//!
//! Deviations:
//! - Output source: the pipeline picks the tool result or the bound shell's
//!   fetched output (`crate::shell_captures`, which lists its deviations).
//!   Upstream never shows the tool result of a call with a `shellID`; ox
//!   shows it for a completed foreground call, whose result is the output.
//! - The `Background` badge shows while the call returned early *and* the
//!   shell is still alive (`DataStore::is_shell_running`); upstream shows it
//!   whenever `completed && metadata.status == "running"`, i.e. forever,
//!   even after the shell exited. Hiding the output of a collapsed
//!   background call follows upstream (metadata flag alone).
//! - Before `tool.called` ox has only streamed raw input text, never a
//!   parsed partial input, so the block says `Writing command…` until the
//!   whole command is known.
//! - Hover band and badge background are `palette.panel` (`raised.high`),
//!   not `theme.decrease(raised.base)` (plan V1); under the pointer the badge
//!   therefore sits on its own band colour.
//! - No permission colour: ox has no permission prompts on tool rows (V2).
//! - ox addition: the `[` / `]` tool focus draws the block on the hover band,
//!   so keyboard focus is visible without a pointer.
//! - history `Message::Shell` (user `!` shell, projected by the server from
//!   `session.shell.*`) is not rendered by this block yet.
//! - A spinner frame rewrites only the spinner span of the cached rows
//!   ([`Rendered::spinner_row`]); upstream re-renders reactively, which ox
//!   cannot afford for up to 1 MiB of expanded output at 12 frames a second.

use std::borrow::Cow;

use ox_client::store::ToolState;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme;

use super::block_tool::{self, BlockSpec};
use super::output_dialog;
use super::shell_collapse;

/// Lines of the collapsed command + output budget (upstream `maxLines`).
const MAX_LINES: usize = 10;
/// Rows a collapsed command wraps to (upstream `maxHeight={2}`).
const COMMAND_ROWS: usize = 2;
/// Columns the spinner and its gap take before a running command.
const SPINNER_COLS: usize = 2;
const WRITING: &str = "Writing command\u{2026}";
/// The line ahead of output whose head was dropped (upstream U4).
const OMITTED: &str = "[earlier output omitted]";

/// One shell call, borrowed from its block, plus the view flags.
#[derive(Debug, Clone, Copy)]
pub struct ShellSpec<'a> {
    /// `input.command`; empty while the call is still being written.
    pub command: &'a str,
    /// The formatted `input.workdir` (`tool_text::format_path`); empty when
    /// absent.
    pub workdir: &'a str,
    pub state: ToolState,
    /// The tool result text (the shell's output, or the failure message), or
    /// the bound shell's fetched output.
    pub output: &'a str,
    /// `output` lacks the shell's earliest bytes: shown as a leading
    /// `[earlier output omitted]` line.
    pub omitted: bool,
    /// The call is bound to a shell (`metadata.shellID`): always expandable.
    pub has_shell: bool,
    /// The call returned while its shell kept running (`metadata.status ==
    /// "running"`); its output is hidden until expanded.
    pub background: bool,
    /// The bound shell is still alive.
    pub shell_running: bool,
    pub expanded: bool,
    /// Under the pointer or holding the tool focus.
    pub hovered: bool,
    pub spinner: char,
}

impl ShellSpec<'_> {
    /// Upstream `isRunning`: the call runs or its shell is still alive.
    fn running(&self) -> bool {
        self.shell_running || matches!(self.state, ToolState::Running | ToolState::Progress)
    }

    fn streaming(&self) -> bool {
        matches!(self.state, ToolState::Input)
    }

    /// Upstream `output()`: nothing while streaming or for a collapsed
    /// background call; the failure text goes to the frame's error instead.
    fn output_text(&self) -> String {
        if self.streaming()
            || matches!(self.state, ToolState::Failed)
            || (self.background && !self.expanded)
        {
            return String::new();
        }
        let text = output_dialog::clean(self.output.trim());
        if self.omitted {
            format!("{OMITTED}\n{text}")
        } else {
            text
        }
    }

    /// Upstream `input()`: `$ ` (dropped next to the spinner), the
    /// `cd <workdir> && ` prefix unless the workdir is `.`, the command.
    fn input_text(&self) -> String {
        if self.command.is_empty() {
            return String::new();
        }
        let dollar = if self.running() { "" } else { "$ " };
        if self.workdir.is_empty() || self.workdir == "." {
            format!("{dollar}{}", self.command)
        } else {
            format!("{dollar}cd {} && {}", self.workdir, self.command)
        }
    }

    fn max_chars(&self, width: u16) -> usize {
        let running = if self.running() { 2 } else { 0 };
        MAX_LINES * usize::from(width).saturating_sub(6 + running).max(20)
    }
}

/// The rendered block and whether a click / `o` toggles it (upstream
/// `expandable`: a bound shell, or a collapse that cut something).
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub expandable: bool,
    /// The row of `lines` whose first content span is the `"{spinner} "`
    /// lead, when the block spins. A spinner-only change rewrites that one
    /// span instead of re-rendering (and re-wrapping) the whole output.
    pub spinner_row: Option<usize>,
}

/// Index of the spinner lead within its row: after the rail and the padding
/// that `block_tool::frame` puts in front of every content span.
pub const SPINNER_SPAN: usize = 2;

/// The spinner lead span's text for `glyph`, as `render` draws it.
#[must_use]
pub fn spinner_lead(glyph: char) -> String {
    format!("{glyph} ")
}

/// Draw the block at `width` columns.
#[must_use]
pub fn render(spec: &ShellSpec<'_>, width: u16) -> Rendered {
    let palette = theme::palette();
    let input = spec.input_text();
    let output = spec.output_text();
    let collapsed =
        shell_collapse::collapse_shell_output(&input, &output, MAX_LINES, spec.max_chars(width));
    let expandable = spec.has_shell || collapsed.overflow;
    let (shown_input, shown_output): (&str, &str) = if spec.expanded {
        (&input, &output)
    } else {
        (&collapsed.input, &collapsed.output)
    };

    let usable = block_tool::content_width(width);
    let mut parts: Vec<Vec<Line<'static>>> = Vec::with_capacity(3);
    // Both spinning first rows sit on body row 0, i.e. frame row 1 (the
    // frame opens with a blank row and there is no title).
    let mut spins = spec.running();
    if spec.command.is_empty() {
        let row = if spec.running() || spec.streaming() {
            spins = true;
            Line::from(vec![
                Span::styled(spinner_lead(spec.spinner), palette.text),
                Span::styled(WRITING, palette.text),
            ])
        } else {
            Line::from(Span::styled(WRITING, palette.dim))
        };
        parts.push(vec![row]);
    } else {
        parts.push(command_rows(spec, shown_input, usable));
        if !shown_output.is_empty() {
            let rows = block_tool::wrap(shown_output, usable)
                .into_iter()
                .map(|row| Line::from(Span::styled(row, palette.dim)))
                .collect();
            parts.push(rows);
        }
    }
    if spec.background && spec.shell_running {
        parts.push(vec![Line::from(Span::styled(
            " Background ",
            palette.dim.patch(palette.panel),
        ))]);
    }

    let mut body = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            body.push(Line::default());
        }
        body.extend(part);
    }
    let error = matches!(spec.state, ToolState::Failed).then_some(spec.output);
    let block = BlockSpec {
        title: None,
        spinner: None,
        error,
        hovered: spec.hovered,
        ..BlockSpec::default()
    };
    Rendered {
        lines: block_tool::lines(&block, body, width),
        expandable,
        spinner_row: spins.then_some(1),
    }
}

/// The command rows: character-wrapped to 2 rows collapsed, word-wrapped
/// without a cap expanded; a running command sits behind the spinner with
/// its continuation rows aligned under the text.
fn command_rows(spec: &ShellSpec<'_>, input: &str, usable: usize) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let running = spec.running();
    let text_width = if running {
        usable.saturating_sub(SPINNER_COLS).max(1)
    } else {
        usable
    };
    let rows: Vec<Cow<'_, str>> = if spec.expanded {
        block_tool::wrap(input, text_width)
            .into_iter()
            .map(Cow::Owned)
            .collect()
    } else {
        char_wrap(input, text_width, COMMAND_ROWS)
    };
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let text = Span::styled(row.into_owned(), palette.text);
            if !running {
                return Line::from(text);
            }
            let lead = if index == 0 {
                spinner_lead(spec.spinner)
            } else {
                " ".repeat(SPINNER_COLS)
            };
            Line::from(vec![Span::styled(lead, palette.text), text])
        })
        .collect()
}

/// `text` broken at grapheme boundaries into rows of at most `width`
/// columns (upstream `wrapMode="char"`), explicit newlines kept, at most
/// `max_rows` rows (`maxHeight`).
fn char_wrap(text: &str, width: usize, max_rows: usize) -> Vec<Cow<'_, str>> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut start = 0;
        let mut used = 0;
        for (offset, grapheme) in line.grapheme_indices(true) {
            let cols = UnicodeWidthStr::width(grapheme);
            if used + cols > width && used > 0 {
                rows.push(Cow::Borrowed(&line[start..offset]));
                start = offset;
                used = 0;
            }
            used += cols;
        }
        rows.push(Cow::Borrowed(&line[start..]));
        if rows.len() >= max_rows {
            break;
        }
    }
    rows.truncate(max_rows);
    rows
}
