//! A paged instance directory. Reading and selecting rows never controls execution.

#[cfg(test)]
use atman_runtime::flow_store::FlowRef;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::Paragraph,
};

use crate::{control::ControlTicket, keys::KeyAction};

pub const PAGE_SIZE: usize = 50;

mod graph;
mod page;
pub use page::{FlowHubPage, HubRequest, SessionDirectory, SessionRow};

#[derive(Clone)]
pub(crate) enum Action {
    None,
    Close,
    Return,
    Load(HubRequest),
    NewSession,
    OpenManagement,
    Focus(crate::control::ViewAddress),
    Float(crate::control::ViewAddress),
}

#[derive(Default)]
pub(crate) struct FlowHub {
    pub pending: Option<ControlTicket>,
    page: Option<FlowHubPage>,
    rows: Vec<page::Row>,
    endpoint: page::Endpoint,
    graph: graph::Layout,
    graph_visible: bool,
    graph_button: Rect,
    graph_hovered: bool,
    cursors: Vec<HubRequest>,
    index: usize,
    requested_index: Option<usize>,
    selected: usize,
    offset: usize,
    query: String,
    error: Option<String>,
    hits: Vec<(Rect, usize)>,
    float_hits: Vec<(Rect, usize)>,
    previous: Rect,
    next: Rect,
    close: Rect,
    refresh: Rect,
    relations: Rect,
    sessions: Rect,
    sessions_hovered: bool,
    relations_hovered: bool,
    refresh_hovered: bool,
    hovered: Option<usize>,
}

/// Retain directory presentation independently of focused conversations and host requests.
#[derive(Default)]
pub(crate) struct History {
    pages: Vec<FlowHub>,
}

impl History {
    pub fn request(&self, request: HubRequest) -> HubRequest {
        self.pages
            .iter()
            .find(|saved| saved.page.as_ref().unwrap().first() == request.first())
            .and_then(|saved| saved.cursors.get(saved.index))
            .cloned()
            .unwrap_or(request)
    }

    pub fn receive(
        &mut self,
        hub: &mut FlowHub,
        ticket: Option<&ControlTicket>,
        result: Result<Box<FlowHubPage>, String>,
    ) {
        if !hub.owns_reply(ticket) {
            return;
        }
        if let Ok(page) = &result
            && hub.leaves_directory(&page.first())
        {
            let next = self
                .pages
                .iter()
                .position(|saved| saved.page.as_ref().unwrap().first() == page.first())
                .map(|index| self.pages.remove(index))
                .unwrap_or_default();
            self.save(std::mem::replace(hub, next));
            hub.pending = ticket.cloned();
        }
        hub.receive(ticket, result);
    }

    pub fn save(&mut self, mut hub: FlowHub) {
        let Some(page) = &hub.page else { return };
        let directory = page.first();
        self.pages
            .retain(|saved| saved.page.as_ref().unwrap().first() != directory);
        // Accepted host work continues, but its late reply no longer owns navigation.
        hub.pending = None;
        hub.requested_index = None;
        self.pages.push(hub);
    }

    pub fn open(&mut self, action: Action, fallback: HubRequest) -> (FlowHub, Action) {
        if matches!(action, Action::Return) {
            return match self.pages.pop() {
                Some(saved) => (saved, Action::None),
                None => (FlowHub::default(), Action::Load(fallback)),
            };
        }
        if let Action::Load(request) = &action
            && let Some(index) = self
                .pages
                .iter()
                .position(|saved| saved.page.as_ref().unwrap().first() == *request)
        {
            let saved = self.pages.remove(index);
            let request = saved
                .cursors
                .get(saved.index)
                .cloned()
                .unwrap_or_else(|| request.clone());
            return (saved, Action::Load(request));
        }
        (FlowHub::default(), action)
    }
}

impl FlowHub {
    pub fn leaves_directory(&self, request: &HubRequest) -> bool {
        self.page
            .as_ref()
            .is_some_and(|page| page.first() != request.first())
    }

    pub fn owns_reply(&self, ticket: Option<&ControlTicket>) -> bool {
        ticket.is_some() && ticket == self.pending.as_ref()
    }

