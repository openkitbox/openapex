use std::borrow::Cow;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use ox_client::store::ToolState;
use ox_client::todo::Todo;
use ox_schema::support::Record;

use crate::theme;

use super::block::{Block, Explored, ExploredCount};
use super::text::{bound_wrapped, wrap_output};
use super::view::{SPINNER_FRAMES, ViewState, tool_expandable};
use crate::ui::tool_row::{self, InlineSpec, RowView};
use crate::ui::tool_text::{self, ToolText};

/// A tool row expands to at most this many output lines: a chatty
/// `cargo build` must not turn one row into ten thousand wrapped lines.
pub(crate) const TOOL_OUTPUT_MAX_LINES: usize = 200;

/// Upstream `StatusBadge raised` (`routes/session/index.tsx:2860`): the
/// label padded by one space each side, muted on the raised background.
fn push_background_badge(header: &mut Line<'static>) {
    let palette = theme::palette();
    let mut badge = palette.dim;
    badge.bg = palette.panel.bg;
    header.spans.push(Span::raw(" "));
    header.spans.push(Span::styled(" Background ", badge));
}

/// The title row, then (expanded) one inline tool row per grouped call, each
/// with its per-tool text (`group-view.tsx:129-157`, `children("tool")`).
pub(super) fn explored_block(
    explored: &Explored,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![explored_line(explored, view)];
    if view.explored_expanded {
        for call in &explored.calls {
            let Some(text) = tool_text::text(
                &call.tool,
                &call.input,
                call.metadata.as_deref(),
                call.state,
                tool_text::paths(),
            ) else {
                continue;
            };
            lines.extend(text_row(&text, call.state, None, view, width));
            lines.extend(loaded_lines(&text));
        }
    }
    lines
}

/// Upstream `group-view.tsx:120-151`: `→ Explored: 2 reads, 1 search` once
/// every call finished, otherwise the spinner and `Exploring: …`.
fn explored_line(explored: &Explored, view: ViewState) -> Line<'static> {
    let palette = theme::palette();
    let mut parts = Vec::with_capacity(explored.counts.len());
    for entry in &explored.counts {
        let ExploredCount { noun, count } = *entry;
        let suffix = match (count, noun) {
            (1, _) => "",
            (_, "search" | "fetch") => "es",
            _ => "s",
        };
        parts.push(format!("{count} {noun}{suffix}"));
    }
    let (icon, label) = if explored.running {
        (
            SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
            "Exploring:",
        )
    } else {
        ('\u{2192}', "Explored:")
    };
    Line::from(Span::styled(
        format!("   {icon} {label} {}", parts.join(", ")),
        explored_title_style(palette, view.hovered),
    ))
}

/// Muted at rest, full text colour under the pointer (`group-view.tsx:146`).
fn explored_title_style(palette: &theme::Palette, hovered: bool) -> ratatui::style::Style {
    if hovered { palette.text } else { palette.dim }
}

/// The head line of a legacy inline tool row (tools without `tool_text`
/// texts), drawn by `tool_row`: the label
/// is `tool summary` both while running and once done; `⚙` on success, `✗`
/// on failure; bold when focused.
fn tool_header(tool: &str, summary: &str, row: RowView, focused: bool) -> Line<'static> {
    let label = if summary.is_empty() {
        Cow::Borrowed(tool)
    } else {
        Cow::Owned(format!("{tool} {summary}"))
    };
    let icon = if matches!(row.state, ToolState::Failed) {
        "\u{2717}"
    } else {
        "\u{2699}"
    };
    let spec = InlineSpec {
        icon,
        pending: &label,
        complete: Some(&label),
        error: None,
    };
    // The header alone: the output body below stays `tool_block`'s, so the
    // row is not cut here (the paragraph clips at the edge as before).
    let mut header = tool_row::lines(&spec, row, u16::MAX)
        .into_iter()
        .next()
        .unwrap_or_default();
    if focused {
        header = header.patch_style(Modifier::BOLD);
    }
    header
}

/// Upstream `InlineToolRow` (`message-parts.tsx:202`): the header, then output
/// hanging below only when expanded (`o`) or when the call failed; the
/// focused row is bold instead of advertising its key.
pub(super) fn tool_block(
    block: &Block,
    tool: &str,
    state: ToolState,
    summary: &str,
    output: &str,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let background = matches!(
        block,
        Block::Tool {
            background: true,
            ..
        }
    );
    let expandable = tool_expandable(block);
    let focused = view.tool_focused && expandable;
    // The pipeline includes live child activity in `state`; the background
    // badge describes the call's result and must not force a spinner.
    let row = RowView {
        state,
        spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
        hovered: view.hovered,
        error_expanded: false,
    };
    let mut header = tool_header(tool, summary, row, focused);
    if background {
        push_background_badge(&mut header);
    }
    let mut lines = vec![header];
    let failed = matches!(state, ToolState::Failed);
    let body_style = if failed { palette.warn } else { palette.dim };
    if view.tool_expanded && expandable {
        let wrapped = wrap_output(output, width.saturating_sub(5));
        let shown = wrapped.len().min(TOOL_OUTPUT_MAX_LINES);
        for line in wrapped.iter().take(shown) {
            lines.push(Line::from(Span::styled(format!("     {line}"), body_style)));
        }
        if wrapped.len() > shown {
            lines.push(Line::from(Span::styled(
                format!(
                    "     \u{2026} \u{5176}\u{4f59} {} \u{884c}",
                    wrapped.len() - shown
                ),
                body_style,
            )));
        }
    } else if failed && !output.is_empty() {
        for line in bound_wrapped(output, width.saturating_sub(5), 4) {
            lines.push(Line::from(Span::styled(format!("     {line}"), body_style)));
        }
    }
    lines
}

