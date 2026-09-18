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
    pub scroll: u32,
    pub(crate) selection_projection: Option<crate::selection::VisibleSelectionProjection>,
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
            .task_handle_index
            .get(&self.handle)
            .and_then(|&index| ctx.snapshots.get(index));
        let item_detail = ctx.task_detail(&self.handle);
        let revision = item_detail
            .map_or(crate::app::OutputRevision::default(), |(_, _, revision)| {
                revision
            });
        self.selection_projection = None;
        let item = item_detail.map(|(_, item, _)| item);
        use crate::app::OutputItem;
        if let Some(OutputItem::Terminal {
            title,
            command,
            screen,
            ..
        }) = item
        {
            if let Some(snap) = snap {
                if let Some((body_area, lines)) = render_terminal_content(
                    frame,
                    area,
                    snap,
                    command.as_deref().or(snap.command.as_deref()),
                    screen,
                    &mut self.scroll,
                ) {
                    self.selection_projection =
                        Some(crate::window::common::window_text_projection(
                            ctx.window_id,
                            "terminal-output",
                            revision,
                            &lines,
                            body_area,
                            0,
                        ));
                }
            } else {
                if let Some((body_area, lines)) = render_terminal_screen(
                    frame,
                    area,
                    title.as_deref().unwrap_or(&self.handle),
                    command.as_deref(),
                    screen,
                    &mut self.scroll,
                ) {
                    self.selection_projection =
                        Some(crate::window::common::window_text_projection(
                            ctx.window_id,
                            "terminal-output",
                            revision,
                            &lines,
                            body_area,
                            0,
                        ));
                }
            }
        } else if let Some(snap) = snap {
            super::common::render_task_meta(
                frame,
                area,
                atman_runtime::TaskKind::Terminal,
                snap,
                &mut self.scroll,
            );
        } else {
            super::common::render_placeholder(frame, area, &self.handle);
        }
        Vec::new()
    }

    fn selection_projection(&self) -> Option<crate::selection::VisibleSelectionProjection> {
        self.selection_projection.clone()
    }

    fn handle_event(&mut self, _event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        WmEventResult::Ignored
    }

    fn sync_state(&mut self, scroll: u32, _h_scroll: u16, _split: bool) {
        self.scroll = scroll;
    }

    fn extract_state(&self) -> (u32, u16, bool) {
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
    command: Option<&str>,
    screen: &atman_runtime::tools::term::TerminalScreen,
    scroll: &mut u32,
) -> Option<(Rect, Vec<ratatui::text::Line<'static>>)> {
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
    if body_area.height == 0 {
        return None;
    }
    let lines = terminal_detail_lines(command, screen, body_area.width as usize);
    let max_scroll = lines.len().saturating_sub(body_area.height as usize) as u32;
    *scroll = (*scroll).min(max_scroll);
    let start = *scroll as usize;
    let end = start
        .saturating_add(body_area.height as usize)
        .min(lines.len());
    let visible = lines[start.min(end)..end].to_vec();
    f.render_widget(Paragraph::new(visible.clone()), body_area);
    Some((body_area, visible))
}

fn render_terminal_screen(
    f: &mut Frame,
    area: Rect,
    title: &str,
    command: Option<&str>,
    screen: &atman_runtime::tools::term::TerminalScreen,
    scroll: &mut u32,
) -> Option<(Rect, Vec<ratatui::text::Line<'static>>)> {
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
    if body_area.height == 0 {
        return None;
    }

    let lines = terminal_detail_lines(command, screen, body_area.width as usize);
    let max_scroll = lines.len().saturating_sub(body_area.height as usize) as u32;
    *scroll = (*scroll).min(max_scroll);
    let start = *scroll as usize;
    let end = start
        .saturating_add(body_area.height as usize)
        .min(lines.len());
    let visible = lines[start.min(end)..end].to_vec();
    f.render_widget(Paragraph::new(visible.clone()), body_area);
    Some((body_area, visible))
}

fn terminal_detail_lines(
    command: Option<&str>,
    screen: &atman_runtime::tools::term::TerminalScreen,
    width: usize,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let mut lines = Vec::new();
    if let Some(command) = command.filter(|command| !command.is_empty()) {
        lines.extend(super::common::detail_command_lines(command, width));
        lines.push(Line::from(""));
    }
    lines.push(super::common::detail_section_label("screen", width));

    let cols = screen.cols as usize;
    let total_rows = screen.rows as usize;
    let bg: Color = t.code_bg.into();
    for row in 0..total_rows {
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
    lines
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

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::task_registry::{TaskSnapshot, TaskStatus};
    use atman_runtime::tools::term::{TerminalCell, TerminalScreen};

    #[test]
    fn floating_panel_shows_the_raw_terminal_command() {
        let command = "ps -axo pid,command | sort -n";
        let snapshot = TaskSnapshot {
            id: atman_runtime::TaskId::now(),
            kind: atman_runtime::TaskKind::Terminal,
            label: "检查进程".into(),
            command: Some(command.into()),
            status: TaskStatus::Running,
            started_at: std::time::Instant::now(),
            ended_at: None,
            source_handle: "term_s_0".into(),
            session_id: "s".into(),
            workspace_id: None,
            flow_run_id: None,
            termination: None,
        };
        let screen = TerminalScreen {
            rows: 1,
            cols: 4,
            cells: vec![TerminalCell::default(); 4],
            cursor: None,
            alt_screen: false,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).expect("terminal");
        let mut scroll = 0;
        terminal
            .draw(|frame| {
                render_terminal_content(
                    frame,
                    frame.area(),
                    &snapshot,
                    Some(command),
                    &screen,
                    &mut scroll,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .chunks(60)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(command));
    }
}
