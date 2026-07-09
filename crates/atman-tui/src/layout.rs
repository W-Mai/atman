use ratatui::layout::{Constraint, Direction, Layout as RatatuiLayout, Rect};

pub const SIDEBAR_WIDTH: u16 = 42;
pub const SIDEBAR_MIN_TOTAL_WIDTH: u16 = 110;

// AppLayout no longer carves out fixed slots for input / approvals: those
// draw as floating overlays on top of the transcript so the user still
// sees message content "underneath" the input as they scroll.
#[derive(Debug, Clone, Copy)]
pub struct AppLayout {
    pub status: Rect,
    pub transcript: Rect,
    pub sidebar: Option<Rect>,
}

pub fn compute(area: Rect, show_sidebar: bool) -> AppLayout {
    compute_ex(area, show_sidebar, 1)
}

pub fn compute_ex(area: Rect, show_sidebar: bool, status_height: u16) -> AppLayout {
    let vertical = RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(status_height.max(1)), Constraint::Min(1)])
        .split(area);
    let status = vertical[0];
    let mid = vertical[1];

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
    }
}

pub fn compute_input_rect(transcript: Rect, buf_lines: u16) -> Rect {
    let outer_width = (transcript.width * 3 / 4)
        .clamp(50, 120)
        .min(transcript.width);
    let content_h = buf_lines.clamp(1, 6);
    let outer_h = content_h.saturating_add(2);
    // One row of transcript peeks under the panel so users feel the
    // messages continue behind the floating input.
    let bottom_margin: u16 = 1;
    let outer_h = outer_h.min(transcript.height.saturating_sub(bottom_margin).max(3));
    let x = transcript.x + (transcript.width.saturating_sub(outer_width)) / 2;
    let y = transcript
        .y
        .saturating_add(transcript.height)
        .saturating_sub(outer_h)
        .saturating_sub(bottom_margin);
    Rect {
        x,
        y,
        width: outer_width,
        height: outer_h,
    }
}

pub fn compute_approvals_rect(transcript: Rect, input_rect: Rect, rows: u16) -> Option<Rect> {
    if rows == 0 {
        return None;
    }
    let width = input_rect.width;
    let x = input_rect.x;
    let above_input = input_rect.y.saturating_sub(rows);
    if above_input <= transcript.y {
        return None;
    }
    Some(Rect {
        x,
        y: above_input,
        width,
        height: rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_area_shows_sidebar() {
        let area = Rect::new(0, 0, 140, 40);
        let l = compute(area, true);
        assert!(l.sidebar.is_some());
        assert_eq!(l.sidebar.unwrap().width, SIDEBAR_WIDTH);
        assert_eq!(l.transcript.width, 140 - SIDEBAR_WIDTH);
    }

    #[test]
    fn narrow_area_hides_sidebar() {
        let area = Rect::new(0, 0, 80, 40);
        let l = compute(area, true);
        assert!(l.sidebar.is_none());
        assert_eq!(l.transcript.width, 80);
    }

    #[test]
    fn show_sidebar_false_forces_hide() {
        let area = Rect::new(0, 0, 200, 40);
        let l = compute(area, false);
        assert!(l.sidebar.is_none());
    }

    #[test]
    fn transcript_extends_full_height_below_status() {
        let area = Rect::new(0, 0, 80, 40);
        let l = compute_ex(area, false, 1);
        assert_eq!(l.transcript.y, 1);
        assert_eq!(l.transcript.height, 39);
    }

    #[test]
    fn input_rect_is_horizontally_centered_and_bottom_aligned() {
        let transcript = Rect::new(0, 1, 100, 30);
        let rect = compute_input_rect(transcript, 1);
        assert_eq!(rect.width, 75);
        assert!(rect.height >= 3);
        assert_eq!(rect.y + rect.height, transcript.y + transcript.height - 1);
        assert_eq!(rect.x, (100 - 75) / 2);
    }

    #[test]
    fn input_rect_grows_with_buffer_lines() {
        let transcript = Rect::new(0, 0, 100, 30);
        let small = compute_input_rect(transcript, 1);
        let big = compute_input_rect(transcript, 5);
        assert!(big.height > small.height);
    }

    #[test]
    fn input_rect_clamps_to_min_width_on_narrow_terminals() {
        let transcript = Rect::new(0, 0, 60, 20);
        let rect = compute_input_rect(transcript, 1);
        assert_eq!(rect.width, 50);
    }

    #[test]
    fn approvals_rect_floats_directly_above_input() {
        let transcript = Rect::new(0, 1, 100, 30);
        let input = compute_input_rect(transcript, 1);
        let rect = compute_approvals_rect(transcript, input, 3).expect("approvals rect");
        assert_eq!(rect.x, input.x);
        assert_eq!(rect.width, input.width);
        assert_eq!(rect.y + rect.height, input.y);
    }

    #[test]
    fn approvals_rect_zero_rows_returns_none() {
        let transcript = Rect::new(0, 1, 100, 30);
        let input = compute_input_rect(transcript, 1);
        assert!(compute_approvals_rect(transcript, input, 0).is_none());
    }
}
