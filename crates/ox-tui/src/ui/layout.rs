use ratatui::layout::{Constraint, Layout, Rect};

/// timeline | composer | dock
pub fn split(area: Rect, composer_height: u16) -> (Rect, Rect, Rect) {
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(composer_height.max(1)),
        Constraint::Length(1),
    ])
    .split(area);
    (chunks[0], chunks[1], chunks[2])
}

/// timeline | queued-prompt dock | composer | dock. `queue_height` is 0 when
/// no prompt waits (`dock::queued_dock_height`).
#[must_use]
pub fn split_with_queue(
    area: Rect,
    composer_height: u16,
    queue_height: u16,
) -> (Rect, Rect, Rect, Rect) {
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(queue_height),
        Constraint::Length(composer_height.max(1)),
        Constraint::Length(1),
    ])
    .split(area);
    (chunks[0], chunks[1], chunks[2], chunks[3])
}

/// Upstream's session screen padding: two columns each side, one below 44
/// columns wide (`routes/session/index.tsx`).
pub fn padded(area: Rect, screen_width: u16) -> Rect {
    let pad: u16 = if screen_width < 44 { 1 } else { 2 };
    Rect {
        x: area.x + pad,
        width: area.width.saturating_sub(pad * 2).max(1),
        ..area
    }
}
