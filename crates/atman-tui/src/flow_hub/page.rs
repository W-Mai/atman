use atman_runtime::event_log::legacy::LegacyFlowKey;
use atman_runtime::flow_runtime::{
    access::InstancePage,
    catalog::{CatalogStatus, SessionSummary},
};
use atman_runtime::flow_store::{
    FlowInstanceId,
    access::AccessTarget,
    relation::{FlowRelation, RelationKind},
};
use uuid::Uuid;

use super::{Action, execution_label};
use crate::project_hub::SessionTarget;

/// Host-supplied directory rows retain their storage origin independently of display text.
pub struct SessionDirectory {
    pub sessions: Vec<SessionRow>,
    pub next: Option<SessionTarget>,
    pub total: usize,
}

pub struct SessionRow {
    pub target: SessionTarget,
    pub title: String,
    pub detail: String,
    pub cleanup_pending: bool,
}

impl From<SessionSummary> for SessionRow {
    fn from(row: SessionSummary) -> Self {
        let status = match row.status {
            CatalogStatus::Running => "Running",
            CatalogStatus::Idle => "Idle",
            CatalogStatus::Closed => "Closed",
            CatalogStatus::Detached => "Detached",
            CatalogStatus::Unavailable => "Unavailable",
        };
        Self {
            cleanup_pending: false,
            target: SessionTarget::Persistent(row.session_id),
            title: row.title.unwrap_or_else(|| "Unavailable session".into()),
            detail: row.unavailable.unwrap_or_else(|| {
                format!(
                    "{} · {status} · {} flows · {}",
                    row.session_id,
                    row.flow_count
                        .map_or_else(|| "?".into(), |count| count.to_string()),
                    row.project_root
                        .as_ref()
                        .map_or_else(|| "No project".into(), |path| path.display().to_string())
                )
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelationRange {
    Session,
    Catalog { before: Option<Uuid> },
}

pub struct RelationProgress {
    pub scanned: usize,
    pub total: usize,
    pub next: Option<Uuid>,
    pub errors: Vec<(Uuid, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubRequest {
    IndexedHistory {
        source: atman_runtime::history_index::IndexedSource,
    },
    SessionProject {
        session: Uuid,
    },
    FlowDetails {
        owner: atman_runtime::flow_store::FlowRef,
    },
    SessionRemoval {
        session: SessionTarget,
    },
    ContinuationRemoval {
        owner: atman_runtime::flow_store::FlowRef,
    },
    ControlHistory {
        before: Option<Uuid>,
    },
    ControlBatch {
        request_id: Uuid,
    },
    Management {
        grant: Option<Uuid>,
    },
    Overview {
        after: Option<atman_runtime::flow_runtime::catalog::FlowCatalogCursor>,
    },
    SessionOverview {
        session: Uuid,
        after: Option<atman_runtime::flow_runtime::catalog::FlowCatalogCursor>,
    },
    FlowWorkspace {
        owner: atman_runtime::flow_store::FlowRef,
    },
    Workspaces {
        after: Option<super::WorkspaceCursor>,
    },
    Workspace {
        location: super::WorkspaceLocation,
    },
    WorkspaceRecord {
        location: super::WorkspaceLocation,
        id: String,
    },
    Inbox {
        owner: atman_runtime::flow_store::FlowRef,
        side: atman_runtime::flow_store::inbox_view::InboxSide,
        before: Option<atman_runtime::flow_store::inbox_view::InboxHistoryCursor>,
    },
    WatchCatalog {
        owner: atman_runtime::flow_store::FlowRef,
        after: Option<atman_runtime::flow_runtime::catalog::FlowCatalogCursor>,
    },
    ProcessSources {
        owner: atman_runtime::flow_store::FlowRef,
    },
    Watchers {
        owner: atman_runtime::flow_store::FlowRef,
    },
    Sessions {
        before: Option<SessionTarget>,
    },
    Flows {
        session: Uuid,
        before: Option<FlowInstanceId>,
    },
    LegacyFlows {
        session: Uuid,
        after: Option<LegacyFlowKey>,
    },
    Relations {
        session: Uuid,
        focus: Option<atman_runtime::flow_store::FlowRef>,
        range: RelationRange,
    },
}

impl HubRequest {
    pub(super) fn first(&self) -> Self {
        match self {
            Self::IndexedHistory { .. } => self.clone(),
            Self::SessionProject { .. } => self.clone(),
            Self::FlowDetails { .. }
            | Self::SessionRemoval { .. }
            | Self::ContinuationRemoval { .. } => self.clone(),
            Self::ControlBatch { .. } => self.clone(),
            Self::ControlHistory { .. } => Self::ControlHistory { before: None },
            Self::Management { .. } => self.clone(),
            Self::Overview { .. } => Self::Overview { after: None },
            Self::SessionOverview { session, .. } => Self::SessionOverview {
                session: *session,
                after: None,
            },
            Self::Workspaces { .. } => Self::Workspaces { after: None },
            Self::Workspace { .. } | Self::WorkspaceRecord { .. } | Self::FlowWorkspace { .. } => {
                self.clone()
            }
            Self::Inbox { owner, side, .. } => Self::Inbox {
                owner: owner.clone(),
                side: *side,
                before: None,
            },
            Self::WatchCatalog { owner, .. } => Self::WatchCatalog {
                owner: owner.clone(),
                after: None,
            },
            Self::Watchers { .. } | Self::ProcessSources { .. } => self.clone(),
            Self::Sessions { .. } => Self::Sessions { before: None },
            Self::Flows { session, .. } => Self::Flows {
                session: *session,
                before: None,
            },
            Self::LegacyFlows { session, .. } => Self::LegacyFlows {
                session: *session,
                after: None,
            },
            Self::Relations {
                session,
                focus,
                range,
            } => Self::Relations {
                session: *session,
                focus: focus.clone(),
                range: match range {
                    RelationRange::Session => RelationRange::Session,
                    RelationRange::Catalog { .. } => RelationRange::Catalog { before: None },
                },
            },
        }
    }
}

fn indexed_rows(page: &atman_runtime::history_index::IndexReview) -> Vec<Row> {
    vec![
        Row {
            key: RowKey::RemovalField("source"),
            title: format!("Indexed session {}", page.source.session),
            detail: page.source.index.display().to_string(),
            action: Action::None,
        },
        Row {
            key: RowKey::RemovalField("coverage"),
            title: format!("{} records · {} missing sequence positions", page.records, page.gaps),
            detail: format!("The original session log is missing. Completeness cannot be established. {} image references and {} full tool outputs are unavailable; indexed text and inline images remain.", page.missing_images, page.missing_outputs),
            action: Action::None,
        },
        Row {
            key: RowKey::RemovalField("preview"),
            title: "Recent indexed text".into(),
            detail: page.preview.join("\n"),
            action: Action::None,
        },
        Row {
            key: RowKey::RemovalField("recover"),
            title: "Recover transcript · Enter".into(),
            detail: "Create an independent read-only history copy. The source index stays unchanged. Missing assets are marked; sending a message from the recovered history starts a new conversation without old tasks or Goals.".into(),
            action: Action::RecoverIndexedHistory(Box::new(page.clone())),
        },
        Row {
            key: RowKey::RemovalField("back"),
            title: "Back · Esc".into(),
            detail: "Return without recovering history".into(),
            action: Action::Return,
        },
    ]
}

pub enum FlowHubPage {
    IndexedHistory(Box<atman_runtime::history_index::IndexReview>),
    SessionProject(Box<super::SessionProjectPage>),
    FlowDetails(Box<super::FlowDetailsPage>),
    SessionRemoval(Box<super::SessionRemovalPage>),
    ControlHistory(atman_runtime::flow_store::profile::ControlBatchPage),
    ControlBatch(Box<super::ControlBatchPage>),
    Management(Box<super::ManagementPage>),
    Overview(super::OverviewDirectory),
    FlowWorkspace(super::FlowWorkspacePage),
    Workspaces(super::WorkspaceDirectory),
    Workspace(atman_runtime::git_workspace::WorkspaceSnapshot),
    WorkspaceRecord(Box<super::WorkspaceRecordPage>),
    Inbox(atman_runtime::flow_store::inbox_view::InboxHistoryPage),
    ProcessSources {
        owner: atman_runtime::flow_store::FlowRef,
        sources: Vec<atman_runtime::flow_runtime::ProcessWatchSource>,
    },
    Watchers(super::WatchDirectory),
    Sessions(SessionDirectory),
    Flows {
        session: Uuid,
        title: String,
        default_flow: Option<FlowInstanceId>,
        page: InstancePage,
        relations: Result<Vec<FlowRelation>, String>,
    },
    LegacyFlows {
        session: Uuid,
        title: String,
        flows: Vec<LegacyFlowKey>,
        next: Option<LegacyFlowKey>,
        total: usize,
    },
    Relations {
        session: Uuid,
        title: String,
        focus: Option<atman_runtime::flow_store::FlowRef>,
        relations: Vec<FlowRelation>,
        range: RelationRange,
        progress: Option<RelationProgress>,
    },
}

impl FlowHubPage {
    /// Resolve session navigation without exposing a flow directory.
    pub(crate) fn session_destination(&self) -> Option<Result<crate::control::ViewAddress, &str>> {
        use crate::control::ViewAddress;
        use atman_runtime::flow_store::FlowRef;
        match self {
            Self::Flows {
                session,
                default_flow,
                ..
            } => Some(
                default_flow
                    .as_ref()
                    .map(|id| {
                        ViewAddress::Instance(FlowRef {
                            session_id: (*session).into(),
                            instance_id: id.clone(),
                        })
                    })
                    .ok_or("This session has no default conversation"),
            ),
            Self::LegacyFlows { session, flows, .. } => Some(
                flows
                    .iter()
                    .find(|key| matches!(key, LegacyFlowKey::Default))
                    .cloned()
                    .map(|owner| ViewAddress::Legacy {
                        session: *session,
                        owner,
                    })
                    .ok_or("This session has no default conversation history"),
            ),
            _ => None,
        }
    }
}

pub(super) struct Row {
    pub key: RowKey,
    pub title: String,
    pub detail: String,
    pub action: Action,
}

impl Row {
    pub fn has_text_details(&self) -> bool {
        matches!(
            self.key,
            RowKey::WorkspaceField(_)
                | RowKey::WorkspaceError(_)
                | RowKey::ManagementField(_)
                | RowKey::ControlField(_)
                | RowKey::RemovalField(_)
                | RowKey::FlowField(_)
        ) && matches!(self.action, Action::None)
    }
}

#[derive(Clone, Copy, Default)]
pub(super) enum Endpoint {
    #[default]
    Neighbor,
    Source,
    Target,
}

impl Endpoint {
    pub fn next(self) -> Self {
        match self {
            Self::Neighbor => Self::Source,
            Self::Source => Self::Target,
            Self::Target => Self::Neighbor,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Neighbor => "\nNeighbor Tab",
            Self::Source => "\nSource Tab",
            Self::Target => "\nTarget Tab",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum RowKey {
    FlowField(&'static str),
    RemovalField(&'static str),
    ControlField(String),
    ManagementField(&'static str),
    ManagementGrant(Uuid),
    Overview,
    OverviewFailure(Uuid),
    WorkspaceLocation(super::WorkspaceLocation),
    Workspace {
        location: super::WorkspaceLocation,
        id: String,
    },
    WorkspaceField(&'static str),
    WorkspaceError(String),
    Delivery {
        owner: atman_runtime::flow_store::FlowRef,
        side: atman_runtime::flow_store::inbox_view::InboxSide,
        id: Uuid,
    },
    WatchFailure {
        session: Uuid,
        flow: Option<FlowInstanceId>,
    },
    Process(atman_runtime::flow_store::process_watch::ProcessSource),
    Watcher {
        owner: atman_runtime::flow_store::FlowRef,
        id: Uuid,
    },
    Session(SessionTarget),
    Flow(crate::control::ViewAddress),
    Relation {
        id: Uuid,
        source: atman_runtime::flow_store::FlowRef,
        target: AccessTarget,
        kind: RelationKind,
    },
}

impl FlowHubPage {
    pub(super) fn control_state(
        &self,
        address: &atman_runtime::flow_store::FlowRef,
    ) -> Option<atman_rt::FlowInstance<atman_runtime::event::AtmanUuid>> {
        match self {
            Self::Flows { page, .. } => page
                .instances
                .iter()
                .find(|row| &row.address == address)?
                .state
                .clone(),
            Self::Overview(directory) => directory.page.entries.iter().find_map(|entry| {
                let atman_runtime::flow_runtime::catalog::FlowCatalogEntry::Flow {
                    owner,
                    snapshot: Ok(snapshot),
                    ..
                } = entry
                else {
                    return None;
                };
                if owner != address {
                    return None;
                }
                Some(match &snapshot.execution {
                    atman_runtime::flow_runtime::catalog::OverviewExecution::Loaded { state } => {
                        state.clone()
                    }
                    atman_runtime::flow_runtime::catalog::OverviewExecution::Detached => {
                        snapshot.recorded_state.clone()
                    }
                    atman_runtime::flow_runtime::catalog::OverviewExecution::Unavailable {
                        ..
                    } => return None,
                })
            }),
            _ => None,
        }
    }
    pub(super) fn nodes(&self) -> bool {
        matches!(self, Self::Flows { .. } | Self::Overview(_))
    }

    pub(super) fn links(&self) -> Result<&[FlowRelation], &str> {
        match self {
            Self::Flows { relations, .. } => relations.as_deref().map_err(String::as_str),
            Self::Overview(directory) => Ok(&directory.relations),
            _ => Ok(&[]),
        }
    }

    /// Append only the next requested catalog batch; refresh replaces the snapshot.
    pub(super) fn append_catalog(&mut self, previous: Option<&mut Self>) -> Result<(), String> {
        if let Self::Overview(directory) = self {
            return directory.append(match previous {
                Some(Self::Overview(old)) => Some(old),
                _ => None,
            });
        }
        if let Self::Workspaces(directory) = self {
            return directory.append(match previous {
                Some(Self::Workspaces(old)) => Some(old),
                _ => None,
            });
        }
        if let Self::Watchers(directory) = self {
            return directory.append(match previous {
                Some(Self::Watchers(old)) => Some(old),
                _ => None,
            });
        }
        let Self::Relations {
            range: RelationRange::Catalog {
                before: Some(before),
            },
            session,
            focus,
            relations,
            progress: Some(progress),
            ..
        } = self
        else {
            return Ok(());
        };
        let Some(Self::Relations {
            session: old_session,
            focus: old_focus,
            relations: old_relations,
            progress: Some(old_progress),
            ..
        }) = previous
        else {
            return Err("Global relation snapshot changed; refresh to restart".into());
        };
        if session != old_session || focus != old_focus || old_progress.next != Some(*before) {
            return Err("Global relation cursor changed; refresh to restart".into());
        }
        let batch = std::mem::replace(relations, std::mem::take(old_relations));
        relations.extend(batch);
        progress.scanned += old_progress.scanned;
        let errors = std::mem::replace(
            &mut progress.errors,
            std::mem::take(&mut old_progress.errors),
        );
        progress.errors.extend(errors);
        Ok(())
    }

    pub fn title(&self) -> &str {
        match self {
            Self::IndexedHistory(_) => "Indexed history / Recover",
            Self::SessionProject(_) => "Session / Project",
            Self::FlowDetails(_) => "Flow details",
            Self::SessionRemoval(_) => "Sessions / Delete",
            Self::ControlBatch(_) => "Control results",
            Self::ControlHistory(_) => "Control history",
            Self::Management(page) => {
                if page.grant.is_some() {
                    "Management grant"
                } else {
                    "Management"
                }
            }
            Self::Overview(directory) => directory.title(),
            Self::FlowWorkspace(_) => "Flow workspace",
            Self::Workspaces(_) => "Worktrees",
            Self::Workspace(snapshot) => snapshot
                .repository_root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Project worktrees"),
            Self::WorkspaceRecord(page) => &page.workspace.record.id,
            Self::Inbox(page) => match page.side {
                atman_runtime::flow_store::inbox_view::InboxSide::Received => "Inbox / Received",
                atman_runtime::flow_store::inbox_view::InboxSide::Sent => "Inbox / Sent",
            },
            Self::ProcessSources { .. } => "Processes",
            Self::Watchers(directory) => &directory.title,
            Self::Sessions(_) => "Sessions",
            Self::Flows { title, .. }
            | Self::LegacyFlows { title, .. }
            | Self::Relations { title, .. } => title,
        }
    }
    pub fn unit(&self) -> &str {
        match self {
            Self::IndexedHistory(_) => "items",
            Self::SessionProject(_) => "project",
            Self::FlowDetails(_) | Self::SessionRemoval(_) => "items",
            Self::ControlBatch(_) => "results",
            Self::ControlHistory(_) => "batches",
            Self::Management(_) => "details",
            Self::Overview(_) => "flows",
            Self::FlowWorkspace(_) => "details",
            Self::Workspaces(_) => "sources",
            Self::Workspace(_) => "worktrees",
            Self::WorkspaceRecord(_) => "details",
            Self::Inbox(_) => "deliveries",
            Self::ProcessSources { .. } => "processes",
            Self::Watchers(_) => "subscriptions",
            Self::Sessions(_) => "sessions",
            Self::Flows { .. } | Self::LegacyFlows { .. } => "flows",
            Self::Relations { .. } => "relations",
        }
    }
    pub fn first(&self) -> HubRequest {
        match self {
            Self::IndexedHistory(page) => HubRequest::IndexedHistory {
                source: page.source.clone(),
            },
            Self::SessionProject(page) => HubRequest::SessionProject {
                session: page.session(),
            },
            Self::FlowDetails(page) => HubRequest::FlowDetails {
                owner: page.selection.flow.clone(),
            },
            Self::SessionRemoval(page) => page.continuation.as_ref().map_or_else(
                || HubRequest::SessionRemoval {
                    session: page.request.target(),
                },
                |owner| HubRequest::ContinuationRemoval {
                    owner: owner.clone(),
                },
            ),
            Self::ControlBatch(page) => HubRequest::ControlBatch {
                request_id: page.record.id,
            },
            Self::ControlHistory(_) => HubRequest::ControlHistory { before: None },
            Self::Management(page) => HubRequest::Management { grant: page.grant },
            Self::Overview(directory) => directory.request(None),
            Self::FlowWorkspace(page) => HubRequest::FlowWorkspace {
                owner: page.owner.clone(),
            },
            Self::Workspaces(_) => HubRequest::Workspaces { after: None },
            Self::Workspace(snapshot) => HubRequest::Workspace {
                location: super::workspaces::location(snapshot),
            },
            Self::WorkspaceRecord(page) => HubRequest::WorkspaceRecord {
                location: page.location.clone(),
                id: page.workspace.record.id.clone(),
            },
            Self::Inbox(page) => HubRequest::Inbox {
                owner: page.owner.clone(),
                side: page.side,
                before: None,
            },
            Self::ProcessSources { owner, .. } => HubRequest::ProcessSources {
                owner: owner.clone(),
            },
            Self::Watchers(directory) => directory.first(),
            Self::Sessions(_) => HubRequest::Sessions { before: None },
            Self::Flows { session, .. } => HubRequest::Flows {
                session: *session,
                before: None,
            },
            Self::LegacyFlows { session, .. } => HubRequest::LegacyFlows {
                session: *session,
                after: None,
            },
            Self::Relations {
                session,
                focus,
                range,
                ..
            } => HubRequest::Relations {
                session: *session,
                focus: focus.clone(),
                range: range.clone(),
            }
            .first(),
        }
    }
    pub fn next(&self) -> Option<HubRequest> {
        match self {
            Self::IndexedHistory(_) => None,
            Self::SessionProject(_) => None,
            Self::FlowDetails(_) | Self::SessionRemoval(_) => None,
            Self::ControlBatch(_) => None,
            Self::ControlHistory(page) => page.next.map(|before| HubRequest::ControlHistory {
                before: Some(before),
            }),
            Self::Management(_) => None,
            Self::Overview(directory) => directory.next(),
            Self::FlowWorkspace(_) => None,
            Self::Workspaces(directory) => directory
                .next
                .clone()
                .map(|after| HubRequest::Workspaces { after: Some(after) }),
            Self::Workspace(_) | Self::WorkspaceRecord(_) => None,
            Self::Inbox(page) => page.next.clone().map(|before| HubRequest::Inbox {
                owner: page.owner.clone(),
                side: page.side,
                before: Some(before),
            }),
            Self::Watchers(directory) => directory.next(),
            Self::ProcessSources { .. } => None,
            Self::Sessions(page) => page.next.clone().map(|before| HubRequest::Sessions {
                before: Some(before),
            }),
            Self::Flows { session, page, .. } => {
                page.next.clone().map(|before| HubRequest::Flows {
                    session: *session,
                    before: Some(before),
                })
            }
            Self::LegacyFlows { session, next, .. } => {
                next.clone().map(|after| HubRequest::LegacyFlows {
                    session: *session,
                    after: Some(after),
                })
            }
            Self::Relations {
                session,
                focus,
                progress,
                ..
            } => progress
                .as_ref()
                .and_then(|page| page.next)
                .map(|before| HubRequest::Relations {
                    session: *session,
                    focus: focus.clone(),
                    range: RelationRange::Catalog {
                        before: Some(before),
                    },
                }),
        }
    }
    pub fn total(&self) -> usize {
        match self {
            Self::IndexedHistory(page) => indexed_rows(page).len(),
            Self::SessionProject(_) => 1,
            Self::FlowDetails(page) => page.rows().len(),
            Self::SessionRemoval(page) => page.rows().len(),
            Self::ControlBatch(page) => page.rows().len(),
            Self::ControlHistory(page) => page.total,
            Self::Management(page) => page.total(),
            Self::Overview(directory) => directory.page.entries.len(),
            Self::FlowWorkspace(page) => page.rows().len(),
            Self::Workspaces(directory) => directory.rows().len(),
            Self::Workspace(snapshot) => snapshot.workspaces.len(),
            Self::WorkspaceRecord(page) => page.rows().len(),
            Self::Inbox(page) => page.total,
            Self::ProcessSources { sources, .. } => sources.len(),
            Self::Watchers(directory) => directory.total(),
            Self::Sessions(page) => page.total,
            Self::Flows { page, .. } => page.total,
            Self::LegacyFlows { total, .. } => *total,
            Self::Relations { relations, .. } => relations.len(),
        }
    }
    pub(super) fn rows(
        &self,
        focused: Option<&crate::control::ViewAddress>,
        endpoint: Endpoint,
    ) -> Vec<Row> {
        match self {
            Self::IndexedHistory(page) => indexed_rows(page),
            Self::SessionProject(_) => vec![],
            Self::FlowDetails(page) => page.rows(),
            Self::SessionRemoval(page) => page.rows(),
            Self::ControlBatch(page) => page.rows(),
            Self::ControlHistory(page) => super::controls::history_rows(page),
            Self::Management(page) => page.rows(),
            Self::Overview(directory) => directory.rows(),
            Self::FlowWorkspace(page) => page.rows(),
            Self::Workspaces(directory) => directory.rows(),
            Self::Workspace(snapshot) => super::workspaces::resource_rows(snapshot),
            Self::WorkspaceRecord(page) => page.rows(),
            Self::Inbox(page) => super::inbox::rows(page),
            Self::ProcessSources { sources, .. } => sources
                .iter()
                .map(|source| Row {
                    key: RowKey::Process(source.source.clone()),
                    title: source.label.clone(),
                    detail: format!("{} · {:?}", source.source.handle(), source.status),
                    action: Action::None,
                })
                .collect(),
            Self::Watchers(directory) => directory.rows(),
            Self::LegacyFlows { session, flows, .. } => flows
                .iter()
                .map(|owner| {
                    let address = crate::control::ViewAddress::Legacy {
                        session: *session,
                        owner: owner.clone(),
                    };
                    Row {
                        key: RowKey::Flow(address.clone()),
                        title: format!(
                            "{}{}",
                            address.label(),
                            if focused == Some(&address) {
                                " · focused"
                            } else {
                                ""
                            }
                        ),
                        detail: "Original history · read-only snapshot".into(),
                        action: Action::Focus(address),
                    }
                })
                .collect(),
            Self::Sessions(page) => page
                .sessions
                .iter()
                .map(|row| {
                    let request = match &row.target {
                        target if row.cleanup_pending => Some(HubRequest::SessionRemoval {
                            session: target.clone(),
                        }),
                        SessionTarget::Persistent(session) => Some(HubRequest::Flows {
                            session: *session,
                            before: None,
                        }),
                        SessionTarget::Indexed(source) => Some(HubRequest::IndexedHistory {
                            source: source.clone(),
                        }),
                        SessionTarget::Legacy(id) => {
                            Uuid::parse_str(id)
                                .ok()
                                .map(|session| HubRequest::LegacyFlows {
                                    session,
                                    after: None,
                                })
                        }
                    };
                    Row {
                        key: RowKey::Session(row.target.clone()),
                        title: row.title.clone(),
                        detail: row.detail.clone(),
                        action: request.map_or(Action::None, Action::Load),
                    }
                })
                .chain(std::iter::once(Row {
                    key: RowKey::Overview,
                    title: "All flows".into(),
                    detail: "Across sessions · Ctrl+B".into(),
                    action: Action::Load(HubRequest::Overview { after: None }),
                }))
                .chain(std::iter::once(Row {
                    key: RowKey::ManagementField("Directory"),
                    title: "Management".into(),
                    detail: "Session binding and explicit flow permissions".into(),
                    action: Action::Load(HubRequest::Management { grant: None }),
                }))
                .collect(),
            Self::Flows {
                default_flow, page, ..
            } => page
                .instances
                .iter()
                .map(|row| {
                    let marker =
                        if Some(&row.address) == focused.and_then(|address| address.instance()) {
                            " · focused"
                        } else if default_flow.as_ref() == Some(&row.address.instance_id) {
                            " · default"
                        } else {
                            ""
                        };
                    let status = row.state.as_ref().map_or("Unavailable", |state| {
                        if row.loaded {
                            execution_label(state.state())
                        } else {
                            "Detached"
                        }
                    });
                    let paused = row
                        .state
                        .as_ref()
                        .is_some_and(|state| !state.pauses().is_empty());
                    Row {
                        key: RowKey::Flow(row.address.clone().into()),
                        title: format!(
                            "{}{marker}{}",
                            row.entry.as_deref().unwrap_or("Unavailable flow"),
                            if row.archived { " · Archived" } else { "" },
                        ),
                        detail: row.unavailable.clone().unwrap_or_else(|| {
                            format!(
                                "{} · {status}{}\n{}",
                                row.address.instance_id,
                                if paused { " · execution paused" } else { "" },
                                activity_label(row.activity.as_ref()),
                            )
                        }),
                        action: Action::Focus(row.address.clone().into()),
                    }
                })
                .collect(),
            Self::Relations {
                relations,
                focus,
                progress,
                ..
            } => relations
                .iter()
                .filter(|relation| focus.as_ref().is_none_or(|flow| incident(relation, flow)))
                .map(|relation| {
                    let kind = match relation.kind {
                        RelationKind::Created => "Created",
                        RelationKind::Read => "Read",
                        RelationKind::Watch => "Watch",
                        RelationKind::Interject => "Interject",
                    };
                    let source = format!(
                        "{}/{}",
                        relation.source.session_id, relation.source.instance_id
                    );
                    let (target, action) = match &relation.target {
                        AccessTarget::Flow(flow) => (
                            format!("{}/{}", flow.session_id, flow.instance_id),
                            Action::Focus(flow.clone().into()),
                        ),
                        AccessTarget::Session(session) => (
                            format!("session {session}"),
                            Action::Load(HubRequest::Flows {
                                session: *session,
                                before: None,
                            }),
                        ),
                        AccessTarget::AllSessions => (
                            "all sessions".into(),
                            Action::Load(HubRequest::Sessions { before: None }),
                        ),
                    };
                    let open_source = matches!(endpoint, Endpoint::Source)
                        || matches!(endpoint, Endpoint::Neighbor)
                            && focus.as_ref().is_some_and(|flow| &relation.source != flow);
                    let action = if open_source {
                        Action::Focus(relation.source.clone().into())
                    } else {
                        action
                    };
                    let label = if open_source {
                        short_flow(&relation.source)
                    } else {
                        match &relation.target {
                            AccessTarget::Flow(flow) => short_flow(flow),
                            AccessTarget::Session(session) => {
                                format!("session {}", &session.to_string()[28..])
                            }
                            AccessTarget::AllSessions => "all sessions".into(),
                        }
                    };
                    Row {
                        key: RowKey::Relation {
                            id: relation.id,
                            source: relation.source.clone(),
                            target: relation.target.clone(),
                            kind: relation.kind,
                        },
                        title: format!("{kind} {} {}", if open_source { "←" } else { "→" }, label),
                        detail: format!(
                            "{source} → {target} · {} · relation {}",
                            if relation.active {
                                "active"
                            } else {
                                "inactive"
                            },
                            relation.id
                        ),
                        action,
                    }
                })
                .chain(
                    progress
                        .as_ref()
                        .into_iter()
                        .flat_map(|progress| &progress.errors)
                        .map(|(session, error)| Row {
                            key: RowKey::Session(SessionTarget::Persistent(*session)),
                            title: format!("Unavailable Session {}", &session.to_string()[28..]),
                            detail: error.clone(),
                            action: Action::Load(HubRequest::Flows {
                                session: *session,
                                before: None,
                            }),
                        }),
                )
                .collect(),
        }
    }
}

pub(super) fn activity_label(
    activity: Option<&atman_runtime::flow_store::activity::RecordedActivity>,
) -> String {
    activity.map_or_else(
        || "Last activity: no timestamp".into(),
        |activity| {
            format!(
                "Last {}",
                activity
                    .at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M %:z")
            )
        },
    )
}

pub(super) fn short_flow(flow: &atman_runtime::flow_store::FlowRef) -> String {
    format!(
        "{}/{}",
        &flow.session_id.to_string()[28..],
        &flow.instance_id.to_string()[28..]
    )
}

/// Broad grants stay broad; the directory does not synthesize per-flow edges.
pub(super) fn incident(relation: &FlowRelation, flow: &atman_runtime::flow_store::FlowRef) -> bool {
    &relation.source == flow
        || match &relation.target {
            AccessTarget::Flow(target) => target == flow,
            AccessTarget::Session(session) => *session == flow.session_id.0,
            AccessTarget::AllSessions => true,
        }
}

#[cfg(test)]
mod tests {
    #[test]
    fn indexed_history_review_requires_explicit_action_and_tracks_keyboard_mouse() {
        use crate::control::{ControlTicket, ViewAddress, ViewBinding};
        use crate::flow_hub::FlowHub;
        use crate::keys::KeyAction;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let source = atman_runtime::history_index::IndexedSource {
            index: "/history/index.db".into(),
            session: Uuid::now_v7(),
        };
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: ViewAddress::Legacy {
                    session: Uuid::now_v7(),
                    owner: LegacyFlowKey::Default,
                },
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::IndexedHistory(Box::new(
                atman_runtime::history_index::IndexReview {
                    source: source.clone(),
                    records: 42,
                    gaps: 1,
                    missing_images: 2,
                    missing_outputs: 1,
                    digest: "digest".into(),
                    preview: vec!["retained text".into()],
                },
            )))),
        );
        assert!(hub.visible());
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        hub.key(KeyAction::Escape);
        for _ in 0..3 {
            hub.key(KeyAction::HistoryDown);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 45)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &ticket.binding.flow))
            .unwrap();
        assert!(matches!(hub.float(), Action::None));
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::RecoverIndexedHistory(review) if review.source == source)
        );
        let (hit, _) = hub
            .hits
            .iter()
            .find(|(_, index)| matches!(hub.rows[*index].action, Action::RecoverIndexedHistory(_)))
            .unwrap();
        assert!(hit.height >= 3);
        assert!(
            matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: hit.x + 1, row: hit.y + 1, modifiers: KeyModifiers::NONE }), Action::RecoverIndexedHistory(review) if review.source == source)
        );
        hub.pending = Some(ticket);
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
    }
    use super::*;
    use atman_runtime::flow_store::FlowRef;

    #[test]
    fn session_navigation_uses_declared_default_independently_of_catalog_page() {
        let session = Uuid::now_v7();
        let default_flow = FlowInstanceId::now();
        let mut page = FlowHubPage::Flows {
            session,
            title: "Example".into(),
            default_flow: Some(default_flow.clone()),
            page: InstancePage {
                instances: vec![],
                next: None,
                total: 0,
                catalog: atman_runtime::event_log::journal::CommitReceipt {
                    journal_id: Uuid::now_v7(),
                    sequence: 1,
                    digest: "test".into(),
                },
            },
            relations: Ok(vec![]),
        };
        let address = FlowRef {
            session_id: session.into(),
            instance_id: default_flow,
        };
        assert_eq!(page.session_destination(), Some(Ok(address.into())));
        if let FlowHubPage::Flows { default_flow, .. } = &mut page {
            *default_flow = None;
        }
        assert!(matches!(page.session_destination(), Some(Err(_))));
        let mut page = FlowHubPage::LegacyFlows {
            session,
            title: "History".into(),
            flows: vec![LegacyFlowKey::Unassigned],
            next: None,
            total: 1,
        };
        assert!(matches!(page.session_destination(), Some(Err(_))));
        if let FlowHubPage::LegacyFlows { flows, .. } = &mut page {
            flows.push(LegacyFlowKey::Default);
        }
        assert_eq!(
            page.session_destination(),
            Some(Ok(crate::control::ViewAddress::Legacy {
                session,
                owner: LegacyFlowKey::Default
            }))
        );
    }

    #[test]
    fn relation_endpoints_keep_identity_direction_and_broad_targets() {
        let observer = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let observed = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let relations = vec![
            FlowRelation {
                source: observer.clone(),
                target: AccessTarget::Flow(observed.clone()),
                kind: RelationKind::Watch,
                id: Uuid::now_v7(),
                active: false,
            },
            FlowRelation {
                source: observer.clone(),
                target: AccessTarget::AllSessions,
                kind: RelationKind::Read,
                id: Uuid::now_v7(),
                active: true,
            },
            FlowRelation {
                source: observer.clone(),
                target: AccessTarget::Session(observed.session_id.0),
                kind: RelationKind::Interject,
                id: Uuid::now_v7(),
                active: true,
            },
        ];
        let page = FlowHubPage::Relations {
            session: observer.session_id.0,
            title: "Scope".into(),
            focus: Some(observed.clone()),
            range: RelationRange::Session,
            progress: None,
            relations,
        };
        let neighbors = page.rows(None, Endpoint::Neighbor);
        assert_eq!(neighbors.len(), 3);
        assert!(neighbors.iter().all(|row| matches!(&row.action, Action::Focus(address) if address.instance() == Some(&observer))));
        assert!(neighbors.iter().all(|row| row.title.contains('←')));
        assert!(neighbors[0].detail.contains("inactive"));
        let sources = page.rows(None, Endpoint::Source);
        let targets = page.rows(None, Endpoint::Target);
        assert!(
            neighbors
                .iter()
                .zip(&targets)
                .zip(&sources)
                .all(|((left, right), source)| left.key == right.key && left.key == source.key)
        );
        assert!(
            matches!(&targets[0].action, Action::Focus(address) if address.instance() == Some(&observed))
        );
        assert!(matches!(
            &targets[1].action,
            Action::Load(HubRequest::Sessions { before: None })
        ));
        assert!(
            matches!(&targets[2].action, Action::Load(HubRequest::Flows { session, before: None }) if *session == observed.session_id.0)
        );
    }
}
