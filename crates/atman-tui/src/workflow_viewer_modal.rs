use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

#[derive(Default, Debug)]
pub struct WorkflowViewerModal {
    pub open: bool,
    pub panel_item_index: usize,
    pub h_offset: u16,
    pub v_offset: u16,
    pub last_content_width: u16,
    pub last_content_rows: u16,
    pub last_visible_cols: u16,
    pub last_visible_rows: u16,
    pub last_inner_rect: Option<Rect>,
    pub last_node_regions: Vec<crate::output::NodeRegion>,
}

impl WorkflowViewerModal {
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

    pub fn scroll_right(&mut self, step: u16) {
        let max = self
            .last_content_width
            .saturating_sub(self.last_visible_cols);
        self.h_offset = self.h_offset.saturating_add(step).min(max);
    }

    pub fn scroll_up(&mut self, step: u16) {
        self.v_offset = self.v_offset.saturating_sub(step);
    }

    pub fn scroll_down(&mut self, step: u16) {
        let max = self
            .last_content_rows
            .saturating_sub(self.last_visible_rows);
        self.v_offset = self.v_offset.saturating_add(step).min(max);
    }

    pub fn home(&mut self) {
        self.h_offset = 0;
    }

    pub fn end(&mut self) {
        self.h_offset = self
            .last_content_width
            .saturating_sub(self.last_visible_cols);
    }
}

const VIEWER_PANEL_WIDTH: u16 = 300;

fn dim_background_outside(buf: &mut ratatui::buffer::Buffer, full: Rect, modal: Rect) {
    use ratatui::style::Modifier;
    let dim = Color::Rgb(70, 70, 70);
    for y in full.y..full.y.saturating_add(full.height) {
        for x in full.x..full.x.saturating_add(full.width) {
            let inside = x >= modal.x
                && x < modal.x.saturating_add(modal.width)
                && y >= modal.y
                && y < modal.y.saturating_add(modal.height);
            if inside {
                continue;
            }
            let cell = &mut buf[(x, y)];
            cell.fg = dim;
            cell.modifier.remove(Modifier::BOLD);
            cell.modifier.remove(Modifier::REVERSED);
            cell.modifier.insert(Modifier::DIM);
        }
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, app: &mut crate::app::AppState) {
    let modal_w = (area.width * 9 / 10).clamp(80, area.width.saturating_sub(4).max(80));
    let modal_h = (area.height * 9 / 10).clamp(20, area.height.saturating_sub(2).max(20));
    let x = area.x + area.width.saturating_sub(modal_w) / 2;
    let y = area.y + area.height.saturating_sub(modal_h) / 2;
    let modal_area = Rect {
        x,
        y,
        width: modal_w,
        height: modal_h,
    };
    dim_background_outside(f.buffer_mut(), area, modal_area);
    f.render_widget(Clear, modal_area);
    let title = format!(
        " Workflow · Esc close · h/l or Shift+←/→ · j/k up/down · offset {},{} ",
        app.workflow_viewer.h_offset, app.workflow_viewer.v_offset
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
    if inner.height < 2 || inner.width < 8 {
        return;
    }
    let idx = app.workflow_viewer.panel_item_index;
    let (graph, expanded_nodes) = match app.items.get(idx) {
        Some(crate::app::OutputItem::WorkflowPanel {
            graph,
            expanded_nodes,
            ..
        }) => (graph.clone(), expanded_nodes.clone()),
        _ => {
            let msg = Paragraph::new("workflow panel is no longer available")
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(msg, inner);
            return;
        }
    };
    let (lines, node_regions) = crate::output::render_workflow_panel_with_regions(
        &graph,
        &expanded_nodes,
        true,
        app.animation_frame,
        VIEWER_PANEL_WIDTH,
    );
    let content_rows = lines.len() as u16;
    let content_width = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum::<usize>() as u16
        })
        .max()
        .unwrap_or(0);
    app.workflow_viewer.last_inner_rect = Some(inner);
    app.workflow_viewer.last_visible_cols = inner.width;
    app.workflow_viewer.last_visible_rows = inner.height;
    app.workflow_viewer.last_content_rows = content_rows;
    app.workflow_viewer.last_content_width = content_width;
    app.workflow_viewer.last_node_regions = node_regions;
    let max_h = content_width.saturating_sub(inner.width);
    let max_v = content_rows.saturating_sub(inner.height);
    if app.workflow_viewer.h_offset > max_h {
        app.workflow_viewer.h_offset = max_h;
    }
    if app.workflow_viewer.v_offset > max_v {
        app.workflow_viewer.v_offset = max_v;
    }
    let para =
        Paragraph::new(lines).scroll((app.workflow_viewer.v_offset, app.workflow_viewer.h_offset));
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_and_close_flip_flag() {
        let mut m = WorkflowViewerModal::default();
        assert!(!m.open);
        m.open(3);
        assert!(m.open);
        assert_eq!(m.panel_item_index, 3);
        m.close();
        assert!(!m.open);
    }

    fn viewer(width: u16, visible: u16) -> WorkflowViewerModal {
        WorkflowViewerModal {
            last_content_width: width,
            last_visible_cols: visible,
            ..Default::default()
        }
    }

    #[test]
    fn scroll_right_clamps_at_content_edge() {
        let mut m = viewer(200, 100);
        m.scroll_right(50);
        assert_eq!(m.h_offset, 50);
        m.scroll_right(200);
        assert_eq!(m.h_offset, 100);
    }

    #[test]
    fn home_and_end_reset_offset() {
        let mut m = viewer(200, 100);
        m.scroll_right(50);
        m.home();
        assert_eq!(m.h_offset, 0);
        m.end();
        assert_eq!(m.h_offset, 100);
    }
}
