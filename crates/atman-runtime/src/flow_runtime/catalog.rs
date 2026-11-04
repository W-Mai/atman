//! Shared session and instance discovery without opening execution owners.

mod overview;
mod scan;
mod watches;
mod workspaces;
pub use overview::{OverviewCatalogPage, OverviewCatalogSnapshot, OverviewExecution};
pub use scan::{FlowCatalogCursor, FlowCatalogEntry, FlowCatalogPage};
pub use watches::{WatchCatalogEntry, WatchCatalogPage, WatchCatalogSnapshot};
pub use workspaces::{WorkspaceCatalogEntry, WorkspaceCatalogPage, WorkspaceCatalogSnapshot};

use std::{io, path::PathBuf, sync::Arc};

use atman_rt::{InstanceActivity, InstanceState};
use serde::Serialize;
use uuid::Uuid;

use super::{
    access::{InstanceObservation, InstancePage, instance_page},
    host::{RuntimeHost, SessionHandle, SessionObservation, SessionSnapshot},
};
use crate::{
    event_log::journal::CommitReceipt,
    flow_store::{
        FlowInstanceId, FlowRef, RuntimeStore, SessionProjection, invalid, relation::FlowRelation,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    Running,
    Idle,
    Closed,
    Detached,
    Unavailable,
}

/// Discoverable metadata; conversation, goal bodies, and configuration are excluded.
#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub session_id: Uuid,
    pub title: Option<String>,
    pub project_root: Option<PathBuf>,
    pub status: CatalogStatus,
    pub flow_count: Option<usize>,
    pub default_flow: Option<FlowInstanceId>,
    pub loaded_flows: Option<usize>,
    pub catalog: Option<CommitReceipt>,
    pub trailing_bytes: Option<u64>,
    pub unavailable: Option<String>,
}

/// Trusted operator overview. Peer discovery exposes only `summary`.
pub struct SessionOverview {
    pub summary: SessionSummary,
    pub default_goal: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub next: Option<Uuid>,
    pub total: usize,
}

/// One Session's independently sampled relation records. Failure does not hide other Sessions.
pub struct SessionRelations {
    pub session_id: Uuid,
    pub relations: Result<Vec<FlowRelation>, String>,
}

/// Incremental relation discovery, bounded by the number of inspected Sessions.
pub struct RelationCatalogPage {
    pub sessions: Vec<SessionRelations>,
    pub next: Option<Uuid>,
    pub total: usize,
}

#[derive(Clone)]
pub struct SessionCatalog {
    host: Option<RuntimeHost>,
    data_dir: Option<PathBuf>,
}

impl SessionCatalog {
    /// Host-only source selection. A missing data directory restricts discovery to attached owners.
    pub fn new(host: Option<RuntimeHost>, data_dir: Option<PathBuf>) -> Self {
        Self { host, data_dir }
    }

