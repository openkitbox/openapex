//! Home 落地页：居中 logo + 共享的 [`Prompt`] 输入面。
//!
//! 视觉规格见 .opencode/opencode-tui-components/16-home-splash.md；输入面本身
//! 由 `ui::prompt` 负责（面板、metadata 行、`╹`/`▀` 收尾、footer、slash 菜单）。
//! 这里只剩 Home 独有的东西：logo 掩码渲染与 75 列内容列。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme;
use crate::ui::prompt::Prompt;

pub const LOGO_LEFT: [&str; 4] = [
    "                   ",
    "█▀▀█ █▀▀█ █▀▀█ █▀▀▄",
    "█__█ █__█ █^^^ █__█",
    "▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀~~▀",
];

pub const LOGO_RIGHT: [&str; 4] = [
    "             ▄     ",
    "█▀▀▀ █▀▀█ █▀▀█ █▀▀█",
    "█___ █__█ █__█ █^^^",
    "▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀",
];

pub const GO_RIGHT: [&str; 4] = ["    ", "█▀▀█", "█__█", "▀▀▀▀"];

/// 把一行掩码字符转成带样式的 span 序列。上游字母表：
/// `_`=底色块（此处渲染为空格；ox 用终端默认底色，不做 tint）、
/// `^`=上半块（前景字色）、`~`=上半块（半影色）、`,`=下半块（半影色）。
fn mask_line(line: &str, fg: Style, shadow: Style, bold: bool) -> Line<'static> {
    let style = if bold {
        fg.add_modifier(Modifier::BOLD)
    } else {
        fg
    };
    let spans: Vec<Span<'static>> = line
        .chars()
        .map(|ch| match ch {
            '_' => Span::styled(" ", style),
            '^' => Span::styled("▀", style),
            '~' => Span::styled("▀", shadow),
            ',' => Span::styled("▄", shadow),
            other => Span::styled(other.to_string(), style),
        })
        .collect();
    Line::from(spans)
}

pub fn logo_lines(width: u16, height: u16, pal: theme::Palette) -> Vec<Line<'static>> {
    if height < 12 {
        return Vec::new();
    }
    if width < 22 {
        return GO_RIGHT
            .iter()
            .skip(1)
            .map(|l| mask_line(l, pal.text, pal.dim, true))
            .collect();
    }
    if width < 44 {
        let mut out: Vec<Line<'static>> = LOGO_LEFT
            .iter()
            .skip(1)
            .map(|l| mask_line(l, pal.dim, pal.dim, false))
            .collect();
        out.extend(
            LOGO_RIGHT
                .iter()
                .map(|l| mask_line(l, pal.text, pal.dim, true)),
        );
        return out;
    }
    LOGO_LEFT
        .iter()
        .zip(LOGO_RIGHT.iter())
        .map(|(l, r)| {
            let mut spans = mask_line(l, pal.dim, pal.dim, false).spans;
            spans.push(Span::raw(" "));
            spans.extend(mask_line(r, pal.text, pal.dim, true).spans);
            Line::from(spans)
        })
        .collect()
}

/// 内容列：上游 composer 的 `width=100% maxWidth=75`，外层 padding 2
/// （终端宽 <44 时收为 1），水平居中。
pub fn column_rect(area: Rect) -> Rect {
    let pad: u16 = if area.width < 44 { 1 } else { 2 };
    let avail = area.width.saturating_sub(pad * 2).max(1);
    let width = avail.min(75);
    let x = area.x + area.width.saturating_sub(width) / 2;
    Rect {
        x,
        y: area.y,
        width,
        height: area.height,
    }
}

/// ratatui panics on out-of-bounds renders; clamp into the frame area and
/// drop pieces that no longer fit.
fn clamp_to(area: Rect, rect: Rect) -> Option<Rect> {
    let bottom = area.bottom().min(rect.bottom());
    if rect.y >= bottom || rect.x >= area.right() {
        return None;
    }
    Some(Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width.min(area.right() - rect.x),
        height: bottom - rect.y,
    })
}

/// 整屏渲染：logo + 空行 + Prompt 块（面板 / ╹▀ / footer）整体垂直居中；
/// 会话创建失败时在块下方多一行错误。
pub fn render(frame: &mut ratatui::Frame, area: Rect, prompt: &mut Prompt, error: Option<&str>) {
    let pal = theme::palette();
    let col = column_rect(area);
    let logo = logo_lines(area.width, area.height, *pal);
    let logo_h = logo.len() as u16;
    let prompt_h = prompt.height(area.height).min(area.height);
    let error_h = u16::from(error.is_some());
    let total = logo_h + 1 + prompt_h + error_h;
    let mut y = area.y + area.height.saturating_sub(total) / 2;

    for line in logo {
        let rect = Rect {
            x: col.x,
            y,
            width: col.width,
            height: 1,
        };
        let Some(rect) = clamp_to(area, rect) else {
            break;
        };
        frame.render_widget(Paragraph::new(line), rect);
        y += 1;
    }
    y += 1; // logo 与面板之间的空行

    let prompt_area = Rect {
        x: col.x,
        y,
        width: col.width,
        height: prompt_h,
    };
    prompt.render(frame, prompt_area, frame.area(), false);
    y += prompt_h;

    if let Some(err) = error {
        let candidate = Rect {
            x: col.x,
            y,
            width: col.width,
            height: 1,
        };
        if let Some(rect) = clamp_to(area, candidate) {
            frame.render_widget(
                Paragraph::new(Line::styled(err.to_string(), pal.warn)),
                rect,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{column_rect, logo_lines, mask_line};
    use crate::theme::Deck;
    use ratatui::layout::Rect;

    fn pal() -> crate::theme::Palette {
        crate::theme::Palette::for_deck(Deck::Truecolor)
    }

    #[test]
    fn logo_is_hidden_below_twelve_rows() {
        assert!(logo_lines(80, 11, pal()).is_empty());
    }

    #[test]
    fn narrow_width_shows_only_the_go_mark() {
        let lines = logo_lines(21, 24, pal());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].content, "█");
    }

    #[test]
    fn medium_width_stacks_left_then_right() {
        let lines = logo_lines(43, 24, pal());
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn wide_width_joins_left_and_right_per_row() {
        let lines = logo_lines(80, 24, pal());
        assert_eq!(lines.len(), 4);
        let width: usize = lines[1]
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(width, 19 + 1 + 19);
    }

    #[test]
    fn mask_alphabet_renders_half_blocks() {
        let line = mask_line("█^~,_", pal().text, pal().dim, false);
        let glyphs: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(glyphs, vec!["█", "▀", "▀", "▄", " "]);
    }

    #[test]
    fn column_is_capped_at_75_and_centered() {
        let area = Rect::new(0, 0, 120, 30);
        let col = column_rect(area);
        assert_eq!(col.width, 75);
        assert_eq!(col.x, (120 - 75) / 2);
    }

    #[test]
    fn narrow_terminal_pads_one_and_shrinks_the_column() {
        let area = Rect::new(0, 0, 40, 30);
        let col = column_rect(area);
        assert_eq!(col.width, 40 - 2);
        let area = Rect::new(0, 0, 30, 30);
        assert_eq!(column_rect(area).width, 28);
    }
}
