//! Read-only project resources and recorded Flow bindings.

use std::path::PathBuf;

use atman_runtime::{
    flow_runtime::catalog::FlowCatalogCursor,
    flow_store::FlowRef,
    git_workspace::{WorkspaceCheckout, WorkspaceInspection, WorkspaceSnapshot},
};
use ratatui::layout::Rect;
use uuid::Uuid;

use super::{
    Action, HubRequest,
    page::{Row, RowKey},
};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct WorkspaceLocation {
    pub repository: PathBuf,
    pub storage: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceCursor {
    Sessions { before: Uuid },
    Bindings { after: Option<FlowCatalogCursor> },
}

pub struct WorkspaceFlowReference {
    pub owner: FlowRef,
    pub title: String,
    pub workspace_id: String,
}

pub struct WorkspaceDirectoryEntry {
    pub location: WorkspaceLocation,
    pub references: Vec<WorkspaceFlowReference>,
}

pub struct WorkspaceDirectory {
    pub after: Option<WorkspaceCursor>,
    pub next: Option<WorkspaceCursor>,
    pub locations: Vec<WorkspaceDirectoryEntry>,
    pub errors: Vec<String>,
}

impl WorkspaceDirectory {
    pub(super) fn append(&mut self, previous: Option<&mut Self>) -> Result<(), String> {
        if self.after.is_none() {
            return Ok(());
        }
        let Some(old) = previous else {
            return Err("Worktrees snapshot changed; refresh to restart".into());
        };
        if old.next != self.after {
            return Err("Worktrees cursor changed; refresh to restart".into());
        }
        let incoming = std::mem::replace(&mut self.locations, std::mem::take(&mut old.locations));
        for entry in incoming {
            self.insert(entry);
        }
        let errors = std::mem::replace(&mut self.errors, std::mem::take(&mut old.errors));
        self.errors.extend(errors);
        self.errors.sort();
        self.errors.dedup();
        Ok(())
    }

    pub fn insert(&mut self, mut entry: WorkspaceDirectoryEntry) {
        if let Some(existing) = self
            .locations
            .iter_mut()
            .find(|row| row.location == entry.location)
        {
            for reference in entry.references.drain(..) {
                if !existing
                    .references
                    .iter()
                    .any(|old| old.owner == reference.owner)
                {
                    existing.references.push(reference);
                }
            }
        } else {
            self.locations.push(entry);
        }
        self.locations.sort_by(|a, b| a.location.cmp(&b.location));
    }

    pub(super) fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for entry in &self.locations {
            let location = &entry.location;
            rows.push(Row {
                key: RowKey::WorkspaceLocation(location.clone()),
                title: format!("{} · Worktrees", project_name(location)),
                detail: format!(
                    "{} · {} recorded bindings · Enter inspect",
                    location.storage.display(),
                    entry.references.len()
                ),
                action: Action::Load(HubRequest::Workspace {
                    location: location.clone(),
                }),
            });
            for reference in &entry.references {
                rows.push(Row {
                    key: RowKey::Flow(reference.owner.clone().into()),
                    title: format!("  ↳ {} · {}", reference.title, reference.workspace_id),
                    detail: format!(
                        "Recorded binding · {} / {}",
                        reference.owner.session_id.0, reference.owner.instance_id.0.0
                    ),
                    action: Action::Focus(reference.owner.clone().into()),
                });
            }
        }
        for error in &self.errors {
            rows.push(Row {
                key: RowKey::WorkspaceError(error.clone()),
                title: "Unavailable source".into(),
                detail: error.clone(),
                action: Action::None,
            });
        }
        rows
    }
}

