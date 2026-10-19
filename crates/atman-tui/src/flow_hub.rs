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

mod page;
pub use page::{FlowHubPage, HubRequest};

#[derive(Clone)]
pub(crate) enum Action {
    None,
    Close,
    Load(HubRequest),
    NewSession,
    Focus(crate::control::ViewAddress),
    Float(crate::control::ViewAddress),
}

#[derive(Default)]
pub(crate) struct FlowHub {
    pub pending: Option<ControlTicket>,
    page: Option<FlowHubPage>,
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
    sessions: Rect,
    sessions_hovered: bool,
    hovered: Option<usize>,
}

impl FlowHub {
    pub fn receive(
        &mut self,
        ticket: Option<&ControlTicket>,
        result: Result<Box<FlowHubPage>, String>,
    ) {
        if ticket != self.pending.as_ref() || ticket.is_none() {
            return;
        }
        self.pending = None;
        match result {
            Ok(page) => {
                if self
                    .page
                    .as_ref()
                    .is_none_or(|current| current.first() != page.first())
                {
                    self.cursors = vec![page.first()];
                    self.index = 0;
                    self.requested_index = None;
                    self.query.clear();
                }
                self.page = Some(*page);
                if let Some(index) = self.requested_index.take() {
                    self.index = index;
                }
                self.selected = 0;
                self.offset = 0;
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
        self.page
            .as_ref()
            .map(|page| {
                page.rows(None)
                    .iter()
                    .enumerate()
                    .filter_map(|(i, row)| {
                        (row.title.to_lowercase().contains(&query)
                            || row.detail.to_lowercase().contains(&query))
                        .then_some(i)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn focus(&self) -> Action {
        let rows = self.filtered();
        self.page
            .as_ref()
            .and_then(|page| {
                rows.get(self.selected)
                    .map(|index| page.rows(None)[*index].action.clone())
            })
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
        self.query.clear();
        Action::Load(self.cursors[target].clone())
    }

    pub fn key(&mut self, key: KeyAction) -> Action {
        if key == KeyAction::Escape {
            return Action::Close;
        }
        if self.pending.is_some() {
            return Action::None;
        }
        match key {
            KeyAction::RedirectPrefill => {
                return self
                    .cursors
                    .get(self.index)
                    .cloned()
                    .map_or(Action::None, Action::Load);
            }
            KeyAction::Save => return Action::Load(HubRequest::Sessions { before: None }),
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
        self.hovered = self
            .hits
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, index)| *index);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.close.contains(point) => Action::Close,
            _ if self.pending.is_some() => Action::None,
            MouseEventKind::Down(MouseButton::Left) if self.sessions.contains(point) => {
                self.key(KeyAction::Save)
            }
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
        if area.width < 40 || area.height < 20 {
            frame.render_widget(Paragraph::new("FlowHub needs 40×20 · Esc back"), area);
            self.hits.clear();
            self.float_hits.clear();
            self.previous = Rect::default();
            self.next = Rect::default();
            self.close = Rect::default();
            self.refresh = Rect::default();
            self.sessions = Rect::default();
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
        frame.render_widget(
            Paragraph::new(format!("FlowHub  /  {session}")).style(fg),
            Rect::new(content.x, content.y, width.saturating_sub(34), 1),
        );
        self.refresh = Rect::new(
            content.right().saturating_sub(32),
            content.y,
            18.min(width),
            1,
        );
        frame.render_widget(Paragraph::new("Refresh  Ctrl+R").style(muted), self.refresh);
        self.close = Rect::new(
            content.right().saturating_sub(12),
            content.y,
            12.min(width),
            1,
        );
        frame.render_widget(
            Paragraph::new("Back  Esc")
                .style(muted)
                .alignment(Alignment::Right),
            self.close,
        );
        self.sessions = Rect::new(content.x, content.y + 1, 20, 3);
        frame.render_widget(
            Paragraph::new("\nSessions  Ctrl+S")
                .style(muted.bg(if self.sessions_hovered {
                    t.work_bg.lerp(t.work_hover_bg, 0.5)
                } else {
                    t.work_bg.into_inner()
                }))
                .alignment(Alignment::Center),
            self.sessions,
        );
        frame.render_widget(
            Paragraph::new(format!("Search this page: {}", self.query)).style(muted),
            Rect::new(content.x + 22, content.y + 2, width.saturating_sub(22), 1),
        );
        let rows = self.filtered();
        let visible = usize::from(content.height.saturating_sub(11) / 4);
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + visible {
            self.offset = (self.selected + 1).saturating_sub(visible);
        }
        self.hits.clear();
        self.float_hits.clear();
        let page_rows = self
            .page
            .as_ref()
            .map(|page| page.rows(Some(focused)))
            .unwrap_or_default();
        for (pos, index) in rows.iter().enumerate().skip(self.offset).take(visible) {
            let row = &page_rows[*index];
            let rect = Rect::new(
                content.x,
                content.y + 4 + ((pos - self.offset) * 4) as u16,
                width,
                4,
            );
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
            frame.render_widget(
                Paragraph::new(row.title.as_str()).style(fg.bg(bg)),
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
            frame.render_widget(
                Paragraph::new(row.detail.as_str()).style(muted.bg(bg)),
                Rect::new(rect.x + 2, rect.y + 2, width.saturating_sub(4), 1),
            );
            self.hits.push((rect, pos));
        }
        let footer = content.bottom().saturating_sub(6);
        let total = self.page.as_ref().map_or(0, FlowHubPage::total);
        let page_count = total.div_ceil(PAGE_SIZE).max(1);
        let message = self.error.clone().unwrap_or_else(|| {
            if self.pending.is_some() {
                "Loading…".into()
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
            (self.previous, "\n‹ Previous  PgUp"),
            (self.next, "\nNext  PgDn ›"),
        ] {
            frame.render_widget(
                Paragraph::new(label)
                    .alignment(Alignment::Center)
                    .style(muted.bg(t.work_bg.into())),
                rect,
            );
        }
        frame.render_widget(
            Paragraph::new("↑↓ select · Enter open · Ctrl+O window · Ctrl+S sessions · Esc back")
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
            hub.page.as_ref().unwrap().rows(Some(&address))[0]
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
        use atman_runtime::flow_runtime::catalog::{SessionPage, SessionSummary};
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
            Box::new(FlowHubPage::Sessions(SessionPage {
                sessions: vec![SessionSummary::unavailable(session, "unavailable fixture")],
                next: Some(session),
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
            matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::Sessions { before: Some(id) }) if id == session)
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
}
