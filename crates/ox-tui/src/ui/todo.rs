//! The `todowrite` block: upstream `TodoWrite` → `BlockTool title="# Todos"`
//! with one `TodoItem` per entry (`routes/session/index.tsx:3628`,
//! `component/todo-item.tsx`). Pure: todos and width in, lines out.

use ox_client::todo::{Todo, TodoStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme;

/// Upstream `todoMarker`.
pub(crate) fn marker(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Completed => "[\u{2713}]",
        TodoStatus::InProgress => "[\u{2022}]",
        TodoStatus::Cancelled => "[-]",
        TodoStatus::Pending => "[ ]",
    }
}

/// `BlockTool`: a raised band with a `┃` rail in the page colour on its left
/// (`SplitBorder`, `borderColor=background.base`), one padded row above and
/// below, content two columns past the rail, and a blank row between the
/// `# Todos` header and the list (`gap=1`).
pub fn lines(todos: &[Todo], width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let raised = palette.raised_base;
    // The rail is the page colour over the raised surface. Without a known
    // page colour (16-colour / colourless decks) it stays a blank cell.
    let (rail_glyph, rail) = match palette.backdrop {
        Some(page) => ("\u{2503}", raised.fg(Color::Rgb(page.r, page.g, page.b))),
        None => (" ", raised),
    };
    let fill = width.saturating_sub(1) as usize;
    let row = |spans: Vec<Span<'static>>| -> Line<'static> {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let mut out = Vec::with_capacity(spans.len() + 2);
        out.push(Span::styled(rail_glyph, rail));
        out.extend(spans);
        out.push(Span::styled(" ".repeat(fill.saturating_sub(used)), raised));
        Line::from(out)
    };
    let blank = || row(Vec::new());

    // Rail, two columns of padding, then the marker column (`[•] `).
    let body = width.saturating_sub(3 + 4).max(1) as usize;
    let options = textwrap::Options::new(body)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);

    let mut lines = vec![
        blank(),
        row(vec![Span::styled("  # Todos", palette.dim.patch(raised))]),
        blank(),
    ];
    for todo in todos {
        let ink = match todo.status {
            TodoStatus::InProgress => palette.warning,
            // Upstream `todo-item.tsx`: cancelled items recede to `faint`.
            TodoStatus::Cancelled => palette.faint,
            _ => palette.dim,
        };
        let content_style = if todo.status == TodoStatus::Cancelled {
            ink.add_modifier(Modifier::CROSSED_OUT)
        } else {
            ink
        };
        let cell = |style: Style| style.patch(raised);
        for (index, part) in textwrap::wrap(&todo.content, &options)
            .into_iter()
            .enumerate()
        {
            let head = if index == 0 {
                format!("  {} ", marker(todo.status))
            } else {
                " ".repeat(2 + 4)
            };
            lines.push(row(vec![
                Span::styled(head, cell(ink)),
                Span::styled(part.into_owned(), cell(content_style)),
            ]));
        }
    }
    lines.push(blank());
    lines
}
