use ox_client::store::DataStore;
use ratatui::text::{Line, Span};

use crate::theme;

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

/// The row under the prompt footer: search/help text, or the queued-prompt
/// count while a turn runs. Read straight from the store.
pub fn status_line(store: &DataStore, mode: StatusMode<'_>) -> Line<'static> {
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
        StatusMode::Normal => {
            // Upstream's session view has no status line and no elapsed timer
            // (the running state is the prompt footer's `esc interrupt`), so
            // this row is blank unless prompts are queued behind a turn.
            if store.running_since.is_none() || store.queued.is_empty() {
                return Line::default();
            }
            Line::from(Span::styled(
                format!("{} queued", store.queued.len()),
                p.accent,
            ))
        }
    }
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
    use ox_client::store::DataStore;

    use super::{StatusMode, format_context, status_line};

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

    #[test]
    fn queued_prompts_are_the_only_thing_the_status_line_shows_while_running() {
        let mut store = DataStore::default();
        store.running_since = Some(0);
        store.queued = vec![
            ox_client::store::Queued {
                inbox_id: "inb_1".into(),
                text: std::sync::Arc::from("first"),
            },
            ox_client::store::Queued {
                inbox_id: "inb_2".into(),
                text: std::sync::Arc::from("second"),
            },
        ];
        let line = text(&status_line(&store, StatusMode::Normal));
        assert_eq!(line, "2 queued", "{line}");
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
