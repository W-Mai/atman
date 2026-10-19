//! Bounded overview of committed Flow facts and independently acknowledged execution.

use std::{io, path::PathBuf};

use atman_rt::FlowInstance;
use uuid::Uuid;

use super::{FlowCatalogCursor, FlowCatalogPage, SessionCatalog};
use crate::{
    event::AtmanUuid,
    event_log::journal::CommitReceipt,
    flow_store::{
        FlowRef, invalid,
        relation::{FlowRelation, access_relations, grant_relations},
    },
    memory::goal::GoalRecord,
};

pub type OverviewCatalogPage = FlowCatalogPage<OverviewCatalogSnapshot>;

#[derive(Debug)]
pub struct OverviewCatalogSnapshot {
    pub title: String,
    pub project_root: Option<PathBuf>,
    pub default_flow: bool,
    pub cursor: CommitReceipt,
    pub trailing_bytes: u64,
    pub recorded_state: FlowInstance<AtmanUuid>,
    pub goal: Option<GoalRecord>,
    pub pending_inputs: usize,
    pub relations: Vec<FlowRelation>,
    /// Sampled after the journal read, without replacing its state or receipt.
    pub execution: OverviewExecution,
}

#[derive(Debug)]
pub enum OverviewExecution {
    Detached,
    Loaded { state: FlowInstance<AtmanUuid> },
    Unavailable { reason: String },
}

impl SessionCatalog {
    /// Inspect bounded, independently committed Flow snapshots without opening execution owners. Includes Goal content: callers must authorize content inspection, not only metadata discovery. Host execution observations and management grants are sampled independently; neither implies a global atomic snapshot or permission to control a target.
    pub async fn overview_page(
        &self,
        cursor: Option<FlowCatalogCursor>,
        limit: usize,
        visible: impl Fn(Uuid) -> io::Result<bool>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<OverviewCatalogPage> {
        self.scan_page_observed(
            cursor,
            limit,
            visible,
            authorize,
            |flow, session| {
                let owner = &flow.descriptor.address;
                let mut relations = flow.relations;
                relations.extend(
                    access_relations(session)
                        .into_iter()
                        .filter(|relation| &relation.source == owner),
                );
                Ok(OverviewCatalogSnapshot {
                    title: flow.descriptor.content.title().into(),
                    project_root: session
                        .descriptor
                        .project
                        .as_ref()
                        .map(|project| project.root().to_owned()),
                    default_flow: session.default_flow.as_ref() == Some(&owner.instance_id),
                    cursor: flow
                        .journal
                        .cursor
                        .ok_or_else(|| invalid("flow journal has no creation"))?,
                    trailing_bytes: flow.journal.trailing_bytes,
                    recorded_state: flow.state,
                    goal: flow.goal,
                    pending_inputs: flow.pending_inputs,
                    relations,
                    execution: OverviewExecution::Detached,
                })
            },
            |owner, mut snapshot| async move {
                snapshot.execution = match self.attached(owner.session_id.0) {
                    Ok(Some(session)) => {
                        match session.describe_loaded_flow(&owner.instance_id).await {
                            Ok(Some(observed)) => OverviewExecution::Loaded {
                                state: observed.state,
                            },
                            Ok(None) => OverviewExecution::Detached,
                            Err(error) => OverviewExecution::Unavailable {
                                reason: error.to_string(),
                            },
                        }
                    }
                    Ok(None) => OverviewExecution::Detached,
                    Err(error) => OverviewExecution::Unavailable {
                        reason: error.to_string(),
                    },
                };
                if let Some(host) = &self.host
                    && let Some(view) = host.management_view().await
                    && view.unavailable.is_none()
                    && let Some(binding) = &view.profile.management
                    && binding.session.id == owner.session_id.0
                {
                    snapshot.relations.extend(grant_relations(
                        binding.grants.iter().filter(|grant| grant.grantee == owner),
                    ));
                }
                snapshot.relations.sort_by_key(|relation| {
                    (
                        format!("{:?}", relation.source),
                        format!("{:?}", relation.target),
                        relation.kind,
                        relation.id,
                    )
                });
                snapshot.relations.dedup();
                Ok(snapshot)
            },
        )
        .await
    }
}
