//! Flow activity rows shared by the main conversation and floating views.

use atman_runtime::flow_runtime::activity::ActivitySnapshot;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
};

#[derive(Clone, Debug, PartialEq)]
enum Hit {
    Open(atman_runtime::flow_store::FlowRef),
    Page(usize),
}

#[derive(Default)]
pub(crate) struct FlowActivity {
    pub snapshot: ActivitySnapshot,
    pub animation_frame: u32,
    page: usize,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
}

impl FlowActivity {
    pub fn is_running(&self) -> bool {
        self.snapshot.flows.iter().any(|flow| {
            flow.state.as_ref().is_some_and(|state| {
                state.pauses().is_empty()
                    && matches!(
                        state.state(),
                        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_))
                    )
            })
        })
    }

    pub fn clear_hits(&mut self) {
        self.hits.clear();
    }

    pub fn project(&mut self, viewport: crate::focus_rail::AuxiliaryViewport) {
        self.hits.retain_mut(|(rect, _)| {
            if let Some(visible) = viewport.project(*rect) {
                *rect = visible;
                true
            } else {
                false
            }
        });
    }

    pub fn lines(&mut self, area: Rect) -> Vec<Line<'static>> {
        self.clear_hits();
        let count = self.snapshot.flows.len();
        if let Some(error) = &self.snapshot.error {
            return vec![Line::from(format!("Activity unavailable: {error}"))];
        }
        if count == 0 || area.height < 7 {
            return Vec::new();
        }
        let t = crate::theme::theme();
        let capacity = (usize::from(area.height).saturating_sub(3) / 4).max(1);
        let capacity = if count > capacity {
            (usize::from(area.height).saturating_sub(6) / 4).max(1)
        } else {
            capacity
        };
        let pages = count.div_ceil(capacity);
        self.page = self.page.min(pages - 1);
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Flows ({count})"),
                Style::default().fg(t.heading.into()),
            )),
            Line::default(),
        ];
        for flow in self
            .snapshot
            .flows
            .iter()
            .skip(self.page * capacity)
            .take(capacity)
        {
            let hit = Hit::Open(flow.address.clone());
            let bg = if self.hover.as_ref() == Some(&hit) {
                t.work_hover_bg
            } else {
                t.work_bg
            };
            use atman_rt::{InstanceActivity, InstanceState};
            let state = match flow.state.as_ref() {
                Some(state) if !state.pauses().is_empty() => "Ⅱ",
                Some(state) => match state.state() {
                    InstanceState::Prepared => "◇",
                    InstanceState::Open(InstanceActivity::Idle) => "◌",
                    InstanceState::Open(InstanceActivity::Active(_)) => {
                        crate::output::spinner_char(self.animation_frame)
                    }
                    InstanceState::Open(InstanceActivity::Interrupted { .. }) => "!",
                    InstanceState::Closing { .. } => "◷",
                    InstanceState::Closed => "✓",
                },
                None => "!",
            };
            self.hits.push((
                Rect::new(area.x, area.y + lines.len() as u16, area.width, 4),
                hit,
            ));
            let style = Style::default().bg(bg.into());
            lines.push(Line::from(" ".repeat(area.width as usize)).style(style));
            lines.push(card_heading(
                &flow.title,
                state,
                &flow.address.instance_id.to_string(),
                area.width as usize,
                bg.into(),
            ));
            let preview = if flow.preview.is_empty() {
                "waiting for output".to_owned()
            } else {
                flow.preview.clone()
            };
            let detail_line = {
                let prefix = "    ".to_owned();
                let budget = (area.width as usize).saturating_sub(crate::width::width(&prefix) + 2);
                let mut spans = vec![Span::styled(prefix, style.fg(t.heading.into()))];
                spans.extend(crate::width::streaming_ticker_spans(
                    vec![Span::styled(preview, style.fg(t.subtle_fg.into()))],
                    budget,
                    bg.into(),
                ));
                let used = crate::width::spans_width(spans.iter());
                spans.push(Span::raw(
                    " ".repeat((area.width as usize).saturating_sub(used)),
                ));
                Line::from(spans).style(style)
            };
            lines.push(detail_line);
            lines.push(Line::from(" ".repeat(area.width as usize)).style(style));
        }
        if pages > 1 {
            let y = area.y + lines.len() as u16;
            let left = area.width / 2;
            let buttons = [
                ("‹ Prev", self.page.saturating_sub(1), 0, left),
                (
                    "Next ›",
                    (self.page + 1).min(pages - 1),
                    left,
                    area.width - left,
                ),
            ];
            let mut middle = Vec::new();
            for (label, target, x, width) in buttons {
                let hit = Hit::Page(target);
                self.hits
                    .push((Rect::new(area.x + x, y, width, 3), hit.clone()));
                let bg = if self.hover.as_ref() == Some(&hit) {
                    t.work_hover_bg
                } else {
                    t.work_bg
                };
                middle.push(Span::styled(
                    format!("{label:^width$}", width = width as usize),
                    Style::default().bg(bg.into()).fg(t.heading.into()),
                ));
            }
            lines.push(
                Line::from(" ".repeat(area.width as usize))
                    .style(Style::default().bg(t.work_bg.into())),
            );
            lines.push(Line::from(middle));
            lines.push(
                Line::from(crate::width::pad_right(
                    &format!("  {} / {pages}", self.page + 1),
                    area.width as usize,
                ))
                .style(Style::default().bg(t.work_bg.into()).fg(t.subtle_fg.into())),
            );
        }
        lines.push(Line::default());
        lines.truncate(area.height as usize);
        self.hits.retain_mut(|(rect, _)| {
            *rect = rect.intersection(area);
            !rect.is_empty()
        });
        lines
    }

    pub fn mouse(&mut self, mouse: MouseEvent) -> (bool, Option<crate::flow_hub::Action>) {
        let hit = self
            .hits
            .iter()
            .find(|(r, _)| r.contains((mouse.column, mouse.row).into()))
            .map(|(_, hit)| hit.clone());
        self.hover = hit.clone();
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            match hit {
                Some(Hit::Open(flow)) => {
                    return (
                        true,
                        Some(crate::flow_hub::Action::Float(
                            crate::control::ViewAddress::Instance(flow),
                        )),
                    );
                }
                Some(Hit::Page(page)) => {
                    self.page = page;
                    return (true, None);
                }
                None => {}
            }
        }
        (false, None)
    }
}

