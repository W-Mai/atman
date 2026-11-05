//! Compact progress presentation shared by asynchronous UI operations.

use ratatui::{
    style::Style,
    text::{Line, Span},
};

pub(crate) const REFRESH_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Clone, Copy, Debug)]
pub(crate) enum ViewPhase {
    Waiting,
    Reading,
    Building,
}

impl ViewPhase {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting",
            Self::Reading => "Reading history",
            Self::Building => "Preparing view",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewLoading {
    pub phase: ViewPhase,
    pub started_at: std::time::Instant,
}

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

pub(crate) fn render_view(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    title: &str,
    loading: ViewLoading,
    tick: u32,
) {
    use ratatui::{layout::Rect, widgets::Paragraph};
    let theme = crate::theme::theme();
    let width = area.width.saturating_sub(4).min(52);
    if width == 0 || area.height < 4 {
        return;
    }
    let x = area.x + (area.width - width) / 2;
    let y = area.y + area.height.saturating_sub(5) / 2;
    let elapsed = format!("{:.1}s", loading.started_at.elapsed().as_secs_f64());
    let title_width = usize::from(width).saturating_sub(elapsed.len() + 2);
    let title = crate::width::truncate(title, title_width);
    let gap = usize::from(width).saturating_sub(crate::width::width(&title) + elapsed.len());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(title, Style::default().fg(theme.tinted_fg.into())),
            Span::raw(" ".repeat(gap)),
            Span::styled(elapsed, Style::default().fg(theme.subtle_fg.into())),
        ])),
        Rect::new(x, y, width, 1),
    );
    frame.render_widget(
        Paragraph::new(line(loading.phase.label(), tick, width)),
        Rect::new(x, y + 2, width, 1),
    );
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