pub(super) fn project_name(location: &WorkspaceLocation) -> String {
    location.repository.file_name().map_or_else(
        || location.repository.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

pub(super) fn location(snapshot: &WorkspaceSnapshot) -> WorkspaceLocation {
    WorkspaceLocation {
        repository: snapshot.repository_root.clone(),
        storage: snapshot.storage_root.clone(),
    }
}

fn checkout_label(checkout: &WorkspaceCheckout) -> String {
    match checkout {
        WorkspaceCheckout::Available { branch, dirty, .. } => format!(
            "{} · {}",
            branch.as_deref().unwrap_or("Detached HEAD"),
            if *dirty { "Changes" } else { "Clean" }
        ),
        WorkspaceCheckout::Missing => "Checkout missing".into(),
        WorkspaceCheckout::Unavailable { reason } => format!("Unavailable · {reason}"),
    }
}

pub(super) fn resource_rows(snapshot: &WorkspaceSnapshot) -> Vec<Row> {
    let location = location(snapshot);
    if !snapshot.registry_present {
        return vec![Row {
            key: RowKey::WorkspaceField("registry"),
            title: "Registry missing".into(),
            detail: "Recorded Flow bindings remain available in Worktrees".into(),
            action: Action::None,
        }];
    }
    snapshot
        .workspaces
        .iter()
        .map(|workspace| Row {
            key: RowKey::Workspace {
                location: location.clone(),
                id: workspace.record.id.clone(),
            },
            title: format!(
                "{} · {}",
                workspace.record.id,
                checkout_label(&workspace.checkout)
            ),
            detail: format!(
                "{} · {} live refs · {}",
                workspace.record.state,
                workspace
                    .record
                    .instances
                    .iter()
                    .filter(|reference| !reference.released)
                    .count(),
                workspace.record.worktree_path.display()
            ),
            action: Action::Load(HubRequest::WorkspaceRecord {
                location: location.clone(),
                id: workspace.record.id.clone(),
            }),
        })
        .collect()
}

pub struct WorkspaceRecordPage {
    pub location: WorkspaceLocation,
    pub workspace: WorkspaceInspection,
}

#[derive(Clone)]
pub struct WorkspaceRetention {
    pub location: WorkspaceLocation,
    pub expected: atman_runtime::git_workspace::WorkspaceRecord,
    pub retained: bool,
}

pub struct FlowWorkspacePage {
    pub owner: FlowRef,
    pub request: Option<atman_runtime::flow_store::workspace::WorkspaceRequest>,
}

impl FlowWorkspacePage {
    pub(super) fn rows(&self) -> Vec<Row> {
        use atman_runtime::flow_store::workspace::WorkspaceSource;
        let mut rows = vec![Row {
            key: RowKey::WorkspaceField("flow"),
            title: "Flow workspace".into(),
            detail: format!(
                "{} / {}",
                self.owner.session_id.0, self.owner.instance_id.0.0
            ),
            action: Action::None,
        }];
        if let Some(request) = &self.request {
            let binding = &request.binding;
            rows.push(Row {
                key: RowKey::WorkspaceField("binding"),
                title: "Recorded checkout binding".into(),
                detail: binding.path.display().to_string(),
                action: Action::None,
            });
            rows.push(Row {
                key: RowKey::WorkspaceField("source"),
                title: "Acquisition source".into(),
                detail: match &request.source {
                    WorkspaceSource::Allocate { base, retained } => format!(
                        "Allocated at {base} · Initial retention: {}",
                        if *retained { "Retain" } else { "Automatic" }
                    ),
                    WorkspaceSource::Share { sponsor } => format!(
                        "Shared from {} / {} · Enter focus · Ctrl+O float",
                        sponsor.session_id.0, sponsor.instance_id.0.0
                    ),
                },
                action: match &request.source {
                    WorkspaceSource::Share { sponsor } => Action::Focus(sponsor.clone().into()),
                    _ => Action::None,
                },
            });
            // The durable request has passed replay's managed-path validation.
            if let Some(storage) = binding.path.ancestors().nth(3) {
                rows.push(Row {
                    key: RowKey::WorkspaceField("resource"),
                    title: "Inspect resource".into(),
                    detail: format!(
                        "{} · Check availability and current references",
                        binding.workspace_id
                    ),
                    action: Action::Load(HubRequest::WorkspaceRecord {
                        location: WorkspaceLocation {
                            repository: binding.repository_root.clone(),
                            storage: storage.to_owned(),
                        },
                        id: binding.workspace_id.clone(),
                    }),
                });
            }
        } else {
            rows.push(Row {
                key: RowKey::WorkspaceField("binding"),
                title: "No managed checkout binding".into(),
                detail: "This flow has no recorded worktree allocation or shared reference.".into(),
                action: Action::None,
            });
        }
        rows.push(Row {
            key: RowKey::WorkspaceField("directory"),
            title: "All worktrees".into(),
            detail: "Browse project resources and recorded Flow bindings".into(),
            action: Action::Load(HubRequest::Workspaces { after: None }),
        });
        rows
    }
}

impl WorkspaceRecordPage {
    pub(super) fn rows(&self) -> Vec<Row> {
        let record = &self.workspace.record;
        let mut rows = Vec::new();
        let mut field = |key, title: String, detail: String| {
            rows.push(Row {
                key: RowKey::WorkspaceField(key),
                title,
                detail,
                action: Action::None,
            })
        };
        field(
            "checkout",
            checkout_label(&self.workspace.checkout),
            record.worktree_path.display().to_string(),
        );
        field(
            "lifecycle",
            format!("Recorded lifecycle · {}", record.state),
            format!(
                "Retention · {}",
                if record.retained {
                    "Retain"
                } else {
                    "Automatic"
                }
            ),
        );
        field(
            "repository",
            "Repository".into(),
            self.location.repository.display().to_string(),
        );
        if let WorkspaceCheckout::Available { head, .. } = &self.workspace.checkout {
            field("head", "HEAD".into(), head.clone());
        }
        if let Some(session) = &record.owner_session {
            field(
                "owner",
                "Allocation owner".into(),
                format!(
                    "{session} / {}",
                    record.owner_flow.as_deref().unwrap_or("—")
                ),
            );
        }
        let target = if record.lease.is_some() {
            None
        } else {
            use atman_runtime::git_workspace::WorkspaceState;
            match record.lifecycle_state() {
                WorkspaceState::Bound if record.is_instance_bound() => Some(!record.retained),
                WorkspaceState::Active => Some(true),
                _ => None,
            }
        };
        rows.push(Row {
            key: RowKey::WorkspaceField("retention_control"),
            title: match target {
                Some(true) => "Retain checkout".into(),
                Some(false) => "Use automatic cleanup".into(),
                None => "Retention control unavailable".into(),
            },
            detail: match target {
                Some(true) => "Keep the checkout after the final Flow reference closes".into(),
                Some(false) => "Clean up after the final reference closes; uncommitted changes remain protected".into(),
                None if record.lease.is_some() => "An invocation lease owns this checkout; use its execution owner".into(),
                None => "This lifecycle does not permit a retention change; existing data and references are preserved".into(),
            },
            action: target.map_or(Action::None, |retained| Action::SetWorkspaceRetention(Box::new(WorkspaceRetention {
                location: self.location.clone(), expected: record.clone(), retained,
            }))),
        });
        for reference in &record.instances {
            rows.push(Row {
                key: RowKey::Flow(reference.owner.clone().into()),
                title: format!(
                    "{} · {}",
                    if reference.released {
                        "Released reference"
                    } else {
                        "Live reference"
                    },
                    reference.owner.instance_id.0.0
                ),
                detail: format!(
                    "Session {} · Enter focus · Ctrl+O float",
                    reference.owner.session_id.0
                ),
                action: Action::Focus(reference.owner.clone().into()),
            });
        }
        rows
    }
}

pub(super) fn cards(
    area: Rect,
    rows: &[Row],
    filtered: &[usize],
    selected: usize,
    offset: &mut usize,
) -> Vec<(usize, Rect)> {
    if area.is_empty() {
        return Vec::new();
    }
    let heights: Vec<u16> = filtered
        .iter()
        .map(|&index| {
            let lines = crate::width::word_wrap(
                &rows[index].detail,
                usize::from(area.width.saturating_sub(4)),
            )
            .len();
            u16::try_from(lines.saturating_add(3))
                .unwrap_or(u16::MAX)
                .max(4)
                .min(area.height)
        })
        .collect();
    while *offset < selected
        && heights[*offset..=selected]
            .iter()
            .map(|height| usize::from(*height))
            .sum::<usize>()
            > usize::from(area.height)
    {
        *offset += 1;
    }
    let mut top = area.y;
    let mut cards = Vec::new();
    for (index, &height) in heights.iter().enumerate().skip(*offset) {
        if top.saturating_add(height) > area.bottom() {
            break;
        }
        cards.push((index, Rect::new(area.x, top, area.width, height)));
        top += height;
    }
    cards
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, History},
        keys::KeyAction,
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    #[test]
    fn focused_workspace_preserves_binding_identity_and_navigation() {
        use atman_runtime::{
            flow_store::workspace::{WorkspaceRequest, WorkspaceSource},
            git_workspace::WorkspaceBinding,
        };
        let owner = FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        let sponsor = FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: owner.instance_id.clone(),
        };
        let mut page = FlowWorkspacePage {
            owner: owner.clone(),
            request: None,
        };
        assert!(
            page.rows()
                .iter()
                .any(|row| row.title == "No managed checkout binding")
        );
        page.request = Some(WorkspaceRequest {
            binding: WorkspaceBinding {
                workspace_id: "checkout".into(),
                repository_root: "/missing/repository".into(),
                path: "/external/.atman/worktrees/checkout".into(),
                branch: None,
            },
            source: WorkspaceSource::Share {
                sponsor: sponsor.clone(),
            },
        });
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::new_v4(),
                revision: 1,
                flow: owner.clone().into(),
            },
            request_id: Uuid::new_v4(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::FlowWorkspace(page))),
        );
        assert_eq!(
            hub.page.as_ref().unwrap().first(),
            HubRequest::FlowWorkspace {
                owner: owner.clone()
            }
        );
        hub.selected = 2;
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&sponsor))
        );
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(address) if address.instance() == Some(&sponsor))
        );
        hub.selected = 3;
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Load(HubRequest::WorkspaceRecord { location, id }) if location.storage == std::path::Path::new("/external") && location.repository == std::path::Path::new("/missing/repository") && id == "checkout")
        );
        for width in [40, 120] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            let (rect, _) = hub.hits.iter().find(|(_, index)| *index == 3).unwrap();
            assert!(matches!(
                hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 2,
                    row: rect.y + 1,
                    modifiers: KeyModifiers::NONE
                }),
                Action::Load(HubRequest::WorkspaceRecord { .. })
            ));
        }
        assert!(matches!(hub.key(KeyAction::Escape), Action::Return));
    }

    #[test]
    fn property_reader_keeps_full_text_and_returns_to_its_record() {
        let owner = FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::new_v4(),
                revision: 0,
                flow: owner.clone().into(),
            },
            request_id: Uuid::new_v4(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::FlowWorkspace(FlowWorkspacePage {
                owner: owner.clone(),
                request: None,
            }))),
        );
        // A property larger than a viewport must remain readable through the same row action.
        hub.rows[1].detail = format!("{}\nlast-property-line", "完整路径/".repeat(400));
        hub.selected = 1;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 24)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let (read, _) = hub
            .float_hits
            .iter()
            .find(|(_, index)| *index == 1)
            .unwrap();
        assert!(matches!(
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: read.x + 2,
                row: read.y + 1,
                modifiers: KeyModifiers::NONE
            }),
            Action::None
        ));
        assert!(hub.property_details.is_some());
        hub.paste("not a query");
        assert!(hub.query.is_empty());
        hub.key(KeyAction::CursorEnd);
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("last-property-line"));
        assert!(hub.property_details.as_ref().unwrap().scroll > 0);
        hub.key(KeyAction::CursorHome);
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        assert_eq!(hub.property_details.as_ref().unwrap().scroll, 0);
        let back = hub.property_details.as_ref().unwrap().back;
        hub.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: back.x + 1,
            row: back.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(hub.property_details.is_none());
        assert_eq!(hub.selected, 1);
        hub.key(KeyAction::Submit);
        assert!(hub.property_details.is_some());
        hub.key(KeyAction::Escape);
        assert!(hub.property_details.is_none());
    }

    #[test]
    fn workspace_navigation_preserves_catalog_progress_and_typed_references() {
        let owner = FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
        };
        let other = FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: owner.instance_id.clone(),
        };
        let location = WorkspaceLocation {
            repository: "/missing/project".into(),
            storage: "/external/with-a-long-resource-storage-directory".into(),
        };
        let cursor = WorkspaceCursor::Bindings { after: None };
        let page = |after, next, refs: Vec<FlowRef>| {
            Box::new(FlowHubPage::Workspaces(WorkspaceDirectory {
                after,
                next,
                errors: Vec::new(),
                locations: vec![WorkspaceDirectoryEntry {
                    location: location.clone(),
                    references: refs
                        .into_iter()
                        .map(|owner| WorkspaceFlowReference {
                            owner,
                            title: "worker".into(),
                            workspace_id: "shared".into(),
                        })
                        .collect(),
                }],
            }))
        };
        let mut ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::new_v4(),
                revision: 1,
                flow: owner.clone().into(),
            },
            request_id: Uuid::new_v4(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        let mut history = History::default();
        hub.receive(Some(&ticket), Ok(page(None, Some(cursor.clone()), vec![])));
        assert!(matches!(
            hub.key(KeyAction::PageDown),
            Action::Load(HubRequest::Workspaces { after: Some(_) })
        ));
        ticket.request_id = Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("read failed".into()));
        assert_eq!(hub.rows.len(), 1);
        hub.pending = Some(ticket.clone());
        hub.receive(
            Some(&ticket),
            Ok(page(
                Some(WorkspaceCursor::Sessions {
                    before: Uuid::new_v4(),
                }),
                None,
                vec![],
            )),
        );
        assert!(hub.error.as_deref().unwrap().contains("cursor changed"));
        hub.pending = Some(ticket.clone());
        hub.receive(
            None,
            Ok(page(Some(cursor.clone()), None, vec![owner.clone()])),
        );
        assert!(hub.pending.is_some());
        hub.receive(
            Some(&ticket),
            Ok(page(Some(cursor), None, vec![owner.clone(), other.clone()])),
        );
        assert_eq!(hub.rows.len(), 3);
        assert!(hub.rows[1].key != hub.rows[2].key);
        hub.selected = 2;
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(address) if address.instance() == Some(&other))
        );
        hub.selected = 0;
        for width in [40, 120] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            let hit = hub.hits[0].0;
            assert_eq!(hit.height, 4);
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: hit.x + 1, row: hit.y + 1, modifiers: KeyModifiers::NONE }), Action::Load(HubRequest::Workspace { location: actual }) if actual == location)
            );
        }
        let record = atman_runtime::git_workspace::WorkspaceRecord {
            id: "shared".into(),
            repository_root: location.repository.clone(),
            worktree_path: location.storage.join(".atman/worktrees/shared"),
            branch: None,
            owner_session: None,
            owner_flow: None,
            state: "bound".into(),
            retained: true,
            release: None,
            allocation_base: None,
            allocation_policy: None,
            lease: None,
            reconciled_at: None,
            reconciliation_reason: None,
            instances: vec![
                atman_runtime::git_workspace::WorkspaceInstanceBinding {
                    owner: owner.clone(),
                    released: false,
                },
                atman_runtime::git_workspace::WorkspaceInstanceBinding {
                    owner: other.clone(),
                    released: true,
                },
            ],
        };
        let inspection = WorkspaceInspection {
            record,
            checkout: WorkspaceCheckout::Missing,
        };
        ticket.request_id = Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Workspace(WorkspaceSnapshot {
                repository_root: location.repository.clone(),
                storage_root: location.storage.clone(),
                registry_present: true,
                workspaces: vec![inspection.clone()],
            }))),
        );
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Load(HubRequest::WorkspaceRecord { id, .. }) if id == "shared")
        );
        ticket.request_id = Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::WorkspaceRecord(Box::new(
                WorkspaceRecordPage {
                    location: location.clone(),
                    workspace: inspection,
                },
            )))),
        );
        assert_eq!(hub.rows[0].title, "Checkout missing");
        let retention = hub
            .rows
            .iter()
            .position(|row| matches!(row.action, Action::SetWorkspaceRetention(_)))
            .unwrap();
        hub.selected = retention;
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::SetWorkspaceRetention(change) if !change.retained && change.expected.instances.len() == 2 && change.location == location)
        );
        hub.selected = 0;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 30)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        assert!(hub.hits[0].0.height > 4);
        assert!(hub.float_hits.iter().all(|(_, index)| {
            let row = &hub.rows[hub.filtered()[*index]];
            row.has_text_details()
                || matches!(
                    row.action,
                    Action::Focus(_) | Action::SetWorkspaceRetention(_)
                )
        }));
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!rendered.contains("Flows  Enter"));
        assert!(
            hub.rows
                .iter()
                .any(|row| row.title.starts_with("Live reference"))
        );
        let released = hub
            .rows
            .iter()
            .position(|row| row.title.starts_with("Released reference"))
            .unwrap();
        hub.selected = released;
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&other))
        );
        let (mut hub, action) = history.open(
            hub.key(KeyAction::Escape),
            HubRequest::Workspaces { after: None },
        );
        assert!(matches!(action, Action::None));
        assert!(matches!(hub.page, Some(FlowHubPage::Workspace(_))));
        let (hub, action) = history.open(
            hub.key(KeyAction::Escape),
            HubRequest::Workspaces { after: None },
        );
        assert!(matches!(action, Action::None));
        assert_eq!(hub.rows.len(), 3);
        assert!(hub.page.unwrap().next().is_none());
    }
}
