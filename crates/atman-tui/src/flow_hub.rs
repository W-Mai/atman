//! A paged instance directory. Reading and selecting rows never controls execution.

use atman_runtime::{
    flow_runtime::access::InstancePage,
    flow_store::{FlowInstanceId, FlowRef},
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::Paragraph,
};

use crate::{control::ControlTicket, keys::KeyAction};

pub const PAGE_SIZE: usize = 50;

pub struct FlowHubPage {
    pub page: InstancePage,
    pub session: String,
    pub default_flow: Option<FlowInstanceId>,
}

pub(crate) enum Action {
    None,
    Close,
    Load(Option<FlowInstanceId>),
    Focus(FlowRef),
}

#[derive(Default)]
pub(crate) struct FlowHub {
    pub pending: Option<ControlTicket>,
    page: Option<FlowHubPage>,
    cursors: Vec<Option<FlowInstanceId>>,
    index: usize,
    requested_index: Option<usize>,
    selected: usize,
    offset: usize,
    query: String,
    error: Option<String>,
    hits: Vec<(Rect, usize)>,
    previous: Rect,
    next: Rect,
    close: Rect,
    refresh: Rect,
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
                page.page
                    .instances
                    .iter()
                    .enumerate()
                    .filter_map(|(i, row)| {
                        (row.entry
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&query)
                            || row.address.instance_id.to_string().contains(&query))
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
                    .map(|index| &page.page.instances[*index])
            })
            .map(|row| Action::Focus(row.address.clone()))
            .unwrap_or(Action::None)
    }

    fn page(&mut self, forward: bool) -> Action {
        if self.pending.is_some() {
            return Action::None;
        }
        if self.cursors.is_empty() {
            self.cursors.push(None);
        }
        let target = if forward {
            let Some(next) = self.page.as_ref().and_then(|page| page.page.next.clone()) else {
                return Action::None;
            };
            self.cursors.truncate(self.index + 1);
            self.cursors.push(Some(next));
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
                return Action::Load(self.cursors.get(self.index).cloned().flatten());
            }
            KeyAction::Submit => return self.focus(),
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
        self.hovered = self
            .hits
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, index)| *index);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.close.contains(point) => Action::Close,
            _ if self.pending.is_some() => Action::None,
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

    pub fn render(&mut self, frame: &mut Frame, focused: &FlowRef) {
        let t = crate::theme::theme();
        let area = frame.area();
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(t.panel_bg.into())),
            area,
        );
        if area.width < 40 || area.height < 20 {
            frame.render_widget(Paragraph::new("FlowHub needs 40×20 · Esc back"), area);
            self.hits.clear();
            self.previous = Rect::default();
            self.next = Rect::default();
            self.close = Rect::default();
            self.refresh = Rect::default();
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
        let session = self.page.as_ref().map_or("Loading", |page| &page.session);
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
        frame.render_widget(
            Paragraph::new(format!("Search this page: {}", self.query)).style(muted),
            Rect::new(content.x, content.y + 2, width, 1),
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
        for (pos, index) in rows.iter().enumerate().skip(self.offset).take(visible) {
            let row = &self.page.as_ref().unwrap().page.instances[*index];
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
            let title = row.entry.as_deref().unwrap_or("Unavailable flow");
            let marker = if &row.address == focused {
                " · focused"
            } else if self.page.as_ref().unwrap().default_flow.as_ref()
                == Some(&row.address.instance_id)
            {
                " · default"
            } else {
                ""
            };
            frame.render_widget(
                Paragraph::new(format!("{title}{marker}")).style(fg.bg(bg)),
                Rect::new(rect.x + 2, rect.y + 1, width.saturating_sub(4), 1),
            );
            let status = row.state.as_ref().map_or("Unavailable", |state| {
                if !row.loaded {
                    "Detached"
                } else {
                    execution_label(state.state())
                }
            });
            let paused = row
                .state
                .as_ref()
                .is_some_and(|state| !state.pauses().is_empty());
            let detail = row.unavailable.clone().unwrap_or_else(|| {
                format!(
                    "{} · {status}{}",
                    row.address.instance_id,
                    if paused { " · execution paused" } else { "" }
                )
            });
            frame.render_widget(
                Paragraph::new(detail).style(muted.bg(bg)),
                Rect::new(rect.x + 2, rect.y + 2, width.saturating_sub(4), 1),
            );
            self.hits.push((rect, pos));
        }
        let footer = content.bottom().saturating_sub(6);
        let total = self.page.as_ref().map_or(0, |page| page.page.total);
        let page_count = total.div_ceil(PAGE_SIZE).max(1);
        let message = self.error.clone().unwrap_or_else(|| {
            if self.pending.is_some() {
                "Loading…".into()
            } else {
                format!(
                    "Snapshot · {} flows · Page {} / {} · {} matches",
                    total,
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
            Paragraph::new("↑↓ select · Enter / click focus · Esc back · :flows / Ctrl+P FlowHub")
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
    use atman_runtime::{event_log::journal::CommitReceipt, flow_runtime::access::InstanceSummary};
    use uuid::Uuid;

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
                flow: flow.clone(),
            },
            request_id: Uuid::now_v7(),
        };
        let page = || {
            Box::new(FlowHubPage {
                session: "Example".into(),
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
            Action::Load(Some(_))
        ));
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("read failed".into()));
        assert_eq!(hub.index, 0);
        assert!(hub.page.is_some());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal.draw(|f| hub.render(f, &flow)).unwrap();
        let rect = hub.hits[0].0;
        assert_eq!(rect.height, 4);
        assert!(matches!(hub.key(KeyAction::Submit), Action::Focus(target) if target == flow));
        for row in rect.y..rect.bottom() {
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Focus(target) if target == flow)
            );
        }
        hub.pending = Some(ticket.clone());
        let mut stale = ticket.clone();
        stale.request_id = Uuid::now_v7();
        hub.receive(Some(&stale), Err("stale".into()));
        assert_eq!(hub.pending.as_ref(), Some(&ticket));
        assert!(matches!(hub.key(KeyAction::Escape), Action::Close));
        let mut tiny = ratatui::Terminal::new(ratatui::backend::TestBackend::new(2, 2)).unwrap();
        tiny.draw(|f| hub.render(f, &flow)).unwrap();
        assert!(hub.hits.is_empty());
    }
}