fn card_heading(
    title: &str,
    icon: &str,
    id: &str,
    width: usize,
    bg: ratatui::style::Color,
) -> Line<'static> {
    let t = crate::theme::theme();
    let style = Style::default().bg(bg);
    let prefix = format!("  {icon} ");
    let prefix_width = crate::width::width(&prefix);
    let id_length = if prefix_width + crate::width::width(title) + 12 > width {
        6
    } else {
        8
    };
    let id = &id[id.len().saturating_sub(id_length)..];
    let title_budget = width.saturating_sub(prefix_width + id.len() + 4);
    let title = crate::width::truncate(title, title_budget);
    let gap = width.saturating_sub(prefix_width + crate::width::width(&title) + id.len() + 2);
    let spans = vec![
        Span::styled(prefix, style.fg(t.heading.into())),
        Span::styled(title, style.fg(t.tinted_fg.into())),
        Span::raw(" ".repeat(gap)),
        Span::styled(format!("{id}  "), style.fg(t.subtle_fg.into())),
    ];
    Line::from(crate::width::truncate_spans(spans, width, Some(bg))).style(style)
}

impl crate::app::AppState {
    pub(crate) fn activity_mouse(&mut self, mouse: MouseEvent) -> bool {
        let before = self.flow_activity.hover.clone();
        let (consumed, action) = self.flow_activity.mouse(mouse);
        if consumed || before != self.flow_activity.hover {
            self.mark_visual_dirty();
        }
        if consumed {
            self.open_flow_hub = action;
        }
        consumed
    }

