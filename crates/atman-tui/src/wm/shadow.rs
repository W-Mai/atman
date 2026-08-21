use std::collections::HashSet;

use ratatui::Frame;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::Color;

fn sanitize_shadow_cell(buf: &mut Buffer, area: Rect, x: u16, y: u16) {
    if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
        return;
    }
    let clear = |cell: &mut ratatui::buffer::Cell| {
        cell.set_symbol(" ");
        cell.set_diff_option(CellDiffOption::None);
    };
    let symbol = buf[(x, y)].symbol().to_string();
    if crate::width::width(&symbol) > 1 {
        let bg = buf[(x, y)].bg;
        clear(&mut buf[(x, y)]);
        if x + 1 < area.x + area.width {
            if matches!(buf[(x + 1, y)].bg, Color::Reset) {
                buf[(x + 1, y)].bg = bg;
            }
            clear(&mut buf[(x + 1, y)]);
        }
    } else if crate::width::width(&symbol) == 1 && symbol.trim().is_empty() && x > area.x {
        let first = x - 1;
        if crate::width::width(buf[(first, y)].symbol()) <= 1 {
            return;
        }
        let bg = buf[(first, y)].bg;
        if matches!(buf[(x, y)].bg, Color::Reset) {
            buf[(x, y)].bg = bg;
        }
        clear(&mut buf[(x, y)]);
        clear(&mut buf[(first, y)]);
    }
}

fn shadow_ring_cells(rect: Rect, area: Rect) -> HashSet<(u16, u16)> {
    let max_x = area.x.saturating_add(area.width).saturating_sub(1);
    let max_y = area.y.saturating_add(area.height).saturating_sub(1);
    let top_y = rect.y.saturating_sub(1);
    let bot_y = (rect.y + rect.height).min(max_y);
    let lx0 = rect.x.saturating_sub(2);
    let lx1 = rect.x.saturating_sub(1);
    let rx0 = (rect.x + rect.width).min(max_x);
    let rx1 = (rect.x + rect.width + 1).min(max_x);
    let mut cells = HashSet::new();
    for y in top_y..=bot_y {
        for x in lx0..=rx1 {
            if y == top_y || y == bot_y || x == lx0 || x == lx1 || x == rx0 || x == rx1 {
                cells.insert((x, y));
            }
        }
    }
    cells
}

fn render_shadow_to_buffer(buf: &mut Buffer, rect: Rect, factor: Color) {
    let area = *buf.area();
    let darken = |buf: &mut ratatui::buffer::Buffer, x: u16, y: u16| {
        if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
            return;
        }
        let cell = &mut buf[(x, y)];
        if matches!(cell.bg, Color::Reset) {
            return;
        }
        cell.bg = multiply_color(cell.bg, factor);
    };

    let ring = shadow_ring_cells(rect, area);
    let top_y = rect.y.saturating_sub(1);
    let bot_y = (rect.y + rect.height).min(area.y + area.height - 1);
    let lx1 = rect.x.saturating_sub(1);
    let rx0 = (rect.x + rect.width).min(area.x + area.width - 1);
    for &(x, y) in &ring {
        let horizontal = y == top_y || y == bot_y;
        let side_segment = x <= lx1 || x >= rx0;
        if !horizontal || side_segment {
            sanitize_shadow_cell(buf, area, x, y);
        }
    }
    for (x, y) in ring {
        darken(buf, x, y);
    }
}

pub fn render_shadow(f: &mut Frame, rect: Rect, t: &crate::theme::Theme) {
    let shadow = t.shadow.into();
    let factor = lerp_color(Color::Rgb(255, 255, 255), shadow, 0.6);
    render_shadow_to_buffer(f.buffer_mut(), rect, factor);
}

pub fn render_backdrop(f: &mut Frame, t: &crate::theme::Theme) {
    let buf = f.buffer_mut();
    let area = *buf.area();
    let shadow = t.shadow.into();
    let factor = lerp_color(Color::Rgb(255, 255, 255), shadow, 0.65);
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = &mut buf[(x, y)];
            if matches!(cell.bg, Color::Reset) {
                continue;
            }
            cell.bg = multiply_color(cell.bg, factor);
            if !matches!(cell.fg, Color::Reset) {
                cell.fg = multiply_color(cell.fg, factor);
            }
        }
    }
}

