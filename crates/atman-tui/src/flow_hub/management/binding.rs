use atman_runtime::event_log::journal::CommitReceipt;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Frame, layout::Rect, style::Style, widgets::Paragraph};
use uuid::Uuid;

use super::ManagementPage;
use crate::{
    flow_hub::{Action, HubRequest, page::RowKey, text::TextDetails},
    keys::KeyAction,
    project_hub::SessionTarget,
    settings_form,
};

/// A Session selection against an acknowledged profile revision.
#[derive(Clone)]
pub struct ManagementSelection {
    pub request_id: Uuid,
    pub expected: CommitReceipt,
    pub session_id: Uuid,
}

pub(in crate::flow_hub) struct Draft {
    expected: CommitReceipt,
    previous: Option<Uuid>,
    grants: usize,
    destination: Option<Uuid>,
    request: Option<ManagementSelection>,
    picking: bool,
    selected: usize,
    reader: TextDetails,
    error: Option<String>,
    hovered: Option<usize>,
    buttons: [Rect; 3],
}

impl Draft {
    pub fn new(page: &ManagementPage) -> Self {
        let mut draft = Self {
            expected: page.view.profile.cursor.clone(),
            previous: page
                .view
                .profile
                .management
                .as_ref()
                .map(|binding| binding.session.id),
            grants: page
                .view
                .profile
                .management
                .as_ref()
                .map_or(0, |binding| binding.grants.len()),
            destination: None,
            request: None,
            picking: false,
            selected: 0,
            reader: TextDetails::new(String::new(), vec![]),
            error: None,
            hovered: None,
            buttons: [Rect::default(); 3],
        };
        draft.update_reader();
        draft
    }

    fn update_reader(&mut self) {
        let mut paragraphs = vec![
            format!("Current session\n{}\n", self.previous.map_or_else(|| "Not configured".into(), |id| id.to_string())),
            format!("Selected session\n{}\n", self.destination.map_or_else(|| "Choose an existing persistent Session".into(), |id| id.to_string())),
            format!("Saving removes all {} explicit profile grants. Permissions are not transferred to the selected Session. Its own Session grants remain.\n", self.grants),
            "Existing Sessions and Flows stay open. No execution is started, resumed, paused or cancelled.".into(),
        ];
        if let Some(error) = &self.error {
            paragraphs.insert(0, format!("Not saved: {error}\n"));
        }
        self.reader = TextDetails::new("Management / Change session".into(), paragraphs);
    }

    pub fn picking(&self) -> bool {
        self.picking
    }

    pub fn picker_request(&self) -> Option<HubRequest> {
        self.picking
            .then_some(HubRequest::Sessions { before: None })
    }

    pub fn choose(&mut self, key: &RowKey) -> Result<bool, String> {
        if !self.picking {
            return Ok(false);
        }
        let RowKey::Session(SessionTarget::Persistent(id)) = key else {
            return Err("Select an existing persistent Session".into());
        };
        if Some(*id) == self.previous {
            return Err("This Session is already bound; its grants will not be changed".into());
        }
        if self.destination != Some(*id) {
            self.destination = Some(*id);
            self.request = None;
        }
        self.picking = false;
        self.error = None;
        self.selected = 1;
        self.update_reader();
        Ok(true)
    }

    pub fn key(&mut self, key: KeyAction, pending: bool) -> Action {
        if key == KeyAction::Escape {
            if self.picking {
                self.picking = false;
                return Action::None;
            }
            return Action::Load(HubRequest::Management { grant: None });
        }
        if pending {
            self.reader.key(key);
            return Action::None;
        }
        match key {
            KeyAction::Tab => self.selected = (self.selected + 1) % 3,
            KeyAction::BackTab => self.selected = (self.selected + 2) % 3,
            KeyAction::Save => return self.submit(),
            KeyAction::Submit | KeyAction::Char(' ') => match self.selected {
                0 => {
                    self.picking = true;
                    return Action::Load(self.picker_request().unwrap());
                }
                1 => return self.submit(),
                _ => return self.key(KeyAction::Escape, false),
            },
            _ => {
                self.reader.key(key);
            }
        }
        Action::None
    }

    fn submit(&mut self) -> Action {
        let Some(session_id) = self.destination else {
            return Action::None;
        };
        let request = self.request.get_or_insert_with(|| ManagementSelection {
            request_id: Uuid::now_v7(),
            expected: self.expected.clone(),
            session_id,
        });
        Action::SelectManagement(Box::new(request.clone()))
    }