    pub(crate) fn sync_workflow_activity(&mut self) {
        use crate::task_panel::{ActivityNode, ActivityStatus};
        use atman_runtime::workflow::{NodeStatus, WorkflowNodeKind};
        let mut updated = std::collections::HashSet::new();
        let mut active = Vec::new();
        let known: std::collections::HashSet<_> = self
            .activity_nodes
            .iter()
            .map(|node| (node.run_id.clone(), node.node_id.clone()))
            .collect();
        let now = std::time::Instant::now();
        let wall = chrono::Utc::now();
        for &index in self.items.updated_workflows() {
            let crate::app::OutputItem::WorkflowPanel { graph, .. } = &self.items[index] else {
                continue;
            };
            self.last_workflow_panel_idx = Some(
                self.last_workflow_panel_idx
                    .map_or(index, |last| last.max(index)),
            );
            let mut nodes: Vec<_> = graph
                .root
                .iter()
                .map(|node| (node, String::new(), None))
                .collect();
            while let Some((node, mut run, parent)) = nodes.pop() {
                let kind = match &node.kind {
                    WorkflowNodeKind::Flow { run_id, flow_name }
                    | WorkflowNodeKind::Subflow { run_id, flow_name } => {
                        run = run_id.clone();
                        updated.insert(run.clone());
                        self.workflow_run_to_panel.insert(run.clone(), index);
                        atman_runtime::nodegraph::NodeKind::Subflow {
                            name: flow_name.clone(),
                        }
                    }
                    WorkflowNodeKind::Stmt { node_kind } => node_kind.clone(),
                    WorkflowNodeKind::ToolCall { tool, .. } => {
                        atman_runtime::nodegraph::NodeKind::ToolCall { path: tool.clone() }
                    }
                    WorkflowNodeKind::FanoutBranch { .. } => {
                        atman_runtime::nodegraph::NodeKind::Fanout
                    }
                };
                nodes.extend(
                    node.children
                        .iter()
                        .map(|child| (child, run.clone(), Some(node.id.clone()))),
                );
                if !run.is_empty()
                    && (node.status == NodeStatus::Running
                        || known.contains(&(run.clone(), node.id.clone())))
                {
                    let elapsed = node
                        .started_at
                        .and_then(|time| (wall - time).to_std().ok())
                        .unwrap_or_default();
                    active.push(ActivityNode {
                        run_id: run,
                        node_id: node.id.clone(),
                        parent_node_id: parent,
                        label: node.label.clone(),
                        kind,
                        status: match node.status {
                            NodeStatus::Running | NodeStatus::Pending => ActivityStatus::Running,
                            NodeStatus::Ok => ActivityStatus::Ok,
                            NodeStatus::Err => ActivityStatus::Err,
                            NodeStatus::Cancelled => ActivityStatus::Cancelled,
                        },
                        started_at: now.checked_sub(elapsed).unwrap_or(now),
                        ended_at: (node.status != NodeStatus::Running).then(|| {
                            node.ended_at
                                .and_then(|time| (wall - time).to_std().ok())
                                .and_then(|elapsed| now.checked_sub(elapsed))
                                .unwrap_or(now)
                        }),
                    });
                }
            }
        }
        self.activity_nodes
            .retain(|node| !updated.contains(&node.run_id));
        self.activity_nodes.extend(active);
        while self.activity_nodes.len() > 128 {
            let Some(index) = self
                .activity_nodes
                .iter()
                .position(|node| node.status != ActivityStatus::Running)
            else {
                break;
            };
            self.activity_nodes.remove(index);
        }
    }

