//! Flow properties use catalog receipts independently of execution controls.

use atman_runtime::{event_log::journal::CommitReceipt, flow_store::FlowRef};

use super::{
    Action,
    page::{Row, RowKey},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlowCatalogSelection {
    pub flow: FlowRef,
    pub expected: CommitReceipt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlowArchiveChange {
    pub selection: FlowCatalogSelection,
    pub archived: bool,
}

pub struct FlowDetailsPage {
    pub selection: FlowCatalogSelection,
    pub title: String,
    pub recorded_state: atman_rt::InstanceState<atman_runtime::event::AtmanUuid>,
    pub is_default: bool,
    pub archived: bool,
}

impl FlowDetailsPage {
    pub(super) fn rows(&self) -> Vec<Row> {
        let flow = &self.selection.flow;
        vec![
            Row {
                key: RowKey::FlowField("identity"),
                title: self.title.clone(),
                detail: format!("{}/{} · Recorded {}", flow.session_id, flow.instance_id, super::execution_label(&self.recorded_state)),
                action: Action::None,
            },
            Row {
                key: RowKey::FlowField("focus"),
                title: "Open flow · Enter".into(),
                detail: "View the conversation; Ctrl+O opens a floating view".into(),
                action: Action::Focus(flow.clone().into()),
            },
            Row {
                key: RowKey::FlowField("default"),
                title: if self.is_default { "Default flow" } else { "Set as default · Enter" }.into(),
                detail: "Initial view when opening this Session. Existing views, execution and permissions remain unchanged.".into(),
                action: if self.is_default { Action::None } else { Action::SelectDefault(Box::new(self.selection.clone())) },
            },
            Row {
                key: RowKey::FlowField("archive"),
                title: if self.archived { "Unarchive flow · Enter" } else if matches!(self.recorded_state, atman_rt::InstanceState::Closed) { "Archive flow · Enter" } else { "Archive unavailable" }.into(),
                detail: if self.archived { "Remove the archive marker. Execution stays closed." } else { "Only closed flows can be archived. History remains readable; search Archived in the directory." }.into(),
                action: if self.archived || matches!(self.recorded_state, atman_rt::InstanceState::Closed) {
                    Action::ArchiveFlow(Box::new(FlowArchiveChange { selection: self.selection.clone(), archived: !self.archived }))
                } else { Action::None },
            },
            Row {
                key: RowKey::FlowField("project"),
                title: "Session project · Enter".into(),
                detail: "Change container membership. Existing flows keep their project and worktrees.".into(),
                action: Action::Load(super::HubRequest::SessionProject { session: flow.session_id.0 }),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, HubRequest},
        keys::KeyAction,
    };
    use atman_runtime::flow_runtime::access::{InstancePage, InstanceSummary};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use uuid::Uuid;

    #[test]
    fn archive_controls_share_padding_request_ownership_and_disabled_guards() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        let selection = FlowCatalogSelection {
            flow: flow.clone(),
            expected: CommitReceipt {
                journal_id: Uuid::now_v7(),
                sequence: 2,
                digest: "x".repeat(64),
            },
        };
        for (width, height) in [(120, 45), (40, 24)] {
            for (state, archived) in [
                (atman_rt::InstanceState::Prepared, false),
                (atman_rt::InstanceState::Closed, false),
                (atman_rt::InstanceState::Closed, true),
            ] {
                let enabled = matches!(state, atman_rt::InstanceState::Closed);
                let mut ticket = ControlTicket {
                    binding: ViewBinding {
                        view_id: Uuid::now_v7(),
                        revision: 1,
                        flow: flow.clone().into(),
                    },
                    request_id: Uuid::now_v7(),
                };
                let mut hub = FlowHub {
                    pending: Some(ticket.clone()),
                    ..Default::default()
                };
                hub.receive(
                    Some(&ticket),
                    Ok(Box::new(FlowHubPage::FlowDetails(Box::new(
                        FlowDetailsPage {
                            selection: selection.clone(),
                            title: "Task".into(),
                            recorded_state: state,
                            is_default: false,
                            archived,
                        },
                    )))),
                );
                for _ in 0..3 {
                    hub.key(KeyAction::HistoryDown);
                }
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal
                    .draw(|frame| hub.render(frame, &ticket.binding.flow))
                    .unwrap();
                assert!(matches!(hub.float(), Action::None));
                let matches_change = |action: Action| matches!(action, Action::ArchiveFlow(change) if change.selection == selection && change.archived != archived);
                assert_eq!(matches_change(hub.key(KeyAction::Submit)), enabled);
                let (rect, _) = hub
                    .hits
                    .iter()
                    .find(|(_, index)| hub.rows[*index].key == RowKey::FlowField("archive"))
                    .unwrap();
                assert!(rect.height >= 3);
                assert_eq!(
                    matches_change(hub.mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: rect.x + 1,
                        row: rect.y + 1,
                        modifiers: KeyModifiers::NONE
                    })),
                    enabled
                );
                ticket.request_id = Uuid::now_v7();
                hub.pending = Some(ticket.clone());
                assert!(matches!(hub.key(KeyAction::Submit), Action::None));
                let mut stale = ticket.clone();
                stale.request_id = Uuid::now_v7();
                hub.receive(Some(&stale), Err("stale response".into()));
                assert!(hub.pending.is_some());
                hub.receive(Some(&ticket), Err("catalog changed".into()));
                assert!(hub.error.is_some());
                assert_eq!(matches_change(hub.key(KeyAction::Submit)), enabled);
                terminal.backend_mut().resize(12, 8);
                terminal
                    .draw(|frame| hub.render(frame, &ticket.binding.flow))
                    .unwrap();
                assert!(matches!(hub.key(KeyAction::Submit), Action::None));
            }
        }
    }

    #[test]
    fn default_selection_shares_directory_targets_and_original_receipts() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        let selection = FlowCatalogSelection {
            flow: flow.clone(),
            expected: CommitReceipt {
                journal_id: Uuid::now_v7(),
                sequence: 4,
                digest: "a".repeat(64),
            },
        };
        let directory = || {
            Box::new(FlowHubPage::Flows {
                session: flow.session_id.0,
                title: "Session".into(),
                default_flow: None,
                relations: Ok(vec![]),
                page: InstancePage {
                    catalog: selection.expected.clone(),
                    next: None,
                    total: 1,
                    instances: vec![InstanceSummary {
                        name: None,
                        archived: false,
                        address: flow.clone(),
                        entry: Some("Worker".into()),
                        state: Some(atman_rt::FlowInstance::prepared(flow.clone())),
                        unavailable: None,
                        loaded: false,
                        trailing_bytes: Some(0),
                        activity: None,
                        observation: None,
                    }],
                },
            })
        };
        let details = |is_default| {
            Box::new(FlowHubPage::FlowDetails(Box::new(FlowDetailsPage {
                selection: selection.clone(),
                title: "Worker".into(),
                recorded_state: atman_rt::InstanceState::Prepared,
                is_default,
                archived: false,
            })))
        };
        let click = |x, y| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        for (width, height) in [(120, 45), (40, 24)] {
            let mut ticket = ControlTicket {
                binding: ViewBinding {
                    view_id: Uuid::now_v7(),
                    revision: 1,
                    flow: flow.clone().into(),
                },
                request_id: Uuid::now_v7(),
            };
            let mut hub = FlowHub {
                pending: Some(ticket.clone()),
                ..Default::default()
            };
            hub.receive(Some(&ticket), Ok(directory()));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| hub.render(frame, &ticket.binding.flow))
                .unwrap();
            let rect = hub.details_button;
            assert!(rect.height >= 3);
            assert!(
                matches!(hub.key(KeyAction::CoursePrefill), Action::Load(HubRequest::FlowDetails { owner }) if owner == flow)
            );
            assert!(
                matches!(hub.mouse(click(rect.x + 1, rect.y + 1)), Action::Load(HubRequest::FlowDetails { owner }) if owner == flow)
            );
            ticket.request_id = Uuid::now_v7();
            hub.pending = Some(ticket.clone());
            hub.receive(Some(&ticket), Ok(details(false)));
            assert!(matches!(hub.key(KeyAction::Escape), Action::Return));
            hub.key(KeyAction::HistoryDown);
            assert!(
                matches!(hub.float(), Action::Float(address) if address.instance() == Some(&flow))
            );
            hub.key(KeyAction::HistoryDown);
            terminal
                .draw(|frame| hub.render(frame, &ticket.binding.flow))
                .unwrap();
            assert!(matches!(hub.float(), Action::None));
            assert!(
                matches!(hub.key(KeyAction::Submit), Action::SelectDefault(change) if *change == selection)
            );
            let (hit, _) = hub
                .hits
                .iter()
                .find(|(_, i)| matches!(hub.rows[*i].action, Action::SelectDefault(_)))
                .unwrap();
            assert!(hit.height >= 3);
            assert!(
                matches!(hub.mouse(click(hit.x + 1, hit.y + 1)), Action::SelectDefault(change) if *change == selection)
            );
            ticket.request_id = Uuid::now_v7();
            hub.pending = Some(ticket.clone());
            assert!(matches!(hub.key(KeyAction::Submit), Action::None));
            let mut stale = ticket.clone();
            stale.request_id = Uuid::now_v7();
            hub.receive(Some(&stale), Ok(details(true)));
            assert!(hub.pending.is_some());
            hub.receive(Some(&ticket), Err("Session catalog changed".into()));
            assert!(matches!(hub.page, Some(FlowHubPage::FlowDetails(_))));
            assert!(hub.error.is_some());
            hub.pending = Some(ticket.clone());
            hub.receive(Some(&ticket), Ok(details(true)));
            assert!(
                !hub.rows
                    .iter()
                    .any(|row| matches!(row.action, Action::SelectDefault(_)))
            );
            assert!(hub.error.is_none());
        }
    }
}
