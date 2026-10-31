//! The `todowrite` block: upstream `TodoWrite` → `BlockTool title="# Todos"`
//! with one `TodoItem` per entry (`routes/session/index.tsx:3628`,
//! `component/todo-item.tsx`). Pure: todos and width in, lines out.

use ox_client::todo::{Todo, TodoStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme;

/// Upstream `todoMarker`.
fn marker(status: TodoStatus) -> &'static str {
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

#[cfg(test)]
mod tests {
    use ox_client::todo::{Todo, TodoStatus};
    use ratatui::style::Modifier;

    use super::lines;

    fn todo(content: &str, status: TodoStatus) -> Todo {
        Todo {
            content: content.into(),
            status,
        }
    }

    /// The rail cell: `┃` where the page colour is known, else a blank.
    fn rail_glyph() -> &'static str {
        if super::theme::palette().backdrop.is_some() {
            "\u{2503}"
        } else {
            " "
        }
    }

    fn text(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn header_gap_markers_and_padding_rows() {
        let rows = text(&lines(
            &[
                todo("done", TodoStatus::Completed),
                todo("doing", TodoStatus::InProgress),
                todo("nope", TodoStatus::Cancelled),
                todo("later", TodoStatus::Pending),
            ],
            30,
        ));
        let rail = rail_glyph();
        let trimmed: Vec<_> = rows.iter().map(|r| r.trim_end().to_owned()).collect();
        assert_eq!(
            trimmed,
            [
                "".to_owned(),
                "  # Todos".to_owned(),
                "".to_owned(),
                "  [✓] done".to_owned(),
                "  [•] doing".to_owned(),
                "  [-] nope".to_owned(),
                "  [ ] later".to_owned(),
                "".to_owned(),
            ]
            .map(|row| format!("{rail}{row}").trim_end().to_owned())
        );
    }

    #[test]
    fn rows_fill_the_full_width() {
        for row in text(&lines(&[todo("x", TodoStatus::Pending)], 24)) {
            assert_eq!(unicode_width::UnicodeWidthStr::width(row.as_str()), 24);
        }
    }

    #[test]
    fn long_content_wraps_under_the_text_not_the_marker() {
        let rows = text(&lines(
            &[todo("alpha beta gamma delta", TodoStatus::Pending)],
            18,
        ));
        let body: Vec<_> = rows[3..rows.len() - 1]
            .iter()
            .map(|r| r.trim_end().to_owned())
            .collect();
        assert!(body.len() >= 2, "{body:?}");
        assert!(body[0].contains("[ ] alpha"), "{body:?}");
        let rail = if super::theme::palette().backdrop.is_some() {
            "┃"
        } else {
            " "
        };
        assert!(body[1].starts_with(&format!("{rail}      ")), "{body:?}");
        assert!(!body[1].contains('['), "{body:?}");
    }

    #[test]
    fn only_cancelled_content_is_struck_through() {
        let out = lines(
            &[
                todo("a", TodoStatus::Cancelled),
                todo("b", TodoStatus::Completed),
            ],
            20,
        );
        let struck = |line: &ratatui::text::Line<'_>| {
            line.spans
                .iter()
                .filter(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT))
                .map(|s| s.content.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(struck(&out[3]), ["a"], "content only, not the marker");
        assert!(struck(&out[4]).is_empty());
    }

    #[test]
    fn in_progress_uses_the_warning_colour_and_the_rest_the_muted_one() {
        let palette = super::theme::palette();
        let out = lines(
            &[
                todo("a", TodoStatus::InProgress),
                todo("b", TodoStatus::Completed),
            ],
            20,
        );
        assert_eq!(out[3].spans[1].style.fg, palette.warning.fg);
        assert_eq!(out[4].spans[1].style.fg, palette.dim.fg);
    }

    #[test]
    fn cancelled_items_use_the_faint_colour() {
        let palette = super::theme::palette();
        let out = lines(&[todo("a", TodoStatus::Cancelled)], 20);
        let faint = palette.faint.patch(palette.raised_base);
        assert_eq!(out[3].spans[1].style.fg, faint.fg, "marker");
        assert_eq!(out[3].spans[2].style.fg, faint.fg, "content");
        assert_eq!(
            out[3].spans[1].style.add_modifier, faint.add_modifier,
            "marker carries faint's modifiers (DIM on colourless decks)"
        );
    }
}
