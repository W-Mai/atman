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
    pub(super) session: Option<(uuid::Uuid, String)>,
    pub after: Option<FlowCatalogCursor>,
    pub page: OverviewCatalogPage,
    pub(super) relations: Vec<FlowRelation>,
}

impl OverviewDirectory {
    pub fn new(after: Option<FlowCatalogCursor>, page: OverviewCatalogPage) -> Self {
        let mut directory = Self {
            session: None,
            after,
            page,
            relations: Vec::new(),
        };
        directory.collect_relations();
        directory
    }

    pub fn for_session(mut self, session: uuid::Uuid, title: String) -> Self {
        self.session = Some((session, title));
        self
    }

    pub(super) fn title(&self) -> &str {
        self.session
            .as_ref()
            .map_or("All sessions", |(_, title)| title.as_str())
    }

    pub(super) fn request(&self, after: Option<FlowCatalogCursor>) -> HubRequest {
        match &self.session {
            Some((session, _)) => HubRequest::SessionOverview {
                session: *session,
                after,
            },
            None => HubRequest::Overview { after },
        }
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
        if old.page.next.as_ref() != Some(after)
            || old.session.as_ref().map(|(id, _)| id) != self.session.as_ref().map(|(id, _)| id)
        {
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
            .map(|after| self.request(Some(after)))
    }

    pub(super) fn status(&self) -> String {
        let flows = self
            .page
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    FlowCatalogEntry::Flow {
                        snapshot: Ok(_),
                        ..
                    }
                )
            })
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
            "{flows} readable flows · {errors} unavailable · {}",
            if self.page.next.is_some() {
                "Load more PgDn"
            } else {
                "All matching records loaded"
            }
        )
    }

    pub(super) fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<_> = self.page
            .entries
            .iter()
            .map(|entry| match entry {
                FlowCatalogEntry::SessionUnavailable { session_id, error } => Row {
                    key: RowKey::OverviewFailure(*session_id),
                    title: "Session unavailable".into(),
                    detail: "Could not read this session. Open details for the cause.".into(),
                    action: Action::Inspect {
                        title: "Session could not be opened".into(),
                        body: format!("Session: {session_id}\n\n{error}\n\nEsc back · Ctrl+R retry the directory"),
                    },
                },
                FlowCatalogEntry::Flow { owner, session_title, snapshot } => {
                    let session = session_title.as_deref().unwrap_or("Untitled session");
                    let (title, detail, action) = match snapshot {
                        Ok(snapshot) => {
                            let status = match &snapshot.execution {
                                OverviewExecution::Loaded { state } if !state.pauses().is_empty() => "Paused",
                                OverviewExecution::Loaded { state } => execution_label(state.state()),
                                OverviewExecution::Detached => "Not running here",
                                OverviewExecution::Unavailable { .. } => "Status unavailable",
                            };
                            let title = snapshot.goal.as_ref().map_or_else(
                                || snapshot.title.clone(),
                                |goal| goal.objective.split_whitespace().collect::<Vec<_>>().join(" "),
                            );
                            (
                                title,
                                format!("{status} · {} pending{}\n{}\n{}",
                                    snapshot.pending_inputs,
                                    if snapshot.archived { " · Archived" } else { "" },
                                    snapshot.title,
                                    super::page::activity_label(snapshot.activity.as_ref())),
                                Action::Focus(owner.clone().into()),
                            )
                        }
                        Err(error) => (
                            session.to_owned(),
                            "Could not read this flow. Open details for the cause.".into(),
                            Action::Inspect {
                                title: format!("{session} · Flow could not be opened"),
                                body: format!("Session: {}\nFlow: {}\n\n{error}\n\nEsc back · Ctrl+R retry the directory",
                                    owner.session_id, owner.instance_id),
                            },
                        ),
                    };
                    Row { key: RowKey::Flow(owner.clone().into()), title, detail, action }
                }
            })
            .collect();
        rows.sort_by_key(|row| matches!(row.action, Action::Inspect { .. }));
        rows
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

