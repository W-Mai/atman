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
    Sort,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ActivitySort {
    #[default]
    WorkingFirst,
    NewestFirst,
    OldestFirst,
    NameAscending,
}

impl ActivitySort {
    pub const ALL: [Self; 4] = [
        Self::WorkingFirst,
        Self::NewestFirst,
        Self::OldestFirst,
        Self::NameAscending,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::WorkingFirst => "Working first",
            Self::NewestFirst => "Newest first",
            Self::OldestFirst => "Oldest first",
            Self::NameAscending => "Name A–Z",
        }
    }

    pub fn compare_tasks(
        self,
        a: &atman_runtime::TaskSnapshot,
        b: &atman_runtime::TaskSnapshot,
    ) -> std::cmp::Ordering {
        use atman_runtime::task_registry::TaskStatus;
        let rank = |status| match status {
            TaskStatus::Running | TaskStatus::Killing => 0,
            TaskStatus::Err => 1,
            TaskStatus::Ok | TaskStatus::Killed => 3,
        };
        (match self {
            Self::WorkingFirst => rank(a.status)
                .cmp(&rank(b.status))
                .then_with(|| b.started_at.cmp(&a.started_at)),
            Self::NewestFirst => b.started_at.cmp(&a.started_at),
            Self::OldestFirst => a.started_at.cmp(&b.started_at),
            Self::NameAscending => a.label.to_lowercase().cmp(&b.label.to_lowercase()),
        })
        .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
    }
}

#[derive(PartialEq, Eq)]
struct SortKey {
    address: atman_runtime::flow_store::FlowRef,
    rank: u8,
    registration: usize,
    name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FooterFocus {
    Previous,
    Sort,
    Next,
}

fn flow_rank(flow: &atman_runtime::flow_runtime::activity::ActivityFlow) -> u8 {
    use atman_rt::{InstanceActivity, InstanceState};
    match flow.state.as_ref().map(|state| state.state()) {
        Some(InstanceState::Closed) => 3,
        Some(InstanceState::Closing { .. }) => 0,
        _ if flow
            .state
            .as_ref()
            .is_none_or(|state| !state.pauses().is_empty()) =>
        {
            1
        }
        Some(InstanceState::Open(InstanceActivity::Active(_))) => 0,
        Some(InstanceState::Open(InstanceActivity::Interrupted { .. })) => 1,
        _ => 2,
    }
}

#[derive(Default)]
pub(crate) struct FlowActivity {
    pub snapshot: ActivitySnapshot,
    pub animation_frame: u32,
    page: usize,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pub sort: ActivitySort,
    pub keyboard_focus: bool,
    footer_focus: Option<FooterFocus>,
    sort_requested: bool,
    selected: Option<atman_runtime::flow_store::FlowRef>,
    reading_anchor: Option<atman_runtime::flow_store::FlowRef>,
    keys: Vec<SortKey>,
    cached_sort: Option<ActivitySort>,
    order: Vec<usize>,
    pages: usize,
    footer: Rect,
    footer_controls: Vec<(FooterFocus, Rect)>,
    pub reveal_footer: bool,
}

impl FlowActivity {
    pub fn take_sort_request(&mut self) -> bool {
        std::mem::take(&mut self.sort_requested)
    }

    pub fn set_sort(&mut self, sort: ActivitySort) {
        if self.sort != sort {
            self.sort = sort;
            self.cached_sort = None;
            self.reading_anchor = self.selected.clone();
            if self.selected.is_none() {
                self.page = 0;
            }
            self.clear_hits();
        }
    }

    fn inline_footer(width: u16) -> bool {
        usize::from(width)
            >= crate::width::width("Sort: Working first ▾")
                + 4
                + 2 * (crate::width::width("‹ Prev") + 4)
                + 2
    }

    pub fn minimum_height(&self, width: u16) -> u16 {
        6 + if Self::inline_footer(width) { 6 } else { 9 }
    }

    pub fn take_reveal_rect(&mut self) -> Option<Rect> {
        if !std::mem::take(&mut self.reveal_footer) || !self.keyboard_focus {
            return None;
        }
        self.footer_controls
            .iter()
            .find_map(|(focus, rect)| (Some(*focus) == self.footer_focus).then_some(*rect))
    }

    pub fn release_focus(&mut self) {
        self.keyboard_focus = false;
        self.footer_focus = None;
    }

