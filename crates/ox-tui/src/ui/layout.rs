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
