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