    pub fn receive(
        &mut self,
        ticket: Option<&ControlTicket>,
        result: Result<Box<FlowHubPage>, String>,
    ) {
        if !self.owns_reply(ticket) {
            return;
        }
        self.pending = None;
        match result {
            Ok(page) => {
                let selected = self
                    .filtered()
                    .get(self.selected)
                    .map(|index| self.rows[*index].key.clone());
                let changed_directory = self
                    .page
                    .as_ref()
                    .is_none_or(|current| current.first() != page.first());
                let changed_page = self
                    .requested_index
                    .is_some_and(|index| index != self.index);
                if changed_directory {
                    self.cursors = vec![page.first()];
                    self.index = 0;
                    self.requested_index = None;
                    self.query.clear();
                }
                if changed_directory {
                    self.endpoint = page::Endpoint::default();
                }
                self.rows = page.rows(None, self.endpoint);
                if changed_directory || changed_page {
                    self.graph = graph::Layout::default();
                }
                if matches!(*page, FlowHubPage::Flows { .. }) {
                    self.graph.sync(&self.rows);
                }
                self.page = Some(*page);
                if let Some(index) = self.requested_index.take() {
                    self.index = index;
                }
                if changed_directory || changed_page {
                    self.selected = 0;
                    self.offset = 0;
                    self.query.clear();
                } else {
                    let filtered = self.filtered();
                    self.selected = selected
                        .and_then(|key| {
                            filtered
                                .iter()
                                .position(|index| self.rows[*index].key == key)
                        })
                        .unwrap_or(self.selected.min(filtered.len().saturating_sub(1)));
                    self.offset = self.offset.min(self.selected);
                }
                self.hits.clear();
                self.float_hits.clear();
                self.hovered = None;
                self.error = None;
            }
            Err(error) => {
                self.requested_index = None;
                self.error = Some(error);
            }
        }
    }

    pub fn failed(&mut self, message: String) {
        self.pending = None;
        self.requested_index = None;
        self.error = Some(message);
    }

