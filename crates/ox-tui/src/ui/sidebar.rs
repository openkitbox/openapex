//! The session sidebar: upstream `routes/session/sidebar.tsx` with the
//! `opencode.sidebar.todo` section (`feature-plugins/sidebar/todo.tsx`,
//! `component/todo-item.tsx`). Pure: title, todos and open state in, cells
//! out.
//!
//! Deviations from upstream:
//! - Only the Todo section; no context / MCP / LSP sections and no footer slot.
//! - No scrollbox or scrollbar: content past the panel's bottom padding is
//!   clipped.
//! - No title shimmer; the title is always bold `text.base`.
//! - The todo open state lives in memory, not in plugin storage, so it resets
//!   on restart.
//! - The untitled fallback is a plain "New session" (upstream appends an ISO
//!   timestamp), and the narrow overlay has no dimmed backdrop.

use ox_client::todo::Todo;
use ox_client::todo::TodoStatus;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::widgets::Clear;
use unicode_width::UnicodeWidthStr;

use crate::theme;
use crate::ui::block_tool;
use crate::ui::todo;

/// Upstream `SESSION_SIDEBAR_WIDTH`.
pub const SIDEBAR_WIDTH: u16 = 42;

/// Upstream `paddingLeft` / `paddingRight` of the panel.
const PAD_X: u16 = 2;
/// Upstream title box `paddingRight={2}`.
const TITLE_PAD_RIGHT: u16 = 2;
/// Upstream scrollbox content `paddingRight={1}`.
const CONTENT_PAD_RIGHT: u16 = 1;
/// The `[•] ` marker column of a todo row.
const MARKER_WIDTH: u16 = 4;

/// Whether the Todo section shows its rows. Upstream default `open: true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TodoOpen {
    #[default]
    Open,
    Closed,
}

impl TodoOpen {
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::Open,
        }
    }
}

/// Upstream shows the arrow and accepts clicks only past two items.
#[must_use]
pub fn collapsible(todos: &[Todo]) -> bool {
    todos.len() > 2
}