pub fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    let mode = crate::theme::current_mode();
    fn to_rgb(c: Color, mode: crate::theme::ThemeMode) -> (u8, u8, u8) {
        use crate::theme::ThemeMode;
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Cyan => (40, 180, 180),
            Color::DarkGray => (96, 96, 96),
            Color::Gray => (128, 128, 128),
            Color::Green => (70, 175, 70),
            Color::Yellow => (190, 175, 55),
            Color::Red => (190, 65, 65),
            Color::Blue => (0, 0, 200),
            Color::Magenta => (200, 0, 200),
            Color::White => (240, 240, 240),
            Color::Black => (16, 16, 16),
            // Reset = terminal default; fg is light in dark mode, dark in light mode
            Color::Reset => match mode {
                ThemeMode::Dark => (220, 220, 220),
                ThemeMode::Light => (40, 40, 40),
            },
            _ => (128, 128, 128),
        }
    }
    let (ar, ag, ab) = to_rgb(a, mode);
    let (br, bg, bb) = to_rgb(b, mode);
    let t = t.clamp(0.0, 1.0);
    Color::Rgb(
        (ar as f64 + (br as f64 - ar as f64) * t).round() as u8,
        (ag as f64 + (bg as f64 - ag as f64) * t).round() as u8,
        (ab as f64 + (bb as f64 - ab as f64) * t).round() as u8,
    )
}

/// Multiply blend: `result = a * b / 255`. Always darkens (or keeps same).
/// `shadow` color acts as a brightness factor — e.g. (128,128,128) = 50% darken.
pub fn multiply_color(a: Color, b: Color) -> Color {
    let mode = crate::theme::current_mode();
    fn to_rgb(c: Color, mode: crate::theme::ThemeMode) -> (u8, u8, u8) {
        use crate::theme::ThemeMode;
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Cyan => (40, 180, 180),
            Color::DarkGray => (96, 96, 96),
            Color::Gray => (128, 128, 128),
            Color::Green => (70, 175, 70),
            Color::Yellow => (190, 175, 55),
            Color::Red => (190, 65, 65),
            Color::Blue => (0, 0, 200),
            Color::Magenta => (200, 0, 200),
            Color::White => (240, 240, 240),
            Color::Black => (16, 16, 16),
            Color::Reset => match mode {
                ThemeMode::Dark => (220, 220, 220),
                ThemeMode::Light => (40, 40, 40),
            },
            _ => (128, 128, 128),
        }
    }
    let (ar, ag, ab) = to_rgb(a, mode);
    let (br, bg, bb) = to_rgb(b, mode);
    Color::Rgb(
        ((ar as u32 * br as u32) / 255) as u8,
        ((ag as u32 * bg as u32) / 255) as u8,
        ((ab as u32 * bb as u32) / 255) as u8,
    )
}

fn darken_cell(buf: &mut ratatui::buffer::Buffer, area: Rect, x: u16, y: u16, ratio: f64) {
    if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
        return;
    }
    let cell = &mut buf[(x, y)];
    if matches!(cell.bg, Color::Reset) {
        return;
    }
    let t = crate::theme::theme();
    let shadow = t.shadow.into();
    let factor = lerp_color(Color::Rgb(255, 255, 255), shadow, ratio);
    cell.bg = multiply_color(cell.bg, factor);
}

/// Top-to-bottom gradient: strongest at `rect.y`, fading to 0 over `rows` rows.
pub fn render_top_fade(f: &mut Frame, rect: Rect, rows: u16) {
    let buf = f.buffer_mut();
    let area = *buf.area();
    let rows = rows.min(rect.height);
    for ry in 0..rows {
        let ratio = 0.8 * (1.0 - ry as f64 / rows as f64);
        let y = rect.y + ry;
        for x in rect.x..rect.x + rect.width {
            darken_cell(buf, area, x, y, ratio);
        }
    }
}

/// Bottom-to-top gradient: strongest at `rect.y + rect.height - 1`,
/// fading to 0 over `rows` rows upward.
pub fn render_bottom_fade(f: &mut Frame, rect: Rect, rows: u16) {
    let buf = f.buffer_mut();
    let area = *buf.area();
    let rows = rows.min(rect.height);
    for ry in 0..rows {
        let ratio = 0.8 * (1.0 - ry as f64 / rows as f64);
        let y = rect.y + rect.height - 1 - ry;
        for x in rect.x..rect.x + rect.width {
            darken_cell(buf, area, x, y, ratio);
        }
    }
}