    pub(crate) fn sync_activity(&mut self, snapshot: ActivitySnapshot) {
        self.task_snapshots = snapshot.tasks.clone();
        self.task_id_index = self
            .task_snapshots
            .iter()
            .enumerate()
            .map(|(i, row)| (row.id.clone(), i))
            .collect();
        self.task_handle_index = self
            .task_snapshots
            .iter()
            .enumerate()
            .map(|(i, row)| (row.source_handle.clone(), i))
            .collect();
        self.task_snapshots_revision = self.task_snapshots_revision.wrapping_add(1);
        self.flow_activity.snapshot = snapshot;
        self.mark_visual_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::{
        flow_runtime::activity::ActivityFlow,
        flow_store::{FlowInstanceId, FlowRef},
    };

    #[test]
    fn flow_cards_fill_every_cell_including_wide_titles_and_hover() {
        use ratatui::{
            buffer::Buffer,
            widgets::{Paragraph, Widget},
        };
        let mut activity = FlowActivity::default();
        let address = FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        activity.snapshot.flows.push(ActivityFlow {
            address: address.clone(),
            title: "任务".into(),
            preview: String::new(),
            state: None,
            error: None,
        });
        let area = Rect::new(0, 0, 40, 7);
        for hovered in [false, true, false] {
            activity.hover = hovered.then(|| Hit::Open(address.clone()));
            let mut buffer = Buffer::empty(area);
            Paragraph::new(activity.lines(area)).render(area, &mut buffer);
            let t = crate::theme::theme();
            let expected: ratatui::style::Color =
                if hovered { t.work_hover_bg } else { t.work_bg }.into();
            for y in 2..6 {
                for x in 0..40 {
                    // Ratatui clears the continuation cell of a wide glyph; the leading
                    // cell supplies the background of both terminal columns.
                    if x > 0 && crate::width::width(buffer[(x - 1, y)].symbol()) == 2 {
                        continue;
                    }
                    assert_eq!(buffer[(x, y)].bg, expected, "cell {x},{y}");
                }
            }
        }
    }

    #[test]
    fn pages_and_clipping_preserve_flow_identity_and_hide_inactive_targets() {
        let mut activity = FlowActivity::default();
        activity.snapshot.flows = (0..9)
            .map(|i| ActivityFlow {
                address: FlowRef {
                    session_id: uuid::Uuid::now_v7().into(),
                    instance_id: FlowInstanceId::now(),
                },
                title: format!("Task {i}"),
                preview: String::new(),
                state: None,
                error: None,
            })
            .collect();
        let mut opened = std::collections::HashSet::new();
        for page in 0..3 {
            let lines = activity.lines(Rect::new(10, 10, 40, 18));
            assert!(lines.len() <= 18);
            let hits = activity.hits.clone();
            for (rect, hit) in hits {
                if let Hit::Open(flow) = hit {
                    let (_, action) = activity.mouse(MouseEvent {
                        column: rect.x + 1,
                        row: rect.y + 1,
                        kind: MouseEventKind::Down(MouseButton::Left),
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    });
                    assert!(
                        matches!(action, Some(crate::flow_hub::Action::Float(crate::control::ViewAddress::Instance(target))) if target == flow)
                    );
                    opened.insert(flow);
                }
            }
            if page < 2 {
                let rect = activity
                    .hits
                    .iter()
                    .find(|(_, hit)| *hit == Hit::Page(page + 1))
                    .unwrap()
                    .0;
                assert!(
                    activity
                        .mouse(MouseEvent {
                            column: rect.x,
                            row: rect.y + 1,
                            kind: MouseEventKind::Down(MouseButton::Left),
                            modifiers: crossterm::event::KeyModifiers::NONE
                        })
                        .0
                );
            }
        }
        assert_eq!(opened.len(), 9);
        activity.project(crate::focus_rail::AuxiliaryViewport {
            canvas: Rect::new(10, 0, 40, 40),
            visible: Rect::new(10, 11, 40, 3),
            target: Rect::new(10, 20, 40, 3),
        });
        assert!(
            activity
                .hits
                .iter()
                .all(|(rect, _)| rect.y >= 20 && rect.bottom() <= 23)
        );
        activity.clear_hits();
        assert!(activity.hits.is_empty());
    }
}
