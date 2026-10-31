//! Home 落地页：居中 logo + 共享的 [`Prompt`] 输入面。
//!
//! 视觉规格见 .ox/plans/opencode-tui-components/16-home-splash.md；输入面本身
//! 由 `ui::prompt` 负责（面板、metadata 行、`╹`/`▀` 收尾、footer、slash 菜单）。
//! 这里只剩 Home 独有的东西：logo 掩码渲染、75 列内容列、垂直排布与底部 footer 行。
//! 上游参照：`routes/home.tsx`、`component/logo.tsx`、`feature-plugins/home/footer.tsx`。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::{self, terminal::Rgb};
use crate::ui::prompt::{Activity, McpSummary, Prompt};

/// Upstream `logo.left` / `logo.right` (`logo.ts:1-4`), verbatim. The mask
/// alphabet (`component/logo.tsx:12-49`): `_` a blank on the shadow colour, `^`
/// an upper half block on the shadow colour, `~` / `,` an upper / lower half
/// block drawn *in* the shadow colour.
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

/// Upstream `go.right` (`logo.ts:8`); below 22 columns only this half is shown.
pub const GO_RIGHT: [&str; 4] = ["    ", "█▀▀█", "█__█", "▀▀▀▀"];

/// Upstream `tint(background.base, fg, 0.25)` (`component/logo.tsx:11`): the
/// colour behind the `_` / `^` cells and the ink of the `~` / `,` cells.
/// `None` when the terminal background or the ink is not known as RGB.
fn shadow_of(ink: Style, backdrop: Option<Rgb>) -> Option<Color> {
    let (Some(Color::Rgb(r, g, b)), Some(base)) = (ink.fg, backdrop) else {
        return None;
    };
    let t = Rgb::tint(base, Rgb::new(r, g, b), 0.25);
    Some(Color::Rgb(t.r, t.g, t.b))
}

/// One mask row as styled cells. Without a known backdrop the shadow falls back
/// to the dim style and the `_` / `^` cells keep the terminal background.
fn mask_line(line: &str, ink: Style, pal: &theme::Palette, bold: bool) -> Line<'static> {
    let ink = if bold {
        ink.add_modifier(Modifier::BOLD)
    } else {
        ink
    };
    let shadow = shadow_of(ink, pal.backdrop);
    let on_shadow = shadow.map_or(ink, |color| ink.bg(color));
    let shadow_ink = shadow.map_or(pal.dim, |color| Style::default().fg(color));
    let spans: Vec<Span<'static>> = line
        .chars()
        .map(|ch| match ch {
            '_' => Span::styled(" ", on_shadow),
            '^' => Span::styled("▀", on_shadow),
            '~' => Span::styled("▀", shadow_ink),
            ',' => Span::styled("▄", shadow_ink),
            other => Span::styled(other.to_string(), ink),
        })
        .collect();
    Line::from(spans)
}

/// Upstream `Logo` (`component/logo.tsx:51-79`): hidden below 12 rows, the go
/// mark below 22 columns, left/right stacked below 44, side by side otherwise.
pub fn logo_lines(width: u16, height: u16, pal: theme::Palette) -> Vec<Line<'static>> {
    if height < 12 {
        return Vec::new();
    }
    if width < 22 {
        return GO_RIGHT
            .iter()
            .skip(1)
            .map(|l| mask_line(l, pal.text, &pal, true))
            .collect();
    }
    if width < 44 {
        let mut out: Vec<Line<'static>> = LOGO_LEFT
            .iter()
            .skip(1)
            .map(|l| mask_line(l, pal.dim, &pal, false))
            .collect();
        out.extend(
            LOGO_RIGHT
                .iter()
                .map(|l| mask_line(l, pal.text, &pal, true)),
        );
        return out;
    }
    LOGO_LEFT
        .iter()
        .zip(LOGO_RIGHT.iter())
        .map(|(l, r)| {
            let mut spans = mask_line(l, pal.dim, &pal, false).spans;
            spans.push(Span::raw(" "));
            spans.extend(mask_line(r, pal.text, &pal, true).spans);
            Line::from(spans)
        })
        .collect()
}

/// Yoga's `alignItems=center`: the child's left edge is rounded half up.
fn centre(outer_x: u16, outer_w: u16, width: u16) -> u16 {
    outer_x + outer_w.saturating_sub(width).div_ceil(2)
}

