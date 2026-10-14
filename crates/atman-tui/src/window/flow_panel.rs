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

/// Message restoration is independent of viewport and disclosure changes.
#[derive(Default)]
pub(crate) struct FlowDocument {
    revision: Option<(u64, u64)>,
    items: Vec<OutputItem>,
}

#[derive(Default)]
pub(crate) struct FlowInteraction {
    hits: Vec<HitRegion>,
    focused: Option<usize>,
    width: u16,
    expanded_nodes: Option<HashSet<String>>,
    workflow_expanded: Option<bool>,
    expanded_groups: HashSet<String>,
}

fn panel_hit_regions(
    cache: &PanelRenderCache,
    area: Rect,
    scroll: u32,
    item_idx: usize,
) -> Vec<HitRegion> {
    let end = scroll.saturating_add(area.height as u32);
    cache
        .regions
        .iter()
        .map(|region| (region, false))
        .chain(cache.document_regions.iter().map(|region| (region, true)))
        .filter_map(|(region, document)| {
            let start_row = region.start_row.max(scroll);
            let end_row = region.end_row.min(end);
            let col_end = region.col_end.min(area.width);
            if start_row >= end_row || region.col_start >= col_end {
                return None;
            }
            Some(HitRegion {
                target: if document {
                    HitTarget::DocumentNode(region.panel_item_index, region.path_key.clone())
                } else {
                    HitTarget::WorkflowNode(item_idx, region.path_key.clone())
                },
                rect: Rect::new(
                    area.x + region.col_start,
                    area.y + (start_row - scroll) as u16,
                    col_end - region.col_start,
                    (end_row - start_row) as u16,
                ),
            })
        })
        .collect()
}

impl FlowDocument {
    fn restore(
        &mut self,
        revision: crate::app::OutputRevision,
        messages: &[Message],
        output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
    ) {
        let key = (revision.id, revision.semantic);
        if self.revision == Some(key) {
            return;
        }
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
            crate::history::flatten_message_with_output_store(
                msg,
                &mut items,
                &tool_map,
                output_store,
                None,
            );
        }
        crate::history::dedup_by_handle(&mut items);

        let disclosures: HashMap<_, _> = self
            .items
            .iter()
            .filter_map(|item| {
                if let OutputItem::ToolDispatch { calls } = item {
                    Some(calls)
                } else {
                    None
                }
            })
            .flatten()
            .map(|call| (call.id.as_str(), call.disclosure))
            .collect();
        for item in &mut items {
            if let OutputItem::ToolDispatch { calls } = item {
                for call in calls {
                    if let Some(disclosure) = disclosures.get(call.id.as_str()) {
                        call.disclosure = *disclosure;
                    }
                }
            }
        }
        self.items = items;
        self.revision = Some(key);
    }
}

pub struct FlowPanelContent {
    pub(crate) document: FlowDocument,
    pub(crate) interaction: FlowInteraction,
    pub(crate) goal_entry: Option<std::sync::Arc<atman_runtime::tools::agent_ctrl::FlowEntry>>,
    pub(crate) goal_rect: Rect,
    pub(crate) goal_hover: bool,
    pub handle: String,
    pub scroll: u32,
    pub render_cache: Option<PanelRenderCache>,
    pub output_store: Option<atman_runtime::tools::tool_output::OutputStore>,
    pub(crate) selection_projection: Option<crate::selection::VisibleSelectionProjection>,
}