    fn reconcile_order(&mut self, capacity: usize) {
        let selected_position = self.selected.as_ref().and_then(|selected| {
            self.order.iter().position(|&index| {
                self.keys
                    .get(index)
                    .is_some_and(|key| &key.address == selected)
            })
        });
        let keys: Vec<_> = self
            .snapshot
            .flows
            .iter()
            .map(|flow| SortKey {
                address: flow.address.clone(),
                rank: if self.sort == ActivitySort::WorkingFirst {
                    flow_rank(flow)
                } else {
                    0
                },
                registration: flow.registration_order,
                name: if self.sort == ActivitySort::NameAscending {
                    flow.title.to_lowercase()
                } else {
                    String::new()
                },
            })
            .collect();
        if self.cached_sort != Some(self.sort) || self.keys != keys {
            self.order = (0..keys.len()).collect();
            self.order.sort_by(|&a, &b| {
                let a = &keys[a];
                let b = &keys[b];
                (match self.sort {
                    ActivitySort::WorkingFirst => a
                        .rank
                        .cmp(&b.rank)
                        .then_with(|| b.registration.cmp(&a.registration)),
                    ActivitySort::NewestFirst => b.registration.cmp(&a.registration),
                    ActivitySort::OldestFirst => a.registration.cmp(&b.registration),
                    ActivitySort::NameAscending => a.name.cmp(&b.name),
                })
                .then_with(|| b.registration.cmp(&a.registration))
                .then_with(|| {
                    (a.address.session_id.0, a.address.instance_id.0.0)
                        .cmp(&(b.address.session_id.0, b.address.instance_id.0.0))
                })
            });
            self.keys = keys;
            self.cached_sort = Some(self.sort);
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !self.keys.iter().any(|key| &key.address == selected))
        {
            self.selected = self
                .order
                .get(
                    selected_position
                        .unwrap_or(0)
                        .min(self.order.len().saturating_sub(1)),
                )
                .map(|&index| self.snapshot.flows[index].address.clone());
        }
        if let Some(anchor) = self.selected.as_ref().or(self.reading_anchor.as_ref())
            && let Some(position) = self
                .order
                .iter()
                .position(|&index| &self.snapshot.flows[index].address == anchor)
        {
            self.page = position / capacity.max(1);
        }
        self.page = self.page.min(self.pages.saturating_sub(1));
        self.reading_anchor = self
            .order
            .get(self.page * capacity.max(1))
            .map(|&index| self.snapshot.flows[index].address.clone());
    }

    fn activate(&mut self, hit: Hit) -> Option<crate::flow_hub::Action> {
        match hit {
            Hit::Open(flow) => {
                self.selected = Some(flow.clone());
                Some(crate::flow_hub::Action::Float(
                    crate::control::ViewAddress::Instance(flow),
                ))
            }
            Hit::Page(page) => {
                self.page = page;
                self.selected = None;
                self.reading_anchor = None;
                None
            }
            Hit::Sort => {
                self.sort_requested = true;
                None
            }
        }
    }

