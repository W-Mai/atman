use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use atman_runtime::message::Message;
use atman_runtime::projection::workflow::WorkflowProjection;
use atman_runtime::workflow::WorkflowNodeKind;

use crate::app::OutputItem;
use crate::wm::PanelRenderCache;
use crate::wm::component::{
    EventCtx, HitRegion, HitTarget, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct FlowPanelContent {
    pub handle: String,
    pub scroll: u32,
    pub render_cache: Option<PanelRenderCache>,
    pub output_store: Option<atman_runtime::tools::tool_output::OutputStore>,
}

impl WindowComponent for FlowPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let mut hitmap_out: Vec<HitRegion> = Vec::new();

        let found = ctx
            .handle_index
            .get(&self.handle)
            .copied()
            .and_then(|index| ctx.items.get(index).map(|item| (index, item)))
            .filter(|(_, item)| matches!(item, OutputItem::SubAgentActivity { .. }));

        if let Some((
            item_idx,
            OutputItem::SubAgentActivity {
                goal,
                model,
                status,
                output,
                iteration,
                done,
                messages,
                workflow_graph,
                expanded_nodes,
                workflow_expanded,
                ..
            },
        )) = found
        {
            render_sub_agent_panel(
                frame,
                area,
                &self.handle,
                goal,
                model,
                status,
                output,
                *iteration,
                *done,
                messages,
                workflow_graph,
                expanded_nodes,
                *workflow_expanded,
                ctx.expanded_tools,
                &mut self.scroll,
                ctx.animation_frame,
                &mut hitmap_out,
                item_idx,
                ctx.item_revisions
                    .get(item_idx)
                    .copied()
                    .unwrap_or_default(),
                ctx.interaction_revision,
                &mut self.render_cache,
                self.output_store.as_ref(),
            );
        } else if let Some(panel_idx) = ctx
            .workflow_run_to_panel
            .get(&self.handle)
            .copied()
            .or_else(|| {
                ctx.items
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, it)| {
                        if let OutputItem::WorkflowPanel { graph, .. } = it {
                            graph.root.iter().any(|node| {
                                matches!(
                                    &node.kind,
                                    WorkflowNodeKind::Flow { run_id, .. } if run_id == &self.handle
                                )
                            })
                        } else {
                            false
                        }
                    })
                    .map(|(index, _)| index)
            })
            && let Some(OutputItem::WorkflowPanel {
                graph,
                expanded_nodes,
                ended_at,
                ..
            }) = ctx.items.get(panel_idx)
        {
            let revision = ctx
                .item_revisions
                .get(panel_idx)
                .copied()
                .unwrap_or_default();
            let cache_hit = self.render_cache.as_ref().is_some_and(|cache| {
                cache.item_id == revision.id
                    && cache.content_revision == revision.layout
                    && cache.interaction_revision == ctx.interaction_revision
                    && cache.width == area.width
                    && cache.workflow_expanded
            });
            if !cache_hit {
                let render_width = area.width.max(300);
                let render_frame = if ended_at.is_none() {
                    crate::output::LAYOUT_ANIMATION_FRAME
                } else {
                    ctx.animation_frame
                };
                let (lines, regions) = crate::output::render_workflow_projection_with_regions(
                    graph,
                    expanded_nodes,
                    true,
                    false,
                    render_frame,
                    render_width,
                    crate::output::MAX_COLLAPSED_BODY_ROWS,
                );
                let dynamic_paint = crate::output::workflow_dynamic_paint(graph, true, &lines, 0);
                self.render_cache = Some(PanelRenderCache {
                    item_id: revision.id,
                    content_revision: revision.layout,
                    interaction_revision: ctx.interaction_revision,
                    width: area.width,
                    workflow_expanded: true,
                    lines,
                    dynamic_paint,
                    regions,
                    tool_headers: Vec::new(),
                    wf_offset: 0,
                });
            }
            let cache = self.render_cache.as_ref().unwrap();
            let max_scroll = (cache.lines.len() as u32).saturating_sub(area.height as u32);
            self.scroll = self.scroll.min(max_scroll);
            let visible_end = self.scroll.saturating_add(area.height as u32);
            for r in cache
                .regions
                .iter()
                .skip_while(|region| region.end_row <= self.scroll)
                .take_while(|region| region.start_row < visible_end)
            {
                let row0 = area.y as u32 + r.start_row.saturating_sub(self.scroll);
                let row1 = area.y as u32 + r.end_row.min(visible_end).saturating_sub(self.scroll);
                let col0 = area.x + r.col_start;
                let col1 = area.x + r.col_end;
                if col1 > col0 && row1 > row0 {
                    hitmap_out.push(HitRegion {
                        target: HitTarget::WorkflowNode(panel_idx, r.path_key.clone()),
                        rect: Rect {
                            x: col0,
                            y: row0 as u16,
                            width: col1 - col0,
                            height: (row1 - row0) as u16,
                        },
                    });
                }
            }
            let start = self.scroll as usize;
            let end = start
                .saturating_add(area.height as usize)
                .min(cache.lines.len());
            let mut lines = cache.lines[start.min(end)..end].to_vec();
            crate::output::patch_animation_lines(
                &mut lines,
                &cache.dynamic_paint,
                ctx.animation_frame,
                start,
            );
            frame.render_widget(Paragraph::new(lines), area);
        } else if let Some(snap) = ctx
            .task_handle_index
            .get(&self.handle)
            .and_then(|&index| ctx.snapshots.get(index))
        {
            super::common::render_task_meta(
                frame,
                area,
                atman_runtime::TaskKind::Flow,
                snap,
                &mut self.scroll,
            );
        } else {
            super::common::render_placeholder(frame, area, &self.handle);
        }

        hitmap_out
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_sub_agent_panel(
    f: &mut Frame,
    area: Rect,
    handle: &str,
    goal: &str,
    model: &str,
    status: &str,
    _output: &str,
    iteration: u64,
    done: bool,
    messages: &[Message],
    workflow_graph: &WorkflowProjection,
    expanded_nodes: &HashSet<String>,
    workflow_expanded: bool,
    expanded_tools: &HashSet<String>,
    scroll: &mut u32,
    animation_frame: u32,
    hitmap_out: &mut Vec<HitRegion>,
    item_idx: usize,
    item_revision: crate::app::OutputRevision,
    interaction_revision: u64,
    render_cache: &mut Option<PanelRenderCache>,
    output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
) {
    if render_cache.as_ref().is_some_and(|cache| {
        cache.item_id == item_revision.id
            && cache.content_revision == item_revision.layout
            && cache.interaction_revision == interaction_revision
            && cache.width == area.width
            && cache.workflow_expanded == workflow_expanded
    }) {
        let cache = render_cache.as_ref().unwrap();
        let max_scroll = (cache.lines.len() as u32).saturating_sub(area.height as u32);
        *scroll = (*scroll).min(max_scroll);
        let visible_end = scroll.saturating_add(area.height as u32);
        for r in cache
            .regions
            .iter()
            .skip_while(|region| region.end_row + cache.wf_offset <= *scroll)
            .take_while(|region| region.start_row + cache.wf_offset < visible_end)
        {
            let row0 = area.y as u32 + (r.start_row + cache.wf_offset).saturating_sub(*scroll);
            let row1 = area.y as u32
                + (r.end_row + cache.wf_offset)
                    .min(visible_end)
                    .saturating_sub(*scroll);
            let col0 = area.x + r.col_start;
            let col1 = area.x + r.col_end;
            if col1 > col0 && row1 > row0 {
                hitmap_out.push(HitRegion {
                    target: HitTarget::WorkflowNode(item_idx, r.path_key.clone()),
                    rect: Rect {
                        x: col0,
                        y: row0 as u16,
                        width: col1 - col0,
                        height: (row1 - row0) as u16,
                    },
                });
            }
        }
        for r in cache
            .tool_headers
            .iter()
            .skip_while(|header| header.row + cache.wf_offset < *scroll)
            .take_while(|header| header.row + cache.wf_offset < visible_end)
        {
            let row = area.y as u32 + (r.row + cache.wf_offset).saturating_sub(*scroll);
            hitmap_out.push(HitRegion {
                target: HitTarget::ToolHeader(r.tool_id.clone()),
                rect: Rect {
                    x: area.x,
                    y: row as u16,
                    width: area.width,
                    height: 1,
                },
            });
        }
        let start = *scroll as usize;
        let end = start
            .saturating_add(area.height as usize)
            .min(cache.lines.len());
        let mut lines = cache.lines[start.min(end)..end].to_vec();
        crate::output::patch_animation_lines(
            &mut lines,
            &cache.dynamic_paint,
            animation_frame,
            start,
        );
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    let t = crate::theme::theme();
    let label_style = Style::default().fg(t.subtle_fg.into());
    let value_style = Style::default().fg(t.tinted_fg.into());
    let accent_style = Style::default().fg(t.accent.into());

    // Section 1: Header table
    let icon = match status {
        "ok" => "✓",
        "err" => "✗",
        "killed" => "⊘",
        "interrupted" => "⚠",
        _ if done => "✓",
        _ => "◐",
    };
    let iter_str = if done {
        String::new()
    } else {
        format!(" (iter {iteration})")
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::styled(
        format!(" {icon} {goal}{iter_str}"),
        accent_style,
    ));
    lines.push(Line::from(vec![
        Span::styled(" flow:   ", label_style),
        Span::styled(handle.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" model:  ", label_style),
        Span::styled(model.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" status: ", label_style),
        Span::styled(subagent_status_label(status, done), value_style),
    ]));
    lines.push(Line::from(""));

    // Section 2: Workflow graph — reuse the same renderer as the main transcript
    let wf_offset = lines.len() as u32;
    let render_frame = if done {
        animation_frame
    } else {
        crate::output::LAYOUT_ANIMATION_FRAME
    };
    let (wf_lines, regions) = crate::output::render_workflow_projection_with_regions(
        workflow_graph,
        expanded_nodes,
        workflow_expanded,
        status == "killed",
        render_frame,
        area.width.max(300),
        crate::output::MAX_COLLAPSED_BODY_ROWS,
    );
    lines.extend(wf_lines);
    lines.push(Line::from(""));

    // Section 3: Sub-document flow — use flatten_message for proper OutputItem
    // types (Bash/DiffPreview/Terminal with collapse support), identical to
    // the main transcript rendering.
    let mut tool_map: HashMap<String, crate::history::ToolDisplayMeta> = HashMap::new();
    for msg in messages {
        if matches!(msg.role, atman_runtime::message::MessageRole::Assistant) {
            for part in &msg.parts {
                if let atman_runtime::message::MessagePart::ToolUse {
                    id,
                    name,
                    input,
                    intent,
                } = part
                {
                    tool_map.insert(
                        id.clone(),
                        crate::history::ToolDisplayMeta::from_tool_use(
                            name,
                            input,
                            intent.as_ref(),
                        ),
                    );
                }
            }
        }
    }
    let mut items: Vec<OutputItem> = Vec::new();
    for msg in messages {
        crate::history::flatten_message_with_output_store(msg, &mut items, &tool_map, output_store);
    }
    crate::history::dedup_by_handle(&mut items);

    let render_ctx = crate::output::RenderCtx {
        expanded_tools,
        messages,
        animation_frame: render_frame,
        panel_width: area.width,
        hovered_thinking_idx: None,
        hovered_output_node: None,
    };
    let (doc_lines, tool_headers) =
        crate::output::build_lines_with_tool_headers(&items, &render_ctx);
    lines.extend(doc_lines);
    let dynamic_paint = if done {
        Default::default()
    } else {
        crate::output::workflow_dynamic_paint(
            workflow_graph,
            workflow_expanded,
            &lines,
            wf_offset as usize,
        )
    };

    let max_scroll = (lines.len() as u32).saturating_sub(area.height as u32);
    *scroll = (*scroll).min(max_scroll);

    let visible_end = scroll.saturating_add(area.height as u32);
    for r in regions
        .iter()
        .skip_while(|region| region.end_row + wf_offset <= *scroll)
        .take_while(|region| region.start_row + wf_offset < visible_end)
    {
        let row0 = area.y as u32 + (r.start_row + wf_offset).saturating_sub(*scroll);
        let row1 = area.y as u32
            + (r.end_row + wf_offset)
                .min(visible_end)
                .saturating_sub(*scroll);
        let col0 = area.x + r.col_start;
        let col1 = area.x + r.col_end;
        if col1 > col0 && row1 > row0 {
            hitmap_out.push(HitRegion {
                target: HitTarget::WorkflowNode(item_idx, r.path_key.clone()),
                rect: Rect {
                    x: col0,
                    y: row0 as u16,
                    width: col1 - col0,
                    height: (row1 - row0) as u16,
                },
            });
        }
    }
    for r in tool_headers
        .iter()
        .skip_while(|header| header.row + wf_offset < *scroll)
        .take_while(|header| header.row + wf_offset < visible_end)
    {
        let row = area.y as u32 + (r.row + wf_offset).saturating_sub(*scroll);
        hitmap_out.push(HitRegion {
            target: HitTarget::ToolHeader(r.tool_id.clone()),
            rect: Rect {
                x: area.x,
                y: row as u16,
                width: area.width,
                height: 1,
            },
        });
    }

    let start = *scroll as usize;
    let end = start.saturating_add(area.height as usize).min(lines.len());
    let mut visible_lines = lines[start.min(end)..end].to_vec();
    crate::output::patch_animation_lines(
        &mut visible_lines,
        &dynamic_paint,
        animation_frame,
        start,
    );
    *render_cache = Some(PanelRenderCache {
        item_id: item_revision.id,
        content_revision: item_revision.layout,
        interaction_revision,
        width: area.width,
        workflow_expanded,
        lines,
        dynamic_paint,
        regions,
        tool_headers,
        wf_offset,
    });

    f.render_widget(Paragraph::new(visible_lines), area);
}

fn subagent_status_label(status: &str, done: bool) -> String {
    match status {
        "running" => "running",
        "ok" => "completed",
        "err" => "failed",
        "killed" => "stopped",
        "interrupted" => "interrupted",
        _ if done => "completed",
        _ => status,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::Instant;

    use atman_runtime::projection::workflow::WorkflowProjection;
    use atman_runtime::workflow::{
        NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;

    use crate::app::OutputItem;
    use crate::wm::WindowComponent;

    use super::{FlowPanelContent, render_sub_agent_panel, subagent_status_label};

    #[test]
    fn subagent_status_uses_task_style_labels() {
        assert_eq!(subagent_status_label("ok", true), "completed");
        assert_eq!(subagent_status_label("err", true), "failed");
        assert_eq!(subagent_status_label("killed", true), "stopped");
    }

    #[test]
    fn animation_ticks_reuse_subagent_panel_projection() {
        let run_id = atman_runtime::event::FlowRunId::now();
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![WorkflowNode {
                id: run_id.to_string(),
                kind: WorkflowNodeKind::Flow {
                    run_id: run_id.to_string(),
                    flow_name: "child".into(),
                },
                label: "child".into(),
                status: NodeStatus::Running,
                started_at: Some(chrono::Utc::now()),
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: Parallelism::Serial,
                approval: None,
                llm_stats: None,
            }],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let graph = WorkflowProjection::from(graph);
        let expanded = HashSet::new();
        let mut scroll = 0;
        let mut hitmap = Vec::new();
        let mut cache = None;
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| {
                render_sub_agent_panel(
                    frame,
                    frame.area(),
                    "child",
                    "inspect",
                    "model",
                    "running",
                    "",
                    1,
                    false,
                    &[],
                    &graph,
                    &expanded,
                    true,
                    &expanded,
                    &mut scroll,
                    0,
                    &mut hitmap,
                    0,
                    crate::app::OutputRevision {
                        id: 1,
                        layout: 1,
                        ..Default::default()
                    },
                    1,
                    &mut cache,
                    None,
                );
            })
            .unwrap();
        let first_screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let cached_lines = cache.as_ref().unwrap().lines.as_ptr();
        assert!(cache.as_ref().unwrap().lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains('\u{e000}'))
        }));

        hitmap.clear();
        terminal
            .draw(|frame| {
                render_sub_agent_panel(
                    frame,
                    frame.area(),
                    "child",
                    "inspect",
                    "model",
                    "running",
                    "",
                    1,
                    false,
                    &[],
                    &graph,
                    &expanded,
                    true,
                    &expanded,
                    &mut scroll,
                    1,
                    &mut hitmap,
                    0,
                    crate::app::OutputRevision {
                        id: 1,
                        layout: 1,
                        ..Default::default()
                    },
                    1,
                    &mut cache,
                    None,
                );
            })
            .unwrap();
        let second_screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(cache.as_ref().unwrap().lines.as_ptr(), cached_lines);
        assert_ne!(first_screen, second_screen);
        assert!(!second_screen.contains('\u{e000}'));
    }

    #[test]
    fn animation_ticks_reuse_root_flow_panel_projection() {
        let run_id = "root-flow".to_string();
        let graph = WorkflowProjection::from(WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![WorkflowNode {
                id: run_id.clone(),
                kind: WorkflowNodeKind::Flow {
                    run_id: run_id.clone(),
                    flow_name: "root".into(),
                },
                label: "root".into(),
                status: NodeStatus::Running,
                started_at: Some(chrono::Utc::now()),
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: Parallelism::Serial,
                approval: None,
                llm_stats: None,
            }],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        });
        let items = vec![OutputItem::WorkflowPanel {
            turn_index: 0,
            graph,
            expanded_nodes: HashSet::new(),
            panel_expanded: true,
            started_at: Instant::now(),
            ended_at: None,
            cancelled: false,
        }];
        let revisions = vec![crate::app::OutputRevision {
            id: 7,
            layout: 9,
            ..Default::default()
        }];
        let workflow_index = HashMap::from([(run_id.clone(), 0)]);
        let empty_index = HashMap::new();
        let empty_set = HashSet::new();
        let resources = HashMap::new();
        let prompts = HashMap::new();
        let browser = crate::mcp_manager::McpBrowserState {
            tab: crate::mcp_manager::McpBrowserTab::default(),
            content_revision: 0,
            resources: &resources,
            prompts: &prompts,
        };
        let mut panel = FlowPanelContent {
            handle: run_id,
            scroll: 0,
            render_cache: None,
            output_store: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut cached_lines = None;
        for animation_frame in [0, 1] {
            terminal
                .draw(|frame| {
                    panel.render_content(
                        frame.area(),
                        frame,
                        &crate::wm::RenderCtx {
                            window_id: crate::wm::WindowId(1),
                            snapshots: &[],
                            items: &items,
                            item_revisions: &revisions,
                            handle_index: &empty_index,
                            task_handle_index: &empty_index,
                            workflow_run_to_panel: &workflow_index,
                            task_snapshots_revision: 0,
                            interaction_revision: 0,
                            animation_frame,
                            expanded_tools: &empty_set,
                            activity_nodes: &[],
                            mcp_servers: &[],
                            expanded_mcp_servers: &empty_set,
                            mcp_selected: 0,
                            hovered_mcp_row: &None,
                            mcp_browser: &browser,
                            hovered_history_row: &None,
                        },
                    );
                })
                .unwrap();
            let pointer = panel.render_cache.as_ref().unwrap().lines.as_ptr();
            if let Some(cached_lines) = cached_lines {
                assert_eq!(pointer, cached_lines);
                assert!(
                    panel
                        .render_cache
                        .as_ref()
                        .unwrap()
                        .lines
                        .iter()
                        .any(|line| {
                            line.spans
                                .iter()
                                .any(|span| span.content == "cache-sentinel")
                        })
                );
            } else {
                panel
                    .render_cache
                    .as_mut()
                    .unwrap()
                    .lines
                    .push(Line::from("cache-sentinel"));
            }
            cached_lines = Some(panel.render_cache.as_ref().unwrap().lines.as_ptr());
        }
    }
}
