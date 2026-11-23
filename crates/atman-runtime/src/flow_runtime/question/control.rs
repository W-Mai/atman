//! Observe and stop the exact leased turn of an accepted temporary question.

use anyhow::{Result, bail};
use atman_rt::{InstanceActivity, InstanceState, TurnLease, TurnPhase};
use uuid::Uuid;

use crate::{
    event::AtmanUuid,
    flow_store::{
        control::{ControlAction, ControlRequest, ExecutionControlObservation},
        invalid,
        local_operator::LocalOperator,
        position::FlowPosition,
    },
};

use super::{QuestionTarget, QuestionUnavailable, RuntimeHost};

pub enum QuestionExecutionState {
    Unavailable,
    Retired,
    Observed(Box<ExecutionControlObservation>),
}

impl RuntimeHost {
    pub async fn observe_question_execution(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        request_id: Option<Uuid>,
    ) -> Result<QuestionExecutionState> {
        if let Some(id) = request_id {
            target.validate_operation(id)?;
        }
        let worker = match self.question_owner(target, operator.id()).await? {
            Ok((_, worker)) => worker,
            Err(QuestionUnavailable::Unavailable) => {
                return Ok(QuestionExecutionState::Unavailable);
            }
            Err(QuestionUnavailable::Retired) => return Ok(QuestionExecutionState::Retired),
        };
        match worker
            .request(move |store| Ok(store.execution_controls(&operator.sender(), request_id)))
            .await
        {
            Ok(execution) => Ok(QuestionExecutionState::Observed(Box::new(execution))),
            Err(_) if worker.is_stopping() || worker.is_stopped() => {
                Ok(QuestionExecutionState::Retired)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn stop_question_turn(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        request_id: Uuid,
        expected_revision: u64,
        expected_lease: TurnLease<AtmanUuid>,
    ) -> Result<FlowPosition> {
        let _host = self.retain_host_operation()?;
        target.validate_operation(request_id)?;
        let (session, worker) = match self.question_owner(target, operator.id()).await? {
            Ok(owner) => owner,
            Err(QuestionUnavailable::Unavailable) => bail!("Quick Ask has no accepted owner"),
            Err(QuestionUnavailable::Retired) => bail!("Quick Ask owner retired"),
        };
        let _session = session.retain_operation().await?;
        let request = ControlRequest {
            id: request_id,
            sender: operator.sender(),
            target: worker.address().clone(),
            expected_revision,
            pause_key: Uuid::from_bytes(*b"atman-user-pause").into(),
            action: ControlAction::CancelTurn,
        };
        Ok(worker.request(move |store| {
            store.validate_transient_position(store.cursor())?;
            if let Some(record) = store.recorded_control(&request)? {
                if matches!(record.execution.state(), InstanceState::Open(InstanceActivity::Active(active)) if active.lease == expected_lease) {
                    return Ok(record.receipt);
                }
                return Err(invalid("Quick Ask stop retry changed the original turn"));
            }
            if !matches!(store.state().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.phase != TurnPhase::Cancelling && active.lease == expected_lease) {
                return Err(invalid("Quick Ask turn is no longer stoppable"));
            }
            Ok(store.control(request)?.receipt)
        }).await?)
    }
}