/// Input box shadow: sides + bottom fade with rounded corners.
pub fn render_input_shadow(f: &mut Frame, rect: Rect) {
    let buf = f.buffer_mut();
    let area = *buf.area();
    let h_cols = 4u16.min(rect.width / 2);
    let v_rows = 2u16;
    let max_ratio = 0.6;

    // iterate over the L-shaped shadow region (left + right + bottom)
    // and compute ratio from elliptical distance to the input box edge
    let offset_y = 1u16;
    let shadow_y0 = rect.y + offset_y;
    let shadow_y1 = rect.y + rect.height + v_rows + offset_y;
    let shadow_x0 = rect.x.saturating_sub(h_cols);
    let shadow_x1 = rect.x + rect.width + h_cols;

    for y in shadow_y0..shadow_y1 {
        for x in shadow_x0..shadow_x1 {
            // skip the input box interior (shifted down by offset_y)
            if x >= rect.x
                && x < rect.x + rect.width
                && y >= rect.y + offset_y
                && y < rect.y + rect.height + offset_y
            {
                continue;
            }

            // distance to nearest input box edge (shifted down by offset_y)
            let dx = if x < rect.x {
                rect.x.saturating_sub(x)
            } else if x >= rect.x + rect.width {
                x.saturating_sub(rect.x + rect.width)
            } else {
                0
            };
            let dy = if y >= rect.y + rect.height + offset_y {
                y.saturating_sub(rect.y + rect.height + offset_y)
            } else if y < rect.y + offset_y {
                (rect.y + offset_y).saturating_sub(y)
            } else {
                0
            };

            // normalize: horizontal distance relative to h_cols,
            // vertical distance relative to v_rows
            let nx = dx as f64 / h_cols as f64;
            let ny = dy as f64 / v_rows as f64;

            // elliptical falloff shaped like a quarter circle.
            // cells beyond 0.8 radius are cut to create the rounded gap.
            let dist = (nx * nx + ny * ny).sqrt();
            if dist > 0.8 {
                continue;
            }
            let ratio = max_ratio * (1.0 - dist / 0.8);
            if ratio > 0.0 {
                darken_cell(buf, area, x, y, ratio);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    #[test]
    fn shadow_ring_does_not_duplicate_corners() {
        let cells = shadow_ring_cells(Rect::new(4, 4, 8, 5), Rect::new(0, 0, 30, 30));
        assert_eq!(cells.len(), 44);
        assert!(cells.contains(&(2, 3)));
        assert!(cells.contains(&(13, 3)));
        assert!(cells.contains(&(2, 8)));
        assert!(cells.contains(&(13, 8)));
    }

    #[test]
    fn continuation_boundary_does_not_expand_shadow_to_wide_glyph_start() {
        let area = Rect::new(0, 0, 10, 6);
        let mut buf = Buffer::empty(area);
        let original = Color::Rgb(120, 120, 120);
        let factor = Color::Rgb(128, 128, 128);
        buf.set_string(2, 2, "回", Style::default().bg(original));
        buf[(3, 2)].bg = Color::Reset;
        buf[(4, 2)].bg = original;

        render_shadow_to_buffer(&mut buf, Rect::new(5, 1, 3, 3), factor);

        assert_eq!(buf[(2, 2)].bg, original);
        assert_eq!(buf[(3, 2)].bg, multiply_color(original, factor));
        assert_eq!(buf[(4, 2)].bg, multiply_color(original, factor));
    }

    #[test]
    fn left_boundary_continuation_clears_only_its_wide_pair() {
        let area = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(2, 0, "回", Style::default().bg(Color::Rgb(28, 32, 40)));
        buf.set_string(4, 0, "X", Style::default().bg(Color::Rgb(60, 64, 72)));
        sanitize_shadow_cell(&mut buf, area, 3, 0);
        assert_eq!(buf[(2, 0)].symbol(), " ");
        assert_eq!(buf[(3, 0)].symbol(), " ");
        assert_eq!(buf[(4, 0)].symbol(), "X");
    }

    #[test]
    fn empty_cell_after_plain_text_is_not_treated_as_continuation() {
        let area = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(2, 0, "ab", Style::default().bg(Color::Rgb(28, 32, 40)));
        buf[(3, 0)].set_symbol(" ");

        sanitize_shadow_cell(&mut buf, area, 3, 0);
        assert_eq!(buf[(2, 0)].symbol(), "a");
        assert_eq!(buf[(3, 0)].symbol(), " ");
    }

    #[test]
    fn sanitizing_wide_glyph_copies_background_to_continuation() {
        let area = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(area);
        let source_bg = Color::Rgb(28, 32, 40);
        buf.set_string(2, 0, "回", Style::default().bg(source_bg));
        buf[(3, 0)].bg = Color::Reset;

        sanitize_shadow_cell(&mut buf, area, 2, 0);

        assert_eq!(buf[(2, 0)].symbol(), " ");
        assert_eq!(buf[(3, 0)].symbol(), " ");
        assert_eq!(buf[(2, 0)].bg, source_bg);
        assert_eq!(buf[(3, 0)].bg, source_bg);
    }
}
