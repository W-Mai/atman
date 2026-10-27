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
    page: usize,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
}

impl FlowActivity {
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
        if count == 0 || area.height < 5 {
            return Vec::new();
        }
        let t = crate::theme::theme();
        let capacity = (usize::from(area.height).saturating_sub(4) / 4).max(1);
        let pages = count.div_ceil(capacity);
        self.page = self.page.min(pages - 1);
        let mut lines = vec![Line::from(Span::styled(
            format!("Flows ({count})"),
            Style::default().fg(t.heading.into()),
        ))];
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
            let state = flow
                .state
                .as_ref()
                .map(|state| {
                    let execution = crate::flow_hub::execution_label(state.state());
                    if state.pauses().is_empty() {
                        execution.to_string()
                    } else {
                        format!("{execution} · paused")
                    }
                })
                .unwrap_or_else(|| "Unavailable".into());
            let title =
                crate::width::truncate_plain(&format!("  {}", flow.title), area.width as usize)
                    .to_string();
            let short = flow.address.instance_id.to_string();
            let detail = crate::width::truncate_plain(
                &format!("  {state} · {}", &short[short.len().saturating_sub(8)..]),
                area.width as usize,
            )
            .to_string();
            self.hits.push((
                Rect::new(area.x, area.y + lines.len() as u16, area.width, 4),
                hit,
            ));
            let style = Style::default().bg(bg.into());
            lines.push(Line::from(" ".repeat(area.width as usize)).style(style));
            lines.push(
                Line::from(crate::width::pad_right(&title, area.width as usize))
                    .style(style.fg(t.tinted_fg.into())),
            );
            lines.push(
                Line::from(crate::width::pad_right(&detail, area.width as usize))
                    .style(style.fg(t.subtle_fg.into())),
            );
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
            state: None,
            error: None,
        });
        let area = Rect::new(0, 0, 40, 5);
        for hovered in [false, true, false] {
            activity.hover = hovered.then(|| Hit::Open(address.clone()));
            let mut buffer = Buffer::empty(area);
            Paragraph::new(activity.lines(area)).render(area, &mut buffer);
            let t = crate::theme::theme();
            let expected: ratatui::style::Color =
                if hovered { t.work_hover_bg } else { t.work_bg }.into();
            for y in 1..5 {
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
                state: None,
                error: None,
            })
            .collect();
        let mut opened = std::collections::HashSet::new();
        for page in 0..3 {
            let lines = activity.lines(Rect::new(10, 10, 40, 16));
            assert!(lines.len() <= 16);
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
