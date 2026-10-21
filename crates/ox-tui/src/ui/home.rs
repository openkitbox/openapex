//! Home 落地页：居中 logo + 居中输入框（首次提交前无 session）。
//! 视觉规格见 .opencode/opencode-tui-components/16-home-splash.md。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

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

pub const PLACEHOLDERS: [&str; 3] = [
    "Fix a TODO in the codebase",
    "What is the tech stack of this project?",
    "Fix broken tests",
];

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

/// 上游 placeholder 格式：`Ask anything… "<随机一句>"`。
pub fn placeholder_line(idx: usize) -> String {
    format!(
        "Ask anything… \"{}\"",
        PLACEHOLDERS[idx % PLACEHOLDERS.len()]
    )
}

/// 启动时随机选一句（上游 randomIndex，无随机源依赖：用时钟纳秒）。
pub fn pick_placeholder() -> usize {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    nanos % PLACEHOLDERS.len()
}

/// 内容列：上游 composer 的 `width=100% maxWidth=75`，外层 padding 2
/// （终端宽 <44 时收为 1），水平居中。
pub fn column_rect(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let pad: u16 = if area.width < 44 { 1 } else { 2 };
    let avail = area.width.saturating_sub(pad * 2).max(1);
    let width = avail.min(75);
    let x = area.x + area.width.saturating_sub(width) / 2;
    ratatui::layout::Rect {
        x,
        y: area.y,
        width,
        height: area.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Deck;

    fn pal() -> theme::Palette {
        theme::Palette::for_deck(Deck::Truecolor)
    }

    #[test]
    fn logo_is_hidden_below_twelve_rows() {
        assert!(logo_lines(80, 11, pal()).is_empty());
    }

    #[test]
    fn narrow_width_shows_only_the_go_mark() {
        // <22: go.right 去掉首行（空白行），共 3 行
        let lines = logo_lines(21, 24, pal());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].content, "█");
    }

    #[test]
    fn medium_width_stacks_left_then_right() {
        // <44: logo.left.slice(1) 3 行 + logo.right 4 行 = 7 行
        let lines = logo_lines(43, 24, pal());
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn wide_width_joins_left_and_right_per_row() {
        // >=44: 4 行，每行 = 左 19 列 + 1 空格 + 右 19 列
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
        use ratatui::layout::Rect;
        let area = Rect::new(0, 0, 120, 30);
        let col = column_rect(area);
        assert_eq!(col.width, 75);
        assert_eq!(col.x, (120 - 75) / 2);
    }

    #[test]
    fn narrow_terminal_pads_one_and_shrinks_the_column() {
        use ratatui::layout::Rect;
        let area = Rect::new(0, 0, 40, 30);
        let col = column_rect(area);
        assert_eq!(col.width, 40 - 2); // padding 1 两侧
        let area = Rect::new(0, 0, 30, 30);
        assert_eq!(column_rect(area).width, 28);
    }

    #[test]
    fn placeholder_line_wraps_a_phrase_in_the_upstream_format() {
        assert_eq!(
            placeholder_line(0),
            "Ask anything… \"Fix a TODO in the codebase\""
        );
        assert_eq!(placeholder_line(3), placeholder_line(0)); // 取模
    }

    #[test]
    fn placeholder_pick_stays_in_range() {
        assert!(pick_placeholder() < PLACEHOLDERS.len());
    }
}