    pub fn mouse(&mut self, event: MouseEvent, pending: bool) -> Action {
        self.hovered = self
            .buttons
            .iter()
            .position(|rect| rect.contains((event.column, event.row).into()));
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(index) = self.hovered
        {
            if index == 2 {
                return self.key(KeyAction::Escape, pending);
            }
            if !pending {
                self.selected = index;
                return self.key(KeyAction::Submit, false);
            }
            return Action::None;
        }
        if self.reader.mouse(event) {
            return self.key(KeyAction::Escape, pending);
        }
        Action::None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, pending: bool, error: Option<&str>) {
        self.buttons = [Rect::default(); 3];
        self.reader.back = Rect::default();
        if area.width < 40 || area.height < 24 {
            frame.render_widget(
                Paragraph::new("Session selection needs 40×24 · Esc back"),
                area,
            );
            return;
        }
        if self.error.as_deref() != error {
            self.error = error.map(str::to_owned);
            self.update_reader();
        }
        let area = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(38) / 2,
            area.width,
            area.height.min(38),
        );
        self.reader.render(
            frame,
            Rect::new(area.x, area.y, area.width, area.height - 6),
        );
        self.buttons[2] = self.reader.back;
        let width = area.width.saturating_sub(4).min(100);
        let left = area.x + (area.width - width) / 2;
        let cell = (width - 1) / 2;
        self.buttons[0] = Rect::new(left, area.bottom() - 6, cell, 3);
        self.buttons[1] = Rect::new(left + cell + 1, area.bottom() - 6, cell, 3);
        let theme = crate::theme::theme();
        for (index, rect) in self.buttons.iter().copied().enumerate() {
            settings_form::render_action(
                frame,
                rect,
                &settings_form::Action {
                    label: match index {
                        0 => "Choose",
                        1 if pending => "Saving",
                        1 => "Save",
                        _ => "Back",
                    },
                    shortcut: match index {
                        1 => "Ctrl+S",
                        2 => "Esc",
                        _ => "Enter",
                    },
                    focused: self.selected == index,
                    hovered: self.hovered == Some(index),
                    disabled: index != 2 && (pending || index == 1 && self.destination.is_none()),
                },
                &theme,
            );
        }
        frame.render_widget(
            Paragraph::new(if pending {
                "Waiting for host acknowledgement"
            } else {
                "Tab action · Enter select · Ctrl+S save\n↑↓ PgUp/Dn scroll · Esc back"
            })
            .style(Style::default().fg(theme.meta_fg.into())),
            Rect::new(left, area.bottom() - 2, width, 2),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, History, SessionDirectory, SessionRow},
    };
    use atman_runtime::{
        flow_runtime::host::ProfileView,
        flow_store::{
            FlowInstanceId, FlowRef, SessionDescriptor,
            profile::{ManagementBinding, ProfileSnapshot},
        },
    };
    use crossterm::event::KeyModifiers;

    #[test]
    fn binding_selection_preserves_retry_identity_and_original_view_acknowledgement() {
        let previous = Uuid::now_v7();
        let destination = Uuid::now_v7();
        let view = ProfileView {
            unavailable: Some("The bound Session is missing".into()),
            profile: ProfileSnapshot {
                profile_id: Uuid::now_v7(),
                cursor: CommitReceipt {
                    journal_id: Uuid::now_v7(),
                    sequence: 3,
                    digest: "snapshot".into(),
                },
                management: Some(ManagementBinding {
                    session: SessionDescriptor {
                        id: previous,
                        title: "Management".into(),
                        project: None,
                        configuration: Default::default(),
                    },
                    grants: vec![],
                }),
            },
        };
        let mut ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: FlowRef {
                    session_id: previous.into(),
                    instance_id: FlowInstanceId::now(),
                }
                .into(),
            },
            request_id: Uuid::now_v7(),
        };
        let page = || {
            Box::new(FlowHubPage::Management(Box::new(
                ManagementPage::new(view.clone(), None).unwrap(),
            )))
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        let mut history = History::default();
        history.receive(&mut hub, Some(&ticket), Ok(page()));
        hub.query = "Change session".into();
        hub.key(KeyAction::Submit);
        assert!(matches!(hub.key(KeyAction::Save), Action::None));
        assert!(matches!(
            hub.key(KeyAction::Submit),
            Action::Load(HubRequest::Sessions { .. })
        ));
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Sessions(SessionDirectory {
                sessions: [previous, destination]
                    .into_iter()
                    .map(|id| SessionRow {
                        cleanup_pending: false,
                        target: SessionTarget::Persistent(id),
                        title: id.to_string(),
                        detail: "Session".into(),
                    })
                    .collect(),
                next: None,
                total: 2,
            }))),
        );
        assert_eq!(hub.filtered().len(), 2);
        assert!(matches!(hub.key(KeyAction::CoursePrefill), Action::None));
        hub.key(KeyAction::Submit);
        assert!(hub.error.as_deref().unwrap().contains("already bound"));
        assert!(hub.edit_management.as_ref().unwrap().picking());
        hub.key(KeyAction::HistoryDown);
        hub.key(KeyAction::Submit);
        let Action::SelectManagement(first) = hub.key(KeyAction::Save) else {
            panic!("missing selection")
        };
        assert_eq!(first.session_id, destination);
        assert_eq!(first.expected, view.profile.cursor);
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        assert!(matches!(hub.key(KeyAction::Save), Action::None));
        history.receive(&mut hub, Some(&ticket), Err("Profile changed".into()));
        let Action::SelectManagement(retry) = hub.key(KeyAction::Save) else {
            panic!("missing retry")
        };
        assert_eq!(retry.request_id, first.request_id);
        // Mouse and keyboard share the same request, including in a narrow viewport.
        let super::super::Draft::Binding(draft) = hub.edit_management.as_mut().unwrap() else {
            panic!("missing form")
        };
        for (width, height) in [(160, 52), (40, 24)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draft.render(frame, frame.area(), false, None))
                .unwrap();
            let button = draft.buttons[1];
            assert_eq!(button.height, 3);
            let Action::SelectManagement(clicked) = draft.mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: button.x + 1,
                    row: button.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
                false,
            ) else {
                panic!("missing mouse save")
            };
            assert_eq!(clicked.request_id, first.request_id);
        }
        hub.pending = Some(ticket.clone());
        let mut stale = ticket.clone();
        stale.request_id = Uuid::now_v7();
        history.receive(&mut hub, Some(&stale), Ok(page()));
        assert!(hub.edit_management.is_some());
        history.receive(&mut hub, Some(&ticket), Ok(page()));
        assert!(hub.edit_management.is_none());
    }
}
