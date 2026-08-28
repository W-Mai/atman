use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct BashPanelContent {
    pub handle: String,
    pub scroll: u16,
}

impl WindowComponent for BashPanelContent {
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
            crate::app::OutputItem::Bash { handle, .. } => *handle == self.handle,
            _ => false,
        });
        use crate::app::OutputItem;
        if let Some(OutputItem::Bash { output, done, .. }) = item {
            if let Some(snap) = snap {
                render_bash_content(frame, area, snap, output, *done);
            } else {
                render_bash_screen(frame, area, &self.handle, output, *done);
            }
        } else if let Some(snap) = snap {
            super::common::render_task_meta(frame, area, atman_runtime::TaskKind::Bash, snap);
        } else {
            render_bash_unavailable(frame, area, &self.handle);
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

fn render_bash_content(
    f: &mut Frame,
    area: Rect,
    snap: &atman_runtime::TaskSnapshot,
    output: &str,
    done: bool,
) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", super::common::status_icon(snap.status)),
            Style::default().fg(super::common::status_color(snap.status)),
        ),
        Span::styled(&snap.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            if done {
                "done".into()
            } else {
                super::common::format_elapsed(snap.elapsed_ms())
            },
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
        return;
    }

    let all_lines: Vec<&str> = output.lines().collect();
    let max_visible = body_area.height as usize;
    let start = all_lines.len().saturating_sub(max_visible);
    let visible: Vec<Line> = all_lines[start..]
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                *l,
                Style::default().fg(t.tinted_fg.into()).bg(t.code_bg.into()),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(visible), body_area);
}

fn render_bash_unavailable(f: &mut Frame, area: Rect, title: &str) {
    let t = crate::theme::theme();
    let mut lines = vec![Line::from(""); area.height as usize / 2];
    lines.push(Line::from(vec![
        Span::styled(" ◐ ", Style::default().fg(t.subtle_fg.into())),
        Span::styled(
            "bash output unavailable",
            Style::default().fg(t.subtle_fg.into()),
        ),
    ]));
    lines.push(Line::from(vec![Span::styled(
        format!("task `{title}`: session log could not be loaded"),
        Style::default().fg(t.meta_fg.into()),
    )]));
    f.render_widget(Paragraph::new(lines), area);
}

fn render_bash_screen(f: &mut Frame, area: Rect, title: &str, output: &str, done: bool) {
    let t = crate::theme::theme();
    let icon = if done { "✓" } else { "◐" };
    let icon_color = if done { t.success } else { t.accent };
    let header = Line::from(vec![
        Span::styled(format!(" {icon} "), Style::default().fg(icon_color.into())),
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
        return;
    }

    let all_lines: Vec<&str> = output.lines().collect();
    let max_visible = body_area.height as usize;
    let start = all_lines.len().saturating_sub(max_visible);
    let visible: Vec<Line> = all_lines[start..]
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                *l,
                Style::default().fg(t.tinted_fg.into()).bg(t.code_bg.into()),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(visible), body_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::OutputItem;
    use atman_runtime::task_registry::{TaskSnapshot, TaskStatus};
    use std::collections::HashSet;

    fn snapshot(src: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: atman_runtime::TaskId::now(),
            kind: atman_runtime::TaskKind::Bash,
            label: format!("bash {src}"),
            status: TaskStatus::Ok,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
            source_handle: src.to_string(),
            session_id: "s".to_string(),
            workspace_id: None,
            flow_run_id: None,
            termination: None,
        }
    }

    fn bash_item(src: &str, output: &str, done: bool) -> crate::app::OutputItem {
        crate::app::OutputItem::Bash {
            handle: src.to_string(),
            output: output.to_string(),
            done,
            expanded: false,
        }
    }

    fn ctx<'a>(
        items: &'a [OutputItem],
        snaps: &'a [TaskSnapshot],
        hovered: &'a Option<String>,
        empty_tools: &'a HashSet<String>,
        empty_mcp: &'a HashSet<String>,
        browser: &'a crate::mcp_manager::McpBrowserState<'a>,
    ) -> RenderCtx<'a> {
        RenderCtx {
            window_id: crate::wm::WindowId(1),
            snapshots: snaps,
            items,
            animation_frame: 0,
            expanded_tools: empty_tools,
            activity_nodes: &[],
            items_version: 0,
            expanded_version: 0,
            mcp_servers: &[],
            expanded_mcp_servers: empty_mcp,
            mcp_selected: 0,
            hovered_mcp_row: &None,
            mcp_browser: browser,
            hovered_history_row: hovered,
        }
    }

    fn render(panel: &mut BashPanelContent, ctx: &RenderCtx<'_>) -> Vec<String> {
        use ratatui::backend::TestBackend;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(50, 10)).unwrap();
        terminal
            .draw(|f| {
                panel.render_content(f.area(), f, ctx);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(50)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn renders_output_when_item_and_snapshot_present() {
        let items = [bash_item("bg_s_1", "hello\nworld", true)];
        let snaps = [snapshot("bg_s_1")];
        let mut panel = BashPanelContent {
            handle: "bg_s_1".into(),
            scroll: 0,
        };
        let empty_tools: HashSet<String> = HashSet::new();
        let empty_mcp: HashSet<String> = HashSet::new();
        let empty_resources = std::collections::HashMap::new();
        let empty_prompts = std::collections::HashMap::new();
        let browser = crate::mcp_manager::McpBrowserState {
            tab: crate::mcp_manager::McpBrowserTab::Resources,
            resources: &empty_resources,
            prompts: &empty_prompts,
        };
        let lines = render(
            &mut panel,
            &ctx(&items, &snaps, &None, &empty_tools, &empty_mcp, &browser),
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("hello") && joined.contains("world"),
            "panel should show bash output, got:\n{joined}"
        );
    }
}
