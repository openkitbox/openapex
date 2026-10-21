use ox_client::store::DataStore;

/// One-line status ribbon, read directly from the store (single source of
/// truth — the view holds no copy of these fields). Truncation priority
/// (drop order): model → ctx → running (running is always kept).
pub fn chips(store: &DataStore, now_ms: i64, width: u16) -> String {
    let mut segments = Vec::new();
    if let Some(model) = &store.model {
        segments.push(model.clone());
    }
    if let Some(ctx) = store.ctx {
        segments.push(format!("ctx {}", format_k(ctx.input + ctx.cache_read)));
    }
    segments.push(match store.running_since {
        Some(started) => format!("● {}", format_elapsed(started, now_ms)),
        None => "○ idle".to_string(),
    });

    let mut line = segments.join(" │ ");
    while display_width(&line) > width as usize && segments.len() > 1 {
        segments.remove(0);
        line = segments.join(" │ ");
    }
    line
}

fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    text.width()
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

    use super::chips;

    #[test]
    fn running_chip_survives_narrow_width() {
        let mut store = DataStore::default();
        store.running_since = Some(0);
        let narrow = chips(&store, 1_000, 16);
        assert!(narrow.contains('●'));
        assert!(!narrow.contains("ctx"));
    }

    #[test]
    fn wide_layout_includes_model_and_ctx() {
        let mut store = DataStore::default();
        store.model = Some("p/m".into());
        store.ctx = Some(ox_client::store::Usage { input: 12_400, cache_read: 0 });
        let wide = chips(&store, 1_000, 60);
        assert!(wide.contains("p/m"));
        assert!(wide.contains("ctx 12.4k"));
    }
}
