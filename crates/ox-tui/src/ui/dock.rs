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

const HELP: &str = "↑↓ j/k 滚动 · g/G 顶部/底部 · / 搜索 · n/N 下一个/上一个 · ⏎ 发送 · ⇧⏎ 换行 · ctrl-c 中断/退出";

/// fx-style ribbon: one plain line, dim separators, one accent for the running
/// dot. Segments are read directly from the store (single source of truth).
/// Truncation drops whole segments from the left — model first, ctx next,
/// session last — and the running state is never dropped.
pub fn status_line(
    store: &DataStore,
    now_ms: i64,
    width: u16,
    mode: StatusMode<'_>,
) -> Line<'static> {
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
            let mut segments: Vec<Vec<Span<'static>>> = Vec::new();
            if let Some(model) = &store.model {
                segments.push(vec![Span::styled(model.clone(), p.text)]);
            }
            if let Some(ctx) = store.ctx {
                segments.push(vec![
                    Span::styled("ctx ", p.dim),
                    Span::styled(format_k(ctx.input + ctx.cache_read), p.text),
                ]);
            }
            if let Some(session) = store.session.as_deref() {
                segments.push(vec![Span::styled(short_session(session), p.dim)]);
            }
            segments.push(vec![
                Span::styled(
                    if store.running_since.is_some() {
                        "●"
                    } else {
                        "○"
                    },
                    if store.running_since.is_some() {
                        p.accent
                    } else {
                        p.dim
                    },
                ),
                Span::styled(
                    format!(
                        " {}",
                        match store.running_since {
                            Some(started) => format_elapsed(started, now_ms),
                            None => "idle".to_string(),
                        }
                    ),
                    if store.running_since.is_some() {
                        p.accent
                    } else {
                        p.dim
                    },
                ),
            ]);

            let mut line = join_segments(&segments);
            while display_width(&line) > width as usize && segments.len() > 1 {
                segments.remove(0);
                line = join_segments(&segments);
            }
            line
        }
    }
}

fn join_segments(segments: &[Vec<Span<'static>>]) -> Line<'static> {
    let dim = theme::palette().dim;
    let mut spans = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" › ", dim));
        }
        spans.extend(segment.iter().cloned());
    }
    Line::from(spans)
}

fn line_width(line: &Line<'_>) -> usize {
    use unicode_width::UnicodeWidthStr;
    line.spans
        .iter()
        .map(|span| span.content.as_ref().width())
        .sum()
}

fn display_width(line: &Line<'_>) -> usize {
    line_width(line)
}

fn short_session(session: &str) -> String {
    let tail: String = session.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    tail
}

fn format_k(tokens: i64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_elapsed(started_ms: i64, now_ms: i64) -> String {
    let seconds = ((now_ms - started_ms).max(0) / 1000) as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use ox_client::store::DataStore;

    use super::{StatusMode, status_line};

    fn text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn running_state_survives_narrow_width() {
        let mut store = DataStore::default();
        store.running_since = Some(0);
        let narrow = text(&status_line(&store, 1_000, 16, StatusMode::Normal));
        assert!(narrow.contains('●'));
        assert!(narrow.contains("00:01"));
        assert!(!narrow.contains("ctx"));
    }

    #[test]
    fn wide_layout_keeps_model_ctx_session_and_running() {
        let mut store = DataStore::default();
        store.model = Some("p/m".into());
        store.session = Some("ses_ea243cfbbffeW3GW0oAChU5mLB".into());
        store.ctx = Some(ox_client::store::Usage {
            input: 12_400,
            cache_read: 0,
        });
        let wide = text(&status_line(&store, 1_000, 80, StatusMode::Normal));
        assert!(wide.contains("p/m"));
        assert!(wide.contains("ctx 12.4k"));
        assert!(wide.contains("AChU5mLB"), "short session tail: {wide}");
        assert!(wide.contains("idle"));
        assert!(wide.contains(" › "));
    }

    #[test]
    fn search_mode_shows_query_and_position() {
        let store = DataStore::default();
        let line = status_line(
            &store,
            0,
            80,
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
        let text = text(&status_line(&store, 0, 120, StatusMode::Help));
        assert!(text.contains("搜索"));
    }
}
