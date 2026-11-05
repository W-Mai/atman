//! Compact progress presentation shared by asynchronous UI operations.

use ratatui::{
    style::Style,
    text::{Line, Span},
};

pub(crate) fn glyph(frame: u32) -> &'static str {
    crate::output::spinner_char(frame % u32::MAX)
}

pub(crate) fn line(label: &str, frame: u32, width: u16) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let theme = crate::theme::theme();
    let indicator = if width >= 12 {
        crate::output::loading_sweep(frame, 8)
    } else {
        glyph(frame).to_owned()
    };
    let remaining = usize::from(width).saturating_sub(crate::width::width(&indicator) + 1);
    let mut spans = vec![Span::styled(
        indicator,
        Style::default().fg(theme.accent.into()),
    )];
    if remaining > 0 && !label.is_empty() {
        spans.push(Span::styled(
            format!(" {}", crate::width::truncate(label, remaining)),
            Style::default().fg(theme.subtle_fg.into()),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    #[test]
    fn loading_projection_fits_its_region_without_changing_width_on_ticks() {
        for label in ["Loading projects", "Loading 历史会话", ""] {
            for width in 0..80 {
                let initial = super::line(label, 0, width).width();
                for tick in 1..20 {
                    let line = super::line(label, tick, width);
                    assert_eq!(line.width(), initial);
                    assert!(line.width() <= usize::from(width));
                }
            }
        }
    }
}