    fn filtered(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let links = match &self.page {
            Some(FlowHubPage::Flows {
                relations: Ok(relations),
                ..
            }) => relations.as_slice(),
            _ => &[],
        };
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                query
                    .split_whitespace()
                    .all(|term| {
                        if let Some(kind) = term.strip_prefix("link:") {
                            graph::has_link(row, links, kind)
                        } else {
                            row.title.to_lowercase().contains(term)
                                || row.detail.to_lowercase().contains(term)
                        }
                    })
                    .then_some(i)
            })
            .collect()
    }

    fn focus(&self) -> Action {
        let rows = self.filtered();
        rows.get(self.selected)
            .map(|index| self.rows[*index].action.clone())
            .unwrap_or(Action::None)
    }

    fn float(&self) -> Action {
        match self.focus() {
            Action::Focus(flow) => Action::Float(flow),
            other => other,
        }
    }

    fn page(&mut self, forward: bool) -> Action {
        if self.pending.is_some() {
            return Action::None;
        }
        if matches!(self.page, Some(FlowHubPage::Relations { .. })) {
            let step = self.hits.len().max(1);
            self.selected = if forward {
                (self.selected + step).min(self.filtered().len().saturating_sub(1))
            } else {
                self.selected.saturating_sub(step)
            };
            return Action::None;
        }
        if self.cursors.is_empty() {
            return Action::None;
        }
        let target = if forward {
            let Some(next) = self.page.as_ref().and_then(FlowHubPage::next) else {
                return Action::None;
            };
            self.cursors.truncate(self.index + 1);
            self.cursors.push(next);
            self.index + 1
        } else if self.index > 0 {
            self.index - 1
        } else {
            return Action::None;
        };
        self.requested_index = Some(target);
        Action::Load(self.cursors[target].clone())
    }

    pub fn key(&mut self, key: KeyAction) -> Action {
        if key == KeyAction::Escape {
            if let Some(FlowHubPage::Relations { session, .. }) = self.page {
                return Action::Load(HubRequest::Flows {
                    session,
                    before: None,
                });
            }
            return Action::Close;
        }
        if self.pending.is_some() {
            return Action::None;
        }
        match key {
            KeyAction::Tab | KeyAction::BackTab
                if matches!(self.page, Some(FlowHubPage::Relations { .. })) =>
            {
                let selected = self
                    .filtered()
                    .get(self.selected)
                    .map(|index| self.rows[*index].key.clone());
                self.endpoint = self.endpoint.next();
                if key == KeyAction::BackTab {
                    self.endpoint = self.endpoint.next();
                }
                self.rows = self.page.as_ref().unwrap().rows(None, self.endpoint);
                let filtered = self.filtered();
                self.selected = selected
                    .and_then(|key| {
                        filtered
                            .iter()
                            .position(|index| self.rows[*index].key == key)
                    })
                    .unwrap_or(self.selected.min(filtered.len().saturating_sub(1)));
            }
            KeyAction::NudgePrefill if matches!(self.page, Some(FlowHubPage::Relations { .. })) => {
                let Some(FlowHubPage::Relations { session, focus, .. }) = &self.page else {
                    unreachable!()
                };
                let focus = if focus.is_some() {
                    None
                } else {
                    match self.focus() {
                        Action::Focus(address) => address.instance().cloned(),
                        _ => {
                            self.error = Some("Choose a Flow endpoint to show its links".into());
                            return Action::None;
                        }
                    }
                };
                return Action::Load(HubRequest::Relations {
                    session: *session,
                    focus,
                });
            }
            KeyAction::NudgePrefill if matches!(self.page, Some(FlowHubPage::Flows { .. })) => {
                self.graph_visible = !self.graph_visible;
            }
            KeyAction::RedirectPrefill => {
                return self
                    .cursors
                    .get(self.index)
                    .cloned()
                    .map_or(Action::None, Action::Load);
            }
            KeyAction::Save => return Action::Load(HubRequest::Sessions { before: None }),
            KeyAction::OpenRelations => {
                return match self.page.as_ref() {
                    Some(FlowHubPage::Flows { session, .. }) => {
                        let focus = match self.focus() {
                            Action::Focus(address) => address.instance().cloned(),
                            _ => None,
                        };
                        Action::Load(HubRequest::Relations {
                            session: *session,
                            focus,
                        })
                    }
                    _ => Action::None,
                };
            }
            KeyAction::Submit => return self.focus(),
            KeyAction::ToggleLastTool => return self.float(),
            KeyAction::HistoryUp => self.selected = self.selected.saturating_sub(1),
            KeyAction::HistoryDown => {
                self.selected = (self.selected + 1).min(self.filtered().len().saturating_sub(1))
            }
            KeyAction::PageUp => return self.page(false),
            KeyAction::PageDown => return self.page(true),
            KeyAction::Char(c) => {
                self.query.push(c);
                self.selected = 0;
                self.offset = 0;
            }
            KeyAction::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.offset = 0;
            }
            _ => {}
        }
        Action::None
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Action {
        let point = (event.column, event.row).into();
        self.sessions_hovered = self.sessions.contains(point);
        self.relations_hovered = self.relations.contains(point);
        self.refresh_hovered = self.refresh.contains(point);
        self.graph_hovered = self.graph_button.contains(point);
        self.hovered = self
            .hits
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, index)| *index);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.close.contains(point) => Action::Close,
            _ if self.pending.is_some() => Action::None,
            MouseEventKind::Down(MouseButton::Left) if self.graph_button.contains(point) => {
                self.key(KeyAction::NudgePrefill)
            }
            MouseEventKind::Down(MouseButton::Left) if self.sessions.contains(point) => {
                self.key(KeyAction::Save)
            }
            MouseEventKind::Down(MouseButton::Left) if self.relations.contains(point) => self.key(
                if matches!(self.page, Some(FlowHubPage::Relations { .. })) {
                    KeyAction::Tab
                } else {
                    KeyAction::OpenRelations
                },
            ),
            MouseEventKind::ScrollDown => self.key(KeyAction::HistoryDown),
            MouseEventKind::ScrollUp => self.key(KeyAction::HistoryUp),
            MouseEventKind::Down(MouseButton::Left) if self.previous.contains(point) => {
                self.page(false)
            }
            MouseEventKind::Down(MouseButton::Left) if self.next.contains(point) => self.page(true),
            MouseEventKind::Down(MouseButton::Left) if self.refresh.contains(point) => {
                self.key(KeyAction::RedirectPrefill)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, index)) = self
                    .float_hits
                    .iter()
                    .find(|(rect, _)| rect.contains(point))
                {
                    self.selected = *index;
                    return self.float();
                }
                if let Some(index) = self.hovered {
                    self.selected = index;
                    self.focus()
                } else {
                    Action::None
                }
            }
            _ => Action::None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, focused: &crate::control::ViewAddress) {
        let t = crate::theme::theme();
        let area = frame.area();
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(t.panel_bg.into())),
            area,
        );
        if area.width < 40 || area.height < 24 {
            frame.render_widget(Paragraph::new("FlowHub needs 40×24 · Esc back"), area);
            self.hits.clear();
            self.float_hits.clear();
            self.previous = Rect::default();
            self.next = Rect::default();
            self.close = Rect::default();
            self.refresh = Rect::default();
            self.relations = Rect::default();
            self.sessions = Rect::default();
            self.graph_button = Rect::default();
            return;
        }
        let width = area.width.saturating_sub(4).min(112);
        let content = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + 1,
            width,
            area.height.saturating_sub(2),
        );
        let fg = Style::default().fg(t.heading.into());
        let muted = Style::default().fg(t.meta_fg.into());
        let session = self.page.as_ref().map_or("Loading", FlowHubPage::title);
        let is_flows = matches!(self.page, Some(FlowHubPage::Flows { .. }));
        let is_relations = matches!(self.page, Some(FlowHubPage::Relations { .. }));
        let scoped_links = matches!(
            self.page,
            Some(FlowHubPage::Relations { focus: Some(_), .. })
        );
        frame.render_widget(
            Paragraph::new(format!("FlowHub  /  {session}")).style(fg),
            Rect::new(content.x, content.y, width.saturating_sub(12), 1),
        );
        self.close = Rect::new(content.right() - 10, content.y, 10, 1);
        frame.render_widget(
            Paragraph::new("Back  Esc")
                .style(muted)
                .alignment(Alignment::Right),
            self.close,
        );
        let button_width = if width < 42 { (width - 2) / 2 } else { 20 };
        self.sessions = Rect::new(content.x, content.y + 1, button_width, 3);
        self.refresh = Rect::new(
            content.right() - button_width,
            content.y + 1,
            button_width,
            3,
        );
        self.relations = if is_flows || is_relations {
            Rect::new(
                content.x + width / 2 + 1,
                content.y + 5,
                width - width / 2 - 1,
                3,
            )
        } else {
            Rect::default()
        };
        self.graph_button = if is_flows || is_relations {
            Rect::new(content.x, content.y + 5, width / 2 - 1, 3)
        } else {
            Rect::default()
        };
        for (rect, label, hovered) in [
            (self.sessions, "\nSessions  Ctrl+S", self.sessions_hovered),
            (self.refresh, "\nRefresh  Ctrl+R", self.refresh_hovered),
            (
                self.relations,
                if is_relations {
                    self.endpoint.label()
                } else if width < 72 {
                    "\nLinks Ctrl+⇧G"
                } else {
                    "\nLinks Ctrl+Shift+G"
                },
                self.relations_hovered,
            ),
            (
                self.graph_button,
                if is_relations {
                    if scoped_links {
                        "\nAll Ctrl+G"
                    } else {
                        "\nRelated Ctrl+G"
                    }
                } else if self.graph_visible {
                    "\nList  Ctrl+G"
                } else {
                    "\nGraph  Ctrl+G"
                },
                self.graph_hovered,
            ),
        ] {
            if rect.is_empty() {
                continue;
            }
            frame.render_widget(
                Paragraph::new(label)
                    .style(muted.bg(if hovered {
                        t.work_bg.lerp(t.work_hover_bg, 0.5)
                    } else {
                        t.work_bg.into_inner()
                    }))
                    .alignment(Alignment::Center),
                rect,
            );
        }
        let search_y = content.y + if is_flows || is_relations { 8 } else { 4 };
        frame.render_widget(
            Paragraph::new(format!("Search this page: {}", self.query)).style(muted),
            Rect::new(content.x, search_y, width, 1),
        );
        let rows_y = search_y + 2;
        let footer = content.bottom().saturating_sub(6);
        let rows = self.filtered();
        let graph_visible = is_flows && self.graph_visible;
        let visible = usize::from(footer.saturating_sub(rows_y) / 4);
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + visible {
            self.offset = (self.selected + 1).saturating_sub(visible);
        }
        self.hits.clear();
        self.float_hits.clear();
        let cards = if graph_visible {
            self.graph.cards(
                Rect::new(content.x, rows_y, width, footer.saturating_sub(rows_y + 1)),
                &self.rows,
                &rows,
                self.selected,
            )
        } else {
            rows.iter()
                .enumerate()
                .skip(self.offset)
                .take(visible)
                .map(|(pos, _)| {
                    (
                        pos,
                        Rect::new(
                            content.x,
                            rows_y + ((pos - self.offset) * 4) as u16,
                            width,
                            4,
                        ),
                    )
                })
                .collect()
        };
        if graph_visible {
            graph::draw_links(
                frame,
                &cards,
                self,
                &rows,
                Rect::new(content.x, footer.saturating_sub(1), width, 1),
            );
        }
        for (pos, rect) in cards {
            let row = &self.rows[rows[pos]];
            let width = rect.width;
            let selected = pos == self.selected;
            let bg = if selected {
                t.work_hover_bg.into_inner()
            } else if self.hovered == Some(pos) {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            frame.render_widget(Paragraph::new("").style(Style::default().bg(bg)), rect);
            if selected {
                frame.render_widget(
                    Paragraph::new("▎\n▎\n▎\n▎").style(Style::default().fg(t.accent.into()).bg(bg)),
                    Rect::new(rect.x, rect.y, 1, 4),
                );
            }
            let title = if matches!(&row.action, Action::Focus(address) if address == focused) {
                std::borrow::Cow::Owned(format!("{} · focused", row.title))
            } else {
                std::borrow::Cow::Borrowed(row.title.as_str())
            };
            frame.render_widget(
                Paragraph::new(title).style(fg.bg(bg)),
                Rect::new(rect.x + 2, rect.y + 1, width.saturating_sub(23), 1),
            );
            let open = Rect::new(rect.right().saturating_sub(20), rect.y, 18, 3);
            frame.render_widget(
                Paragraph::new(if matches!(row.action, Action::Focus(_)) {
                    "\nWindow Ctrl+O"
                } else {
                    "\nFlows  Enter"
                })
                .alignment(Alignment::Center)
                .style(muted.bg(bg)),
                open,
            );
            self.float_hits.push((open, pos));
            let detail = if graph_visible {
                std::borrow::Cow::Owned(graph::compact_detail(row))
            } else {
                std::borrow::Cow::Borrowed(row.detail.as_str())
            };
            frame.render_widget(
                Paragraph::new(detail).style(muted.bg(bg)),
                Rect::new(rect.x + 2, rect.y + 2, width.saturating_sub(4), 1),
            );
            self.hits.push((rect, pos));
        }
        let total = if is_relations {
            self.rows.len()
        } else {
            self.page.as_ref().map_or(0, FlowHubPage::total)
        };
        let page_count = total.div_ceil(PAGE_SIZE).max(1);
        let message = self.error.clone().unwrap_or_else(|| {
            if self.pending.is_some() {
                "Loading…".into()
            } else if is_relations {
                format!("Session links · {total} records · {} matches", rows.len())
            } else {
                format!(
                    "Snapshot · {} {} · Page {} / {} · {} matches",
                    total,
                    self.page.as_ref().map_or("flows", FlowHubPage::unit),
                    self.index + 1,
                    page_count,
                    rows.len()
                )
            }
        });
        frame.render_widget(
            Paragraph::new(message).style(muted),
            Rect::new(content.x, footer, width, 1),
        );
        self.previous = Rect::new(content.x, footer + 1, width / 2 - u16::from(width > 1), 3);
        self.next = Rect::new(
            content.x + width / 2 + 1,
            footer + 1,
            width.saturating_sub(width / 2 + 1),
            3,
        );
        for (rect, label) in [
            (
                self.previous,
                if is_relations {
                    "\n‹ Earlier PgUp"
                } else {
                    "\n‹ Previous  PgUp"
                },
            ),
            (
                self.next,
                if is_relations {
                    "\nLater PgDn ›"
                } else {
                    "\nNext  PgDn ›"
                },
            ),
        ] {
            frame.render_widget(
                Paragraph::new(label)
                    .alignment(Alignment::Center)
                    .style(muted.bg(t.work_bg.into())),
                rect,
            );
        }
        frame.render_widget(
            Paragraph::new(if is_flows {
                "↑↓ select · Enter open · Ctrl+O window · Ctrl+G graph/list · Esc back"
            } else if is_relations {
                "↑↓ select · Tab endpoint · Enter open · Ctrl+O window · Esc flows"
            } else {
                "↑↓ select · Enter open · Ctrl+O window · Ctrl+S sessions · Esc back"
            })
            .style(muted),
            Rect::new(content.x, footer + 5, width, 1),
        );
    }
}

