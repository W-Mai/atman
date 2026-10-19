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
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 30)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        assert!(hub.hits[0].0.height > 4);
        assert!(
            hub.float_hits.iter().all(|(_, index)| matches!(
                hub.rows[hub.filtered()[*index]].action,
                Action::Focus(_)
            ))
        );
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
