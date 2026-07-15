use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::{AppState, OutputItem, TerminalViewMode};
use crate::output::NodeRegion;

const PAD: u16 = 2;

#[derive(Default, Debug)]
pub struct TerminalViewerModal {
    pub open: bool,
    pub panel_item_index: usize,
    pub h_offset: u16,
    pub v_offset: u16,
    pub last_inner_rect: Option<Rect>,
    pub last_node_regions: Vec<NodeRegion>,
}

impl TerminalViewerModal {
    pub fn open(&mut self, panel_item_index: usize) {
        if self.panel_item_index != panel_item_index {
            self.h_offset = 0;
            self.v_offset = 0;
        }
        self.open = true;
        self.panel_item_index = panel_item_index;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn scroll_left(&mut self, step: u16) {
        self.h_offset = self.h_offset.saturating_sub(step);
    }

    pub fn scroll_right(&mut self, step: u16, max: u16) {
        self.h_offset = self.h_offset.saturating_add(step).min(max);
    }

    pub fn scroll_up(&mut self, step: u16) {
        self.v_offset = self.v_offset.saturating_sub(step);
    }

    pub fn scroll_down(&mut self, step: u16, max: u16) {
        self.v_offset = self.v_offset.saturating_add(step).min(max);
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, app: &mut AppState) {
    let Some(OutputItem::Terminal {
        handle,
        screen,
        accumulated_bytes,
        mode,
        done,
        ..
    }) = app.items.get(app.terminal_viewer.panel_item_index).cloned()
    else {
        app.terminal_viewer.close();
        return;
    };

    let t = crate::theme::theme();
    let bg = t.code_bg;

    let pty_cols = screen.cols.max(1);
    let pty_rows = screen.rows.max(1);

    let border_w: u16 = 2;
    let border_h: u16 = 2;
    let modal_w = (pty_cols + PAD * 2 + border_w).min(area.width.saturating_sub(2));
    let modal_h = (pty_rows + PAD * 2 + border_h).min(area.height.saturating_sub(1));
    let x = area.x + area.width.saturating_sub(modal_w) / 2;
    let y = area.y + area.height.saturating_sub(modal_h) / 2;
    let modal_area = Rect {
        x,
        y,
        width: modal_w,
        height: modal_h,
    };
    crate::sanitize_widget_edges(f, modal_area);
    f.render_widget(Clear, modal_area);

    let glyph = if done { "✓" } else { "●" };
    let mode_label = match mode {
        TerminalViewMode::Capture => "capture",
        TerminalViewMode::Stream => "stream",
    };
    let title = format!(
        " {glyph} terminal[{handle}] {mode_label} {cols}×{rows} · Esc close · j/k scroll ",
        cols = pty_cols,
        rows = pty_rows,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);
    app.terminal_viewer.last_inner_rect = Some(inner);

    let pad_style = Style::default().bg(bg);
    for _ in 0..PAD.min(inner.height) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " ".repeat(inner.width as usize),
                pad_style,
            ))),
            inner,
        );
    }

    let content_area = Rect {
        x: inner.x + PAD.min(inner.width / 2),
        y: inner.y + PAD.min(inner.height / 2),
        width: pty_cols.min(inner.width.saturating_sub(PAD * 2)),
        height: pty_rows.min(inner.height.saturating_sub(PAD * 2)),
    };

    if content_area.width > 0 && content_area.height > 0 {
        f.render_widget(Clear, content_area);
        let content_bg = Paragraph::new(
            std::iter::repeat_n(
                Line::from(Span::styled(
                    " ".repeat(content_area.width as usize),
                    pad_style,
                )),
                content_area.height as usize,
            )
            .collect::<Vec<_>>(),
        );
        f.render_widget(content_bg, content_area);

        let lines = match mode {
            TerminalViewMode::Capture => render_capture_full(&screen, content_area.width),
            TerminalViewMode::Stream => render_stream_full(&accumulated_bytes, content_area.width),
        };

        let total_rows = lines.len() as u16;
        let visible_rows = content_area.height;
        let max_v = total_rows.saturating_sub(visible_rows);
        if app.terminal_viewer.v_offset > max_v {
            app.terminal_viewer.v_offset = max_v;
        }
        let max_h = pty_cols.saturating_sub(content_area.width);
        if app.terminal_viewer.h_offset > max_h {
            app.terminal_viewer.h_offset = max_h;
        }

        let p = Paragraph::new(lines).scroll((app.terminal_viewer.v_offset, 0));
        f.render_widget(p, content_area);
    }
}

fn render_capture_full(
    screen: &atman_runtime::tools::term::TerminalScreen,
    width: u16,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg = t.code_bg;
    let pad_style = Style::default().bg(bg);
    let cols = screen.cols as usize;
    let target = width as usize;
    let mut lines = Vec::with_capacity(screen.rows as usize);
    for row in 0..screen.rows as usize {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols + 1);
        let mut row_w = 0usize;
        for col in 0..cols {
            let idx = row * cols + col;
            if idx >= screen.cells.len() {
                break;
            }
            let cell = &screen.cells[idx];
            let cell_style = crate::output::cell_style_for_viewer(cell, bg);
            let chars = if cell.chars.is_empty() {
                " "
            } else {
                &cell.chars
            };
            let cw = unicode_width::UnicodeWidthStr::width(chars);
            row_w += cw;
            spans.push(Span::styled(chars.to_string(), cell_style));
        }
        let pad = target.saturating_sub(row_w);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), pad_style));
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(" ".repeat(target), pad_style)));
    }
    lines
}

fn render_stream_full(accumulated_bytes: &[u8], width: u16) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let body_style = Style::default().fg(t.subtle_fg).bg(t.code_bg);
    let text = String::from_utf8_lossy(accumulated_bytes).into_owned();
    let target = width.max(20) as usize;
    let mut lines = Vec::new();
    for line in text.lines() {
        let rows = crate::output::wrap_with_prefix(line, target, "  ", "  ");
        for row in rows {
            lines.push(crate::output::line_with_right_pad(
                &row.prefix,
                &row.body,
                target,
                body_style,
                body_style,
            ));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(" ".repeat(target), body_style)));
    }
    lines
}