    pub async fn ids(&self) -> io::Result<Vec<Uuid>> {
        let mut ids = match self.data_dir.clone() {
            Some(data) => tokio::task::spawn_blocking(move || RuntimeStore::session_ids(&data))
                .await
                .map_err(io::Error::other)??,
            None => Vec::new(),
        };
        if let Some(host) = &self.host {
            ids.extend(host.sessions());
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// Page authorized metadata without opening execution owners. Authorization is rechecked before returning counts or rows.
    pub async fn page(
        &self,
        before: Option<Uuid>,
        limit: usize,
        visible: impl Fn(Uuid) -> io::Result<bool>,
    ) -> io::Result<SessionPage> {
        if !(1..=50).contains(&limit) {
            return Err(invalid("session page limit must be between 1 and 50"));
        }
        let mut ids = Vec::new();
        for id in self.ids().await? {
            if visible(id)? {
                ids.push(id);
            }
        }
        let eligible: Vec<_> = ids
            .iter()
            .rev()
            .copied()
            .filter(|id| before.is_none_or(|before| *id < before))
            .collect();
        let mut sessions = Vec::new();
        for &id in eligible.iter().take(limit) {
            sessions.push(match self.overview(id).await {
                Ok(view) => view.summary,
                Err(error) => SessionSummary::unavailable(id, error),
            });
        }
        for &id in &ids {
            if !visible(id)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "session discovery authority changed",
                ));
            }
        }
        Ok(SessionPage {
            next: (eligible.len() > limit).then(|| eligible[limit - 1]),
            total: ids.len(),
            sessions,
        })
    }

    /// Scan authorized Session relations without creating execution owners. The cursor advances past failed and empty Sessions as well.
    pub async fn relation_page(
        &self,
        before: Option<Uuid>,
        limit: usize,
        visible: impl Fn(Uuid) -> io::Result<bool>,
    ) -> io::Result<RelationCatalogPage> {
        if !(1..=50).contains(&limit) {
            return Err(invalid(
                "relation page limit must be between 1 and 50 Sessions",
            ));
        }
        let mut ids = Vec::new();
        for id in self.ids().await? {
            if visible(id)? {
                ids.push(id);
            }
        }
        let eligible: Vec<_> = ids
            .iter()
            .rev()
            .copied()
            .filter(|id| before.is_none_or(|before| *id < before))
            .collect();
        let mut sessions = Vec::new();
        for &session_id in eligible.iter().take(limit) {
            // A changing policy must not authorize disk access using an earlier snapshot.
            require_visible(session_id, &visible)?;
            sessions.push(SessionRelations {
                session_id,
                relations: self
                    .relations(session_id)
                    .await
                    .map_err(|error| error.to_string()),
            });
        }
        for &id in &ids {
            require_visible(id, &visible)?;
        }
        Ok(RelationCatalogPage {
            sessions,
            next: (eligible.len() > limit).then(|| eligible[limit - 1]),
            total: ids.len(),
        })
    }

    fn attached(&self, id: Uuid) -> io::Result<Option<SessionHandle>> {
        match self.host.as_ref().map(|host| host.session(id)) {
            Some(Ok(session)) => Ok(Some(session)),
            Some(Err(error)) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(None),
        }
    }

    async fn recorded(&self, id: Uuid) -> io::Result<crate::flow_store::SessionInspection> {
        let data = self.data_dir.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "session is not attached and no catalog directory is bound",
            )
        })?;
        tokio::task::spawn_blocking(move || RuntimeStore::inspect_session(&data, id))
            .await
            .map_err(io::Error::other)?
    }

    pub async fn overview(&self, id: Uuid) -> io::Result<SessionOverview> {
        if let Some(session) = self.attached(id)? {
            return Ok(SessionOverview::live(session.observe().await?));
        }
        let read = self.recorded(id).await;
        // Prefer an owner which attached during disk inspection.
        if let Some(session) = self.attached(id)? {
            return Ok(SessionOverview::live(session.observe().await?));
        }
        let read = read?;
        Ok(SessionOverview {
            summary: SessionSummary::recorded(
                &read.projection,
                read.journal
                    .cursor
                    .ok_or_else(|| invalid("session journal has no creation"))?,
                Some(read.journal.trailing_bytes),
            ),
            default_goal: None,
        })
    }

    /// Read the durable relation projection without opening a flow writer or scheduler.
    pub async fn relations(&self, id: Uuid) -> io::Result<Vec<FlowRelation>> {
        let read = self.recorded(id).await?;
        let mut relations = tokio::task::spawn_blocking(move || read.relations())
            .await
            .map_err(io::Error::other)??;
        if let Some(host) = &self.host
            && let Some(view) = host.management_view().await
            && view.unavailable.is_none()
            && view
                .profile
                .management
                .as_ref()
                .is_some_and(|binding| binding.session.id == id)
        {
            let grants = view
                .profile
                .management
                .as_ref()
                .map(|binding| binding.grants.iter())
                .into_iter()
                .flatten();
            relations.extend(crate::flow_store::relation::grant_relations(grants));
        }
        relations.sort_by_key(|relation| {
            (
                format!("{:?}", relation.source),
                format!("{:?}", relation.target),
                relation.kind,
                relation.id,
            )
        });
        relations.dedup();
        Ok(relations)
    }

    /// Inspect one registered flow's committed facts without attaching an execution owner. Recorded activity does not imply a live worker.
    pub async fn inspect_flow(
        &self,
        flow: &FlowRef,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<crate::flow_store::FlowInspection> {
        authorize(flow)?;
        let read = self.recorded(flow.session_id.0).await?;
        authorize(flow)?;
        let instance = flow.instance_id.clone();
        let inspected = tokio::task::spawn_blocking(move || read.inspect_flow(&instance))
            .await
            .map_err(io::Error::other)??;
        authorize(flow)?;
        Ok(inspected)
    }

    /// Observe authorized recorded or attached facts without opening an execution owner.
    pub async fn describe_flow(
        &self,
        flow: &FlowRef,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<InstanceObservation> {
        authorize(flow)?;
        if let Some(session) = self.attached(flow.session_id.0)? {
            let result = session.describe_flow(flow.instance_id.clone()).await?;
            authorize(flow)?;
            return Ok(result);
        }
        let recorded = self.inspect_flow(flow, &authorize).await?;
        if let Some(session) = self.attached(flow.session_id.0)? {
            let result = session.describe_flow(flow.instance_id.clone()).await?;
            authorize(flow)?;
            return Ok(result);
        }
        authorize(flow)?;
        Ok(InstanceObservation::recorded(recorded))
    }

    /// Read a bounded delivery-history page without attaching execution services. Authorization is checked before reading and before returning any contents or counts.
    pub(crate) async fn observe_input_request(
        &self,
        flow: &FlowRef,
        sender: crate::flow_store::inbox::SenderIdentity,
        request_id: Uuid,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<Option<crate::flow_store::input_observation::InputObservation>> {
        authorize(flow)?;
        let result = if let Some(session) = self.attached(flow.session_id.0)? {
            session
                .observe_input_request(flow.instance_id.clone(), sender, request_id)
                .await?
        } else {
            let read = self.recorded(flow.session_id.0).await?;
            authorize(flow)?;
            let id = flow.instance_id.clone();
            tokio::task::spawn_blocking(move || {
                read.inspect_input_request(&id, &sender, request_id)
            })
            .await
            .map_err(io::Error::other)??
        };
        authorize(flow)?;
        Ok(result)
    }

    /// Read a bounded delivery-history page without attaching execution services. Authorization is checked before reading and before returning any contents or counts.
    pub async fn inbox_page(
        &self,
        flow: &FlowRef,
        side: crate::flow_store::inbox_view::InboxSide,
        before: Option<crate::flow_store::inbox_view::InboxHistoryCursor>,
        limit: usize,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<crate::flow_store::inbox_view::InboxHistoryPage> {
        authorize(flow)?;
        let read = self.recorded(flow.session_id.0).await?;
        authorize(flow)?;
        let instance = flow.instance_id.clone();
        let page = tokio::task::spawn_blocking(move || {
            read.inspect_inbox(&instance, side, before.as_ref(), limit)
        })
        .await
        .map_err(io::Error::other)??;
        authorize(flow)?;
        Ok(page)
    }

    pub async fn instances(
        &self,
        id: Uuid,
        before: Option<FlowInstanceId>,
        limit: usize,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<InstancePage> {
        if let Some(session) = self.attached(id)? {
            return session.instances(before, limit, authorize).await;
        }
        let read = self.recorded(id).await;
        if let Some(session) = self.attached(id)? {
            return session.instances(before, limit, authorize).await;
        }
        let read = Arc::new(read?);
        let snapshot = SessionSnapshot {
            catalog: read.projection.clone(),
            cursor: read
                .journal
                .cursor
                .clone()
                .ok_or_else(|| invalid("session journal has no creation"))?,
        };
        instance_page(snapshot, before, limit, authorize, move |instance| {
            let read = read.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    read.inspect_flow(&instance)
                        .map(InstanceObservation::recorded)
                })
                .await
                .map_err(io::Error::other)?
            }
        })
        .await
    }
}

fn require_visible(id: Uuid, visible: &impl Fn(Uuid) -> io::Result<bool>) -> io::Result<()> {
    if visible(id)? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "catalog discovery authority changed",
        ))
    }
}