    pub fn key(
        &mut self,
        action: &crate::keys::KeyAction,
    ) -> (bool, Option<crate::flow_hub::Action>) {
        use crate::keys::KeyAction;
        if !self.keyboard_focus {
            return (false, None);
        }
        let choices = if self.pages > 1 {
            vec![FooterFocus::Previous, FooterFocus::Sort, FooterFocus::Next]
        } else {
            vec![FooterFocus::Sort]
        };
        match action {
            KeyAction::Tab
            | KeyAction::BackTab
            | KeyAction::CursorLeft
            | KeyAction::CursorRight => {
                if matches!(action, KeyAction::CursorLeft | KeyAction::CursorRight)
                    && self.footer_focus.is_none()
                {
                    return (false, None);
                }
                let previous = matches!(action, KeyAction::BackTab | KeyAction::CursorLeft);
                let current = self
                    .footer_focus
                    .as_ref()
                    .and_then(|hit| choices.iter().position(|candidate| candidate == hit));
                let mut index = current.unwrap_or(if previous { 0 } else { choices.len() - 1 });
                crate::directional_selector::move_wrapped(
                    &mut index,
                    choices.len(),
                    if previous {
                        crate::directional_selector::SelectorDirection::Previous
                    } else {
                        crate::directional_selector::SelectorDirection::Next
                    },
                );
                self.footer_focus = Some(choices[index]);
                (true, None)
            }
            KeyAction::Submit | KeyAction::Char(' ') => {
                let Some(focus) = self.footer_focus else {
                    return (false, None);
                };
                let hit = match focus {
                    FooterFocus::Sort => Hit::Sort,
                    FooterFocus::Previous => Hit::Page(self.page.saturating_sub(1)),
                    FooterFocus::Next => {
                        Hit::Page((self.page + 1).min(self.pages.saturating_sub(1)))
                    }
                };
                let action = self.activate(hit);
                (true, action)
            }
            KeyAction::Escape => {
                self.keyboard_focus = false;
                self.footer_focus = None;
                (true, None)
            }
            _ => (false, None),
        }
    }
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
        if area.is_empty() {
            return Vec::new();
        }
        let t = crate::theme::theme();
        let preferred = if Self::inline_footer(area.width) {
            6
        } else {
            9
        };
        // Preserve one complete card where possible; decide once from geometry, before pagination.
        let footer_height = (if area.height >= preferred + 6 {
            preferred
        } else if area.height >= 12 {
            6
        } else {
            3
        })
        .min(area.height);
        let footer_inline = Self::inline_footer(area.width) || footer_height < 9;
        self.footer = Rect::new(
            area.x,
            area.bottom() - footer_height,
            area.width,
            footer_height,
        );
        let capacity = usize::from(self.footer.y.saturating_sub(area.y + 2)) / 4;
        self.pages = count.div_ceil(capacity.max(1)).max(1);
        self.reconcile_order(capacity);
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Flows ({count})"),
                Style::default().fg(t.heading.into()),
            )),
            Line::default(),
        ];
        if let Some(error) = &self.snapshot.error {
            lines.push(Line::from(format!("Activity unavailable: {error}")));
        } else if count == 0 {
            lines.push(Line::from("No flows"));
        } else if capacity == 0 {
            lines.push(Line::from("Expand to show flows"));
        }
        for &index in self.order.iter().skip(self.page * capacity.max(1)).take(
            if self.snapshot.error.is_none() {
                capacity
            } else {
                0
            },
        ) {
            let flow = &self.snapshot.flows[index];
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
        let content_height = usize::from(self.footer.y.saturating_sub(area.y));
        lines.truncate(content_height);
        lines.resize(content_height, Line::default());
        let sort_label = format!("Sort: {} ▾", self.sort.label());
        let sort_label = if crate::width::width(&sort_label) + 2 <= usize::from(area.width) {
            sort_label
        } else {
            "Sort ▾".into()
        };
        let mut buttons = Vec::new();
        if self.pages > 1 && footer_inline && area.width >= 9 {
            let side = ((crate::width::width("‹ Prev") + 4) as u16).min(area.width / 4);
            buttons.push((
                Rect::new(area.x, self.footer.y, side, 3),
                "‹ Prev".to_owned(),
                Hit::Page(self.page.saturating_sub(1)),
                FooterFocus::Previous,
            ));
            buttons.push((
                Rect::new(
                    area.x + side + 1,
                    self.footer.y,
                    area.width.saturating_sub(2 * side + 2),
                    3,
                ),
                sort_label,
                Hit::Sort,
                FooterFocus::Sort,
            ));
            buttons.push((
                Rect::new(area.right() - side, self.footer.y, side, 3),
                "Next ›".to_owned(),
                Hit::Page((self.page + 1).min(self.pages - 1)),
                FooterFocus::Next,
            ));
        } else {
            buttons.push((
                Rect::new(area.x, self.footer.y, area.width, 3),
                sort_label,
                Hit::Sort,
                FooterFocus::Sort,
            ));
            if self.pages > 1 && self.footer.height >= 9 {
                let left = area.width / 2;
                buttons.push((
                    Rect::new(area.x, self.footer.y + 3, left, 3),
                    "‹ Prev".to_owned(),
                    Hit::Page(self.page.saturating_sub(1)),
                    FooterFocus::Previous,
                ));
                buttons.push((
                    Rect::new(area.x + left, self.footer.y + 3, area.width - left, 3),
                    "Next ›".to_owned(),
                    Hit::Page((self.page + 1).min(self.pages - 1)),
                    FooterFocus::Next,
                ));
            }
        }
        for (rect, _, _, _) in &mut buttons {
            *rect = rect.intersection(area);
        }
        self.footer_controls = buttons
            .iter()
            .map(|(rect, _, _, focus)| (*focus, *rect))
            .collect();
        for (rect, _, hit, _) in &buttons {
            let visible = rect.intersection(area);
            if !visible.is_empty() {
                self.hits.push((visible, hit.clone()));
            }
        }
        let page_y = if footer_inline {
            self.footer.y + 4
        } else {
            self.footer.y + 7
        };
        for y in self.footer.y..area.bottom() {
            let mut spans = Vec::new();
            let mut x = area.x;
            for (rect, label, hit, focus) in &buttons {
                if y < rect.y || y >= rect.bottom() {
                    continue;
                }
                if rect.x > x {
                    spans.push(Span::raw(" ".repeat(usize::from(rect.x - x))));
                }
                let focused = self.keyboard_focus && self.footer_focus == Some(*focus);
                let bg = if focused || self.hover.as_ref() == Some(hit) {
                    t.work_hover_bg
                } else {
                    t.work_bg
                };
                let style = Style::default().bg(bg.into()).fg(if focused {
                    t.accent.into()
                } else {
                    t.heading.into()
                });
                let text = if y == rect.y + rect.height.saturating_sub(1) / 2 {
                    let label = if crate::width::width(label) + 2 > usize::from(rect.width) {
                        match focus {
                            FooterFocus::Sort => "Sort ▾",
                            FooterFocus::Previous => "‹",
                            FooterFocus::Next => "›",
                        }
                    } else {
                        label.as_str()
                    };
                    crate::width::truncate(label, usize::from(rect.width.saturating_sub(2)))
                } else {
                    String::new()
                };
                let padding = usize::from(rect.width).saturating_sub(crate::width::width(&text));
                spans.push(Span::styled(
                    format!(
                        "{}{}{}",
                        " ".repeat(padding / 2),
                        text,
                        " ".repeat(padding - padding / 2)
                    ),
                    style,
                ));
                x = rect.right();
            }
            if x < area.right() {
                spans.push(Span::raw(" ".repeat(usize::from(area.right() - x))));
            }
            if self.pages > 1 && y == page_y {
                spans = vec![Span::styled(
                    crate::width::pad_right(
                        &format!("  {} / {}", self.page + 1, self.pages),
                        usize::from(area.width),
                    ),
                    Style::default().fg(t.subtle_fg.into()),
                )];
            }
            lines.push(Line::from(spans).style(Style::default().bg(t.work_bg.into())));
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
            if let Some(hit) = hit {
                self.keyboard_focus = true;
                self.footer_focus = match &hit {
                    Hit::Sort => Some(FooterFocus::Sort),
                    Hit::Page(page) if *page < self.page || self.page == 0 => {
                        Some(FooterFocus::Previous)
                    }
                    Hit::Page(_) => Some(FooterFocus::Next),
                    Hit::Open(_) => None,
                };
                return (true, self.activate(hit));
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
    pub(crate) fn activity_key(&mut self, event: &crossterm::event::Event) -> bool {
        use crossterm::event::{Event, KeyEventKind, MouseButton, MouseEventKind};
        if let Event::Mouse(mouse) = event {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && self
                    .composer_rect
                    .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
            {
                self.flow_activity.keyboard_focus = false;
                self.flow_activity.footer_focus = None;
            }
            return false;
        }
        let Event::Key(key) = event else { return false };
        let action = crate::keys::map(*key);
        if key.kind == KeyEventKind::Release
            || !self.focus_rail.activity
            || !self.flow_activity.keyboard_focus
        {
            return false;
        }
        if action == crate::keys::KeyAction::Escape {
            self.flow_activity.release_focus();
            return false;
        }
        let (consumed, action) = self.flow_activity.key(&action);
        if consumed {
            self.flow_activity.reveal_footer = true;
            self.open_flow_hub = action;
            self.reveal_activity_controls();
            self.mark_visual_dirty();
        } else if matches!(key.code, crossterm::event::KeyCode::Char(_)) {
            self.flow_activity.keyboard_focus = false;
            self.flow_activity.footer_focus = None;
        }
        consumed
    }

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
    fn activity_sort_is_global_deterministic_and_ignores_preview_updates() {
        use atman_rt::{FlowInstance, InstanceState, Interruption, TurnOutcome};
        let mut activity = FlowActivity::default();
        for (index, name) in ["Zulu", "alpha", "Bravo", "echo", "Delta", "charlie"]
            .iter()
            .enumerate()
        {
            let address = FlowRef {
                session_id: uuid::Uuid::now_v7().into(),
                instance_id: FlowInstanceId::now(),
            };
            let mut state = FlowInstance::prepared(address.clone());
            state.activate().unwrap();
            match index {
                0 => {
                    state.begin_close().unwrap();
                    state.finish_close().unwrap();
                }
                2 | 3 => {
                    let lease = state
                        .begin_turn(atman_rt::TurnId::now(), atman_rt::RunId::now())
                        .unwrap();
                    if index == 3 {
                        state
                            .finish_turn(&lease, TurnOutcome::Interrupted(Interruption::Failed))
                            .unwrap();
                    }
                }
                4 => state.pause(uuid::Uuid::now_v7().into()).unwrap(),
                _ => {}
            }
            activity.snapshot.flows.push(ActivityFlow {
                registration_order: [4, 3, 0, 1, 5, 6][index],
                address,
                title: (*name).into(),
                preview: String::new(),
                state: (index != 5).then_some(state),
                error: None,
            });
        }
        for (sort, expected) in [
            (
                ActivitySort::WorkingFirst,
                vec!["Bravo", "charlie", "Delta", "echo", "alpha", "Zulu"],
            ),
            (
                ActivitySort::NewestFirst,
                vec!["charlie", "Delta", "Zulu", "alpha", "echo", "Bravo"],
            ),
            (
                ActivitySort::OldestFirst,
                vec!["Bravo", "echo", "alpha", "Zulu", "Delta", "charlie"],
            ),
            (
                ActivitySort::NameAscending,
                vec!["alpha", "Bravo", "charlie", "Delta", "echo", "Zulu"],
            ),
        ] {
            activity.set_sort(sort);
            activity.lines(Rect::new(0, 0, 60, 48));
            assert_eq!(
                activity
                    .order
                    .iter()
                    .map(|&index| activity.snapshot.flows[index].title.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            let order = activity.order.as_ptr();
            activity.snapshot.flows[0].preview = "new tokens".into();
            activity.animation_frame += 1;
            activity.lines(Rect::new(0, 0, 60, 48));
            assert_eq!(activity.order.as_ptr(), order);
        }
        assert!(matches!(
            activity.snapshot.flows[0].state.as_ref().unwrap().state(),
            InstanceState::Closed
        ));
        let selected = activity.snapshot.flows[2].address.clone();
        activity.selected = Some(selected.clone());
        activity.set_sort(ActivitySort::NewestFirst);
        activity.lines(Rect::new(0, 0, 40, 18));
        assert!(
            activity
                .hits
                .iter()
                .any(|(_, hit)| *hit == Hit::Open(selected.clone()))
        );
        activity.snapshot.flows.clear();
        activity.set_sort(ActivitySort::NameAscending);
        activity.lines(Rect::new(0, 0, 40, 18));
        assert!(activity.order.is_empty());
        assert!(activity.selected.is_none());
    }

    #[test]
    fn activity_sort_control_exists_without_pages_or_data_and_uses_its_full_hitbox() {
        for (width, height) in [(3, 1), (12, 3), (24, 12), (60, 24)] {
            let area = Rect::new(2, 3, width, height);
            let mut activity = FlowActivity::default();
            for error in [None, Some("read failed".to_owned())] {
                activity.snapshot.error = error;
                activity.lines(area);
                let sorts: Vec<_> = activity
                    .hits
                    .iter()
                    .filter(|(_, hit)| *hit == Hit::Sort)
                    .collect();
                assert_eq!(sorts.len(), 1);
                let rect = sorts[0].0;
                assert_eq!(rect.intersection(area), rect);
                assert!(
                    activity
                        .mouse(MouseEvent {
                            column: rect.x,
                            row: rect.y,
                            kind: MouseEventKind::Down(MouseButton::Left),
                            modifiers: crossterm::event::KeyModifiers::NONE
                        })
                        .0
                );
                assert!(activity.take_sort_request());
                assert!(
                    !activity
                        .hits
                        .iter()
                        .any(|(_, hit)| matches!(hit, Hit::Page(_)))
                );
            }
        }
    }

    #[test]
    fn activity_keyboard_focus_reaches_sort_and_releases_to_the_composer() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::app::AppState::default();
        app.sidebar_collapsed = false;
        app.select_focus_rail(true);
        app.flow_activity.lines(Rect::new(0, 0, 50, 30));
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(app.activity_key(&key(KeyCode::Tab)));
        assert!(app.activity_key(&key(KeyCode::Enter)));
        assert!(app.flow_activity.take_sort_request());
        app.composer_rect = Some(Rect::new(0, 40, 50, 3));
        assert!(!app.activity_key(&Event::Mouse(MouseEvent {
            column: 2,
            row: 41,
            kind: MouseEventKind::Down(MouseButton::Left),
            modifiers: KeyModifiers::NONE
        })));
        assert!(!app.activity_key(&key(KeyCode::Tab)));
        app.select_focus_rail(true);
        assert!(!app.activity_key(&key(KeyCode::Esc)));
        assert!(!app.flow_activity.keyboard_focus);
        app.focus_rail.configure(true);
        app.select_focus_rail(true);
        assert!(app.activity_key(&key(KeyCode::Tab)));
        assert!(app.focus_rail.page_open());
    }

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
            registration_order: 0,
            address: address.clone(),
            title: "任务".into(),
            preview: String::new(),
            state: None,
            error: None,
        });
        // Heading, one complete four-row card, and the persistent three-row Sort control.
        let area = Rect::new(0, 0, 40, 9);
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
    fn pagination_records_keep_padding_background_and_hit_ownership() {
        use ratatui::{
            buffer::Buffer,
            widgets::{Paragraph, Widget},
        };
        let mut activity = FlowActivity::default();
        activity.snapshot.flows = (0..9)
            .map(|i| ActivityFlow {
                registration_order: i,
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
        for (width, height) in [(24, 12), (40, 18), (41, 24)] {
            let area = Rect::new(2, 3, width, height);
            for page in [0, 1, usize::MAX] {
                activity.page = page;
                activity.lines(area);
                let buttons: Vec<_> = activity
                    .hits
                    .iter()
                    .filter(|(_, hit)| matches!(hit, Hit::Page(_)))
                    .cloned()
                    .collect();
                assert_eq!(buttons.len(), 2);
                for hovered in [None, Some(buttons[0].1.clone()), Some(buttons[1].1.clone())] {
                    activity.hover = hovered;
                    let lines = activity.lines(area);
                    let mut buffer = Buffer::empty(area);
                    Paragraph::new(lines.clone()).render(area, &mut buffer);
                    let t = crate::theme::theme();
                    for (rect, hit) in &buttons {
                        assert_eq!(rect.height, 3);
                        let bg = if activity.hover.as_ref() == Some(hit) {
                            t.work_hover_bg
                        } else {
                            t.work_bg
                        };
                        for y in rect.y..rect.bottom() {
                            for x in rect.x..rect.right() {
                                assert_eq!(
                                    buffer[(x, y)].bg,
                                    ratatui::style::Color::from(bg),
                                    "cell {x},{y}"
                                );
                                if y != rect.y + 1 {
                                    assert_eq!(buffer[(x, y)].symbol(), " ");
                                }
                                assert!(
                                    activity
                                        .hits
                                        .iter()
                                        .any(|(r, h)| r.contains((x, y).into()) && h == hit)
                                );
                            }
                        }
                    }
                    let footer_y = buttons[0].0.bottom();
                    assert!(footer_y + 3 <= area.y + lines.len() as u16);
                    for y in footer_y..footer_y + 3 {
                        for x in area.x..area.right() {
                            if y != footer_y + 1 {
                                assert_eq!(buffer[(x, y)].symbol(), " ");
                            }
                            let before = activity.page;
                            assert!(
                                !activity
                                    .mouse(MouseEvent {
                                        column: x,
                                        row: y,
                                        kind: MouseEventKind::Down(MouseButton::Left),
                                        modifiers: crossterm::event::KeyModifiers::NONE,
                                    })
                                    .0
                            );
                            assert_eq!(activity.page, before);
                        }
                    }
                    assert!(
                        (area.x..area.right())
                            .map(|x| buffer[(x, footer_y + 1)].symbol())
                            .collect::<String>()
                            .contains(" / ")
                    );
                }
            }
        }
    }

    #[test]
    fn pages_and_clipping_preserve_flow_identity_and_hide_inactive_targets() {
        let mut activity = FlowActivity::default();
        activity.snapshot.flows = (0..9)
            .map(|i| ActivityFlow {
                registration_order: i,
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
        for _ in 0..9 {
            let lines = activity.lines(Rect::new(10, 10, 40, 20));
            assert!(lines.len() <= 20);
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
            if activity.page + 1 < activity.pages {
                let rect = activity
                    .hits
                    .iter()
                    .find(|(_, hit)| *hit == Hit::Page(activity.page + 1))
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
            } else {
                break;
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
