use ratatui::layout::{Constraint, Direction, Layout as RatatuiLayout, Rect};

pub struct AppLayout {
    pub status: Rect,
    pub output: Rect,
    pub input: Rect,
}

pub fn compute(area: Rect, input_height: u16) -> AppLayout {
    let chunks = RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(input_height.max(1)),
        ])
        .split(area);
    AppLayout {
        status: chunks[0],
        output: chunks[1],
        input: chunks[2],
    }
}
