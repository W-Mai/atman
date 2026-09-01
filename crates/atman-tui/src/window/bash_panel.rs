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
    pub scroll: u32,
    projection: BashPanelProjection,
}

impl BashPanelContent {
    pub fn new(handle: String) -> Self {
        Self {
            handle,
            scroll: 0,
            projection: BashPanelProjection::default(),
        }
    }
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
            .task_handle_index
            .get(&self.handle)
            .and_then(|&index| ctx.snapshots.get(index));
        let item_index = ctx.handle_index.get(&self.handle).copied();
        let item = item_index.and_then(|index| ctx.items.get(index));
        let source_generation = item_index
            .and_then(|index| ctx.item_revisions.get(index))
            .map_or(0, |revision| revision.source_generation);
        use crate::app::OutputItem;
        if let Some(OutputItem::Bash {
            title,
            command,
            output,
            done,
            ..
        }) = item
        {
            let source = BashPanelSource {
                command: command
                    .as_deref()
                    .or(snap.and_then(|snap| snap.command.as_deref())),
                output,
                generation: source_generation,
            };
            if let Some(snap) = snap {
                render_bash_content(
                    frame,
                    area,
                    snap,
                    source,
                    &mut self.scroll,
                    &mut self.projection,
                );
            } else {
                render_bash_screen(
                    frame,
                    area,
                    title.as_deref().unwrap_or(&self.handle),
                    source,
                    *done,
                    &mut self.scroll,
                    &mut self.projection,
                );
            }
        } else if let Some(snap) = snap {
            super::common::render_task_meta(
                frame,
                area,
                atman_runtime::TaskKind::Bash,
                snap,
                &mut self.scroll,
            );
        } else {
            render_bash_unavailable(frame, area, &self.handle);
        }
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
            min: (20, 6),
            max: None,
            preferred: (88, 29),
        }
    }
}

#[derive(Clone, Copy)]
struct BashPanelSource<'a> {
    command: Option<&'a str>,
    output: &'a str,
    generation: u64,
}

