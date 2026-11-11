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
    use atman_proto::responses::{FlowInputObservation, InputConsumption, InputTurn};
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
                lease: InputTurn {
                    flow: super::address(&lease.flow),
                    turn: lease.turn.0.0,
                    run: lease.run.0.0,
                    epoch: lease.epoch,
                },
            },
        },
    })
}
