//! Session discovery observes existing owners and reads detached catalogs without opening them.

use anyhow::Result;
use atman_proto::{PersistentSessionSummary, SessionId, SessionStatus, SessionSummary};
use atman_rt::{InstanceActivity, InstanceState};
use atman_runtime::flow_store::{RuntimeStore, SessionInspection};

use crate::DaemonState;

pub(crate) async fn list(state: &DaemonState) -> Result<Vec<SessionSummary>> {
    let data = state.data_dir().to_path_buf();
    let attached = state
        .session_service
        .get()
        .map(|service| service.host.sessions())
        .unwrap_or_default();
    let initially_attached = attached.clone();
    let mut rows = tokio::task::spawn_blocking(move || {
        let mut rows = Vec::new();
        for id in RuntimeStore::session_ids(&data)? {
            if attached.contains(&id) {
                continue;
            }
            rows.push(match RuntimeStore::inspect_session(&data, id) {
                Ok(read) => recorded(read),
                Err(error) => unavailable(id, error),
            });
        }
        Ok::<_, std::io::Error>(rows)
    })
    .await??;
    if let Some(service) = state.session_service.get() {
        let observed_ids: std::collections::BTreeSet<_> = initially_attached
            .into_iter()
            .chain(service.host.sessions())
            .collect();
        for id in observed_ids {
            // A session attached during the disk scan supersedes its detached observation.
            rows.retain(|row| row.id.0 != id);
            let result = async {
                let session = service.session(id)?;
                Ok::<_, anyhow::Error>(session.observe().await?)
            }
            .await;
            rows.push(match result {
                Ok(view) => {
                    let mut row = summary(&view.snapshot.catalog, view.snapshot.cursor.sequence);
                    row.status = if view.loaded.iter().any(|instance| {
                        matches!(
                            instance.execution.state(),
                            InstanceState::Open(InstanceActivity::Active(_))
                                | InstanceState::Closing { active: Some(_) }
                        )
                    }) {
                        SessionStatus::Running
                    } else if view.loaded.len() < view.snapshot.catalog.flows.len() {
                        SessionStatus::Detached
                    } else if !view.loaded.is_empty()
                        && view.loaded.iter().all(|instance| {
                            matches!(instance.execution.state(), InstanceState::Closed)
                        })
                    {
                        SessionStatus::Finished
                    } else {
                        SessionStatus::Idle
                    };
                    row.persistent.as_mut().unwrap().loaded_flows = Some(view.loaded.len());
                    row.goal = view.loaded.iter().find_map(|instance| {
                        (view.snapshot.catalog.default_flow.as_ref()
                            == Some(&instance.execution.address().instance_id))
                        .then(|| instance.goal.as_ref().map(|goal| goal.objective.clone()))
                        .flatten()
                    });
                    row
                }
                Err(error) => unavailable(id, error),
            });
        }
    }
    Ok(rows)
}

fn summary(
    catalog: &atman_runtime::flow_store::SessionProjection,
    sequence: u64,
) -> SessionSummary {
    SessionSummary {
        id: SessionId(catalog.descriptor.id),
        title: catalog.descriptor.title.clone(),
        project_root: catalog
            .descriptor
            .project
            .as_ref()
            .map(|project| project.root().display().to_string()),
        status: SessionStatus::Detached,
        event_count: None,
        first_ts: None,
        goal: None,
        name_source: Default::default(),
        persistent: Some(PersistentSessionSummary {
            default_flow: catalog.default_flow.as_ref().map(|id| id.0.0),
            flow_count: Some(catalog.flows.len()),
            loaded_flows: None,
            catalog_sequence: Some(sequence),
            trailing_bytes: None,
            unavailable: None,
        }),
    }
}

fn recorded(read: SessionInspection) -> SessionSummary {
    let mut row = summary(
        &read.projection,
        read.journal.cursor.as_ref().unwrap().sequence,
    );
    row.persistent.as_mut().unwrap().trailing_bytes = Some(read.journal.trailing_bytes);
    row
}

fn unavailable(id: uuid::Uuid, error: impl std::fmt::Display) -> SessionSummary {
    SessionSummary {
        id: SessionId(id),
        title: "Unavailable session".into(),
        project_root: None,
        status: SessionStatus::Unavailable,
        event_count: None,
        first_ts: None,
        goal: None,
        name_source: Default::default(),
        persistent: Some(PersistentSessionSummary {
            default_flow: None,
            flow_count: None,
            loaded_flows: None,
            catalog_sequence: None,
            trailing_bytes: None,
            unavailable: Some(error.to_string()),
        }),
    }
}