impl FlowPanelContent {
    fn activate(&mut self, target: HitTarget) -> Vec<crate::wm::WmCommand> {
        use crate::output::*;
        self.render_cache = None;
        match target {
            HitTarget::WorkflowNode(_, path) => {
                if path.is_empty() {
                    let expanded = self.interaction.workflow_expanded.get_or_insert(true);
                    *expanded = !*expanded;
                } else {
                    let nodes = self.interaction.expanded_nodes.get_or_insert_default();
                    if !nodes.remove(&path) {
                        nodes.insert(path);
                    }
                }
            }
            HitTarget::DocumentNode(index, path) => {
                if path.starts_with(WORKING_GROUP_REGION_PREFIX) {
                    if !self.interaction.expanded_groups.remove(&path) {
                        self.interaction.expanded_groups.insert(path);
                    }
                } else if let Some(OutputItem::ToolDispatch { calls }) =
                    self.document.items.get_mut(index)
                {
                    let content = path.strip_prefix(TOOL_CALL_REGION_PREFIX);
                    let detail = path.strip_prefix(TOOL_DETAIL_REGION_PREFIX);
                    let fullscreen = path
                        .strip_prefix(TOOL_FULLSCREEN_REGION_PREFIX)
                        .or_else(|| path.strip_prefix(TOOL_DETAIL_FULLSCREEN_REGION_PREFIX));
                    if let Some(call) = calls
                        .iter_mut()
                        .find(|call| Some(call.id.as_str()) == content.or(detail).or(fullscreen))
                    {
                        if fullscreen.is_some() {
                            if let Some(output) = call.detail.as_deref() {
                                if let Some(handle) = output.handle() {
                                    return vec![crate::wm::WmCommand::OpenTaskPanel {
                                        handle: handle.to_owned(),
                                        maximized: true,
                                    }];
                                }
                                return vec![crate::wm::WmCommand::OpenOutputSnapshot {
                                    id: format!("{}:{}", self.handle, call.id),
                                    item: Box::new(output.clone()),
                                }];
                            }
                        } else if content.is_some() {
                            call.disclosure = toggle_tool_call_content_disclosure(call);
                        } else {
                            call.disclosure =
                                toggle_tool_call_detail_disclosure(call, self.interaction.width);
                        }
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

impl WindowComponent for FlowPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let mut area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        self.goal_rect = Rect::default();
        if let Some(entry) = &self.goal_entry
            && let Some(presentation) = crate::agent_presentation::GoalPresentation::for_flow(entry)
            && area.height >= 6
        {
            self.goal_rect = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 3);
            crate::settings_form::render_action(
                frame,
                self.goal_rect,
                &crate::settings_form::Action {
                    label: &if presentation.action.is_some() {
                        format!("{} · {}", presentation.status, presentation.action_label)
                    } else {
                        presentation.status.clone()
                    },
                    shortcut: if presentation.action.is_some() {
                        "Ctrl+Y"
                    } else {
                        ""
                    },
                    focused: false,
                    hovered: self.goal_hover,
                    disabled: presentation.action.is_none(),
                },
                &crate::theme::theme(),
            );
            area.y += 4;
            area.height -= 4;
        }
        self.selection_projection = None;
        let mut hitmap_out: Vec<HitRegion> = Vec::new();

        let found = ctx
            .task_detail(&self.handle)
            .filter(|(_, item, _)| matches!(item, OutputItem::SubAgentActivity { .. }));

        if let Some((
            item_idx,
            OutputItem::SubAgentActivity {
                goal,
                model,
                status,
                output,
                iteration,
                messages,
                workflow_graph,
                expanded_nodes,
                workflow_expanded,
                ..
            },
            revision,
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
                messages,
                workflow_graph,
                self.interaction
                    .expanded_nodes
                    .get_or_insert_with(|| expanded_nodes.clone()),
                *self
                    .interaction
                    .workflow_expanded
                    .get_or_insert(*workflow_expanded),
                &self.interaction.expanded_groups,
                &mut self.scroll,
                ctx.animation_frame,
                &mut hitmap_out,
                item_idx,
                revision,
                ctx.interaction_revision,
                &mut self.render_cache,
                &mut self.document,
                self.output_store.as_ref(),
            );
            if let Some(cache) = self.render_cache.as_ref() {
                self.selection_projection = Some(crate::window::common::window_text_projection(
                    ctx.window_id,
                    "working-output",
                    revision,
                    &cache.lines,
                    area,
                    self.scroll,
                ));
            }
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
            let workflow_expanded = *self.interaction.workflow_expanded.get_or_insert(true);
            let cache_hit = self.render_cache.as_ref().is_some_and(|cache| {
                cache.item_id == revision.id
                    && cache.content_revision == revision.layout
                    && cache.interaction_revision == ctx.interaction_revision
                    && cache.width == area.width
                    && cache.workflow_expanded == workflow_expanded
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
                    self.interaction
                        .expanded_nodes
                        .get_or_insert_with(|| expanded_nodes.clone()),
                    workflow_expanded,
                    false,
                    render_frame,
                    render_width,
                    crate::output::MAX_COLLAPSED_BODY_ROWS,
                );
                let dynamic_paint =
                    crate::output::workflow_dynamic_paint(graph, workflow_expanded, &lines, 0);
                self.render_cache = Some(PanelRenderCache {
                    item_id: revision.id,
                    content_revision: revision.layout,
                    interaction_revision: ctx.interaction_revision,
                    width: area.width,
                    workflow_expanded,
                    lines,
                    dynamic_paint,
                    regions,
                    document_regions: Vec::new(),
                });
            }
            let cache = self.render_cache.as_ref().unwrap();
            let max_scroll = (cache.lines.len() as u32).saturating_sub(area.height as u32);
            self.scroll = self.scroll.min(max_scroll);
            hitmap_out.extend(panel_hit_regions(cache, area, self.scroll, panel_idx));
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
            self.selection_projection = Some(crate::window::common::window_text_projection(
                ctx.window_id,
                "workflow-output",
                revision,
                &cache.lines,
                area,
                self.scroll,
            ));
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

        self.interaction.hits = hitmap_out;
        self.interaction.width = area.width;
        if let Some(hit) = self
            .interaction
            .focused
            .and_then(|index| self.interaction.hits.get(index))
        {
            frame.buffer_mut()[(hit.rect.x, hit.rect.y)]
                .set_symbol("▎")
                .set_fg(crate::theme::theme().accent.into());
        }
        Vec::new()
    }

    fn selection_projection(&self) -> Option<crate::selection::VisibleSelectionProjection> {
        self.selection_projection.clone()
    }

    fn handle_event(&mut self, event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        let was_hovered = self.goal_hover;
        let activate = match event {
            WmEvent::Key(crate::keys::KeyAction::ToggleGoal) => true,
            WmEvent::Mouse(mouse) => {
                self.goal_hover = self.goal_rect.contains((mouse.column, mouse.row).into());
                self.goal_hover
                    && mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
            }
            _ => false,
        };
        if activate
            && let Some(entry) = &self.goal_entry
            && let Some(control) = entry.goal_control.get()
        {
            let commands = crate::agent_presentation::GoalPresentation::for_flow(entry)
                .and_then(|p| p.action)
                .map(|action| {
                    super::goal_panel::command(control, Some(self.handle.clone()), action)
                })
                .into_iter()
                .collect();
            return WmEventResult::Consumed(commands);
        }
        if was_hovered != self.goal_hover
            && matches!(event, WmEvent::Mouse(mouse) if mouse.kind == crossterm::event::MouseEventKind::Moved)
        {
            return WmEventResult::Consumed(Vec::new());
        }
        let focus = match event {
            WmEvent::Mouse(mouse)
                if mouse.kind
                    == crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ) =>
            {
                self.interaction
                    .hits
                    .iter()
                    .rposition(|hit| hit.rect.contains((mouse.column, mouse.row).into()))
            }
            WmEvent::Key(crate::keys::KeyAction::Submit | crate::keys::KeyAction::Char(' ')) => {
                self.interaction.focused
            }
            WmEvent::Key(key @ (crate::keys::KeyAction::Tab | crate::keys::KeyAction::BackTab))
                if !self.interaction.hits.is_empty() =>
            {
                let count = self.interaction.hits.len();
                self.interaction.focused = Some(match (self.interaction.focused, key) {
                    (Some(index), crate::keys::KeyAction::Tab) => (index + 1) % count,
                    (Some(index), _) => (index + count - 1) % count,
                    (None, crate::keys::KeyAction::Tab) => 0,
                    _ => count - 1,
                });
                return WmEventResult::Consumed(Vec::new());
            }
            _ => None,
        };
        if let Some(index) = focus
            && let Some(hit) = self.interaction.hits.get(index)
        {
            let target = hit.target.clone();
            self.interaction.focused = Some(index);
            return WmEventResult::Consumed(self.activate(target));
        }
        WmEventResult::Ignored
    }

    fn title_suffix(&self) -> Option<String> {
        Some("Tab: focus · Enter: toggle".into())
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
    status: &crate::agent_presentation::AgentState,
    _output: &str,
    iteration: u64,
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
    document: &mut FlowDocument,
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
        hitmap_out.extend(panel_hit_regions(cache, area, *scroll, item_idx));
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
    let icon = status.glyph();
    let done = !status.is_running();
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
        Span::styled(status.label(), value_style),
    ]));
    lines.push(Line::from(""));

    // Section 2: Workflow graph — reuse the same renderer as the main transcript
    let wf_offset = lines.len() as u32;
    let render_frame = if done {
        animation_frame
    } else {
        crate::output::LAYOUT_ANIMATION_FRAME
    };
    let (wf_lines, mut regions) = crate::output::render_workflow_projection_with_regions(
        workflow_graph,
        expanded_nodes,
        workflow_expanded,
        *status == crate::agent_presentation::AgentState::Stopped,
        render_frame,
        area.width.max(300),
        crate::output::MAX_COLLAPSED_BODY_ROWS,
    );
    for region in &mut regions {
        region.start_row += wf_offset;
        region.end_row += wf_offset;
    }
    lines.extend(wf_lines);
    lines.push(Line::from(""));

    // Section 3: Sub-document flow — use flatten_message for proper OutputItem
    // types (Bash/DiffPreview/Terminal with collapse support), identical to
    // the main transcript rendering.
    document.restore(item_revision, messages, output_store);

    let render_ctx = crate::output::RenderCtx {
        expanded_tools,
        messages,
        animation_frame: render_frame,
        panel_width: area.width,
        hovered_thinking_idx: None,
        hovered_output_node: None,
    };
    let (doc_lines, _, mut document_regions, _) =
        crate::output::build_lines_with_ranges(&document.items, area.width, &render_ctx);
    let doc_offset = lines.len() as u32;
    for region in &mut document_regions {
        region.start_row += doc_offset;
        region.end_row += doc_offset;
    }
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
        document_regions,
    });

    hitmap_out.extend(panel_hit_regions(
        render_cache.as_ref().unwrap(),
        area,
        *scroll,
        item_idx,
    ));
    f.render_widget(Paragraph::new(visible_lines), area);
}