fn render_bash_content(
    f: &mut Frame,
    area: Rect,
    snap: &atman_runtime::TaskSnapshot,
    source: BashPanelSource<'_>,
    scroll: &mut u32,
    projection: &mut BashPanelProjection,
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
            format!(
                "{} · {}",
                snap.status.display_label(),
                super::common::format_elapsed(snap.elapsed_ms())
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
        return;
    }

    let lines = projection.visible_lines(
        source.command,
        source.output,
        source.generation,
        body_area.width as usize,
        body_area.height as usize,
        scroll,
    );
    f.render_widget(Paragraph::new(lines), body_area);
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

fn render_bash_screen(
    f: &mut Frame,
    area: Rect,
    title: &str,
    source: BashPanelSource<'_>,
    done: bool,
    scroll: &mut u32,
    projection: &mut BashPanelProjection,
) {
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

    let lines = projection.visible_lines(
        source.command,
        source.output,
        source.generation,
        body_area.width as usize,
        body_area.height as usize,
        scroll,
    );
    f.render_widget(Paragraph::new(lines), body_area);
}

#[derive(Default)]
struct BashPanelProjection {
    index: crate::wrapped_text::WrappedRowIndex,
    command: Option<String>,
    width: usize,
    theme: Option<crate::theme::ThemeMode>,
    fixed_lines: Vec<Line<'static>>,
    prepared_start: usize,
    prepared_end: usize,
    prepared_lines: Vec<Line<'static>>,
    #[cfg(test)]
    indexed_source_bytes: usize,
    #[cfg(test)]
    materialized_output_rows: usize,
}

impl BashPanelProjection {
    const OUTPUT_PREFIX: &'static str = " ";
    const RIGHT_PAD: usize = 2;

    fn visible_lines(
        &mut self,
        command: Option<&str>,
        output: &str,
        source_generation: u64,
        width: usize,
        viewport_rows: usize,
        scroll: &mut u32,
    ) -> Vec<Line<'static>> {
        let width = width.max(1);
        let body_width = width
            .saturating_sub(crate::width::width(Self::OUTPUT_PREFIX))
            .saturating_sub(Self::RIGHT_PAD)
            .max(1);
        let update = self.index.update(
            output,
            source_generation,
            body_width,
            crate::wrapped_text::WrappedLineMode::SplitNewline,
        );
        #[cfg(test)]
        {
            self.indexed_source_bytes = self
                .indexed_source_bytes
                .saturating_add(update.indexed_bytes);
        }
        let theme = crate::theme::current_mode();
        let fixed_changed =
            self.width != width || self.command.as_deref() != command || self.theme != Some(theme);
        if fixed_changed {
            self.width = width;
            self.theme = Some(theme);
            self.command = command.map(str::to_owned);
            self.fixed_lines.clear();
            if let Some(command) = command.filter(|command| !command.is_empty()) {
                self.fixed_lines
                    .extend(super::common::detail_command_lines(command, width));
                self.fixed_lines.push(Line::from(""));
            }
            self.fixed_lines
                .push(super::common::detail_section_label("output", width));
        }
        if fixed_changed || update.rebuilt || update.indexed_bytes > 0 {
            self.prepared_start = 0;
            self.prepared_end = 0;
            self.prepared_lines.clear();
        }

        let total_rows = self.fixed_lines.len().saturating_add(self.index.len());
        let max_scroll = total_rows
            .saturating_sub(viewport_rows)
            .min(u32::MAX as usize) as u32;
        *scroll = (*scroll).min(max_scroll);
        let start = *scroll as usize;
        let end = start.saturating_add(viewport_rows).min(total_rows);
        if start < self.prepared_start || end > self.prepared_end {
            let t = crate::theme::theme();
            let body_style = Style::default().fg(t.tinted_fg.into()).bg(t.code_bg.into());
            let output_offset = self.fixed_lines.len();
            let mut lines = Vec::with_capacity(end.saturating_sub(start));
            for row in start..end {
                if row < output_offset {
                    lines.push(self.fixed_lines[row].clone());
                } else {
                    let body = self
                        .index
                        .row(output, row.saturating_sub(output_offset))
                        .unwrap_or_default();
                    lines.push(crate::output::line_with_right_pad(
                        Self::OUTPUT_PREFIX,
                        body,
                        width,
                        body_style,
                        body_style,
                    ));
                    #[cfg(test)]
                    {
                        self.materialized_output_rows =
                            self.materialized_output_rows.saturating_add(1);
                    }
                }
            }
            self.prepared_start = start;
            self.prepared_end = end;
            self.prepared_lines = lines;
        }
        let local_start = start.saturating_sub(self.prepared_start);
        let local_end = end.saturating_sub(self.prepared_start);
        self.prepared_lines[local_start..local_end].to_vec()
    }
}