/// The same card is used in session scope and global scope, in either view placement.
pub(super) fn render_card(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    row: &Row,
    selected: bool,
    hovered: bool,
) -> ratatui::layout::Rect {
    use ratatui::{layout::Rect, style::Style, widgets::Paragraph};
    let t = crate::theme::theme();
    let bg = if selected {
        t.work_hover_bg.into_inner()
    } else if hovered {
        t.work_bg.lerp(t.work_hover_bg, 0.5)
    } else {
        t.work_bg.into_inner()
    };
    let muted = Style::default().fg(t.meta_fg.into()).bg(bg);
    frame.render_widget(Paragraph::new("").style(muted), area);
    if selected {
        frame.render_widget(
            Paragraph::new("▎\n".repeat(usize::from(area.height)))
                .style(Style::default().fg(t.accent.into()).bg(bg)),
            Rect::new(area.x, area.y, 1, area.height),
        );
    }
    frame.render_widget(
        Paragraph::new(crate::width::truncate(
            &row.title,
            area.width.saturating_sub(4) as usize,
        ))
        .style(Style::default().fg(t.heading.into()).bg(bg)),
        Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), 1),
    );
    let body = Rect::new(
        area.x + 2,
        area.y + 2,
        area.width.saturating_sub(4),
        area.height.saturating_sub(5),
    );
    frame.render_widget(Paragraph::new(row.detail.as_str()).style(muted), body);
    let action_height = if area.height >= 6 { 3 } else { 1 };
    let bottom = area
        .bottom()
        .saturating_sub(if action_height == 3 { 3 } else { 2 });
    let open = Rect::new(
        area.x + 2,
        bottom,
        area.width.saturating_sub(4),
        action_height,
    );
    let can_open = matches!(row.action, Action::Focus(_));
    let prefix = if action_height == 3 { "\n" } else { "" };
    frame.render_widget(
        Paragraph::new(format!(
            "{prefix}{}",
            if can_open {
                "Open chat Enter"
            } else {
                "Details Enter"
            }
        ))
        .style(muted),
        open,
    );
    let width = if area.width >= 40 { 15 } else { 12 };
    let action = Rect::new(
        area.right().saturating_sub(width + 2),
        bottom,
        width,
        action_height,
    );
    let label = if can_open {
        if area.width >= 40 {
            "Window Ctrl+O"
        } else {
            "Window ^O"
        }
    } else {
        "Retry ^R"
    };
    frame.render_widget(
        Paragraph::new(format!("{prefix}{label}"))
            .style(muted)
            .alignment(ratatui::layout::Alignment::Right),
        action,
    );
    action
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
                archived: false,
                title: format!("Worker {index}"),
                project_root: None,
                default_flow: index == 1,
                cursor: CommitReceipt {
                    journal_id: Uuid::now_v7(),
                    sequence: 1,
                    digest: "snapshot".into(),
                },
                trailing_bytes: 0,
                activity: Some(atman_runtime::flow_store::activity::RecordedActivity {
                    at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                    receipt: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "activity".into(),
                    }
                    .into(),
                }),
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
    fn scoped_overview_keeps_diagnostics_out_of_conversations_and_preserves_navigation() {
        let owner = FlowRef {
            session_id: Uuid::from_u128(3).into(),
            instance_id: FlowInstanceId::now(),
        };
        let focused = owner.clone().into();
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: focused,
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        let entries = vec![
            node(3, 1),
            FlowCatalogEntry::Flow {
                owner: owner.clone(),
                session_title: Some("Broken record".into()),
                snapshot: Err("invalid test journal".into()),
            },
        ];
        let scoped = OverviewDirectory::new(
            None,
            OverviewCatalogPage {
                inspected: 2,
                entries,
                next: None,
                total_sessions: 1,
            },
        )
        .for_session(owner.session_id.0, "Current session".into());
        hub.receive(Some(&ticket), Ok(Box::new(FlowHubPage::Overview(scoped))));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 50)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Open chat Enter") && text.contains("Window Ctrl+O"));
        assert!(text.contains("Couldn't open") && text.contains("Retry ^R"));
        assert!(!text.contains("invalid test journal"));
        assert!(!text.contains(&owner.instance_id.to_string()));
        assert_eq!(hub.retry_hits.len(), 1);
        assert_eq!(hub.float_hits.len(), 1);
        hub.selected = 1;
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        assert!(hub.property_details.is_some());
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("invalid test journal") && text.contains(&owner.instance_id.to_string())
        );
        hub.key(KeyAction::Escape);
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let retry = hub.retry_hits[0];
        assert!(
            matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: retry.x + 1, row: retry.y + 1, modifiers: KeyModifiers::NONE }), Action::Load(HubRequest::SessionOverview { session, after: None }) if session == owner.session_id.0)
        );
        assert!(matches!(
            hub.key(KeyAction::Save),
            Action::Load(HubRequest::Overview { after: None })
        ));
        hub.query = "no-match".into();
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let clear = hub.clear_search;
        hub.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: clear.x + 1,
            row: clear.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(hub.query.is_empty());
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
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 60)).unwrap();
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
        assert!(text.contains("Last 2023-11"));
        assert!(text.contains("3 readable flows"));
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
            hub.layout = if graph {
                super::super::Layout::Graph
            } else {
                super::super::Layout::List
            };
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
