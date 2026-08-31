use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::task_panel::{ActivityNode, ActivityStatus};
use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct ActivityPanelContent {
    pub run_id: String,
    pub scroll: u32,
}

impl WindowComponent for ActivityPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let parts: Vec<&str> = self.run_id.splitn(2, ':').collect();
        if parts.len() == 2 {
            let node = ctx
                .activity_nodes
                .iter()
                .find(|n| n.run_id == parts[0] && n.node_id == parts[1]);
            if let Some(node) = node {
                render_activity_content(frame, area, node);
                return Vec::new();
            }
        }
        super::common::render_placeholder(frame, area, &self.run_id);
        Vec::new()
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
            min: (40, 10),
            max: None,
            preferred: (70, 30),
        }
    }
}

fn render_activity_content(f: &mut Frame, area: Rect, node: &ActivityNode) {
    let t = crate::theme::theme();
    let (kind_icon, kind_color) = crate::task_panel::node_kind_glyph(&node.kind);
    let icon = match node.status {
        ActivityStatus::Running => "◐",
        ActivityStatus::Ok => "✓",
        ActivityStatus::Err => "✗",
        ActivityStatus::Cancelled => "⊘",
    };
    let color: Color = match node.status {
        ActivityStatus::Running => t.accent.into(),
        ActivityStatus::Ok => t.success.into(),
        ActivityStatus::Err => t.error.into(),
        ActivityStatus::Cancelled => t.subtle_fg.into(),
    };
    let elapsed = node
        .ended_at
        .map(|e| e.duration_since(node.started_at).as_millis() as u64)
        .unwrap_or_else(|| node.started_at.elapsed().as_millis() as u64);
    let elapsed_str = format_elapsed(elapsed);

    let header = Line::from(vec![
        Span::styled(format!(" {kind_icon} "), Style::default().fg(kind_color)),
        Span::styled(&node.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(icon.to_string(), Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(&elapsed_str, Style::default().fg(t.subtle_fg.into())),
    ]);
    let header_area = Rect { height: 1, ..area };
    f.render_widget(Paragraph::new(header), header_area);

    let body_area = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    if body_area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("status ", Style::default().fg(t.subtle_fg.into())),
        Span::styled(node.status.display_label(), Style::default().fg(color)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("run_id ", Style::default().fg(t.subtle_fg.into())),
        Span::styled(&node.run_id, Style::default().fg(t.tinted_fg.into())),
    ]));
    lines.push(Line::from(vec![
        Span::styled("node_id", Style::default().fg(t.subtle_fg.into())),
        Span::raw(" "),
        Span::styled(&node.node_id, Style::default().fg(t.tinted_fg.into())),
    ]));
    if let Some(parent_node_id) = &node.parent_node_id {
        lines.push(Line::from(vec![
            Span::styled("parent ", Style::default().fg(t.subtle_fg.into())),
            Span::styled(parent_node_id, Style::default().fg(t.tinted_fg.into())),
        ]));
    }
    f.render_widget(Paragraph::new(lines), body_area);
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
