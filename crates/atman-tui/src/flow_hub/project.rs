//! Session project edits retain the reviewed catalog version and never rebind a flow.

use std::path::PathBuf;

use atman_runtime::event_log::journal::CommitReceipt;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Paragraph, Wrap},
};
use uuid::Uuid;

use super::Action;
use crate::{
    input::InputEditor,
    keys::KeyAction,
    settings_form::{self, Field, FieldHit, FormLayout},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionProjectChange {
    pub session: Uuid,
    pub expected: CommitReceipt,
    pub project: Option<PathBuf>,
}

pub struct SessionProjectPage {
    session: Uuid,
    expected: CommitReceipt,
    title: String,
    path: InputEditor,
    selected: usize,
    hit: Option<FieldHit>,
    buttons: Vec<Rect>,
    hovered: Option<usize>,
}

impl SessionProjectPage {
    pub fn new(
        session: Uuid,
        expected: CommitReceipt,
        title: String,
        project: Option<PathBuf>,
    ) -> Self {
        let mut path = InputEditor::default();
        if let Some(project) = project {
            path.replace_with(&project.to_string_lossy());
        }
        Self {
            session,
            expected,
            title,
            path,
            selected: 0,
            hit: None,
            buttons: vec![],
            hovered: None,
        }
    }

    pub(super) fn session(&self) -> Uuid {
        self.session
    }

    fn submit(&self) -> Action {
        if self.hit.is_none() || self.buttons.is_empty() {
            return Action::None;
        }
        let path = self.path.buf().trim();
        Action::ChangeSessionProject(Box::new(SessionProjectChange {
            session: self.session,
            expected: self.expected.clone(),
            project: (!path.is_empty()).then(|| PathBuf::from(path)),
        }))
    }

    pub(super) fn key(&mut self, key: KeyAction, pending: bool) -> Action {
        if key == KeyAction::Escape {
            return Action::Return;
        }
        if pending {
            return Action::None;
        }
        match key {
            KeyAction::Save => return self.submit(),
            KeyAction::Tab | KeyAction::HistoryDown => self.selected = (self.selected + 1) % 3,
            KeyAction::BackTab | KeyAction::HistoryUp => self.selected = (self.selected + 2) % 3,
            KeyAction::Submit if self.selected == 2 => return Action::Return,
            KeyAction::Submit => return self.submit(),
            KeyAction::Newline => {}
            _ if self.selected == 0 => {
                self.path.handle_key(&key);
            }
            _ => {}
        }
        Action::None
    }

    pub(super) fn paste(&mut self, text: &str, pending: bool) {
        if !pending && self.selected == 0 {
            self.path.paste_single_line(text);
        }
    }

    pub(super) fn mouse(&mut self, event: MouseEvent, pending: bool) -> Action {
        let point = (event.column, event.row).into();
        self.hovered = self
            .hit
            .as_ref()
            .filter(|hit| hit.record.contains(point))
            .map(|_| 0)
            .or_else(|| {
                self.buttons
                    .iter()
                    .position(|rect| rect.contains(point))
                    .map(|i| i + 1)
            });
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return Action::None;
        }
        let Some(selected) = self.hovered else {
            return Action::None;
        };
        if selected == 2 {
            return Action::Return;
        }
        if pending {
            return Action::None;
        }
        self.selected = selected;
        if selected == 1 {
            return self.submit();
        }
        if let Some(hit) = self.hit.filter(|hit| hit.value.contains(point)) {
            self.path.set_cursor_by_display(
                0,
                event
                    .column
                    .saturating_sub(hit.value.x)
                    .saturating_add(hit.offset as u16),
            );
        }
        Action::None
    }

    pub(super) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        pending: bool,
        error: Option<&str>,
    ) {
        self.hit = None;
        self.buttons.clear();
        if area.width < 40 || area.height < 16 {
            frame.render_widget(Paragraph::new("Project form needs 40×16 · Esc back"), area);
            return;
        }
        let t = crate::theme::theme();
        let width = area.width.saturating_sub(4).min(100);
        let height = area.height.saturating_sub(2).min(16);
        let content = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(
            Paragraph::new("Session / Project").style(Style::default().fg(t.heading.into())),
            Rect::new(content.x + 1, content.y, width - 2, 1),
        );
        frame.render_widget(
            Paragraph::new(format!("{} · {}", self.title, self.session))
                .style(Style::default().fg(t.meta_fg.into())),
            Rect::new(content.x + 1, content.y + 1, width - 2, 1),
        );
        let layout = FormLayout::with_actions(
            Rect::new(content.x, content.y + 2, width, height - 2),
            1,
            self.selected,
            2,
        );
        for (_, rect) in layout.fields {
            self.hit = Some(settings_form::render_field(
                frame,
                rect,
                &Field {
                    label: "Project directory",
                    value: self.path.buf(),
                    placeholder: "Blank removes the project association",
                    focused: self.selected == 0 && !pending,
                    hovered: self.hovered == Some(0) && !pending,
                    cursor: Some(self.path.cursor_display_col()),
                    selector: false,
                    locked: pending,
                },
                &t,
            ));
        }
        let info = if pending {
            "Saving project association…"
        } else {
            error.unwrap_or(
                "Existing flows keep their project.\nNew flows use this project.\nBlank removes the association."
        )
        };
        frame.render_widget(
            Paragraph::new(info)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(t.meta_fg.into())),
            layout.info,
        );
        for (index, rect) in layout.actions.iter().copied().enumerate() {
            settings_form::render_action(
                frame,
                rect,
                &settings_form::Action {
                    label: if index == 0 { "Save" } else { "Back" },
                    shortcut: if index == 0 { "Ctrl+S" } else { "Esc" },
                    focused: self.selected == index + 1 && !pending,
                    hovered: self.hovered == Some(index + 1) && !pending,
                    disabled: pending && index == 0,
                },
                &t,
            );
        }
        self.buttons = layout.actions;
        frame.render_widget(
            Paragraph::new("Tab focus · Ctrl+S save · Esc back")
                .style(Style::default().fg(t.meta_fg.into())),
            layout.help,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage},
    };
    use crossterm::event::KeyModifiers;

    #[test]
    fn project_form_keeps_receipt_draft_and_click_geometry_until_accepted() {
        let session = Uuid::now_v7();
        let expected = CommitReceipt {
            journal_id: Uuid::now_v7(),
            sequence: 5,
            digest: "a".repeat(64),
        };
        let ticket = ControlTicket {
            history_head: None,
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: atman_runtime::flow_store::FlowRef {
                    session_id: session.into(),
                    instance_id: atman_rt::FlowInstanceId::now(),
                }
                .into(),
            },
            request_id: Uuid::now_v7(),
        };
        for (width, height) in [(120, 40), (40, 24)] {
            let mut hub = FlowHub {
                pending: Some(ticket.clone()),
                ..Default::default()
            };
            hub.receive(
                Some(&ticket),
                Ok(Box::new(FlowHubPage::SessionProject(Box::new(
                    SessionProjectPage::new(session, expected.clone(), "Container".into(), None),
                )))),
            );
            hub.paste("/projects/目标");
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| hub.render(frame, &ticket.binding.flow))
                .unwrap();
            let Action::ChangeSessionProject(keyboard) = hub.key(KeyAction::Save) else {
                panic!("save missing")
            };
            assert_eq!(
                *keyboard,
                SessionProjectChange {
                    session,
                    expected: expected.clone(),
                    project: Some("/projects/目标".into())
                }
            );
            let Some(FlowHubPage::SessionProject(page)) = &hub.page else {
                unreachable!()
            };
            let save = page.buttons[0];
            assert_eq!(save.height, 3);
            let click = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: save.x + 1,
                row: save.y + 1,
                modifiers: KeyModifiers::NONE,
            };
            assert!(
                matches!(hub.mouse(click), Action::ChangeSessionProject(change) if change == keyboard)
            );
            hub.pending = Some(ticket.clone());
            assert!(matches!(hub.key(KeyAction::Save), Action::None));
            assert!(matches!(hub.mouse(click), Action::None));
            hub.paste("must not append");
            let mut stale = ticket.clone();
            stale.request_id = Uuid::now_v7();
            hub.receive(Some(&stale), Err("wrong request".into()));
            assert!(hub.pending.is_some());
            hub.receive(Some(&ticket), Err("Session catalog changed".into()));
            assert!(hub.pending.is_none());
            assert!(
                matches!(hub.key(KeyAction::Save), Action::ChangeSessionProject(change) if change == keyboard)
            );
            assert!(matches!(hub.key(KeyAction::Escape), Action::Return));
            terminal.backend_mut().resize(20, 8);
            terminal
                .draw(|frame| hub.render(frame, &ticket.binding.flow))
                .unwrap();
            assert!(matches!(hub.key(KeyAction::Save), Action::None));
            hub.pending = Some(ticket.clone());
            hub.receive(
                Some(&ticket),
                Ok(Box::new(FlowHubPage::Sessions(
                    crate::flow_hub::SessionDirectory {
                        sessions: vec![],
                        next: None,
                        total: 0,
                    },
                ))),
            );
            assert!(matches!(hub.page, Some(FlowHubPage::Sessions(_))));
        }
    }
}
