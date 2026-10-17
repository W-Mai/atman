//! Session discovery shares the runtime's read-only catalog projection.

use anyhow::Result;
use atman_proto::{PersistentSessionSummary, SessionId, SessionStatus, SessionSummary};
use atman_runtime::flow_runtime::catalog::{
    CatalogStatus, SessionCatalog, SessionOverview, SessionSummary as RuntimeSummary,
};

use crate::DaemonState;

pub(crate) async fn list(state: &DaemonState) -> Result<Vec<SessionSummary>> {
    // Listing must not initialize the Session service, profile, or any execution owner.
    let catalog = SessionCatalog::new(
        state
            .session_service
            .get()
            .map(|service| service.host.clone()),
        Some(state.data_dir().to_path_buf()),
    );
    let mut rows = Vec::new();
    for id in catalog.ids().await? {
        let view = catalog
            .overview(id)
            .await
            .unwrap_or_else(|error| SessionOverview {
                summary: RuntimeSummary::unavailable(id, error),
                default_goal: None,
            });
        rows.push(summary(view));
    }
    Ok(rows)
}

fn summary(view: SessionOverview) -> SessionSummary {
    let row = view.summary;
    SessionSummary {
        id: SessionId(row.session_id),
        title: row.title.unwrap_or_else(|| "Unavailable session".into()),
        project_root: row.project_root.map(|root| root.display().to_string()),
        status: match row.status {
            CatalogStatus::Running => SessionStatus::Running,
            CatalogStatus::Idle => SessionStatus::Idle,
            CatalogStatus::Closed => SessionStatus::Finished,
            CatalogStatus::Detached => SessionStatus::Detached,
            CatalogStatus::Unavailable => SessionStatus::Unavailable,
        },
        event_count: None,
        first_ts: None,
        goal: view.default_goal,
        name_source: Default::default(),
        persistent: Some(PersistentSessionSummary {
            default_flow: row.default_flow.map(|id| id.0.0),
            flow_count: row.flow_count,
            loaded_flows: row.loaded_flows,
            catalog_sequence: row.catalog.map(|cursor| cursor.sequence),
            trailing_bytes: row.trailing_bytes,
            unavailable: row.unavailable,
        }),
    }
}
