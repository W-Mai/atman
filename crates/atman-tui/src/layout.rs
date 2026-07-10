use ratatui::layout::{Constraint, Direction, Layout as RatatuiLayout, Rect};

// Kept as public constants so the palette / status bar helpers can still
// query "will the sidebar be visible?" without duplicating the width math.
pub const SIDEBAR_WIDTH: u16 = 48;
pub const SIDEBAR_MIN_TOTAL_WIDTH: u16 = 80;

// AppLayout only reserves status + transcript. Input, approvals, and the
// sidebar all draw as floating overlays on top of the transcript so
// message content still visually flows underneath them.
#[derive(Debug, Clone, Copy)]
pub struct AppLayout {
    pub status: Rect,
    pub transcript: Rect,
}

pub fn compute(area: Rect) -> AppLayout {
    compute_ex(area, 1)
}

pub fn compute_ex(area: Rect, status_height: u16) -> AppLayout {
    let vertical = RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(status_height.max(1)), Constraint::Min(1)])
        .split(area);
    AppLayout {
        status: vertical[0],
        transcript: vertical[1],
    }
}

pub fn compute_sidebar_rect(area: Rect, show: bool) -> Option<Rect> {
    if !show {
        return None;
    }
    if area.width < SIDEBAR_MIN_TOTAL_WIDTH {
        return None;
    }
    let width = SIDEBAR_WIDTH;
    let height = 24u16.min(area.height.saturating_sub(4));
    if height == 0 {
        return None;
    }
    let x = area
        .x
        .saturating_add(area.width)
        .saturating_sub(width)
        .saturating_sub(1);
    let y = area.y.saturating_add(2);
    Some(Rect {
        x,
        y,
        width,
        height,
    })
}

pub fn compute_input_rect(transcript: Rect, buf_lines: u16) -> Rect {
    input_rect_at(transcript, buf_lines, InputYAnchor::Bottom)
}

pub fn compute_input_rect_centered(transcript: Rect, buf_lines: u16) -> Rect {
    input_rect_at(transcript, buf_lines, InputYAnchor::Center)
}

pub fn compute_input_rect_at_row(transcript: Rect, buf_lines: u16, target_y: u16) -> Rect {
    let base = input_rect_at(transcript, buf_lines, InputYAnchor::Bottom);
    let max_y = transcript
        .y
        .saturating_add(transcript.height)
        .saturating_sub(base.height);
    Rect {
        x: base.x,
        y: target_y.min(max_y).max(transcript.y),
        width: base.width,
        height: base.height,
    }
}

// t=0 → centered, t=1 → bottom. Used for the startup-splash slide.
pub fn compute_input_rect_lerped(transcript: Rect, buf_lines: u16, t: f32) -> Rect {
    let center = input_rect_at(transcript, buf_lines, InputYAnchor::Center);
    let bottom = input_rect_at(transcript, buf_lines, InputYAnchor::Bottom);
    let clamped = t.clamp(0.0, 1.0);
    let y_f = center.y as f32 + (bottom.y as f32 - center.y as f32) * clamped;
    Rect {
        x: bottom.x,
        y: y_f.round() as u16,
        width: bottom.width,
        height: bottom.height,
    }
}

#[derive(Clone, Copy)]
enum InputYAnchor {
    Bottom,
    Center,
}

fn input_rect_at(transcript: Rect, buf_lines: u16, anchor: InputYAnchor) -> Rect {
    let outer_width = (transcript.width * 3 / 4)
        .clamp(50, 120)
        .min(transcript.width);
    // Default height fits three content rows even when the buffer is empty,
    // so the panel never looks like a single squished line.
    let content_h = buf_lines.clamp(3, 6);
    let outer_h = content_h.saturating_add(2);
    // One row of transcript peeks under the panel so users feel the
    // messages continue behind the floating input.
    let bottom_margin: u16 = 1;
    let outer_h = outer_h.min(transcript.height.saturating_sub(bottom_margin).max(3));
    let x = transcript.x + (transcript.width.saturating_sub(outer_width)) / 2;
    let y = match anchor {
        InputYAnchor::Bottom => transcript
            .y
            .saturating_add(transcript.height)
            .saturating_sub(outer_h)
            .saturating_sub(bottom_margin),
        InputYAnchor::Center => transcript.y + (transcript.height.saturating_sub(outer_h)) / 2,
    };
    Rect {
        x,
        y,
        width: outer_width,
        height: outer_h,
    }
}

