//! Session discovery shares the runtime's read-only catalog projection.

use anyhow::Result;
use atman_proto::{PersistentSessionSummary, SessionId, SessionStatus, SessionSummary};
use atman_runtime::flow_runtime::catalog::{
    CatalogStatus, SessionCatalog, SessionOverview, SessionSummary as RuntimeSummary,
};
use atman_runtime::flow_store::local_operator::LocalOperator;

use crate::DaemonState;

pub(super) async fn input_status(
    state: &DaemonState,
    request: atman_proto::flows::InputStatusRequest,
) -> Result<serde_json::Value> {
    input_status_for(state, request, LocalOperator::new(state.operator_id())).await
}

pub(crate) async fn input_status_for(
    state: &DaemonState,
    request: atman_proto::flows::InputStatusRequest,
    operator: LocalOperator,
) -> Result<serde_json::Value> {
    let catalog = SessionCatalog::new(
        state
            .session_service
            .get()
            .map(|service| service.host.clone()),
        Some(state.data_dir().to_path_buf()),
    );
    let observation = catalog
        .observe_operator_input(
            &super::core_address(request.flow.clone()),
            operator,
            request.request_id,
            |_| Ok(()),
        )
        .await?;
    Ok(serde_json::to_value(
        atman_proto::responses::InputStatusResponse {
            flow: request.flow,
            request_id: request.request_id,
            observation: observation
                .map(super::wire::input_observation)
                .transpose()?,
        },
    )?)
}

pub(super) async fn execution_control(
    state: &DaemonState,
    request: atman_proto::execution::FlowExecutionRequest,
) -> Result<serde_json::Value> {
    use atman_proto::{execution::*, flows::FlowControl};
    use atman_rt::{InstanceActivity, InstanceState, Interruption, TurnPhase};
    use atman_runtime::flow_store::{control::ControlAction, inbox::SenderIdentity};

    let service = state.session_service.get();
    let catalog = SessionCatalog::new(
        service.map(|service| service.host.clone()),
        Some(state.data_dir().to_path_buf()),
    );
    let operator = service.map_or(super::CONTROL_OPERATOR, |service| service.control_operator);
    let observed = catalog
        .observe_execution_control(
            &super::core_address(request.flow.clone()),
            SenderIdentity::User(operator),
            request.request_id,
        )
        .await?;
    let attached =
        service.is_some_and(|service| service.host.session(request.flow.session_id.0).is_ok());
    let mut allowed_actions = Vec::new();
    let mut interruption = None;
    let phase = FlowExecutionPhase::observe(observed.execution.state(), observed.loaded);
    match observed.execution.state() {
        InstanceState::Open(InstanceActivity::Active(active))
            if observed.loaded && !state.is_stopping() && active.phase != TurnPhase::Cancelling =>
        {
            allowed_actions.push(FlowControl::CancelTurn);
        }
        InstanceState::Open(InstanceActivity::Interrupted { reason, .. }) => {
            if !state.is_stopping() && attached {
                allowed_actions.push(FlowControl::Recover);
            }
            interruption = Some(match reason {
                Interruption::Cancelled => FlowInterruption::Cancelled,
                Interruption::Failed => FlowInterruption::Failed,
                Interruption::HostLost => FlowInterruption::HostLost,
            });
        }
        _ => {}
    }
    let control = observed
        .control
        .map(|record| -> anyhow::Result<_> {
            Ok(FlowExecutionControlReceipt {
                action: match record.action {
                    ControlAction::CancelTurn => FlowControl::CancelTurn,
                    ControlAction::Recover => FlowControl::Recover,
                    _ => unreachable!("execution projection only contains stop and recover"),
                },
                accepted: super::wire::cursor(&record.accepted)?,
                applied: record
                    .applied
                    .as_ref()
                    .map(super::wire::cursor)
                    .transpose()?,
            })
        })
        .transpose()?;
    Ok(serde_json::to_value(FlowExecutionResponse {
        flow: request.flow,
        cursor: super::wire::cursor(&observed.cursor)?,
        revision: observed.execution.revision(),
        loaded: observed.loaded,
        paused: !observed.execution.pauses().is_empty(),
        phase,
        interruption,
        allowed_actions,
        request_id: request.request_id,
        control,
    })?)
}

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
