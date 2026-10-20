//! Session removal review uses the same directory records and tracked replies as browsing.

use atman_runtime::flow_store::SessionRemoval;

use super::{
    Action, HubRequest,
    page::{Row, RowKey},
};

pub struct SessionRemovalPage {
    pub request: SessionRemoval,
    pub title: String,
    pub detail: String,
    pub cleanup: bool,
}

impl SessionRemovalPage {
    pub(super) fn rows(&self) -> Vec<Row> {
        vec![
            Row {
                key: RowKey::RemovalField("summary"),
                title: self.title.clone(),
                detail: format!("{} · {}", self.request.session_id, self.detail),
                action: Action::None,
            },
            Row {
                key: RowKey::RemovalField("effects"),
                title: if self.cleanup { "Finish interrupted cleanup" } else { "Permanently delete this session" }.into(),
                detail: "Removes this session's flow histories and artifacts. Other sessions, project files and shared knowledge remain. All flows must be closed and the session detached; execution is never stopped by deletion.".into(),
                action: Action::None,
            },
            Row {
                key: RowKey::RemovalField("back"),
                title: "Back · Esc".into(),
                detail: "Return to sessions".into(),
                action: Action::Load(HubRequest::Sessions { before: None }),
            },
            Row {
                key: RowKey::RemovalField("delete"),
                title: if self.cleanup { "Finish cleanup · Enter" } else { "Delete session · Enter" }.into(),
                detail: "Uses this confirmed catalog version. A changed session requires a fresh review.".into(),
                action: Action::RemoveSession(Box::new(self.request.clone())),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, SessionDirectory, SessionRow},
        keys::KeyAction,
        project_hub::SessionTarget,
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use uuid::Uuid;

    #[test]
    fn session_removal_requires_review_and_tracks_keyboard_mouse_and_reply_ownership() {
        let id = Uuid::now_v7();
        let address = atman_runtime::flow_store::FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        }
        .into();
        let mut ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address,
            },
            request_id: Uuid::now_v7(),
        };
        let directory = || {
            Box::new(FlowHubPage::Sessions(SessionDirectory {
                sessions: vec![SessionRow {
                    target: SessionTarget::Persistent(id),
                    title: "Example".into(),
                    detail: "Closed".into(),
                    cleanup_pending: false,
                }],
                next: None,
                total: 1,
            }))
        };
        let request = SessionRemoval {
            session_id: id,
            expected: atman_runtime::event_log::journal::CommitReceipt {
                journal_id: id,
                sequence: 1,
                digest: "a".repeat(64),
            },
        };
        let review = || {
            Box::new(FlowHubPage::SessionRemoval(Box::new(SessionRemovalPage {
                request: request.clone(),
                title: "Example".into(),
                detail: "Closed · no running flows".into(),
                cleanup: false,
            })))
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(directory()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 45)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &ticket.binding.flow))
            .unwrap();
        assert!(
            matches!(hub.key(KeyAction::Delete), Action::Load(HubRequest::SessionRemoval { session }) if session == id)
        );
        let button = hub.controls_button;
        assert_eq!(button.height, 3);
        let click = |x, y| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(
            matches!(hub.mouse(click(button.x + 1, button.y + 1)), Action::Load(HubRequest::SessionRemoval { session }) if session == id)
        );
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(review()));
        // Arrival never selects the destructive row, even when Enter is repeated.
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        hub.key(KeyAction::Escape);
        for _ in 0..3 {
            hub.key(KeyAction::HistoryDown);
        }
        terminal
            .draw(|frame| hub.render(frame, &ticket.binding.flow))
            .unwrap();
        assert!(matches!(hub.float(), Action::None));
        let Action::RemoveSession(sent) = hub.key(KeyAction::Submit) else {
            panic!("missing confirmed removal");
        };
        assert_eq!(*sent, request);
        let (hit, _) = hub
            .hits
            .iter()
            .find(|(_, index)| matches!(hub.rows[*index].action, Action::RemoveSession(_)))
            .unwrap();
        assert!(hit.height >= 3);
        assert!(
            matches!(hub.mouse(click(hit.x + 1, hit.y + 1)), Action::RemoveSession(sent) if *sent == request)
        );
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        let mut stale = ticket.clone();
        stale.request_id = Uuid::now_v7();
        hub.receive(Some(&stale), Ok(directory()));
        assert!(hub.pending.is_some());
        hub.receive(Some(&ticket), Err("Session is still attached".into()));
        assert!(matches!(hub.page, Some(FlowHubPage::SessionRemoval(_))));
        assert!(hub.error.as_deref().unwrap().contains("still attached"));
        let mut tiny = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 8)).unwrap();
        tiny.draw(|frame| hub.render(frame, &ticket.binding.flow))
            .unwrap();
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        terminal
            .draw(|frame| hub.render(frame, &ticket.binding.flow))
            .unwrap();
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::RemoveSession(sent) if *sent == request)
        );
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(directory()));
        assert!(matches!(hub.page, Some(FlowHubPage::Sessions(_))));
        assert!(hub.error.is_none());
    }
}
