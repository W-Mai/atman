use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use atman_runtime::TaskSnapshot;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct TerminalPanelContent {
    pub handle: String,
    pub scroll: u16,
}

impl WindowComponent for TerminalPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let snap = ctx
            .snapshots
            .iter()
            .find(|s| s.source_handle == self.handle);
        let item = ctx.items.iter().rev().find(|it| match it {
            crate::app::OutputItem::Terminal { handle, .. } => *handle == self.handle,
            _ => false,
        });
        use crate::app::OutputItem;
        if let Some(OutputItem::Terminal {
            screen,
            accumulated_bytes,
            done,
            ..
        }) = item
        {
            if let Some(snap) = snap {
                render_terminal_content(frame, area, snap, screen, accumulated_bytes, *done);
            } else {
                render_terminal_screen(frame, area, &self.handle, screen);
            }
        } else if let Some(snap) = snap {
            super::common::render_task_meta(frame, area, atman_runtime::TaskKind::Terminal, snap);
        } else {
            super::common::render_placeholder(frame, area, &self.handle);
        }
        Vec::new()
    }

    fn handle_event(&mut self, _event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        WmEventResult::Ignored
    }

    fn sync_state(&mut self, scroll: u16, _h_scroll: u16, _split: bool) {
        self.scroll = scroll;
    }

    fn extract_state(&self) -> (u16, u16, bool) {
        (self.scroll, 0, false)
    }

    fn preferred_size(&self, _viewport: Rect) -> SizeHint {
        SizeHint {
            min: (20, 6),
            max: None,
            preferred: (88, 29),
        }
    }
}

fn render_terminal_content(
    f: &mut Frame,
    area: Rect,
    snap: &TaskSnapshot,
    screen: &atman_runtime::tools::term::TerminalScreen,
    _accumulated_bytes: &[u8],
    _done: bool,
) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", status_icon(snap.status)),
            Style::default().fg(status_color(snap.status)),
        ),
        Span::styled(&snap.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            format!(
                "{} · {}",
                snap.status.display_label(),
                format_elapsed(snap.elapsed_ms())
            ),
            Style::default().fg(t.subtle_fg.into()),
        ),
    ]);
    let header_area = Rect { height: 1, ..area };
    f.render_widget(Paragraph::new(header), header_area);

    let body_area = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    if body_area.height == 0 || screen.cells.is_empty() {
        return;
    }

    let cols = screen.cols as usize;
    let total_rows = screen.rows as usize;
    let max_rows = (body_area.height as usize).min(total_rows);
    let start_row = total_rows.saturating_sub(max_rows);
    let bg: Color = t.code_bg.into();
    let mut lines: Vec<Line> = Vec::with_capacity(max_rows);
    for row in start_row..total_rows {
        let mut spans: Vec<Span> = Vec::with_capacity(cols);
        for col in 0..cols {
            let idx = row * cols + col;
            if idx >= screen.cells.len() {
                spans.push(Span::raw(" "));
                continue;
            }
            let cell = &screen.cells[idx];
            if cell.wide_continuation {
                continue;
            }
            let style = crate::output::cell_style_for_viewer(cell, bg);
            let text = if cell.chars.is_empty() {
                " ".to_string()
            } else {
                cell.chars.clone()
            };
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_terminal_screen(
    f: &mut Frame,
    area: Rect,
    title: &str,
    screen: &atman_runtime::tools::term::TerminalScreen,
) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(" ✓ ", Style::default().fg(t.success.into())),
        Span::styled(title, Style::default().fg(t.tinted_fg.into())),
    ]);
    let header_area = Rect { height: 1, ..area };
    f.render_widget(Paragraph::new(header), header_area);

    let body_area = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    if body_area.height == 0 || screen.cells.is_empty() {
        return;
    }

    let cols = screen.cols as usize;
    let total_rows = screen.rows as usize;
    let max_rows = (body_area.height as usize).min(total_rows);
    let start_row = total_rows.saturating_sub(max_rows);
    let bg: Color = t.code_bg.into();
    let mut lines: Vec<Line> = Vec::with_capacity(max_rows);
    for row in start_row..total_rows {
        let mut spans: Vec<Span> = Vec::with_capacity(cols);
        for col in 0..cols {
            let idx = row * cols + col;
            if idx >= screen.cells.len() {
                spans.push(Span::raw(" "));
                continue;
            }
            let cell = &screen.cells[idx];
            if cell.wide_continuation {
                continue;
            }
            let style = crate::output::cell_style_for_viewer(cell, bg);
            let text = if cell.chars.is_empty() {
                " ".to_string()
            } else {
                cell.chars.clone()
            };
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), body_area);
}

fn status_icon(status: atman_runtime::TaskStatus) -> &'static str {
    match status {
        atman_runtime::TaskStatus::Running => "◐",
        atman_runtime::TaskStatus::Killing => "◑",
        atman_runtime::TaskStatus::Ok => "✓",
        atman_runtime::TaskStatus::Err => "✗",
        atman_runtime::TaskStatus::Killed => "⊘",
    }
}

fn status_color(status: atman_runtime::TaskStatus) -> Color {
    let t = crate::theme::theme();
    match status {
        atman_runtime::TaskStatus::Running => t.accent.into(),
        atman_runtime::TaskStatus::Killing => t.warn.into(),
        atman_runtime::TaskStatus::Ok => t.success.into(),
        atman_runtime::TaskStatus::Err => t.error.into(),
        atman_runtime::TaskStatus::Killed => t.subtle_fg.into(),
    }
}

fn format_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    let raw = if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    };
    format!("{:>5}", raw)
}
