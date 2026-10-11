use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub fn centered_rect(w_pct: u16, h_pct: u16, r: Rect) -> Rect {
    let w_pct = w_pct.min(100);
    let h_pct = h_pct.min(100);
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - h_pct) / 2),
            Constraint::Percentage(h_pct),
            Constraint::Percentage((100 - h_pct) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - w_pct) / 2),
            Constraint::Percentage(w_pct),
            Constraint::Percentage((100 - w_pct) / 2),
        ])
        .split(vert[1])[1]
}