/// An inline tool row with per-tool texts (`tool_text`), drawn by
/// `tool_row`: Read's `↳ Loaded` rows below it, the error of a failed row
/// once opened, and a generic row's `key: value` list once expanded
/// (upstream `GenericTool`, `index.tsx:2741-2767`). Bold when focused.
///
/// Deviation: an expanded generic body keeps at most
/// [`TOOL_OUTPUT_MAX_LINES`] rows, like a shell row, where upstream lists
/// everything.
pub(super) fn text_tool_block(
    block: &Block,
    text: &ToolText<'_>,
    input: &Record,
    output: &str,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let Block::Tool {
        state, background, ..
    } = block
    else {
        return Vec::new();
    };
    let state = *state;
    let failed = matches!(state, ToolState::Failed);
    let error = (failed && !output.is_empty()).then_some(output);
    let mut lines = text_row(text, state, error, view, width);
    if let Some(header) = lines.first_mut() {
        if *background {
            push_background_badge(header);
        }
        if view.tool_focused && tool_expandable(block) {
            *header = std::mem::take(header).patch_style(Modifier::BOLD);
        }
    }
    lines.extend(loaded_lines(text));
    if text.generic && view.tool_expanded && !failed {
        let rows = tool_text::generic_details(input, output);
        lines.extend(detail_lines(&rows, width));
    }
    lines
}

/// One `tool_text` row drawn by `tool_row` (upstream `InlineToolRow`): a
/// spinner row shows the label next to the spinner; otherwise icon + label
/// once complete or failed, else the spinner + pending text. The background
/// badge is independent of spinner activity.
fn text_row(
    text: &ToolText<'_>,
    state: ToolState,
    error: Option<&str>,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let failed = matches!(state, ToolState::Failed);
    let spinning = text.spinner;
    let spec = InlineSpec {
        icon: text.icon,
        pending: if spinning { &text.label } else { text.pending },
        complete: (!spinning && (text.complete || failed)).then_some(text.label.as_str()),
        error,
    };
    let row = RowView {
        state: if spinning { ToolState::Running } else { state },
        spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
        hovered: view.hovered,
        error_expanded: view.tool_expanded,
    };
    tool_row::lines(&spec, row, width)
}

/// Read's `↳ Loaded <file>` rows under its tool row.
fn loaded_lines<'a>(text: &'a ToolText<'_>) -> impl Iterator<Item = Line<'static>> + 'a {
    let palette = theme::palette();
    text.loaded.iter().map(move |file| {
        Line::from(Span::styled(
            format!("      \u{21b3} Loaded {file}"),
            palette.dim,
        ))
    })
}

/// Upstream `GenericTool`'s expanded body: per row a muted `key: ` and the
/// value word-wrapped beside it, everything indented past the icon.
fn detail_lines(rows: &[(&str, String)], width: u16) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    const PAD: usize = 5;
    let palette = theme::palette();
    let mut out = Vec::new();
    for (key, value) in rows {
        let key = format!("{key}: ");
        let hang = PAD + key.width();
        let usable = usize::from(width).saturating_sub(hang).max(1);
        let options = textwrap::Options::new(usable)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);
        let mut first = true;
        for line in value.split('\n') {
            let parts = textwrap::wrap(line, &options);
            let parts = if parts.is_empty() {
                vec![Cow::Borrowed("")]
            } else {
                parts
            };
            for part in parts {
                if out.len() >= TOOL_OUTPUT_MAX_LINES {
                    out.push(Line::from(Span::styled(
                        format!("{:PAD$}\u{2026}", ""),
                        palette.dim,
                    )));
                    return out;
                }
                let lead = if first {
                    Span::styled(format!("{:PAD$}{key}", ""), palette.dim)
                } else {
                    Span::raw(" ".repeat(hang))
                };
                first = false;
                out.push(Line::from(vec![
                    lead,
                    Span::styled(part.into_owned(), palette.text),
                ]));
            }
        }
    }
    out
}

/// Upstream `TodoWrite`: the panel when the call completed with todos, else
/// an `InlineTool` row (`index.tsx:3623-3647`): `⚙`, pending `Updating
/// todos…`, complete `Todo update failed` on error and `Cleared todos`
/// otherwise; a click on a failed row lists the error.
pub(super) fn todo_block(
    state: ToolState,
    todos: &[Todo],
    output: &str,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    if matches!(state, ToolState::Success) && !todos.is_empty() {
        return crate::ui::todo::lines(todos, width);
    }
    let failed = matches!(state, ToolState::Failed);
    let spec = InlineSpec {
        icon: "\u{2699}",
        pending: "Updating todos\u{2026}",
        complete: Some(if failed {
            "Todo update failed"
        } else {
            "Cleared todos"
        }),
        error: (!output.is_empty()).then_some(output),
    };
    let row = RowView {
        state,
        spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
        hovered: false,
        error_expanded: view.tool_expanded,
    };
    let mut lines = tool_row::lines(&spec, row, width);
    // The `[` / `]` focus reads as bold, like a tool row.
    if view.tool_focused
        && failed
        && let Some(header) = lines.first_mut()
    {
        *header = std::mem::take(header).patch_style(Modifier::BOLD);
    }
    lines
}
