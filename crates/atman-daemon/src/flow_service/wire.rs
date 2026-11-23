//! Explicit durable projections for the published JSON-RPC response contracts.

use atman_proto::{
    flows::{FlowAddress, FlowCursor},
    responses::{FlowAcceptance, InputDisposition, LaunchFlowResponse},
};
use atman_runtime::flow_store::{
    inbox::{Acceptance, InboxDisposition},
    launch::LaunchRecord,
    position::FlowPosition,
};

pub(super) fn core_head(
    value: atman_proto::history::HistoryHead,
) -> atman_runtime::flow_store::branches::HistoryHead {
    atman_runtime::flow_store::branches::HistoryHead {
        branch: value.branch.into(),
        version: value.version.into(),
    }
}

pub(super) fn head(
    value: &atman_runtime::flow_store::branches::HistoryHead,
) -> atman_proto::history::HistoryHead {
    atman_proto::history::HistoryHead {
        branch: *value.branch.as_uuid(),
        version: value.version.get(),
    }
}

pub(super) fn application_error(error: anyhow::Error) -> atman_proto::JsonRpcError {
    use atman_proto::history::{HISTORY_ADMISSION_ERROR, HistoryAdmissionError};
    use atman_runtime::flow_store::branches::HistoryAdmissionError as Core;
    let conflict = error.chain().find_map(|cause| {
        cause.downcast_ref::<Core>().or_else(|| {
            cause
                .downcast_ref::<std::io::Error>()?
                .get_ref()?
                .downcast_ref::<Core>()
        })
    });
    let Some(conflict) = conflict else {
        return atman_proto::JsonRpcError::application(error.to_string());
    };
    let detail = match conflict {
        Core::Required { current } => HistoryAdmissionError::Required {
            current: head(current),
        },
        Core::Changed { expected, current } => HistoryAdmissionError::Changed {
            expected: head(expected),
            current: head(current),
        },
    };
    atman_proto::JsonRpcError {
        code: HISTORY_ADMISSION_ERROR,
        message: conflict.to_string(),
        data: Some(serde_json::json!(detail)),
    }
}

pub(super) fn cursor(position: &FlowPosition) -> std::io::Result<FlowCursor> {
    Ok(super::events::cursor(position.require_durable()?))
}

pub(super) fn acceptance(value: &Acceptance) -> std::io::Result<FlowAcceptance> {
    Ok(FlowAcceptance {
        message_id: value.message_id,
        receipt: cursor(&value.receipt)?,
        disposition: match &value.disposition {
            InboxDisposition::Pending => InputDisposition::Pending,
            InboxDisposition::InputCommitted { by } => {
                InputDisposition::InputCommitted { by: cursor(by)? }
            }
            InboxDisposition::Cancelled { by } => InputDisposition::Cancelled { by: cursor(by)? },
        },
    })
}

pub(super) fn launch(
    flow: FlowAddress,
    record: &LaunchRecord,
) -> std::io::Result<LaunchFlowResponse> {
    Ok(LaunchFlowResponse {
        flow,
        receipt: cursor(&record.receipt)?,
        acceptance: record.acceptance.as_ref().map(acceptance).transpose()?,
    })
}

pub(super) fn input_observation(
    value: atman_runtime::flow_store::input_observation::InputObservation,
) -> std::io::Result<atman_proto::responses::FlowInputObservation> {
    use atman_proto::responses::{FlowInputObservation, InputConsumption};
    use atman_runtime::flow_store::input_observation::InputDisposition;
    Ok(FlowInputObservation {
        message_id: value.message_id,
        accepted: cursor(&value.accepted)?,
        observed_receipt: cursor(&value.observed_receipt)?,
        input: match value.input {
            InputDisposition::Pending => InputConsumption::Pending,
            InputDisposition::Cancelled { receipt } => InputConsumption::Cancelled {
                receipt: cursor(&receipt)?,
            },
            InputDisposition::Committed { lease, receipt } => InputConsumption::Committed {
                receipt: cursor(&receipt)?,
                lease: input_turn(&lease),
            },
        },
    })
}

pub(super) fn input_turn(
    lease: &atman_rt::TurnLease<atman_runtime::event::AtmanUuid>,
) -> atman_proto::responses::InputTurn {
    atman_proto::responses::InputTurn {
        flow: super::address(&lease.flow),
        turn: lease.turn.0.0,
        run: lease.run.0.0,
        epoch: lease.epoch,
    }
}

pub(super) fn core_turn(
    lease: atman_proto::responses::InputTurn,
) -> atman_rt::TurnLease<atman_runtime::event::AtmanUuid> {
    atman_rt::TurnLease {
        flow: super::core_address(lease.flow),
        turn: atman_rt::TurnId(lease.turn.into()),
        run: atman_rt::RunId(lease.run.into()),
        epoch: lease.epoch,
    }
}
