use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use atman_runtime::message::Message;
use atman_runtime::workflow::WorkflowGraph;

use crate::app::OutputItem;
use crate::wm::component::{
    EventCtx, HitRegion, HitTarget, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};
use crate::wm::floating::PanelRenderCache;

pub struct FlowPanelContent {
    pub handle: String,
    pub scroll: u16,
    pub expanded_tools: std::collections::HashSet<String>,
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
    let cache_af = if done { None } else { Some(animation_frame) };

    if render_cache.as_ref().is_some_and(|cache| {
        cache.items_version == items_version
            && cache.expanded_version == expanded_version
            && cache.width == area.width
            && cache.animation_frame == cache_af
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
        f.render_widget(
            Paragraph::new(cache.lines.clone()).scroll((*scroll, 0)),
            area,
        );
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
        format!(" {icon} {handle}{iter_str}"),
        accent_style,
    ));
    lines.push(Line::from(vec![
        Span::styled(" goal:   ", label_style),
        Span::styled(goal.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" model:  ", label_style),
        Span::styled(model.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" status: ", label_style),
        Span::styled(status.to_string(), value_style),
    ]));
    lines.push(Line::from(""));

    // Section 2: Workflow graph — reuse the same renderer as the main transcript
    let wf_offset = lines.len() as u32;
    let (wf_lines, regions) = crate::output::render_workflow_panel_with_regions(
        workflow_graph,
        expanded_nodes,
        workflow_expanded,
        status == "killed",
        animation_frame,
        area.width.max(300),
        crate::output::MAX_COLLAPSED_BODY_ROWS,
    );
    lines.extend(wf_lines);
    lines.push(Line::from(""));

    // Section 3: Sub-document flow — use flatten_message for proper OutputItem
    // types (Bash/DiffPreview/Terminal with collapse support), identical to
    // the main transcript rendering.
    let mut tool_map: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if matches!(msg.role, atman_runtime::message::MessageRole::Assistant) {
            for part in &msg.parts {
                if let atman_runtime::message::MessagePart::ToolUse { id, name, .. } = part {
                    tool_map.insert(id.clone(), name.clone());
                }
            }
        }
    }
    let mut items: Vec<OutputItem> = Vec::new();
    for msg in messages {
        crate::history::flatten_message(msg, &mut items, &tool_map);
    }

    let render_ctx = crate::output::RenderCtx {
        expanded_tools,
        messages,
        animation_frame,
        panel_width: area.width,
        hovered_thinking_idx: None,
    };
    let doc_lines = crate::output::build_lines(&items, &render_ctx);
    lines.extend(doc_lines);

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

    *render_cache = Some(PanelRenderCache {
        items_version,
        expanded_version,
        width: area.width,
        animation_frame: cache_af,
        messages_len: messages.len(),
        workflow_expanded,
        expanded_tools_len: expanded_tools.len(),
        lines: lines.clone(),
        regions: regions.clone(),
        wf_offset,
    });

    f.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), area);
}
