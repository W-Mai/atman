use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;

pub fn render_shadow(f: &mut Frame, rect: Rect, t: &crate::theme::Theme) {
    use ratatui::buffer::CellDiffOption;
    let buf = f.buffer_mut();
    let area = *buf.area();

    let sanitize = |buf: &mut ratatui::buffer::Buffer, x: u16, y: u16| {
        if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
            return;
        }
        let cell = &mut buf[(x, y)];
        if crate::width::width(cell.symbol()) > 1 || cell.symbol().is_empty() {
            cell.set_symbol(" ");
            cell.set_diff_option(CellDiffOption::None);
        }
    };

    let shadow = t.shadow.into();
    let factor = lerp_color(Color::Rgb(255, 255, 255), shadow, 0.6);
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

    let max_x = area.x.saturating_add(area.width).saturating_sub(1);
    let max_y = area.y.saturating_add(area.height).saturating_sub(1);

    let top_y = rect.y.saturating_sub(1);
    let bot_y = (rect.y + rect.height).min(max_y);
    let lx0 = rect.x.saturating_sub(2);
    let lx1 = rect.x.saturating_sub(1);
    let rx0 = (rect.x + rect.width).min(max_x);
    let rx1 = (rect.x + rect.width + 1).min(max_x);

    // sanitize wide glyphs in shadow ring
    for x in lx0..=rx1 {
        sanitize(buf, x, top_y);
        sanitize(buf, x, bot_y);
    }
    for y in top_y..=bot_y {
        sanitize(buf, lx0, y);
        sanitize(buf, lx1, y);
        sanitize(buf, rx0, y);
        sanitize(buf, rx1, y);
    }

    // spiral shadow: top row, bottom row, right 2 cols, left 2 cols (offset to avoid corner overlap)
    for x in lx0..=rx1 {
        darken(buf, x, top_y);
    }
    for x in lx0..=rx1 {
        darken(buf, x, bot_y);
    }
    for y in top_y..=bot_y {
        darken(buf, rx0, y);
        darken(buf, rx1, y);
    }
    for y in top_y.saturating_add(1)..=bot_y {
        darken(buf, lx0, y);
        darken(buf, lx1, y);
    }
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
