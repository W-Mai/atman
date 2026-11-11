//! Explicit checkout removal uses an immutable resource snapshot and host acknowledgement.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Frame, layout::Rect, style::Style, widgets::Paragraph};

use super::{Action, HubRequest, text::TextDetails, workspaces::WorkspaceRemoval};
use crate::{keys::KeyAction, settings_form};

pub(super) struct Confirmation {
    request: WorkspaceRemoval,
    reader: TextDetails,
    error: Option<String>,
    selected: usize,
    hovered: Option<usize>,
    buttons: [Rect; 4],
}

impl Confirmation {
    pub fn new(request: WorkspaceRemoval) -> Self {
        let reader = Self::reader(&request, None);
        Self {
            request,
            reader,
            error: None,
            selected: 3,
            hovered: None,
            buttons: [Rect::default(); 4],
        }
    }

    fn reader(request: &WorkspaceRemoval, error: Option<&str>) -> TextDetails {
        let mut paragraphs = Vec::new();
        if let Some(error) = error {
            paragraphs.push(format!(
                "Removal not confirmed: {error}\nRefresh before retrying.\n"
            ));
        }
        paragraphs.extend([
            format!("Checkout: {}\n", request.expected.worktree_path.display()),
            format!("Repository: {}\n", request.location.repository.display()),
            "Removes the checkout directory and its Git worktree registration. Named branches and Flow history remain.\n".into(),
            "Detached commits can become unreachable. Create a branch before removal if those commits must be kept.\n".into(),
            "Force is off by default. Turning it on discards uncommitted files in this checkout.\n".into(),
            "Live Flow references and invocation leases prevent removal. Changed records require a refresh and another confirmation.".into(),
        ]);
        if let Some(intent) = &request.expected.release {
            paragraphs.push(format!(
                "\nPending release {} · Force {} is fixed for this request.",
                intent.request_id,
                if intent.force { "on" } else { "off" }
            ));
        }
        TextDetails::new("Worktrees / Remove checkout".into(), paragraphs)
    }

    pub fn key(&mut self, key: KeyAction, pending: bool) -> Action {
        if key == KeyAction::Escape {
            return Action::Return;
        }
        if !pending {
            match key {
                KeyAction::Tab => self.selected = (self.selected + 1) % 4,
                KeyAction::BackTab => self.selected = (self.selected + 3) % 4,
                KeyAction::RedirectPrefill => return self.refresh(),
                KeyAction::Submit | KeyAction::Char(' ') => match self.selected {
                    0 if self.request.expected.release.is_none() && !self.buttons[0].is_empty() => {
                        self.request.force = !self.request.force
                    }
                    1 if !self.buttons[1].is_empty() => {
                        return Action::ReleaseWorkspace(Box::new(self.request.clone()));
                    }
                    2 => return self.refresh(),
                    3 => return Action::Return,
                    _ => {}
                },
                _ => {
                    self.reader.key(key);
                }
            }
        } else {
            self.reader.key(key);
        }
        Action::None
    }

    fn refresh(&self) -> Action {
        Action::Load(HubRequest::WorkspaceRecord {
            location: self.request.location.clone(),
            id: self.request.expected.id.clone(),
        })
    }