pub fn apply_horizontal_padding(rect: Rect, pad: u16) -> Rect {
    let two_pad = pad.saturating_mul(2);
    if rect.width <= two_pad {
        return rect;
    }
    Rect {
        x: rect.x.saturating_add(pad),
        y: rect.y,
        width: rect.width.saturating_sub(two_pad),
        height: rect.height,
    }
}

pub const CONTENT_GUTTER: u16 = 3;
pub const CONTENT_MAX_WIDTH: u16 = 120;

// Excess space beyond CONTENT_MAX_WIDTH stays blank on the right so
// long lines don't run edge-to-edge on wide terminals.
pub fn compute_content_rect(transcript: Rect) -> Rect {
    let gutter = CONTENT_GUTTER;
    if transcript.width <= gutter.saturating_mul(2) {
        return transcript;
    }
    let inner_width = transcript.width.saturating_sub(gutter.saturating_mul(2));
    let content_width = inner_width.min(CONTENT_MAX_WIDTH);
    Rect {
        x: transcript.x.saturating_add(gutter),
        y: transcript.y,
        width: content_width,
        height: transcript.height,
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
    fn transcript_extends_full_height_below_status() {
        let area = Rect::new(0, 0, 80, 40);
        let l = compute_ex(area, 1);
        assert_eq!(l.transcript.y, 1);
        assert_eq!(l.transcript.height, 39);
    }

    #[test]
    fn sidebar_hidden_when_closed() {
        let area = Rect::new(0, 0, 200, 40);
        assert!(compute_sidebar_rect(area, false).is_none());
    }

    #[test]
    fn sidebar_hidden_on_narrow_terminals() {
        let area = Rect::new(0, 0, 60, 40);
        assert!(compute_sidebar_rect(area, true).is_none());
    }

    #[test]
    fn sidebar_floats_in_top_right_when_open() {
        let area = Rect::new(0, 1, 140, 40);
        let rect = compute_sidebar_rect(area, true).unwrap();
        assert_eq!(rect.width, SIDEBAR_WIDTH);
        assert_eq!(rect.x + rect.width, area.x + area.width - 1);
        assert_eq!(rect.y, area.y + 2);
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
    fn horizontal_padding_shrinks_both_sides() {
        let rect = Rect::new(10, 5, 60, 20);
        let padded = apply_horizontal_padding(rect, 2);
        assert_eq!(padded.x, 12);
        assert_eq!(padded.width, 56);
        assert_eq!(padded.y, rect.y);
        assert_eq!(padded.height, rect.height);
    }

    #[test]
    fn horizontal_padding_leaves_narrow_rects_alone() {
        let rect = Rect::new(0, 0, 3, 5);
        assert_eq!(apply_horizontal_padding(rect, 2), rect);
    }

    #[test]
    fn input_rect_centered_sits_at_vertical_middle() {
        let transcript = Rect::new(0, 1, 100, 30);
        let rect = compute_input_rect_centered(transcript, 1);
        let bottom = compute_input_rect(transcript, 1);
        assert!(rect.y < bottom.y);
        assert_eq!(rect.width, bottom.width);
    }

    #[test]
    fn input_rect_lerp_t0_matches_centered() {
        let transcript = Rect::new(0, 1, 100, 30);
        let lerped = compute_input_rect_lerped(transcript, 1, 0.0);
        let centered = compute_input_rect_centered(transcript, 1);
        assert_eq!(lerped.y, centered.y);
    }

    #[test]
    fn input_rect_lerp_t1_matches_bottom() {
        let transcript = Rect::new(0, 1, 100, 30);
        let lerped = compute_input_rect_lerped(transcript, 1, 1.0);
        let bottom = compute_input_rect(transcript, 1);
        assert_eq!(lerped.y, bottom.y);
    }

    #[test]
    fn input_rect_lerp_mid_sits_between() {
        let transcript = Rect::new(0, 1, 100, 30);
        let centered = compute_input_rect_centered(transcript, 1);
        let bottom = compute_input_rect(transcript, 1);
        let mid = compute_input_rect_lerped(transcript, 1, 0.5);
        assert!(mid.y > centered.y);
        assert!(mid.y < bottom.y);
    }

    #[test]
    fn approvals_rect_zero_rows_returns_none() {
        let transcript = Rect::new(0, 1, 100, 30);
        let input = compute_input_rect(transcript, 1);
        assert!(compute_approvals_rect(transcript, input, 0).is_none());
    }
}