pub(crate) fn execution_label(
    state: &atman_rt::InstanceState<atman_runtime::event::AtmanUuid>,
) -> &'static str {
    match state {
        atman_rt::InstanceState::Prepared => "Prepared",
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle) => "Idle",
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_)) => "Running",
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Interrupted { .. }) => {
            "Interrupted"
        }
        atman_rt::InstanceState::Closing { .. } => "Closing",
        atman_rt::InstanceState::Closed => "Closed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_runtime::access::{InstancePage, InstanceSummary},
        flow_store::FlowInstanceId,
    };
    use uuid::Uuid;

    #[test]
    fn neighborhood_navigation_commits_on_reply_and_restores_the_flow_directory() {
        use atman_runtime::flow_store::{
            access::AccessTarget,
            relation::{FlowRelation, RelationKind},
        };
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let observer = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let page = || {
            Box::new(FlowHubPage::Flows {
                session: flow.session_id.0,
                title: "Example".into(),
                default_flow: None,
                page: InstancePage {
                    instances: vec![InstanceSummary {
                        address: flow.clone(),
                        entry: Some("Worker".into()),
                        state: None,
                        unavailable: None,
                        loaded: false,
                        trailing_bytes: None,
                    }],
                    next: None,
                    total: 51,
                    catalog: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "test".into(),
                    },
                },
                relations: Ok(vec![]),
            })
        };
        let links = || {
            Box::new(FlowHubPage::Relations {
                session: flow.session_id.0,
                title: "Links".into(),
                focus: Some(flow.clone()),
                relations: vec![FlowRelation {
                    source: observer.clone(),
                    target: AccessTarget::Flow(flow.clone()),
                    kind: RelationKind::Watch,
                    id: Uuid::now_v7(),
                    active: true,
                }],
            })
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        let mut history = History::default();
        history.receive(&mut hub, Some(&ticket), Ok(page()));
        let cursor = HubRequest::Flows {
            session: flow.session_id.0,
            before: Some(FlowInstanceId::now()),
        };
        hub.cursors.push(cursor.clone());
        hub.index = 1;
        hub.query = "Worker".into();
        hub.graph_visible = true;
        assert!(!hub.leaves_directory(&cursor));
        assert!(
            matches!(hub.key(KeyAction::OpenRelations), Action::Load(HubRequest::Relations { focus: Some(target), .. }) if target == flow)
        );
        hub.pending = Some(ticket.clone());
        history.receive(&mut hub, Some(&ticket), Err("cannot read links".into()));
        assert_eq!(hub.query, "Worker");
        assert!(history.pages.is_empty());
        hub.pending = Some(ticket.clone());
        history.receive(&mut hub, Some(&ticket), Ok(links()));
        assert_eq!(history.pages.len(), 1);
        assert!(hub.query.is_empty());
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&observer))
        );
        hub.key(KeyAction::Tab); // Source.
        hub.key(KeyAction::Tab); // Target.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &flow.clone().into()))
            .unwrap();
        let row = hub.hits[0].0;
        assert!(
            matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: row.x + 1, row: row.y, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Focus(address) if address.instance() == Some(&flow))
        );
        let endpoint = hub.relations;
        hub.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: endpoint.x + 1,
            row: endpoint.y + 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(address) if address.instance() == Some(&observer))
        );
        let Action::Load(back) = hub.key(KeyAction::Escape) else {
            panic!("missing back target")
        };
        assert_eq!(history.request(back), cursor);
        let mut fresh = ticket.clone();
        fresh.request_id = Uuid::now_v7();
        hub.pending = Some(fresh.clone());
        history.receive(&mut hub, Some(&ticket), Ok(page()));
        assert!(matches!(hub.page, Some(FlowHubPage::Relations { .. })));
        history.receive(&mut hub, Some(&fresh), Ok(page()));
        assert_eq!(hub.index, 1);
        assert_eq!(hub.query, "Worker");
        assert!(hub.graph_visible);
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&flow))
        );
    }

    #[test]
    fn refreshed_directory_retains_identity_and_return_state_without_reusing_requests() {
        use crate::control::{ViewAddress, ViewBinding};
        use crate::project_hub::SessionTarget;
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let address = ViewAddress::Instance(flow);
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone(),
            },
            request_id: Uuid::now_v7(),
        };
        let ids: Vec<_> = (0..20).map(|_| Uuid::now_v7()).collect();
        let page = |ids: &[Uuid]| {
            Box::new(FlowHubPage::Sessions(SessionDirectory {
                sessions: ids
                    .iter()
                    .map(|id| SessionRow {
                        target: SessionTarget::Persistent(*id),
                        title: "Same title".into(),
                        detail: id.to_string(),
                    })
                    .collect(),
                next: Some(SessionTarget::Persistent(ids[ids.len() - 1])),
                total: 60,
            }))
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(page(&ids)));
        hub.query = "same".into();
        hub.selected = 7;
        hub.offset = 4;
        let mut refreshed = vec![Uuid::now_v7()];
        refreshed.extend_from_slice(&ids);
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(page(&refreshed)));
        assert_eq!(hub.query, "same");
        assert_eq!(hub.offset, 4);
        assert_eq!(hub.selected, 8);
        assert!(
            matches!(hub.focus(), Action::Load(HubRequest::Flows {session,..}) if session == ids[7])
        );
        assert!(matches!(hub.key(KeyAction::PageDown), Action::Load(_)));
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("retry later".into()));
        assert_eq!(hub.query, "same");
        assert_eq!(hub.selected, 8);
        let mut history = History::default();
        hub.pending = Some(ticket.clone());
        history.save(hub);
        let (mut restored, action) = history.open(Action::Return, address.directory());
        assert!(matches!(action, Action::None));
        assert!(restored.pending.is_none());
        assert_eq!(restored.offset, 4);
        assert_eq!(restored.selected, 8);
        assert_eq!(restored.query, "same");
        restored.receive(Some(&ticket), Ok(page(&ids)));
        assert_eq!(
            restored.selected, 8,
            "a reply from the closed directory cannot mutate its restored view"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|frame| restored.render(frame, &address))
            .unwrap();
        let rect = restored
            .hits
            .iter()
            .find(|(_, index)| *index == restored.selected)
            .unwrap()
            .0;
        assert!(matches!(restored.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row: rect.y + 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }), Action::Load(HubRequest::Flows {session,..}) if session == ids[7]));
        history.save(restored);
        let (restored, action) = history.open(
            Action::Load(HubRequest::Sessions { before: None }),
            address.directory(),
        );
        assert_eq!(restored.query, "same");
        assert!(matches!(
            action,
            Action::Load(HubRequest::Sessions { before: None })
        ));
    }

    #[test]
    fn mixed_session_rows_keep_their_directory_targets_on_keyboard_and_mouse() {
        use crate::{
            control::{ViewAddress, ViewBinding},
            project_hub::SessionTarget,
        };
        let id = Uuid::now_v7();
        let address = ViewAddress::Legacy {
            session: id,
            owner: atman_runtime::event_log::legacy::LegacyFlowKey::Default,
        };
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone(),
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Sessions(SessionDirectory {
                sessions: vec![
                    SessionRow {
                        target: SessionTarget::Persistent(id),
                        title: "Same name".into(),
                        detail: "Persistent".into(),
                    },
                    SessionRow {
                        target: SessionTarget::Legacy(id.to_string()),
                        title: "Same name".into(),
                        detail: "Original history".into(),
                    },
                ],
                next: Some(SessionTarget::Legacy(id.to_string())),
                total: 51,
            }))),
        );
        assert!(
            matches!(hub.key(KeyAction::Submit),Action::Load(HubRequest::Flows {session,..}) if session==id)
        );
        hub.key(KeyAction::HistoryDown);
        assert!(
            matches!(hub.key(KeyAction::Submit),Action::Load(HubRequest::LegacyFlows {session,..}) if session==id)
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal.draw(|f| hub.render(f, &address)).unwrap();
        for (index, (rect, _)) in hub.hits.clone().into_iter().enumerate() {
            let action = hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
            match action {
                Action::Load(HubRequest::Flows { session, .. }) => {
                    assert_eq!(index, 0);
                    assert_eq!(session, id);
                }
                Action::Load(HubRequest::LegacyFlows { session, .. }) => {
                    assert_eq!(index, 1);
                    assert_eq!(session, id);
                }
                _ => panic!("wrong source"),
            }
        }
        assert!(
            matches!(hub.key(KeyAction::PageDown),Action::Load(HubRequest::Sessions {before:Some(SessionTarget::Legacy(cursor))}) if cursor==id.to_string())
        );
    }

    #[test]
    fn legacy_pages_preserve_addresses_across_keyboard_mouse_and_pagination() {
        use crate::control::{ViewAddress, ViewBinding};
        use atman_runtime::event_log::legacy::LegacyFlowKey;
        let session = Uuid::now_v7();
        let owner = LegacyFlowKey::Default;
        let address = ViewAddress::Legacy {
            session,
            owner: owner.clone(),
        };
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone(),
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::LegacyFlows {
                session,
                title: "Original history".into(),
                flows: vec![owner.clone()],
                next: Some(owner.clone()),
                total: 51,
            })),
        );
        assert!(matches!(hub.key(KeyAction::Submit), Action::Focus(target) if target == address));
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(target) if target == address)
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal.draw(|f| hub.render(f, &address)).unwrap();
        assert!(
            hub.page
                .as_ref()
                .unwrap()
                .rows(Some(&address), page::Endpoint::default())[0]
                .title
                .contains("focused")
        );
        for (rect, floating) in [(hub.hits[0].0, false), (hub.float_hits[0].0, true)] {
            for row in rect.y..rect.bottom() {
                let action = hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 1,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                });
                match action {
                    Action::Focus(target) => {
                        assert!(!floating);
                        assert_eq!(target, address);
                    }
                    Action::Float(target) => {
                        assert!(floating);
                        assert_eq!(target, address);
                    }
                    _ => panic!("missing row action"),
                }
            }
        }
        assert!(
            matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::LegacyFlows {session:id, after:Some(key)}) if id==session && key==owner)
        );
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("source unavailable".into()));
        assert_eq!(hub.index, 0);
        assert!(matches!(hub.key(KeyAction::Escape), Action::Close));
    }

    #[test]
    fn session_directory_and_flow_pages_share_navigation_and_ignore_stale_replies() {
        use crate::project_hub::SessionTarget;
        use atman_runtime::flow_runtime::catalog::SessionSummary;
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let session = Uuid::now_v7();
        let page = || {
            Box::new(FlowHubPage::Sessions(SessionDirectory {
                sessions: vec![SessionSummary::unavailable(session, "unavailable fixture").into()],
                next: Some(SessionTarget::Persistent(session)),
                total: 51,
            }))
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(page()));
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Load(HubRequest::Flows { session: id, before: None }) if id == session)
        );
        assert!(
            matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::Sessions { before: Some(SessionTarget::Persistent(id)) }) if id == session)
        );
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(page()));
        assert_eq!(hub.index, 1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|f| hub.render(f, &flow.clone().into()))
            .unwrap();
        let rect = hub.hits[0].0;
        for row in rect.y..rect.bottom() {
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Load(HubRequest::Flows { session: id, .. }) if id == session)
            );
        }
        assert!(matches!(
            hub.key(KeyAction::Save),
            Action::Load(HubRequest::Sessions { before: None })
        ));
        let button = hub.sessions;
        for row in button.y..button.bottom() {
            assert!(matches!(
                hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: button.x + 1,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE
                }),
                Action::Load(HubRequest::Sessions { before: None })
            ));
        }
        hub.query = "keep this filter".into();
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("unreadable session".into()));
        assert_eq!(hub.index, 1);
        assert_eq!(hub.query, "keep this filter");
        hub.pending = Some(ticket.clone());
        let mut stale = ticket.clone();
        stale.request_id = Uuid::now_v7();
        hub.receive(Some(&stale), Ok(page()));
        assert_eq!(hub.query, "keep this filter");
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Flows {
                session,
                title: "Chosen session".into(),
                default_flow: None,
                page: InstancePage {
                    instances: vec![],
                    next: None,
                    total: 0,
                    catalog: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "test".into(),
                    },
                },
                relations: Ok(vec![]),
            })),
        );
        assert_eq!(hub.index, 0);
        assert!(hub.query.is_empty());
        assert_eq!(
            hub.cursors,
            vec![HubRequest::Flows {
                session,
                before: None
            }]
        );
    }

    #[test]
    fn native_hub_pages_commit_on_ack_and_rows_have_equal_keyboard_and_mouse_targets() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let page = || {
            Box::new(FlowHubPage::Flows {
                session: flow.session_id.0,
                title: "Example".into(),
                default_flow: Some(flow.instance_id.clone()),
                page: InstancePage {
                    instances: vec![InstanceSummary {
                        address: flow.clone(),
                        entry: Some("Example flow".into()),
                        state: None,
                        unavailable: None,
                        loaded: false,
                        trailing_bytes: None,
                    }],
                    next: Some(flow.instance_id.clone()),
                    total: 51,
                    catalog: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "test".into(),
                    },
                },
                relations: Ok(vec![]),
            })
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(page()));
        assert!(matches!(
            hub.key(KeyAction::PageDown),
            Action::Load(HubRequest::Flows {
                before: Some(_),
                ..
            })
        ));
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("read failed".into()));
        assert_eq!(hub.index, 0);
        assert!(hub.page.is_some());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|f| hub.render(f, &flow.clone().into()))
            .unwrap();
        let rect = hub.hits[0].0;
        assert_eq!(rect.height, 4);
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(target) if target.instance() == Some(&flow))
        );
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(target) if target.instance() == Some(&flow))
        );
        let window = hub.float_hits[0].0;
        for row in window.y..window.bottom() {
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: window.x + 1, row, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Float(target) if target.instance() == Some(&flow))
            );
        }
        for row in rect.y..rect.bottom() {
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Focus(target) if target.instance() == Some(&flow))
            );
        }
        for width in [40, 60, 76, 120] {
            let mut compact =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
            compact
                .draw(|frame| hub.render(frame, &flow.clone().into()))
                .unwrap();
            let controls = [hub.sessions, hub.refresh, hub.relations];
            for (index, rect) in controls.iter().enumerate() {
                assert_eq!(rect.height, 3);
                assert!(rect.x >= 2 && rect.right() <= width - 2);
                assert!(rect.bottom() <= hub.hits[0].0.y);
                for other in controls.iter().skip(index + 1) {
                    assert!(rect.intersection(*other).is_empty());
                }
            }
            for row in hub.relations.y..hub.relations.bottom() {
                assert!(matches!(hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hub.relations.x + 1,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                }), Action::Load(HubRequest::Relations { session, focus: Some(ref target) }) if session == flow.session_id.0 && target == &flow));
            }
        }
        hub.pending = Some(ticket.clone());
        let mut stale = ticket.clone();
        stale.request_id = Uuid::now_v7();
        hub.receive(Some(&stale), Err("stale".into()));
        assert_eq!(hub.pending.as_ref(), Some(&ticket));
        assert!(matches!(hub.key(KeyAction::Escape), Action::Close));
        let mut tiny = ratatui::Terminal::new(ratatui::backend::TestBackend::new(2, 2)).unwrap();
        tiny.draw(|f| hub.render(f, &flow.clone().into())).unwrap();
        assert!(hub.hits.is_empty());
    }

    #[test]
    fn relation_page_opens_flow_targets_from_keyboard_and_mouse() {
        use atman_runtime::flow_store::relation::{FlowRelation, RelationKind};

        let source = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let target = FlowRef {
            session_id: source.session_id,
            instance_id: FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: source.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Relations {
                session: source.session_id.0,
                title: "Example".into(),
                focus: None,
                relations: vec![FlowRelation {
                    source,
                    target: atman_runtime::flow_store::access::AccessTarget::Flow(target.clone()),
                    kind: RelationKind::Watch,
                    id: Uuid::now_v7(),
                    active: true,
                }],
            })),
        );
        assert!(matches!(
            hub.key(KeyAction::Submit),
            Action::Focus(crate::control::ViewAddress::Instance(address))
                if address.instance_id == target.instance_id
        ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &target.clone().into()))
            .unwrap();
        let row = hub.hits[0].0;
        assert!(matches!(
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: row.x + 1,
                row: row.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
            Action::Focus(crate::control::ViewAddress::Instance(address))
                if address.instance_id == target.instance_id
        ));
    }
}
