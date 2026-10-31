//! The rows around the prompt: the queued-prompt dock above it and the
//! status line below it.
//!
//! The queued-prompt dock is upstream `QueuedPromptDock`
//! (`routes/session/index.tsx:2468-2499`). Deviations:
//! - Hover uses `palette.panel` (`raised.high`) rather than computing
//!   `decrease(raised.base)`, as every block does (`block_tool::surface`).
//! - The queued-prompts dialog has no shell-mode undo (ox has no shell
//!   composer mode to restore) and no "Search queued prompts" placeholder:
//!   the Select's typed filter searches the prompt text.
//! - `<leader>q` opens the dialog while prompts are queued and exits
//!   otherwise, as upstream's two bindings on the key resolve.
//! - The dialog is also a `/queued` slash command: ox's palette and slash
//!   menu share one registry (`ui::commands`).
//! - The dock is drawn in the session's two-column side padding, like the
//!   prompt, and the dialog closes on enter at once rather than after the
//!   steer succeeds (a failure still toasts).

use ox_client::store::{DataStore, Queued};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;
use crate::ui::block_tool;

/// What the single status line is showing this frame. One row, never scrolls,
/// only ever current values (the timeline owns history).
pub enum StatusMode<'a> {
    Normal,
    Search {
        query: &'a str,
        matches: usize,
        current: Option<usize>,
    },
    Help,
}

const HELP: &str = "↑↓ j/k 滚动 · g/G 顶部/底部 · / 搜索 · n/N 下一个/上一个 · r 推理展开 · o 工具输出 · [ ] 切换工具行 · ⏎ 发送 · ⇧⏎ 换行 · ctrl-c 中断/退出";

/// The row under the prompt footer: search/help text, else blank (the
/// queued-prompt count lives in the dock above the prompt).
#[must_use]
pub fn status_line(_store: &DataStore, mode: StatusMode<'_>) -> Line<'static> {
    let p = theme::palette();
    match mode {
        StatusMode::Help => Line::from(Span::styled(HELP, p.dim)),
        StatusMode::Search {
            query,
            matches,
            current,
        } => {
            let mut spans = vec![
                Span::styled("/", p.dim),
                Span::styled(query.to_owned(), p.accent),
            ];
            if !query.is_empty() {
                let counter = match current {
                    Some(index) if matches > 0 => format!(" {}/{}", index + 1, matches),
                    _ => " 无匹配".to_string(),
                };
                spans.push(Span::styled(counter, p.dim));
            }
            Line::from(spans)
        }
        // Upstream's session view has no status line and no elapsed timer
        // (the running state is the prompt footer's `esc interrupt`; the
        // queue is `queued_dock`).
        StatusMode::Normal => Line::default(),
    }
}

/// Whether the pointer is over the queued-prompt dock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockHover {
    #[default]
    Off,
    On,
}

/// Rows the queued-prompt dock draws: padding, the line, padding.
pub const QUEUED_DOCK_ROWS: u16 = 3;

/// Rows the dock takes above the prompt: nothing for an empty queue, else
/// its rows and the bottom margin (`marginBottom={1}`).
#[must_use]
pub fn queued_dock_height(queued: &[Queued]) -> u16 {
    if queued.is_empty() {
        0
    } else {
        QUEUED_DOCK_ROWS + 1
    }
}

/// Upstream `QueuedPromptDock` (`routes/session/index.tsx:2468-2499`): a
/// left-railed band with `<N> queued · <next prompt>` on one truncated line.
/// Empty for an empty queue.
#[must_use]
pub fn queued_dock(queued: &[Queued], width: u16, hover: DockHover) -> Vec<Line<'static>> {
    let Some(first) = queued.first() else {
        return Vec::new();
    };
    let p = theme::palette();
    let band = block_tool::surface(p, hover == DockHover::On);
    // paddingRight={1} on top of the rail and the left padding.
    let room = block_tool::content_width(width).saturating_sub(1);
    let count = format!("{} queued", queued.len());
    let next = format!(" · {}", first.text.replace('\n', " "));
    let mut spans = Vec::with_capacity(2);
    let mut used = 0;
    for (piece, style) in [(count, p.text), (next, p.dim)] {
        let (clipped, width) = clip(&piece, room.saturating_sub(used));
        used += width;
        if !clipped.is_empty() {
            spans.push(Span::styled(clipped, style));
        }
    }
    let rail = Span::styled(block_tool::RAIL, p.menu_border.patch(band));
    block_tool::frame(vec![Line::from(spans)], &rail, band, width)
}

/// `text` cut to `room` display columns, ending in `…` when it overflows
/// (`truncate`, `wrapMode="none"`). Returns the text and its width.
fn clip(text: &str, room: usize) -> (String, usize) {
    if room == 0 {
        return (String::new(), 0);
    }
    let full = UnicodeWidthStr::width(text);
    if full <= room {
        return (text.to_owned(), full);
    }
    let budget = room - 1;
    let mut out = String::with_capacity(text.len().min(room * 4));
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    (out, used + 1)
}