/// Upstream's outer box padding (`routes/home.tsx:93-95`).
fn side_pad(width: u16) -> u16 {
    if width < 44 { 1 } else { 2 }
}

/// 内容列：上游 composer 的 `width=100% maxWidth=75`，外层 padding 2
/// （终端宽 <44 时收为 1），在内缩后的屏宽里居中。
pub fn column_rect(area: Rect) -> Rect {
    let pad = side_pad(area.width);
    let inner = area.width.saturating_sub(pad * 2).max(1);
    let width = inner.min(75);
    Rect {
        x: centre(area.x + pad, inner, width),
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

/// Rows the footer box takes: shown from 14 rows and 44 columns, with a row of
/// padding above and below from 16 rows (`feature-plugins/home/footer.tsx:96-103`).
fn footer_height(width: u16, height: u16) -> u16 {
    if height < 14 || width < 44 {
        0
    } else if height < 16 {
        1
    } else {
        3
    }
}

/// The footer's left (MCP) and right (version) segments. Upstream shows the
/// `/mcps` hint and the version from 64 columns
/// (`feature-plugins/home/footer.tsx:6-12`).
fn footer_spans(
    mcp: Option<McpSummary>,
    width: u16,
    pal: &theme::Palette,
) -> (Vec<Span<'static>>, Option<Span<'static>>) {
    let wide = width >= 64;
    let mut left = Vec::new();
    if let Some(mcp) = mcp {
        let (dot, label) = if mcp.failed > 0 {
            (pal.warn, format!("{} MCP failed", mcp.failed))
        } else if mcp.connected > 0 {
            (pal.success, format!("{} MCP", mcp.connected))
        } else {
            (pal.dim, format!("{} MCP", mcp.connected))
        };
        left.push(Span::styled("⊙ ", dot));
        left.push(Span::styled(label, pal.text));
        if wide {
            left.push(Span::raw(" "));
            left.push(Span::styled("/mcps", pal.dim));
        }
    }
    let version = wide.then(|| Span::styled(env!("CARGO_PKG_VERSION"), pal.dim));
    (left, version)
}

/// Rows from the top of the content box: where the logo and the prompt box
/// start. Mirrors the flex column of `routes/home.tsx:96-123`: a growing
/// spacer, a 3-row spacer that shrinks first, the logo, a 1-row gap, the
/// prompt box, a growing spacer; the two growing spacers split what is left
/// and the first takes the odd row (Yoga rounds half up).
#[derive(Debug, PartialEq, Eq)]
struct Stack {
    logo_y: u16,
    prompt_y: u16,
}

fn stack(content_h: u16, logo_h: u16, prompt_box_h: u16) -> Stack {
    let fixed = logo_h + 1 + prompt_box_h;
    let free = content_h.saturating_sub(fixed);
    let (top, spacer) = if free >= 3 {
        ((free - 3).div_ceil(2), 3)
    } else {
        (0, free)
    };
    let logo_y = top + spacer;
    Stack {
        logo_y,
        prompt_y: logo_y + logo_h + 1,
    }
}

/// 整屏渲染（上游 `routes/home.tsx`）：内容区里 logo + 空行 + Prompt 块整体垂直
/// 居中，底部是独立的 footer 行（MCP 摘要 + 版本号）。会话创建失败走 toast
/// （`ui::toast`），不再占用 Home 版面。
pub fn render(frame: &mut ratatui::Frame, area: Rect, prompt: &mut Prompt) {
    let pal = theme::palette();
    let footer_h = footer_height(area.width, area.height);
    let content = Rect {
        height: area.height - footer_h,
        ..area
    };
    let col = column_rect(content);
    let logo = logo_lines(area.width, area.height, *pal);
    let logo_h = logo.len() as u16;
    // paddingTop(1) + the prompt block itself.
    let prompt_h = prompt.height(area.height).min(content.height);
    let layout = stack(content.height, logo_h, 1 + prompt_h);

    for (y, line) in (content.y + layout.logo_y..).zip(logo) {
        // 上游外层 `alignItems="center"`：logo 在内缩后的屏宽里居中，而不是
        // 与 75 列输入框共用左缘。
        let pad = side_pad(area.width);
        let inner = area.width.saturating_sub(pad * 2);
        let width = (line.width() as u16).min(inner);
        let rect = Rect {
            x: centre(area.x + pad, inner, width),
            y,
            width,
            height: 1,
        };
        let Some(rect) = clamp_to(area, rect) else {
            break;
        };
        frame.render_widget(Paragraph::new(line), rect);
    }

    let prompt_area = Rect {
        x: col.x,
        y: content.y + layout.prompt_y + 1,
        width: col.width,
        height: prompt_h,
    };
    if let Some(prompt_area) = clamp_to(area, prompt_area) {
        let prompt_area = Rect {
            height: prompt_h.min(prompt_area.height),
            ..prompt_area
        };
        prompt.render(frame, prompt_area, frame.area(), Activity::Idle);
    }

    if footer_h > 0 {
        render_footer(frame, area, footer_h, prompt.chrome().mcp, pal);
    }
}

/// The footer row: MCP summary left at column 2, version right ending 2 columns
/// before the edge (`feature-plugins/home/footer.tsx:99-117`).
fn render_footer(
    frame: &mut ratatui::Frame,
    area: Rect,
    footer_h: u16,
    mcp: Option<McpSummary>,
    pal: &theme::Palette,
) {
    let y = area.bottom() - footer_h + u16::from(footer_h == 3);
    let (left, version) = footer_spans(mcp, area.width, pal);
    let inner = Rect {
        x: area.x + 2,
        y,
        width: area.width.saturating_sub(4),
        height: 1,
    };
    let left_w = left.iter().map(|span| span.width()).sum::<usize>() as u16;
    if left_w > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(left)),
            Rect {
                width: left_w.min(inner.width),
                ..inner
            },
        );
    }
    if let Some(version) = version {
        let w = version.width() as u16;
        // Upstream separates footer items by a two-column gap.
        if inner.width >= left_w + w + if left_w > 0 { 2 } else { 0 } {
            frame.render_widget(
                Paragraph::new(Line::from(version)),
                Rect {
                    x: inner.right() - w,
                    width: w,
                    ..inner
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Stack, column_rect, footer_height, footer_spans, logo_lines, mask_line, stack};
    use crate::theme::Deck;
    use crate::theme::terminal::Rgb;
    use crate::ui::prompt::McpSummary;
    use ratatui::layout::Rect;
    use ratatui::text::Span;

    fn pal() -> crate::theme::Palette {
        crate::theme::Palette::for_deck(Deck::Truecolor)
    }

    #[test]
    fn logo_masks_are_rectangular_and_use_the_documented_alphabet() {
        // Hand-edited art: a stray character shifts every following glyph and a
        // wrong width breaks the medium-tier stacking, so pin both.
        const ALPHABET: &str = "█▀▄ _^~,";
        for line in super::LOGO_LEFT.iter().chain(super::LOGO_RIGHT.iter()) {
            assert_eq!(line.chars().count(), 19, "logo rows are 19 cells: {line:?}");
            for ch in line.chars() {
                assert!(
                    ALPHABET.contains(ch),
                    "unknown mask char {ch:?} in row {line:?}"
                );
            }
        }
        for line in super::GO_RIGHT {
            assert_eq!(
                line.chars().count(),
                4,
                "go mark rows are 4 cells: {line:?}"
            );
        }
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
        let pal = pal();
        let line = mask_line("█^~,_", pal.text, &pal, false);
        let glyphs: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(glyphs, vec!["█", "▀", "▀", "▄", " "]);
    }

    #[test]
    fn shadow_cells_tint_the_backdrop_a_quarter_of_the_way_to_the_ink() {
        use ratatui::style::Color;
        let mut pal = pal();
        pal.backdrop = Some(Rgb::new(0, 0, 0));
        pal.text = pal.text.fg(Color::Rgb(200, 100, 40));
        let line = mask_line("█_^~,", pal.text, &pal, false);
        let shadow = Color::Rgb(50, 25, 10);
        // `_` / `^`: ink on the shadow; `~` / `,`: the shadow *as* the ink.
        assert_eq!(line.spans[1].style.bg, Some(shadow));
        assert_eq!(line.spans[2].style.bg, Some(shadow));
        assert_eq!(line.spans[2].style.fg, pal.text.fg);
        assert_eq!(line.spans[3].style.fg, Some(shadow));
        assert_eq!(line.spans[3].style.bg, None);
        assert_eq!(line.spans[4].style.fg, Some(shadow));
    }

    #[test]
    fn without_a_known_backdrop_the_shadow_degrades_to_plain_cells() {
        let mut pal = pal();
        pal.backdrop = None;
        let line = mask_line("_^~", pal.text, &pal, false);
        assert!(line.spans.iter().all(|span| span.style.bg.is_none()));
        assert_eq!(line.spans[2].style.fg, pal.dim.fg);
    }

    #[test]
    fn stack_centres_logo_and_prompt_like_the_upstream_flex_column() {
        // 30x100 gold: 27 content rows, logo 4, prompt box 7 -> logo at row 9.
        assert_eq!(
            stack(27, 4, 7),
            Stack {
                logo_y: 9,
                prompt_y: 14
            }
        );
        // 24x80 gold: 21 content rows -> logo at row 6.
        assert_eq!(
            stack(21, 4, 7),
            Stack {
                logo_y: 6,
                prompt_y: 11
            }
        );
        // Tight: the 3-row spacer shrinks before anything else moves.
        assert_eq!(stack(13, 4, 7).logo_y, 1);
        assert_eq!(stack(12, 4, 7).logo_y, 0);
    }

    #[test]
    fn footer_shows_from_14_rows_and_44_columns_with_padding_from_16_rows() {
        assert_eq!(footer_height(100, 13), 0);
        assert_eq!(footer_height(43, 30), 0);
        assert_eq!(footer_height(100, 14), 1);
        assert_eq!(footer_height(100, 15), 1);
        assert_eq!(footer_height(100, 16), 3);
    }

    #[test]
    fn footer_segments_follow_the_mcp_state_and_width() {
        let pal = pal();
        let text =
            |spans: &[Span<'_>]| spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        let ok = Some(McpSummary {
            connected: 4,
            failed: 0,
        });
        let (left, version) = footer_spans(ok, 100, &pal);
        assert_eq!(text(&left), "⊙ 4 MCP /mcps");
        assert_eq!(left[0].style, pal.success);
        assert_eq!(version.unwrap().content, env!("CARGO_PKG_VERSION"));

        let (left, version) = footer_spans(ok, 63, &pal);
        assert_eq!(text(&left), "⊙ 4 MCP");
        assert!(version.is_none());

        let failed = Some(McpSummary {
            connected: 3,
            failed: 1,
        });
        let (left, _) = footer_spans(failed, 100, &pal);
        assert_eq!(text(&left), "⊙ 1 MCP failed /mcps");
        assert_eq!(left[0].style, pal.warn);

        let none = Some(McpSummary::default());
        assert_eq!(footer_spans(none, 100, &pal).0[0].style, pal.dim);
        assert!(footer_spans(None, 100, &pal).0.is_empty());
    }

    #[test]
    fn column_is_capped_at_75_and_centered() {
        let area = Rect::new(0, 0, 120, 30);
        let col = column_rect(area);
        assert_eq!(col.width, 75);
        // Yoga rounds the half column up: (116 - 75) / 2 = 20.5 -> 21, plus the padding.
        assert_eq!(col.x, 2 + 21);
    }

    #[test]
    fn logo_is_horizontally_centered_on_the_screen() {
        use crate::ui::prompt::{Chrome, Footer, Prompt, placeholder_line};

        // Upstream centers the logo inside the padded screen
        // (routes/home.tsx `alignItems="center"`); the 75-column prompt is a
        // separate centered child, so the logo must not share its left edge.
        let mut prompt = Prompt::new(&placeholder_line(0), Chrome::default(), Footer::Home);
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                super::render(frame, frame.area(), &mut prompt);
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // The logo is the topmost band of non-blank rows; measure its bounding
        // box (the mask rows carry the full 39-column run).
        let row_has_glyph = |y: u16| (0..100).any(|x| buf[(x, y)].symbol() != " ");
        let top = (0..30).find(|&y| row_has_glyph(y)).expect("logo renders");
        let mut bottom = top;
        while bottom < 30 && row_has_glyph(bottom) {
            bottom += 1;
        }
        let mut min_x = u16::MAX;
        let mut max_x = 0;
        for y in top..bottom {
            for x in 0..100 {
                if buf[(x, y)].symbol() != " " {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                }
            }
        }
        let left_gap = min_x;
        let right_gap = 99 - max_x;
        assert!(
            // Yoga rounds the odd column up, so the left gap is the larger one.
            left_gap == right_gap || left_gap == right_gap + 1,
            "logo box spans {min_x}..{max_x}: left gap {left_gap}, right gap {right_gap}"
        );
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