#[cfg(test)]
fn bash_detail_lines(command: Option<&str>, output: &str, width: usize) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let body_style = Style::default().fg(t.tinted_fg.into()).bg(t.code_bg.into());
    let mut lines = Vec::new();
    if let Some(command) = command.filter(|command| !command.is_empty()) {
        lines.extend(super::common::detail_command_lines(command, width));
        lines.push(Line::from(""));
    }
    lines.push(super::common::detail_section_label("output", width));
    for row in crate::output::wrap_with_prefix(output, width, " ", " ") {
        lines.push(crate::output::line_with_right_pad(
            &row.prefix,
            &row.body,
            width,
            body_style,
            body_style,
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::task_registry::{TaskSnapshot, TaskStatus};
    use std::collections::HashSet;

    fn snapshot(src: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: atman_runtime::TaskId::now(),
            kind: atman_runtime::TaskKind::Bash,
            label: format!("bash {src}"),
            command: Some("cargo test --workspace".into()),
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
            title: None,
            command: Some("cargo test --workspace".into()),
            output: output.to_string(),
            done,
            expanded: false,
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
        let mut panel = BashPanelContent::new("bg_s_1".into());
        let empty_tools: HashSet<String> = HashSet::new();
        let empty_mcp: HashSet<String> = HashSet::new();
        let empty_resources = std::collections::HashMap::new();
        let empty_prompts = std::collections::HashMap::new();
        let browser = crate::mcp_manager::McpBrowserState {
            tab: crate::mcp_manager::McpBrowserTab::Resources,
            content_revision: 0,
            resources: &empty_resources,
            prompts: &empty_prompts,
        };
        let handle_index = std::collections::HashMap::from([("bg_s_1".to_string(), 0)]);
        let task_handle_index = std::collections::HashMap::from([("bg_s_1".to_string(), 0)]);
        let ctx = RenderCtx {
            window_id: crate::wm::WindowId(1),
            snapshots: &snaps,
            items: &items,
            item_revisions: &[],
            handle_index: &handle_index,
            task_handle_index: &task_handle_index,
            workflow_run_to_panel: &handle_index,
            task_snapshots_revision: 0,
            interaction_revision: 0,
            animation_frame: 0,
            expanded_tools: &empty_tools,
            activity_nodes: &[],
            mcp_servers: &[],
            expanded_mcp_servers: &empty_mcp,
            mcp_selected: 0,
            hovered_mcp_row: &None,
            mcp_browser: &browser,
            hovered_history_row: &None,
        };
        let lines = render(&mut panel, &ctx);
        let joined = lines.join("\n");
        assert!(
            joined.contains("hello") && joined.contains("world"),
            "panel should show bash output, got:\n{joined}"
        );
        assert!(
            joined.contains("cargo test --workspace"),
            "panel should show the raw command, got:\n{joined}"
        );
    }

    #[test]
    fn projection_matches_direct_top_middle_and_tail_slices() {
        let command = "printf 'first'\nprintf 'second'";
        let output = (0..40)
            .map(|index| format!("line {index:02} 你好 {}", "x".repeat(index % 7)))
            .collect::<Vec<_>>()
            .join("\n");
        let width = 30;
        let viewport = 7;
        let direct = bash_detail_lines(Some(command), &output, width);
        let max_scroll = direct.len().saturating_sub(viewport);
        let mut projection = BashPanelProjection::default();

        for requested in [0, max_scroll / 2, max_scroll] {
            let mut scroll = requested as u32;
            let projected =
                projection.visible_lines(Some(command), &output, 1, width, viewport, &mut scroll);
            assert_eq!(scroll as usize, requested);
            assert_eq!(projected, direct[requested..requested + viewport]);
        }
    }

    #[test]
    fn unchanged_projection_does_not_reindex_or_rematerialize_source() {
        let output = (0..100)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        let mut projection = BashPanelProjection::default();
        let mut scroll = 20;
        let first = projection.visible_lines(None, &output, 1, 40, 8, &mut scroll);
        let indexed = projection.indexed_source_bytes;
        let materialized = projection.materialized_output_rows;
        let second = projection.visible_lines(None, &output, 1, 40, 8, &mut scroll);

        assert_eq!(second, first);
        assert_eq!(projection.indexed_source_bytes, indexed);
        assert_eq!(projection.materialized_output_rows, materialized);
    }

    #[test]
    fn line_appends_and_scroll_materialize_only_new_and_visible_rows() {
        let mut output = "first\n".to_string();
        let mut projection = BashPanelProjection::default();
        let mut scroll = 0;
        projection.visible_lines(None, &output, 4, 40, 6, &mut scroll);
        let indexed = projection.indexed_source_bytes;
        let materialized = projection.materialized_output_rows;

        output.push_str("second\nthird\n");
        scroll = u32::MAX;
        projection.visible_lines(None, &output, 4, 40, 6, &mut scroll);

        assert_eq!(
            projection.indexed_source_bytes.saturating_sub(indexed),
            "second\nthird\n".len()
        );
        assert!(
            projection
                .materialized_output_rows
                .saturating_sub(materialized)
                <= 6
        );
    }
}