/// Draw the sidebar into `area`. Returns the screen row of the Todo header
/// when it is clickable (more than two items), for the caller's hit-test.
#[must_use]
pub fn render(
    title: &str,
    todos: &[Todo],
    open: TodoOpen,
    area: Rect,
    buf: &mut Buffer,
) -> Option<u16> {
    use ratatui::widgets::Widget;

    let palette = theme::palette();
    let raised = palette.raised_base;
    // The narrow overlay sits on the transcript: wipe its glyphs first.
    Clear.render(area, buf);
    buf.set_style(area, raised);
    if area.width <= PAD_X * 2 || area.height <= 2 {
        return None;
    }
    let x = area.x + PAD_X;
    let inner = area.width - PAD_X * 2;
    let bottom = area.y + area.height - 1;
    let mut y = area.y + 1;
    let cell = |style: Style| style.patch(raised);

    let title_width = inner.saturating_sub(TITLE_PAD_RIGHT).max(1);
    let title_style = cell(palette.text.add_modifier(Modifier::BOLD));
    for part in block_tool::wrap(title, title_width as usize) {
        if y >= bottom {
            return None;
        }
        buf.set_stringn(x, y, &part, title_width as usize, title_style);
        y += 1;
    }
    // Title box `paddingBottom={1}`.
    y += 1;

    if todos.is_empty() || y >= bottom {
        return None;
    }
    let content = inner.saturating_sub(CONTENT_PAD_RIGHT).max(1);
    let header = y;
    let text = cell(palette.text);
    let mut col = x;
    if collapsible(todos) {
        let arrow = match open {
            TodoOpen::Open => "\u{25bc}",
            TodoOpen::Closed => "\u{25b6}",
        };
        buf.set_string(col, y, arrow, text);
        // `gap={1}` between the arrow and the label.
        col += arrow.width() as u16 + 1;
    }
    let (col, _) = buf.set_stringn(
        col,
        y,
        "Todo",
        content as usize,
        text.add_modifier(Modifier::BOLD),
    );
    let show_rows = !collapsible(todos) || open == TodoOpen::Open;
    if !show_rows {
        let remaining = todos
            .iter()
            .filter(|t| matches!(t.status, TodoStatus::Pending | TodoStatus::InProgress))
            .count();
        let used = col - x;
        buf.set_stringn(
            col,
            y,
            format!(" ({remaining} remaining)"),
            content.saturating_sub(used) as usize,
            cell(palette.dim),
        );
    }
    y += 1;

    if show_rows {
        let body = content.saturating_sub(MARKER_WIDTH).max(1);
        'rows: for item in todos {
            let ink = match item.status {
                TodoStatus::InProgress => palette.warning,
                TodoStatus::Cancelled => palette.faint,
                _ => palette.dim,
            };
            let content_style = if item.status == TodoStatus::Cancelled {
                ink.add_modifier(Modifier::CROSSED_OUT)
            } else {
                ink
            };
            for (index, part) in block_tool::wrap(&item.content, body as usize)
                .iter()
                .enumerate()
            {
                if y >= bottom {
                    break 'rows;
                }
                if index == 0 {
                    buf.set_string(x, y, todo::marker(item.status), cell(ink));
                }
                buf.set_stringn(
                    x + MARKER_WIDTH,
                    y,
                    part,
                    body as usize,
                    cell(content_style),
                );
                y += 1;
            }
        }
    }

    collapsible(todos).then_some(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(content: &str, status: TodoStatus) -> Todo {
        Todo {
            content: content.to_owned(),
            status,
        }
    }

    fn four() -> Vec<Todo> {
        vec![
            todo("read the spec", TodoStatus::Completed),
            todo("write the tests", TodoStatus::InProgress),
            todo("implement", TodoStatus::Pending),
            todo("old idea", TodoStatus::Cancelled),
        ]
    }

    fn draw(todos: &[Todo], open: TodoOpen) -> (Buffer, Option<u16>) {
        let area = Rect::new(0, 0, SIDEBAR_WIDTH, 16);
        let mut buf = Buffer::empty(area);
        let header = render("My session", todos, open, area, &mut buf);
        (buf, header)
    }

    fn row(buf: &Buffer, y: u16) -> String {
        let width = buf.area.width;
        let text: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
        text.trim_end().to_owned()
    }

    fn rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height).map(|y| row(buf, y)).collect()
    }

    #[test]
    fn title_is_bold_then_a_blank_row_and_the_whole_panel_is_raised() {
        let (buf, header) = draw(&[], TodoOpen::Open);
        let pal = theme::palette();
        assert_eq!(row(&buf, 0), "");
        assert_eq!(row(&buf, 1), "  My session");
        assert!(
            buf[(2, 1)].modifier.contains(Modifier::BOLD),
            "title must be bold"
        );
        assert_eq!(
            buf[(2, 1)].fg,
            pal.text.patch(pal.raised_base).fg.unwrap_or_default(),
            "title in text.base"
        );
        assert_eq!(header, None, "empty list has no header");
        let bg = pal.raised_base.bg.unwrap_or_default();
        assert_eq!(buf[(41, 15)].bg, bg, "fill must cover the panel corner");
    }

    #[test]
    fn empty_list_hides_the_section() {
        let (buf, _) = draw(&[], TodoOpen::Open);
        assert!(
            rows(&buf).iter().all(|r| !r.contains("Todo")),
            "{:?}",
            rows(&buf)
        );
    }

    #[test]
    fn two_items_have_no_arrow_and_always_list() {
        let todos = vec![
            todo("a", TodoStatus::Pending),
            todo("b", TodoStatus::Completed),
        ];
        let (buf, header) = draw(&todos, TodoOpen::Closed);
        assert_eq!(row(&buf, 3), "  Todo");
        assert_eq!(row(&buf, 4), "  [ ] a");
        assert_eq!(row(&buf, 5), "  [\u{2713}] b");
        assert_eq!(header, None, "two items are not clickable");
    }

    #[test]
    fn more_than_two_open_shows_down_arrow_and_rows() {
        let (buf, header) = draw(&four(), TodoOpen::Open);
        assert_eq!(row(&buf, 3), "  \u{25bc} Todo");
        assert_eq!(row(&buf, 4), "  [\u{2713}] read the spec");
        assert_eq!(row(&buf, 5), "  [\u{2022}] write the tests");
        assert_eq!(row(&buf, 7), "  [-] old idea");
        assert_eq!(
            header,
            Some(3),
            "more than two items make the header clickable"
        );
    }

    #[test]
    fn more_than_two_closed_shows_remaining_and_no_rows() {
        let (buf, header) = draw(&four(), TodoOpen::Closed);
        assert_eq!(row(&buf, 3), "  \u{25b6} Todo (2 remaining)");
        assert_eq!(row(&buf, 4), "");
        assert_eq!(header, Some(3), "a folded header stays clickable");
        let pal = theme::palette();
        assert_eq!(
            buf[(10, 3)].fg,
            pal.dim.patch(pal.raised_base).fg.unwrap_or_default(),
            "remaining count is muted"
        );
    }

    #[test]
    fn in_progress_is_warning_and_cancelled_is_struck_through() {
        let (buf, _) = draw(&four(), TodoOpen::Open);
        let pal = theme::palette();
        let warning = pal.warning.patch(pal.raised_base).fg.unwrap_or_default();
        assert_eq!(buf[(2, 5)].fg, warning, "in_progress marker");
        assert_eq!(buf[(6, 5)].fg, warning, "in_progress content");
        assert!(
            buf[(6, 7)].modifier.contains(Modifier::CROSSED_OUT),
            "cancelled content is struck through"
        );
        assert!(
            !buf[(2, 7)].modifier.contains(Modifier::CROSSED_OUT),
            "the marker is not struck through"
        );
        let dim = pal.dim.patch(pal.raised_base).fg.unwrap_or_default();
        assert_eq!(buf[(6, 6)].fg, dim, "pending is muted");
    }

    #[test]
    fn long_content_wraps_with_a_hanging_indent() {
        let todos = vec![todo(
            "one two three four five six seven eight nine ten",
            TodoStatus::Pending,
        )];
        let (buf, _) = draw(&todos, TodoOpen::Open);
        // 42 - 4 padding - 1 content padding - 4 marker = 33 columns.
        assert_eq!(row(&buf, 4), "  [ ] one two three four five six seven");
        assert_eq!(row(&buf, 5), "      eight nine ten");
    }

    #[test]
    fn the_panel_wipes_what_was_under_it() {
        let area = Rect::new(0, 0, SIDEBAR_WIDTH, 4);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 2, "transcript text", Style::default());
        let _ = render("t", &[], TodoOpen::Open, area, &mut buf);
        assert_eq!(row(&buf, 2), "", "overlay must not show the transcript");
    }

    #[test]
    fn toggled_flips() {
        assert_eq!(TodoOpen::Open.toggled(), TodoOpen::Closed, "open folds");
        assert_eq!(TodoOpen::Closed.toggled(), TodoOpen::Open, "closed unfolds");
    }
}
