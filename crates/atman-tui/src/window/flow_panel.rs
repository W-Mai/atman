use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use atman_runtime::message::Message;
use atman_runtime::workflow::{WorkflowGraph, WorkflowNodeKind};

use crate::app::OutputItem;
use crate::wm::PanelRenderCache;
use crate::wm::component::{
    EventCtx, HitRegion, HitTarget, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct FlowPanelContent {
    pub handle: String,
    pub scroll: u16,
    pub render_cache: Option<PanelRenderCache>,
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

        let mut found: Option<(usize, &OutputItem)> = None;
        for (i, item) in ctx.items.iter().enumerate().rev() {
            if let OutputItem::SubAgentActivity { handle, .. } = item {
                if handle == &self.handle {
                    found = Some((i, item));
                    break;
                }
            }
        }

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
                ctx.items_version,
                ctx.expanded_version,
                &mut self.render_cache,
            );
        } else if let Some((panel_idx, _)) = ctx.items.iter().enumerate().rev().find(|(_, it)| {
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
        }) && let Some(OutputItem::WorkflowPanel {
            graph,
            expanded_nodes,
            ended_at,
            ..
        }) = ctx.items.get(panel_idx)
        {
            let cache_hit = self.render_cache.as_ref().is_some_and(|cache| {
                cache.items_version == ctx.items_version
                    && cache.expanded_version == ctx.expanded_version
                    && cache.width == area.width
                    && cache.messages_len == 0
                    && cache.workflow_expanded
                    && cache.expanded_tools_len == 0
            });
            if !cache_hit {
                let render_width = area.width.max(300);
                let render_frame = if ended_at.is_none() {
                    crate::output::LAYOUT_ANIMATION_FRAME
                } else {
                    ctx.animation_frame
                };
                let (lines, regions) = crate::output::render_workflow_panel_with_regions(
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
                    items_version: ctx.items_version,
                    expanded_version: ctx.expanded_version,
                    width: area.width,
                    messages_len: 0,
                    workflow_expanded: true,
                    expanded_tools_len: 0,
                    lines,
                    dynamic_paint,
                    regions,
                    tool_headers: Vec::new(),
                    wf_offset: 0,
                });
            }
            let cache = self.render_cache.as_ref().unwrap();
            let max_scroll = (cache.lines.len() as u16).saturating_sub(area.height);
            self.scroll = self.scroll.min(max_scroll);
            for r in &cache.regions {
                let row0 = area.y as u32 + r.start_row.saturating_sub(self.scroll as u32);
                let row1 = area.y as u32 + r.end_row.saturating_sub(self.scroll as u32);
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
            .snapshots
            .iter()
            .find(|s| s.source_handle == self.handle)
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

    fn wants_background_updates(&self) -> bool {
        true
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
    workflow_graph: &WorkflowGraph,
    expanded_nodes: &HashSet<String>,
    workflow_expanded: bool,
    expanded_tools: &HashSet<String>,
    scroll: &mut u16,
    animation_frame: u32,
    hitmap_out: &mut Vec<HitRegion>,
    item_idx: usize,
    items_version: u64,
    expanded_version: u64,
    render_cache: &mut Option<PanelRenderCache>,
) {
    if render_cache.as_ref().is_some_and(|cache| {
        cache.items_version == items_version
            && cache.expanded_version == expanded_version
            && cache.width == area.width
            && cache.messages_len == messages.len()
            && cache.workflow_expanded == workflow_expanded
            && cache.expanded_tools_len == expanded_tools.len()
    }) {
        let cache = render_cache.as_ref().unwrap();
        let max_scroll = (cache.lines.len() as u16).saturating_sub(area.height);
        *scroll = (*scroll).min(max_scroll);
        for r in &cache.regions {
            let row0 =
                area.y as u32 + (r.start_row + cache.wf_offset).saturating_sub(*scroll as u32);
            let row1 = area.y as u32 + (r.end_row + cache.wf_offset).saturating_sub(*scroll as u32);
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
        for r in &cache.tool_headers {
            let row = area.y as u32 + (r.row + cache.wf_offset).saturating_sub(*scroll as u32);
            if row < area.y as u32 || row >= area.y as u32 + area.height as u32 {
                continue;
            }
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
    let (wf_lines, regions) = crate::output::render_workflow_panel_with_regions(
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
        crate::history::flatten_message(msg, &mut items, &tool_map);
    }
    crate::history::dedup_by_handle(&mut items);

    let render_ctx = crate::output::RenderCtx {
        expanded_tools,
        messages,
        animation_frame: render_frame,
        panel_width: area.width,
        hovered_thinking_idx: None,
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

    let max_scroll = (lines.len() as u16).saturating_sub(area.height);
    *scroll = (*scroll).min(max_scroll);

    for r in &regions {
        let row0 = area.y as u32 + (r.start_row + wf_offset).saturating_sub(*scroll as u32);
        let row1 = area.y as u32 + (r.end_row + wf_offset).saturating_sub(*scroll as u32);
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
    for r in &tool_headers {
        let row = area.y as u32 + (r.row + wf_offset).saturating_sub(*scroll as u32);
        if row < area.y as u32 {
            continue;
        }
        if row >= area.y as u32 + area.height as u32 {
            continue;
        }
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
        items_version,
        expanded_version,
        width: area.width,
        messages_len: messages.len(),
        workflow_expanded,
        expanded_tools_len: expanded_tools.len(),
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
    use std::collections::HashSet;

    use atman_runtime::workflow::{
        NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::{render_sub_agent_panel, subagent_status_label};

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
                    1,
                    1,
                    &mut cache,
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
                    1,
                    1,
                    &mut cache,
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
}
