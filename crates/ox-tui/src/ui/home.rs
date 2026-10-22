//! Home 落地页：居中 logo + composer 面板（底色、左竖线、metadata 行）+ footer。
//! 视觉规格见 .opencode/opencode-tui-components/16-home-splash.md 与 07-composer.md。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme;
use crate::ui::composer::Composer;

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

/// footer 右段的固定快捷键提示（上游 home footer 同款文案）。
pub const FOOTER_HINTS: &str = "shift+tab agents  ctrl+p commands";

/// Home 的静态装饰数据（bootstrap::chrome 组装；取不到就留空降级）。
#[derive(Debug, Clone, Default)]
pub struct Chrome {
    /// 默认 agent 的显示名（如 `Build`）。
    pub agent: Option<String>,
    /// model 行标签（如 `k3-256k Cardinal`）。
    pub model: Option<String>,
    /// 目录的 `~` 缩写显示。
    pub directory: String,
    /// git 分支名。
    pub branch: Option<String>,
}

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

/// composer 面板的 metadata 行：`agent · model`（上游 metadata 行的 Home 形态，
/// 无 auto/variant 段）。agent 取 `pal.agent`（紫），model 取面板底上的正文字色，
/// `·` 分隔符 muted。窄屏（<44）按上游降级链先丢 agent。两者皆无 → 行消失。
pub fn metadata_line(
    agent: Option<&str>,
    model: Option<&str>,
    width: u16,
    pal: theme::Palette,
) -> Option<Line<'static>> {
    let agent = if width < 44 { None } else { agent };
    let model = model.filter(|m| !m.is_empty());
    if agent.is_none() && model.is_none() {
        return None;
    }
    let on_panel = |role: Style| pal.panel.patch(role);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(agent) = agent {
        spans.push(Span::styled(agent.to_owned(), pal.agent));
    }
    if let Some(model) = model {
        if agent.is_some() {
            spans.push(Span::styled(" · ", on_panel(pal.dim)));
        }
        spans.push(Span::styled(model.to_owned(), on_panel(pal.text)));
    }
    Some(Line::from(spans))
}

