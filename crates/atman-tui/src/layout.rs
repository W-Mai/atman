use ratatui::layout::{Constraint, Direction, Layout as RatatuiLayout, Rect};

pub const SIDEBAR_WIDTH: u16 = 30;
pub const SIDEBAR_MIN_TOTAL_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy)]
pub struct AppLayout {
    pub status: Rect,
    pub transcript: Rect,
    pub sidebar: Option<Rect>,
    pub input: Rect,
}

pub fn compute(area: Rect, input_height: u16, show_sidebar: bool) -> AppLayout {
    compute_ex(area, input_height, show_sidebar, 1)
}

pub fn compute_ex(
    area: Rect,
    input_height: u16,
    show_sidebar: bool,
    status_height: u16,
) -> AppLayout {
    let vertical = RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(status_height.max(1)),
            Constraint::Min(3),
            Constraint::Length(input_height.max(3)),
        ])
        .split(area);
    let status = vertical[0];
    let mid = vertical[1];
    let input = vertical[2];

    let (transcript, sidebar) = if show_sidebar && area.width >= SIDEBAR_MIN_TOTAL_WIDTH {
        let cols = RatatuiLayout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(SIDEBAR_WIDTH)])
            .split(mid);
        (cols[0], Some(cols[1]))
    } else {
        (mid, None)
    };
    AppLayout {
        status,
        transcript,
        sidebar,
        input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_area_shows_sidebar() {
        let area = Rect::new(0, 0, 120, 40);
        let l = compute(area, 3, true);
        assert!(l.sidebar.is_some());
        assert_eq!(l.sidebar.unwrap().width, SIDEBAR_WIDTH);
        assert_eq!(l.transcript.width, 120 - SIDEBAR_WIDTH);
    }

    #[test]
    fn narrow_area_hides_sidebar() {
        let area = Rect::new(0, 0, 80, 40);
        let l = compute(area, 3, true);
        assert!(l.sidebar.is_none());
        assert_eq!(l.transcript.width, 80);
    }

    #[test]
    fn show_sidebar_false_forces_hide() {
        let area = Rect::new(0, 0, 200, 40);
        let l = compute(area, 3, false);
        assert!(l.sidebar.is_none());
    }
}
