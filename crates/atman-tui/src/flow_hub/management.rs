use atman_runtime::flow_runtime::host::ProfileView;
use atman_runtime::flow_store::{
    access::{AccessTarget, FlowAction},
    profile::ReplaceManagement,
};
use uuid::Uuid;

mod edit;
pub(super) use edit::Draft;

use super::{
    Action, HubRequest,
    page::{Row, RowKey},
};

/// An acknowledged profile snapshot. Mutation identity remains fixed until refresh.
pub struct ManagementPage {
    pub view: ProfileView,
    pub grant: Option<Uuid>,
    revoke: Option<ReplaceManagement>,
}

impl ManagementPage {
    pub fn new(view: ProfileView, grant: Option<Uuid>) -> Result<Self, String> {
        let revoke = if let Some(id) = grant {
            let mut binding = view
                .profile
                .management
                .clone()
                .ok_or("Management is not configured")?;
            let index = binding
                .grants
                .iter()
                .position(|grant| grant.id == id)
                .ok_or("Grant is no longer present; return to Management and refresh")?;
            binding.grants.remove(index);
            Some(ReplaceManagement {
                request_id: Uuid::now_v7(),
                expected: view.profile.cursor.clone(),
                binding,
            })
        } else {
            None
        };
        Ok(Self {
            view,
            grant,
            revoke,
        })
    }

    pub(super) fn total(&self) -> usize {
        usize::from(self.view.unavailable.is_some())
            + if self.grant.is_some() {
                8
            } else {
                self.view
                    .profile
                    .management
                    .as_ref()
                    .map_or(2, |binding| 4 + binding.grants.len())
            }
    }

    pub(super) fn rows(&self) -> Vec<Row> {
        let mut rows = vec![field(
            "Profile",
            format!(
                "{} · revision {}",
                self.view.profile.profile_id, self.view.profile.cursor.sequence
            ),
        )];
        if let Some(error) = &self.view.unavailable {
            rows.push(field("Authority unavailable", error.clone()));
        }
        let Some(binding) = &self.view.profile.management else {
            rows.push(Row {
                key: RowKey::ManagementField("Initialize"),
                title: "Open management".into(),
                detail: "Create a projectless Session · no cross-session grants".into(),
                action: if self.view.unavailable.is_none() {
                    Action::OpenManagement
                } else {
                    Action::None
                },
            });
            return rows;
        };
        if let Some(id) = self.grant {
            let grant = binding.grants.iter().find(|grant| grant.id == id).unwrap();
            rows.extend([
                Row { key: RowKey::ManagementField("Edit"), title: "Edit grant".into(), detail: "Change source, target or permissions".into(), action: if self.view.unavailable.is_none() { Action::EditManagement } else { Action::None } },
                field("Grant", grant.id.to_string()),
                field("Source flow", format!("Flow {}\nSession {}", grant.grantee.instance_id, grant.grantee.session_id)),
                field("Target", target(&grant.target)),
                field("Permissions", grant.actions.iter().map(|action| action_label(*action)).collect::<Vec<_>>().join(" · ")),
                field("Revocation effect", "Remove only this explicit grant. Other grants and same-session access remain. Execution is not cancelled.".into()),
                Row {
                    key: RowKey::ManagementField("Revoke"),
                    title: "Revoke grant".into(),
                    detail: "Save removal against this profile revision".into(),
                    action: if self.view.unavailable.is_none() {
                        Action::UpdateManagement(Box::new(self.revoke.as_ref().unwrap().clone()))
                    } else { Action::None },
                },
            ]);
        } else {
            rows.push(Row {
                key: RowKey::ManagementField("Session"),
                title: "Management session".into(),
                detail: binding.session.id.to_string(),
                action: Action::Load(HubRequest::Flows {
                    session: binding.session.id,
                    before: None,
                }),
            });
            rows.push(field(
                "Explicit grants",
                format!(
                    "{} grants · Session membership alone grants no cross-session access",
                    binding.grants.len()
                ),
            ));
            rows.push(Row {
                key: RowKey::ManagementField("New"),
                title: "New grant".into(),
                detail: "Choose a Flow, target scope and permissions".into(),
                action: if self.view.unavailable.is_none() {
                    Action::EditManagement
                } else {
                    Action::None
                },
            });
            rows.extend(binding.grants.iter().map(|grant| Row {
                key: RowKey::ManagementGrant(grant.id),
                title: format!("Flow {}", grant.grantee.instance_id),
                detail: format!(
                        "{}\n{}\nGrant {}",
                        target(&grant.target),
                        grant
                            .actions
                            .iter()
                            .map(|action| action_label(*action))
                            .collect::<Vec<_>>()
                            .join(" · "),
                        grant.id
                    ),
                action: Action::Load(HubRequest::Management {
                    grant: Some(grant.id),
                }),
            }));
        }
        rows
    }
}