#[cfg(test)]
mod tests {
    use crate::wm::{EventCtx, WmEvent, WmEventResult};
    use std::collections::{HashMap, HashSet};
    use std::time::Instant;

    use atman_runtime::projection::workflow::WorkflowProjection;
    use atman_runtime::workflow::{
        NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;

    #[test]
    fn detached_child_disclosure_uses_rendered_regions_and_survives_updates() {
        use crate::keys::KeyAction;
        use crate::wm::component::HitTarget;
        use atman_runtime::message::{Message, MessagePart};
        let turn = atman_rt::TurnId::<atman_runtime::event::AtmanUuid>::now();
        let mut call = Message::assistant_text(turn.clone(), "");
        call.parts = vec![MessagePart::ToolUse {
            id: "call".into(),
            name: "bash.spawn".into(),
            input: serde_json::json!({"command":"echo output"}),
            intent: None,
        }];
        let mut result = call.clone();
        result.role = atman_runtime::message::MessageRole::Tool;
        result.parts = vec![MessagePart::ToolResult {
            tool_use_id: "call".into(),
            is_error: false,
            content: serde_json::json!({"handle":"process", "output": "line\n".repeat(40)})
                .to_string(),
        }];
        let mut tasks = HashMap::from([(
            "child".into(),
            crate::app::DetachedTaskDetail {
                revision: 1,
                item: OutputItem::SubAgentActivity {
                    handle: "child".into(),
                    goal: "inspect".into(),
                    child_run_id: "child".into(),
                    model: "model".into(),
                    status: crate::agent_presentation::AgentState::Completed,
                    output: String::new(),
                    iteration: 1,
                    expanded: false,
                    messages: vec![call, result],
                    workflow_graph: WorkflowProjection::new(turn),
                    expanded_nodes: HashSet::new(),
                    workflow_expanded: true,
                },
            },
        )]);
        let mut panel = FlowPanelContent {
            document: Default::default(),
            interaction: Default::default(),
            goal_entry: None,
            goal_rect: Default::default(),
            goal_hover: false,
            handle: "child".into(),
            scroll: 0,
            render_cache: None,
            output_store: None,
            selection_projection: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 50)).unwrap();
        let draw = |panel: &mut FlowPanelContent,
                    terminal: &mut Terminal<TestBackend>,
                    tasks: &HashMap<String, crate::app::DetachedTaskDetail>| {
            let resources = HashMap::new();
            let prompts = HashMap::new();
            let browser = crate::mcp_manager::McpBrowserState {
                tab: Default::default(),
                content_revision: 0,
                hovered_action: None,
                remove_armed: None,
                resources: &resources,
                prompts: &prompts,
            };
            terminal
                .draw(|frame| {
                    panel.render_content(
                        ratatui::layout::Rect::new(3, 2, 70, 45),
                        frame,
                        &crate::wm::RenderCtx {
                            window_id: crate::wm::WindowId(1),
                            snapshots: &[],
                            items: &[],
                            item_revisions: &[],
                            handle_index: &HashMap::new(),
                            detached_task_details: tasks,
                            task_handle_index: &HashMap::new(),
                            workflow_run_to_panel: &HashMap::new(),
                            task_snapshots_revision: 0,
                            interaction_revision: 0,
                            animation_frame: 0,
                            expanded_tools: &HashSet::new(),
                            activity_nodes: &[],
                            mcp_servers: &[],
                            expanded_mcp_servers: &HashSet::new(),
                            mcp_selected: 0,
                            hovered_mcp_row: &None,
                            mcp_browser: &browser,
                            hovered_history_row: &None,
                        },
                    );
                })
                .unwrap();
        };
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut event_ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        draw(&mut panel, &mut terminal, &tasks);
        assert!(panel.selection_projection.is_some());
        let restored = panel.document.items.as_ptr();
        for prefix in [
            crate::output::WORKING_GROUP_REGION_PREFIX,
            crate::output::TOOL_CALL_REGION_PREFIX,
            crate::output::TOOL_DETAIL_REGION_PREFIX,
        ] {
            let hit = panel.interaction.hits.iter().find(|hit| matches!(&hit.target, HitTarget::DocumentNode(_, key) if key.starts_with(prefix))).unwrap();
            let row = hit.rect.y;
            if prefix == crate::output::WORKING_GROUP_REGION_PREFIX {
                let text: String = (hit.rect.x..hit.rect.right())
                    .map(|x| terminal.backend().buffer()[(x, row + 1)].symbol())
                    .collect();
                assert!(text.contains("working"), "{text}");
            }
            let event = WmEvent::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: hit.rect.x,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
            assert!(matches!(
                panel.handle_event(&event, &mut event_ctx),
                WmEventResult::Consumed(_)
            ));
            draw(&mut panel, &mut terminal, &tasks);
            assert_eq!(restored, panel.document.items.as_ptr());
        }
        let OutputItem::ToolDispatch { calls } = &panel.document.items[0] else {
            panic!()
        };
        assert_eq!(calls[0].disclosure, crate::app::Disclosure::Full);
        tasks.get_mut("child").unwrap().revision += 1;
        draw(&mut panel, &mut terminal, &tasks);
        let OutputItem::ToolDispatch { calls } = &panel.document.items[0] else {
            panic!()
        };
        assert_eq!(calls[0].disclosure, crate::app::Disclosure::Full);
        // The cached layout must project the same scrolled, clipped hit boxes.
        panel.scroll = 2;
        draw(&mut panel, &mut terminal, &tasks);
        let cached: Vec<_> = panel.interaction.hits.iter().map(|hit| hit.rect).collect();
        panel.render_cache = None;
        draw(&mut panel, &mut terminal, &tasks);
        assert_eq!(
            cached,
            panel
                .interaction
                .hits
                .iter()
                .map(|hit| hit.rect)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            panel.handle_event(&WmEvent::Key(KeyAction::Tab), &mut event_ctx),
            WmEventResult::Consumed(_)
        ));
        assert!(matches!(
            panel.handle_event(&WmEvent::Key(KeyAction::Submit), &mut event_ctx),
            WmEventResult::Consumed(_)
        ));
        let commands = panel.activate(HitTarget::DocumentNode(
            0,
            format!("{}call", crate::output::TOOL_FULLSCREEN_REGION_PREFIX),
        ));
        assert!(
            matches!(commands.as_slice(), [crate::wm::WmCommand::OpenTaskPanel { handle, maximized: true }] if handle == "process")
        );
        panel.activate(HitTarget::WorkflowNode(usize::MAX, "node".into()));
        assert!(
            panel
                .interaction
                .expanded_nodes
                .as_ref()
                .unwrap()
                .contains("node")
        );
    }

    use crate::app::OutputItem;
    use crate::wm::WindowComponent;

    use super::{FlowPanelContent, render_sub_agent_panel};

    #[test]
    fn document_restoration_tracks_content_not_layout() {
        let mut document = super::FlowDocument::default();
        let turn = atman_rt::TurnId::<atman_runtime::event::AtmanUuid>::now();
        let mut revision = crate::app::OutputRevision {
            id: 1,
            semantic: 1,
            layout: 1,
            ..Default::default()
        };
        let messages = vec![atman_runtime::message::Message::user_text(turn, "first")];
        document.restore(revision, &messages, None);
        let items = document.items.as_ptr();
        revision.layout += 1;
        document.restore(revision, &messages, None);
        assert_eq!(items, document.items.as_ptr());
        revision.semantic += 1;
        document.restore(revision, &[], None);
        assert!(document.items.is_empty());
    }

    #[test]
    fn blocked_child_toggle_does_not_fall_through_to_the_parent() {
        use atman_runtime::memory::goal::{GoalActor, GoalPauseReason};
        let session = atman_runtime::Session::open_ephemeral();
        session.goal_control.set("parent", None).unwrap();
        let child = session
            .goal_control
            .child("child".into(), "child".into(), None)
            .unwrap();
        let entry = session.flow_registry.create_entry(
            "child".into(),
            "child".into(),
            String::new(),
            atman_rt::RunId::now(),
        );
        entry.goal_control.set(child).ok().unwrap();
        session
            .goal_control
            .pause(
                GoalActor::User,
                GoalPauseReason::UserRequested,
                "stop".into(),
                None,
            )
            .unwrap();
        let mut panel = FlowPanelContent {
            document: Default::default(),
            interaction: Default::default(),
            goal_entry: Some(entry),
            goal_rect: ratatui::layout::Rect::new(1, 1, 40, 3),
            goal_hover: false,
            handle: "child".into(),
            scroll: 0,
            render_cache: None,
            output_store: None,
            selection_projection: None,
        };
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        for event in [
            WmEvent::Key(crate::keys::KeyAction::ToggleGoal),
            WmEvent::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
        ] {
            assert!(
                matches!(panel.handle_event(&event, &mut ctx), WmEventResult::Consumed(commands) if commands.is_empty())
            );
        }
    }

    #[test]
    fn animation_ticks_reuse_subagent_panel_projection() {
        let run_id = atman_rt::RunId::<atman_runtime::event::AtmanUuid>::now();
        let graph = WorkflowGraph {
            turn_id: atman_rt::TurnId::<atman_runtime::event::AtmanUuid>::now(),
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
        let mut document = super::FlowDocument::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| {
                render_sub_agent_panel(
                    frame,
                    frame.area(),
                    "child",
                    "inspect",
                    "model",
                    &crate::agent_presentation::AgentState::Running,
                    "",
                    1,
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
                    &mut document,
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
                    &crate::agent_presentation::AgentState::Running,
                    "",
                    1,
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
                    &mut document,
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
            turn_id: atman_rt::TurnId::<atman_runtime::event::AtmanUuid>::now(),
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
            hovered_action: None,
            remove_armed: None,
            resources: &resources,
            prompts: &prompts,
        };
        let mut panel = FlowPanelContent {
            document: Default::default(),
            interaction: Default::default(),
            goal_entry: None,
            goal_rect: ratatui::layout::Rect::default(),
            goal_hover: false,
            handle: run_id,
            scroll: 0,
            render_cache: None,
            output_store: None,
            selection_projection: None,
        };
        panel.goal_hover = true;
        let mut scroll = 0;
        let mut h_scroll = 0;
        assert!(
            matches!(
                panel.handle_event(
                    &WmEvent::Mouse(crossterm::event::MouseEvent {
                        kind: crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left
                        ),
                        column: 5,
                        row: 5,
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    }),
                    &mut EventCtx {
                        scroll: &mut scroll,
                        h_scroll: &mut h_scroll
                    },
                ),
                WmEventResult::Ignored
            ),
            "leaving the goal button must not swallow content clicks"
        );
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
                            detached_task_details: &std::collections::HashMap::new(),
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