impl SessionOverview {
    fn live(view: SessionObservation) -> Self {
        let mut summary =
            SessionSummary::recorded(&view.snapshot.catalog, view.snapshot.cursor, None);
        summary.status = if view.loaded.iter().any(|flow| {
            matches!(
                flow.execution.state(),
                InstanceState::Open(InstanceActivity::Active(_))
                    | InstanceState::Closing { active: Some(_) }
            )
        }) {
            CatalogStatus::Running
        } else if view.loaded.len() < view.snapshot.catalog.flows.len() {
            CatalogStatus::Detached
        } else if !view.loaded.is_empty()
            && view
                .loaded
                .iter()
                .all(|flow| matches!(flow.execution.state(), InstanceState::Closed))
        {
            CatalogStatus::Closed
        } else {
            CatalogStatus::Idle
        };
        summary.loaded_flows = Some(view.loaded.len());
        let default_goal = view.loaded.iter().find_map(|flow| {
            (view.snapshot.catalog.default_flow.as_ref()
                == Some(&flow.execution.address().instance_id))
            .then(|| flow.goal.as_ref().map(|goal| goal.objective.clone()))
            .flatten()
        });
        Self {
            summary,
            default_goal,
        }
    }
}

impl SessionSummary {
    fn recorded(
        catalog: &SessionProjection,
        cursor: CommitReceipt,
        trailing_bytes: Option<u64>,
    ) -> Self {
        Self {
            session_id: catalog.descriptor.id,
            title: catalog.title.clone(),
            project_root: catalog
                .project
                .as_ref()
                .map(|project| project.root().to_owned()),
            status: CatalogStatus::Detached,
            flow_count: Some(catalog.flows.len()),
            default_flow: catalog.default_flow.clone(),
            loaded_flows: None,
            catalog: Some(cursor),
            trailing_bytes,
            unavailable: None,
        }
    }

    pub fn unavailable(id: Uuid, error: impl std::fmt::Display) -> Self {
        Self {
            session_id: id,
            title: None,
            project_root: None,
            status: CatalogStatus::Unavailable,
            flow_count: None,
            default_flow: None,
            loaded_flows: None,
            catalog: None,
            trailing_bytes: None,
            unavailable: Some(error.to_string()),
        }
    }
}