    pub fn mouse(&mut self, event: MouseEvent, pending: bool) -> Action {
        self.hovered = self
            .buttons
            .iter()
            .position(|rect| rect.contains((event.column, event.row).into()));
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(index) = self.hovered
        {
            if index == 3 {
                return Action::Return;
            }
            self.selected = index;
            return self.key(KeyAction::Submit, pending);
        }
        if self.reader.mouse(event) {
            return Action::Return;
        }
        Action::None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, pending: bool, error: Option<&str>) {
        self.buttons = [Rect::default(); 4];
        if area.width < 40 || area.height < 24 {
            self.reader.back = Rect::default();
            frame.render_widget(Paragraph::new("Removal needs 40×24 · Esc back"), area);
            return;
        }
        let area = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(34) / 2,
            area.width,
            area.height.min(34),
        );
        if self.error.as_deref() != error {
            self.error = error.map(str::to_owned);
            self.reader = Self::reader(&self.request, error);
        }
        let t = crate::theme::theme();
        self.reader.render(
            frame,
            Rect::new(area.x, area.y, area.width, area.height - 6),
        );
        self.buttons[3] = self.reader.back;
        let width = area.width.saturating_sub(4).min(100);
        let left = area.x + (area.width - width) / 2;
        let cell = (width - 2) / 3;
        for index in 0..3 {
            self.buttons[index] =
                Rect::new(left + index as u16 * (cell + 1), area.bottom() - 6, cell, 3);
        }
        for (index, rect) in self.buttons.iter().copied().enumerate() {
            let label = match index {
                0 if self.request.force => "Force Yes",
                0 => "Force No",
                1 if pending => "Working",
                1 => "Remove",
                2 => "Refresh",
                _ => "Back Esc",
            };
            settings_form::render_action(
                frame,
                rect,
                &settings_form::Action {
                    label,
                    focused: self.selected == index,
                    hovered: self.hovered == Some(index),
                    disabled: (pending && index != 3)
                        || (index == 0 && self.request.expected.release.is_some()),
                    ..Default::default()
                },
                &t,
            );
        }
        frame.render_widget(
            Paragraph::new(
                "Tab / Shift+Tab · Enter / Space\nCtrl+R refresh · ↑↓ scroll · Esc back",
            )
            .style(Style::default().fg(t.meta_fg.into())),
            Rect::new(left, area.bottom() - 2, width, 2),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, WorkspaceLocation, WorkspaceRecordPage},
    };
    use atman_runtime::{
        flow_store::FlowRef,
        git_workspace::{WorkspaceCheckout, WorkspaceInspection, WorkspaceRecord},
    };

    #[test]
    fn removal_confirmation_preserves_snapshot_and_waits_for_its_acknowledgement() {
        let flow = FlowRef {
            session_id: uuid::Uuid::new_v4().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        let mut ticket = ControlTicket {
            history_head: None,
            binding: ViewBinding {
                view_id: uuid::Uuid::new_v4(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: uuid::Uuid::new_v4(),
        };
        let location = WorkspaceLocation {
            repository: "/repository".into(),
            storage: "/storage".into(),
        };
        let record = WorkspaceRecord {
            id: "checkout".into(),
            repository_root: location.repository.clone(),
            worktree_path: location
                .storage
                .join(format!(".atman/worktrees/{}-END", "长路径/".repeat(80))),
            branch: None,
            owner_session: None,
            owner_flow: None,
            state: "retained".into(),
            retained: true,
            release: None,
            allocation_base: None,
            allocation_policy: None,
            lease: None,
            reconciled_at: None,
            reconciliation_reason: None,
            instances: Vec::new(),
        };
        let page = |record| {
            Box::new(FlowHubPage::WorkspaceRecord(Box::new(
                WorkspaceRecordPage {
                    location: location.clone(),
                    workspace: WorkspaceInspection {
                        record,
                        checkout: WorkspaceCheckout::Available {
                            head: "a".repeat(40),
                            branch: None,
                            dirty: true,
                        },
                    },
                },
            )))
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(page(record.clone())));
        hub.selected = hub
            .rows
            .iter()
            .position(|row| matches!(row.action, Action::ReviewWorkspaceRelease(_)))
            .unwrap();
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        assert!(!hub.release_workspace.as_ref().unwrap().request.force);
        // Enter starts on Back, never on the destructive action.
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        assert!(hub.release_workspace.is_none());
        hub.key(KeyAction::Submit);
        for width in [40, 120] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| hub.render(frame, &flow.clone().into()))
                .unwrap();
            let confirmation = hub.release_workspace.as_ref().unwrap();
            assert!(confirmation.buttons.iter().all(|rect| rect.height == 3));
            let force = confirmation.buttons[0];
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: force.x + 1,
                row: force.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
        }
        assert!(!hub.release_workspace.as_ref().unwrap().request.force);
        hub.key(KeyAction::Submit); // Toggle force on the selected field.
        hub.key(KeyAction::Tab); // Remove.
        let mut tiny = ratatui::Terminal::new(ratatui::backend::TestBackend::new(39, 23)).unwrap();
        tiny.draw(|frame| hub.render(frame, &flow.clone().into()))
            .unwrap();
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        let mut wide = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 65)).unwrap();
        wide.draw(|frame| hub.render(frame, &flow.clone().into()))
            .unwrap();
        assert!(hub.release_workspace.as_ref().unwrap().buttons[3].y > 10);
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::ReleaseWorkspace(request) if request.force && request.expected == record && request.location == location)
        );
        ticket.request_id = uuid::Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        let mut stale = ticket.clone();
        stale.request_id = uuid::Uuid::new_v4();
        hub.receive(Some(&stale), Ok(page(record.clone())));
        assert!(hub.release_workspace.is_some() && hub.pending.is_some());
        hub.receive(Some(&ticket), Err("snapshot changed".into()));
        assert!(hub.release_workspace.is_some() && hub.pending.is_none());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &flow.clone().into()))
            .unwrap();
        assert_eq!(
            hub.release_workspace.as_ref().unwrap().error.as_deref(),
            Some("snapshot changed")
        );
        hub.paste("must not change hidden search");
        assert!(hub.query.is_empty());
        assert!(matches!(
            hub.key(KeyAction::RedirectPrefill),
            Action::Load(HubRequest::WorkspaceRecord { .. })
        ));
        ticket.request_id = uuid::Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        let mut released = record;
        released.state = "released".into();
        released.retained = false;
        hub.receive(Some(&ticket), Ok(page(released)));
        assert!(hub.release_workspace.is_none());
        assert!(
            !hub.rows
                .iter()
                .any(|row| matches!(row.action, Action::ReviewWorkspaceRelease(_)))
        );
    }
}