/// footer 左段：`~/path·branch`（分支缺省时只显路径）。
pub fn footer_left(chrome: &Chrome) -> String {
    match &chrome.branch {
        Some(branch) if !branch.is_empty() => format!("{}·{branch}", chrome.directory),
        _ => chrome.directory.clone(),
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

/// 整屏渲染：logo 块 + composer 面板（上 pad、左竖线、底 `╹`、metadata 行）
/// + footer 行，整体垂直居中于内容列。
pub fn render(
    frame: &mut ratatui::Frame,
    area: Rect,
    composer: &mut Composer,
    chrome: &Chrome,
    error: Option<&str>,
) {
    let pal = theme::palette();
    let col = column_rect(area);
    let logo = logo_lines(area.width, area.height, *pal);
    let logo_h = logo.len() as u16;
    let composer_h = composer.height().max(1);
    let meta = metadata_line(
        chrome.agent.as_deref(),
        chrome.model.as_deref(),
        col.width,
        *pal,
    );
    let meta_h = u16::from(meta.is_some());
    let panel_h = 1 + composer_h + 1 + meta_h; // 上 pad + 文本行 + 下 pad + metadata
    let error_h = u16::from(error.is_some());
    // logo + 空行 + 面板 + ╹ + error + footer
    let total = logo_h + 1 + panel_h + 1 + error_h + 1;
    let top = area.y + area.height.saturating_sub(total) / 2;
    let mut y = top;

    // logo 行
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

    // composer 面板：整面板填底色 + 左列竖线，文本与 metadata 渲染在内部。
    let panel = Rect {
        x: col.x,
        y,
        width: col.width,
        height: panel_h,
    };
    if let Some(panel) = clamp_to(area, panel) {
        let buf = frame.buffer_mut();
        for row in panel.y..panel.bottom() {
            for x in panel.x..panel.right() {
                buf[(x, row)].set_style(pal.panel);
            }
            buf[(panel.x, row)].set_style(pal.panel_border);
            buf[(panel.x, row)].set_char('▌');
        }
        let inner_x = panel.x + 1;
        let inner_w = panel.width.saturating_sub(1);
        let text_area = Rect {
            x: inner_x,
            y: panel.y + 1,
            width: inner_w,
            height: composer_h.min(panel.height.saturating_sub(1)),
        };
        composer.render(frame, text_area);
        if let Some(meta) = meta {
            let meta_y = panel.y + 1 + composer_h + 1;
            if meta_y < panel.bottom() {
                let meta_rect = Rect {
                    x: inner_x,
                    y: meta_y,
                    width: inner_w,
                    height: 1,
                };
                frame.render_widget(Paragraph::new(meta), meta_rect);
            }
        }
        y = panel.bottom();
    } else {
        y += panel_h;
    }

    // 面板底左角的 `╹`（上游 customBorderChars 的 bottomLeft 收尾）。
    if y < area.bottom() {
        let cell = &mut frame.buffer_mut()[(col.x, y)];
        cell.set_char('╹');
        if let Some(fg) = pal.panel_border.fg {
            cell.set_fg(fg);
        }
    }
    y += 1;

    if let Some(err) = error {
        let candidate = Rect {
            x: col.x,
            y,
            width: col.width,
            height: 1,
        };
        if let Some(rect) = clamp_to(area, candidate) {
            let line = Line::styled(err.to_string(), pal.warn);
            frame.render_widget(Paragraph::new(line), rect);
        }
        y += 1;
    }

    // footer：左 `~/path·branch`，右快捷键提示（列内两端对齐）。
    let left = footer_left(chrome);
    if !left.is_empty() {
        let left_rect = Rect {
            x: col.x,
            y,
            width: col.width,
            height: 1,
        };
        if let Some(rect) = clamp_to(area, left_rect) {
            frame.render_widget(Paragraph::new(Line::styled(left, pal.dim)), rect);
        }
    }
    let hints_w = FOOTER_HINTS.chars().count() as u16;
    let right_rect = Rect {
        x: col.right().saturating_sub(hints_w),
        y,
        width: hints_w.min(col.width),
        height: 1,
    };
    if let Some(rect) = clamp_to(area, right_rect) {
        frame.render_widget(Paragraph::new(Line::styled(FOOTER_HINTS, pal.dim)), rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Deck;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn pal() -> theme::Palette {
        theme::Palette::for_deck(Deck::Truecolor)
    }

    fn chrome() -> Chrome {
        Chrome {
            agent: Some("Build".to_owned()),
            model: Some("k3-256k Cardinal".to_owned()),
            directory: "~/Developer/openkitbox/ox".to_owned(),
            branch: Some("main".to_owned()),
        }
    }

    /// 取 buffer 某行 y 在列区间的文本。
    fn row_text(backend: &TestBackend, y: u16, x_from: u16, x_to: u16) -> String {
        let buf = backend.buffer();
        (x_from..x_to)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
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
        let area = Rect::new(0, 0, 120, 30);
        let col = column_rect(area);
        assert_eq!(col.width, 75);
        assert_eq!(col.x, (120 - 75) / 2);
    }

    #[test]
    fn narrow_terminal_pads_one_and_shrinks_the_column() {
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

    #[test]
    fn metadata_line_joins_agent_and_model_with_muted_dot() {
        let line = metadata_line(Some("Build"), Some("k3-256k Cardinal"), 75, pal())
            .expect("both segments present");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Build · k3-256k Cardinal");
        assert_eq!(line.spans.len(), 3);
    }

    #[test]
    fn metadata_line_hides_agent_on_narrow_terminals() {
        let line = metadata_line(Some("Build"), Some("k3-256k Cardinal"), 43, pal())
            .expect("model survives");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "k3-256k Cardinal");
    }

    #[test]
    fn metadata_line_disappears_when_nothing_is_known() {
        assert!(metadata_line(None, None, 75, pal()).is_none());
        assert!(metadata_line(None, Some(""), 75, pal()).is_none());
        // 只有 model 时行仍在（上游降级链允许只显 model）
        assert!(metadata_line(None, Some("k3-256k"), 75, pal()).is_some());
    }

    #[test]
    fn footer_left_joins_branch_with_a_dot() {
        assert_eq!(footer_left(&chrome()), "~/Developer/openkitbox/ox·main");
        let mut detached = chrome();
        detached.branch = None;
        assert_eq!(footer_left(&detached), "~/Developer/openkitbox/ox");
    }

    #[test]
    fn home_render_paints_the_composer_panel() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let chrome = chrome();
        terminal
            .draw(|frame| {
                let mut composer = Composer::new_with_placeholder(&placeholder_line(0));
                render(frame, frame.area(), &mut composer, &chrome, None);
            })
            .unwrap();
        let backend = terminal.backend();
        let col = column_rect(Rect::new(0, 0, 100, 30));
        // logo 4 + 空行 1 + 面板(1+1+1+1) + ╹ 1 + footer 1 = 11；top = (30-11)/2 = 9
        let panel_top = 9 + 4 + 1;
        // 左竖线贯穿上 pad 到 metadata 行
        assert_eq!(backend.buffer()[(col.x, panel_top)].symbol(), "▌");
        assert_eq!(backend.buffer()[(col.x, panel_top + 3)].symbol(), "▌");
        // 面板内部有底色（含 composer 文本行——textarea 不得抹掉底色）
        let panel_bg = pal().panel.bg.expect("truecolor panel bg");
        assert_eq!(backend.buffer()[(col.x + 5, panel_top)].bg, panel_bg);
        assert_eq!(backend.buffer()[(col.x + 5, panel_top + 1)].bg, panel_bg);
        // metadata 行在文本行下方隔一行
        let meta_row = row_text(backend, panel_top + 3, col.x, col.x + 30);
        assert!(
            meta_row.contains("Build · k3-256k Cardinal"),
            "{meta_row:?}"
        );
        // 面板底左角 ╹
        assert_eq!(backend.buffer()[(col.x, panel_top + 4)].symbol(), "╹");
        // footer：左路径·分支，右快捷键提示
        let footer = row_text(backend, panel_top + 5, col.x, col.right());
        assert!(
            footer.contains("~/Developer/openkitbox/ox·main"),
            "{footer:?}"
        );
        assert!(
            footer.contains("shift+tab agents  ctrl+p commands"),
            "{footer:?}"
        );
    }

    #[test]
    fn home_render_tolerates_a_one_cell_terminal() {
        let backend = TestBackend::new(1, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let chrome = chrome();
        terminal
            .draw(|frame| {
                let mut composer = Composer::new();
                render(frame, frame.area(), &mut composer, &chrome, None);
            })
            .unwrap();
    }
}