fn field(name: &'static str, detail: String) -> Row {
    Row {
        key: RowKey::ManagementField(name),
        title: name.into(),
        detail,
        action: Action::None,
    }
}

fn target(target: &AccessTarget) -> String {
    match target {
        AccessTarget::Flow(flow) => {
            format!("Session {}\nFlow {}", flow.session_id, flow.instance_id)
        }
        AccessTarget::Session(id) => format!("Session {id} · all flows"),
        AccessTarget::AllSessions => "All sessions on this host".into(),
    }
}

fn action_label(action: FlowAction) -> &'static str {
    match action {
        FlowAction::Discover => "Discover",
        FlowAction::ReadOutput => "Read output",
        FlowAction::ReadContext => "Read context",
        FlowAction::Send => "Send messages",
        FlowAction::Watch => "Watch",
        FlowAction::Control => "Control execution",
        FlowAction::ManageGoal => "Manage goal",
        FlowAction::Delegate => "Delegate authority",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage},
        keys::KeyAction,
    };
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_store::{
            FlowInstanceId, FlowRef, SessionDescriptor,
            access::FlowGrant,
            profile::{ManagementBinding, ProfileSnapshot},
        },
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    #[test]
    fn management_controls_preserve_snapshot_retry_and_view_ownership() {
        let owner = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let grant = FlowGrant {
            id: Uuid::now_v7(),
            grantee: owner.clone(),
            target: AccessTarget::AllSessions,
            actions: [
                FlowAction::Discover,
                FlowAction::ReadContext,
                FlowAction::Control,
            ]
            .into(),
        };
        let retained = FlowGrant {
            id: Uuid::now_v7(),
            target: AccessTarget::Session(Uuid::now_v7()),
            ..grant.clone()
        };
        let view = ProfileView {
            unavailable: None,
            profile: ProfileSnapshot {
                profile_id: Uuid::now_v7(),
                cursor: CommitReceipt {
                    journal_id: Uuid::now_v7(),
                    sequence: 3,
                    digest: "snapshot".into(),
                },
                management: Some(ManagementBinding {
                    session: SessionDescriptor {
                        id: owner.session_id.0,
                        project: None,
                        configuration: Default::default(),
                        title: "Atman".into(),
                    },
                    grants: vec![grant.clone(), retained.clone()],
                }),
            },
        };
        let address = owner.into();
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address,
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Management(Box::new(
                ManagementPage::new(view.clone(), Some(grant.id)).unwrap(),
            )))),
        );
        hub.query = "Source flow".into();
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        assert!(hub.property_details.is_some());
        hub.key(KeyAction::Escape);
        hub.query = "Revoke grant".into();
        let Action::UpdateManagement(change) = hub.key(KeyAction::Submit) else {
            panic!("missing revoke")
        };
        assert_eq!(change.expected, view.profile.cursor);
        assert_eq!(change.binding.grants, vec![retained]);
        for (width, height) in [(40, 24), (100, 40)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| hub.render(frame, &ticket.binding.flow))
                .unwrap();
            let (rect, _) = hub.hits[0];
            assert_eq!(rect.height, 4);
            let Action::UpdateManagement(clicked) = hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            }) else {
                panic!("missing row hit")
            };
            assert_eq!(clicked, change);
        }
        hub.pending = Some(ticket.clone());
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        hub.receive(Some(&ticket), Err("Profile revision changed".into()));
        let Action::UpdateManagement(retry) = hub.key(KeyAction::Submit) else {
            panic!("lost retry")
        };
        assert_eq!(retry, change);
        let current_ticket = ControlTicket {
            request_id: Uuid::now_v7(),
            ..ticket.clone()
        };
        hub.pending = Some(current_ticket.clone());
        let mut updated = view.clone();
        updated.profile.management = Some(change.binding);
        let response = || {
            Ok(Box::new(FlowHubPage::Management(Box::new(
                ManagementPage::new(updated.clone(), None).unwrap(),
            ))))
        };
        hub.receive(Some(&ticket), response());
        assert_eq!(hub.pending.as_ref(), Some(&current_ticket));
        hub.receive(Some(&current_ticket), response());
        assert!(hub.error.is_none());
        assert!(
            matches!(hub.page, Some(FlowHubPage::Management(ref page)) if page.grant.is_none())
        );
        assert!(ManagementPage::new(updated, Some(grant.id)).is_err());

        let mut unavailable = view;
        unavailable.unavailable = Some("Profile unavailable".into());
        let page = ManagementPage::new(unavailable, Some(grant.id)).unwrap();
        assert!(
            page.rows()
                .iter()
                .all(|row| !matches!(row.action, Action::UpdateManagement(_)))
        );
    }
}
