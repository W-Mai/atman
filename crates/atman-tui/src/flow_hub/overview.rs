//! Progressive cross-Session nodes use the shared directory and focus actions.

use atman_runtime::flow_runtime::catalog::{
    FlowCatalogCursor, FlowCatalogEntry, OverviewCatalogPage, OverviewExecution,
};
use atman_runtime::flow_store::relation::FlowRelation;

use super::{
    Action, HubRequest, execution_label,
    page::{Row, RowKey},
};

pub struct OverviewDirectory {
    pub after: Option<FlowCatalogCursor>,
    pub page: OverviewCatalogPage,
    pub(super) relations: Vec<FlowRelation>,
}

impl OverviewDirectory {
    pub fn new(after: Option<FlowCatalogCursor>, page: OverviewCatalogPage) -> Self {
        let mut directory = Self {
            after,
            page,
            relations: Vec::new(),
        };
        directory.collect_relations();
        directory
    }

    fn collect_relations(&mut self) {
        self.relations = self
            .page
            .entries
            .iter()
            .filter_map(|entry| match entry {
                FlowCatalogEntry::Flow {
                    snapshot: Ok(snapshot),
                    ..
                } => Some(&snapshot.relations),
                _ => None,
            })
            .flatten()
            .cloned()
            .collect();
        self.relations.sort_by_key(|edge| {
            (
                format!("{:?}", edge.source),
                format!("{:?}", edge.target),
                edge.kind,
                edge.id,
            )
        });
        self.relations.dedup();
    }

    pub(super) fn append(&mut self, previous: Option<&mut Self>) -> Result<(), String> {
        let Some(after) = &self.after else {
            return Ok(());
        };
        let Some(old) = previous else {
            return Err("Overview changed; refresh to restart".into());
        };
        if old.page.next.as_ref() != Some(after) {
            return Err("Overview cursor changed; refresh to restart".into());
        }
        let batch = std::mem::replace(
            &mut self.page.entries,
            std::mem::take(&mut old.page.entries),
        );
        self.page.entries.extend(batch);
        self.page.inspected += old.page.inspected;
        self.collect_relations();
        Ok(())
    }

    pub(super) fn next(&self) -> Option<HubRequest> {
        self.page
            .next
            .clone()
            .map(|after| HubRequest::Overview { after: Some(after) })
    }