/// Upstream `Locale.number`: `1.2M`, `12.4K`, or the plain count. The prompt
/// footer shows context usage with this.
pub fn format_tokens(tokens: i64) -> String {
    if tokens >= 999_950 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Upstream `formatContextUsage`: `12.4K (8%)`, or the bare count when the
/// model's window is unknown. `tokens / limit` rounds half up like `Math.round`.
pub fn format_context(tokens: i64, limit: Option<i64>) -> String {
    let value = format_tokens(tokens);
    match limit.filter(|limit| *limit > 0) {
        Some(limit) => format!(
            "{value} ({}%)",
            (tokens as f64 / limit as f64 * 100.0).round() as i64
        ),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ox_client::store::{DataStore, Queued};
    use unicode_width::UnicodeWidthStr;

    use super::{
        DockHover, QUEUED_DOCK_ROWS, StatusMode, format_context, queued_dock, queued_dock_height,
        status_line,
    };
    use crate::theme;

    #[test]
    fn context_usage_shows_the_percent_only_when_the_window_is_known() {
        assert_eq!(format_context(16_300, Some(200_000)), "16.3K (8%)");
        assert_eq!(format_context(1_000, Some(8_000)), "1.0K (13%)");
        assert_eq!(format_context(16_300, Some(0)), "16.3K");
        assert_eq!(format_context(16_300, None), "16.3K");
    }

    fn text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn queued(texts: &[&str]) -> Vec<Queued> {
        texts
            .iter()
            .enumerate()
            .map(|(index, text)| Queued {
                inbox_id: format!("inb_{index}"),
                text: Arc::from(*text),
            })
            .collect()
    }

    #[test]
    fn the_status_line_leaves_queued_prompts_to_the_dock() {
        let mut store = DataStore::default();
        store.running_since = Some(0);
        store.queued = queued(&["first", "second"]);
        let line = text(&status_line(&store, StatusMode::Normal));
        assert_eq!(line, "", "the queue dock owns the count: {line:?}");
    }

    #[test]
    fn the_queued_dock_shows_the_count_and_the_next_prompt_on_one_line() {
        let prompts = queued(&["first\nprompt", "second"]);
        let lines = queued_dock(&prompts, 40, DockHover::Off);
        assert_eq!(lines.len(), usize::from(QUEUED_DOCK_ROWS), "{lines:?}");
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(rows[0], format!("┃{}", " ".repeat(39)), "padding top");
        assert_eq!(rows[2], format!("┃{}", " ".repeat(39)), "padding bottom");
        assert_eq!(
            rows[1].trim_end(),
            "┃  2 queued · first prompt",
            "newlines become spaces"
        );
        for row in &rows {
            assert_eq!(UnicodeWidthStr::width(row.as_str()), 40, "{row:?}");
        }
        let palette = theme::palette();
        let count = lines[1]
            .spans
            .iter()
            .find(|span| span.content.contains("queued"))
            .expect("count span");
        assert_eq!(count.style.fg, palette.text.fg, "count in text.base");
        let next = lines[1]
            .spans
            .iter()
            .find(|span| span.content.contains("first"))
            .expect("next span");
        assert_eq!(next.style.fg, palette.dim.fg, "next prompt in text.muted");
        assert_eq!(count.style.bg, palette.raised_base.bg, "raised band");
    }

    #[test]
    fn the_queued_dock_truncates_and_never_wraps() {
        let prompts = queued(&["一二三四五六七八九十 and a long tail"]);
        // rail 1 + pad 2 + pad right 1: 16 columns of text.
        let lines = queued_dock(&prompts, 20, DockHover::Off);
        assert_eq!(lines.len(), 3, "long text truncates instead of wrapping");
        let row = text(&lines[1]);
        assert_eq!(UnicodeWidthStr::width(row.as_str()), 20, "{row:?}");
        assert!(row.starts_with("┃  1 queued · 一"), "{row:?}");
        assert!(!row.contains("tail"), "{row:?}");
    }

    #[test]
    fn hovering_the_queued_dock_changes_its_band() {
        let prompts = queued(&["first"]);
        let calm = queued_dock(&prompts, 30, DockHover::Off);
        let lit = queued_dock(&prompts, 30, DockHover::On);
        let band = |lines: &[ratatui::text::Line<'_>]| lines[1].spans.last().map(|s| s.style.bg);
        assert_eq!(
            band(&calm),
            Some(theme::palette().raised_base.bg),
            "idle: raised band"
        );
        assert_eq!(
            band(&lit),
            Some(theme::palette().panel.bg),
            "hovered: panel band"
        );
    }

    #[test]
    fn no_queue_means_no_dock() {
        let lines = queued_dock(&[], 40, DockHover::Off);
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(queued_dock_height(&[]), 0, "no dock takes no rows");
        assert_eq!(
            queued_dock_height(&queued(&["a"])),
            QUEUED_DOCK_ROWS + 1,
            "three rows and the bottom margin"
        );
    }

    #[test]
    fn the_status_line_is_blank_when_idle_and_when_running() {
        // model/ctx/session were dropped: they duplicate the footer and crowd
        // the dock. The line is running-state only.
        let mut store = DataStore::default();
        store.model = Some("p/m".into());
        store.session = Some("ses_ea243cfbbffeW3GW0oAChU5mLB".into());
        let idle = text(&status_line(&store, StatusMode::Normal));
        assert_eq!(idle, "", "idle is a blank row: {idle:?}");

        // Upstream draws no elapsed timer: the running state lives in the
        // prompt footer, so a running turn leaves this row blank too.
        store.running_since = Some(0);
        let running = text(&status_line(&store, StatusMode::Normal));
        assert_eq!(running, "", "{running}");
    }

    #[test]
    fn search_mode_shows_query_and_position() {
        let store = DataStore::default();
        let line = status_line(
            &store,
            StatusMode::Search {
                query: "cache",
                matches: 12,
                current: Some(2),
            },
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "/cache 3/12");
    }

    #[test]
    fn help_mode_replaces_the_line() {
        let store = DataStore::default();
        let text = text(&status_line(&store, StatusMode::Help));
        assert!(text.contains("搜索"));
    }
}