    pub(super) fn status(&self) -> String {
        let flows = self
            .page
            .entries
            .iter()
            .filter(|entry| matches!(entry, FlowCatalogEntry::Flow { .. }))
            .count();
        let errors = self
            .page
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    FlowCatalogEntry::SessionUnavailable { .. }
                        | FlowCatalogEntry::Flow {
                            snapshot: Err(_),
                            ..
                        }
                )
            })
            .count();
        format!(
            "{flows} flows loaded · {} sessions listed · {errors} errors · {}",
            self.page.total_sessions,
            if self.page.next.is_some() {
                "More PgDn"
            } else {
                "End of snapshot"
            }
        )
    }

    pub(super) fn rows(&self) -> Vec<Row> {
        self.page
            .entries
            .iter()
            .map(|entry| match entry {
                FlowCatalogEntry::SessionUnavailable { session_id, error } => Row {
                    key: RowKey::OverviewFailure(*session_id),
                    title: "Unavailable session".into(),
                    detail: format!("{session_id}\n{error}"),
                    action: Action::Load(HubRequest::Flows {
                        session: *session_id,
                        before: None,
                    }),
                },
                FlowCatalogEntry::Flow {
                    owner,
                    session_title,
                    snapshot,
                } => {
                    let session = session_title.as_deref().unwrap_or("Untitled session");
                    let (title, detail) = match snapshot {
                        Ok(snapshot) => {
                            let (status, state) = match &snapshot.execution {
                                OverviewExecution::Loaded { state } => {
                                    (execution_label(state.state()).to_owned(), state)
                                }
                                OverviewExecution::Detached => {
                                    ("Detached".into(), &snapshot.recorded_state)
                                }
                                OverviewExecution::Unavailable { reason } => {
                                    (format!("Unavailable: {reason}"), &snapshot.recorded_state)
                                }
                            };
                            (
                                format!(
                                    "{}{}",
                                    snapshot.title,
                                    if snapshot.default_flow {
                                        " · default"
                                    } else {
                                        ""
                                    }
                                ),
                                format!(
                                    "{status}{} · {} pending\n{}\nRecorded {} · {}\n{session} · {}/{}",
                                    if state.pauses().is_empty() {
                                        ""
                                    } else {
                                        " · paused"
                                    },
                                    snapshot.pending_inputs,
                                    snapshot
                                        .goal
                                        .as_ref()
                                        .map_or("No goal", |goal| goal.objective.as_str()).split_whitespace().collect::<Vec<_>>().join(" "),
                                    execution_label(snapshot.recorded_state.state()),
                                    &owner.instance_id.to_string()[28..],
                                    owner.session_id,
                                    owner.instance_id
                                ),
                            )
                        }
                        Err(error) => (
                            "Unavailable flow".into(),
                            format!(
                                "{session}\n{error}\n{}/{}",
                                owner.session_id, owner.instance_id
                            ),
                        ),
                    };
                    Row {
                        key: RowKey::Flow(owner.clone().into()),
                        title,
                        detail,
                        action: Action::Focus(owner.clone().into()),
                    }
                }
            })
            .collect()
    }

    pub(super) fn session_title(&self, id: uuid::Uuid) -> &str {
        self.page
            .entries
            .iter()
            .find_map(|entry| match entry {
                FlowCatalogEntry::Flow {
                    owner,
                    session_title,
                    ..
                } if owner.session_id.0 == id => session_title.as_deref(),
                _ => None,
            })
            .unwrap_or("Unavailable session")
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
        flow_runtime::catalog::OverviewCatalogSnapshot,
        flow_store::{FlowInstanceId, FlowRef},
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use uuid::Uuid;

    fn node(session: u128, index: u128) -> FlowCatalogEntry<OverviewCatalogSnapshot> {
        let owner = FlowRef {
            session_id: Uuid::from_u128(session).into(),
            instance_id: FlowInstanceId::new(Uuid::from_u128(index).into()),
        };
        let state = atman_rt::FlowInstance::prepared(owner.clone());
        FlowCatalogEntry::Flow {
            owner,
            session_title: Some(format!("Session {session}")),
            snapshot: Ok(OverviewCatalogSnapshot {
                title: format!("Worker {index}"),
                project_root: None,
                default_flow: index == 1,
                cursor: CommitReceipt {
                    journal_id: Uuid::now_v7(),
                    sequence: 1,
                    digest: "snapshot".into(),
                },
                trailing_bytes: 0,
                recorded_state: state,
                goal: None,
                pending_inputs: 2,
                relations: vec![],
                execution: OverviewExecution::Detached,
            }),
        }
    }

    fn page(
        after: Option<FlowCatalogCursor>,
        entries: Vec<FlowCatalogEntry<OverviewCatalogSnapshot>>,
        next: Option<FlowCatalogCursor>,
    ) -> Box<FlowHubPage> {
        Box::new(FlowHubPage::Overview(OverviewDirectory::new(
            after,
            OverviewCatalogPage {
                inspected: entries.len(),
                entries,
                next,
                total_sessions: 3,
            },
        )))
    }

    #[test]
    fn overview_preserves_progress_identity_navigation_and_grouped_viewports() {
        let owner = FlowRef {
            session_id: Uuid::from_u128(9).into(),
            instance_id: FlowInstanceId::now(),
        };
        let focused = owner.clone().into();
        let mut ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: focused,
            },
            request_id: Uuid::now_v7(),
        };
        let cursor = FlowCatalogCursor {
            session_id: Uuid::from_u128(2),
            before: None,
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(page(
                None,
                vec![node(3, 1), node(3, 2)],
                Some(cursor.clone()),
            )),
        );
        hub.key(KeyAction::HistoryDown);
        let target = match hub.focus() {
            Action::Focus(address) => address,
            _ => panic!("missing node"),
        };
        hub.query = "Worker".into();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 45)).unwrap();
        hub.key(KeyAction::NudgePrefill);
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let first = hub.hits.clone();
        assert!(
            matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::Overview { after: Some(after) }) if after == cursor)
        );
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        hub.receive(
            Some(&ticket),
            Ok(page(Some(cursor.clone()), vec![node(2, 3)], None)),
        );
        assert_eq!(hub.query, "Worker");
        assert!(matches!(hub.focus(), Action::Focus(address) if address == target));
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        assert_eq!(&hub.hits[..first.len()], first.as_slice());
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Session 3") && text.contains("Session 2"));
        assert!(text.contains("2 pending"));
        assert!(text.contains("3 flows loaded"));
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(address) if address == target)
        );
        let rect = hub
            .hits
            .iter()
            .find(|(_, index)| *index == hub.selected)
            .unwrap()
            .0;
        for row in rect.y..rect.bottom() {
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row, modifiers: KeyModifiers::NONE }), Action::Focus(address) if address == target)
            );
        }
        for graph in [true, false] {
            hub.graph_visible = graph;
            for (width, height) in [(40, 24), (60, 30), (140, 45)] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                for selected in 0..3 {
                    hub.selected = selected;
                    terminal
                        .draw(|frame| hub.render(frame, &owner.clone().into()))
                        .unwrap();
                    assert!(
                        hub.hits.iter().any(|(_, index)| *index == selected),
                        "selection must remain visible at {width}x{height}"
                    );
                    assert!(
                        hub.hits.iter().all(
                            |(rect, _)| rect.right() <= width && rect.bottom() < hub.previous.y
                        )
                    );
                }
            }
        }
        // Wrong cursors and late replies retain the accumulated directory.
        let old = ticket.clone();
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&old), Ok(page(None, vec![], None)));
        assert_eq!(hub.rows.len(), 3);
        hub.receive(Some(&ticket), Ok(page(Some(cursor), vec![], None)));
        assert_eq!(hub.rows.len(), 3);
        assert!(hub.error.as_ref().unwrap().contains("cursor"));
        assert!(matches!(hub.key(KeyAction::Escape), Action::Close));
    }
}
