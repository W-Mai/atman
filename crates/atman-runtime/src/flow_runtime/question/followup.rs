//! Admit and reconcile authored follow-ups on the original temporary worker.

use anyhow::{Result, bail};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    flow_store::{
        inbox::{Acceptance, AuthoredInputFingerprint, UserContent},
        input_observation::InputObservation,
        local_operator::LocalOperator,
        position::{FlowPosition, TransientPosition},
    },
    provider::ReasoningSelection,
};

use super::{QuestionTarget, QuestionUnavailable, RuntimeHost, question_followup};

#[derive(Serialize)]
pub struct QuestionFollowupRequest {
    pub request_id: Uuid,
    pub observed: TransientPosition,
    pub content: UserContent,
    pub model: String,
    pub effort: Option<ReasoningSelection>,
}

pub enum QuestionFollowupState {
    Unavailable,
    Retired,
    Unseen,
    Observed(Box<InputObservation>),
}

impl RuntimeHost {
    pub async fn submit_question_followup(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        followup: QuestionFollowupRequest,
    ) -> Result<Acceptance> {
        let _host = self.retain_host_operation()?;
        target.validate_operation(followup.request_id)?;
        let (session, worker) = match self.question_owner(target, operator.id()).await? {
            Ok(owner) => owner,
            Err(QuestionUnavailable::Unavailable) => bail!("Quick Ask has no accepted owner"),
            Err(QuestionUnavailable::Retired) => bail!("Quick Ask owner retired"),
        };
        let _session = session.retain_operation().await?;
        let request_id = followup.request_id;
        let authored = AuthoredInputFingerprint::for_request(&(worker.address(), &followup))?;
        if let Some(accepted) = worker
            .observe_authored_input(operator, request_id, authored)
            .await?
        {
            return Ok(accepted);
        }
        let result = async {
            let observed = FlowPosition::Transient {
                transient: followup.observed,
            };
            worker
                .request({
                    let observed = observed.clone();
                    move |store| store.validate_transient_position(&observed)
                })
                .await?;
            let executor = session.executor_for(worker.project().await?).await?;
            let request = question_followup(
                worker.address().clone(),
                request_id,
                operator.id(),
                followup.content,
                &followup.model,
                followup.effort.as_ref(),
                &executor,
            )?;
            Ok(worker
                .request(move |store| {
                    store.validate_transient_position(&observed)?;
                    store.accept_authored_input(operator, request, authored, |_| Ok(()))
                })
                .await?)
        }
        .await;
        match result {
            Ok(accepted) => Ok(accepted),
            Err(error) => worker
                .observe_authored_input(operator, request_id, authored)
                .await?
                .ok_or(error),
        }
    }

    /// Poll the original inbox without opening an owner or resolving the current model.
    pub async fn observe_question_followup(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        request_id: Uuid,
    ) -> Result<QuestionFollowupState> {
        use QuestionFollowupState::{Observed, Retired, Unavailable, Unseen};

        target.validate_operation(request_id)?;
        let worker = match self.question_owner(target, operator.id()).await? {
            Ok((_, worker)) => worker,
            Err(QuestionUnavailable::Unavailable) => return Ok(Unavailable),
            Err(QuestionUnavailable::Retired) => return Ok(Retired),
        };
        match worker.observe_operator_input(operator, request_id).await {
            Ok(Some(input)) => Ok(Observed(Box::new(input))),
            Ok(None) => Ok(Unseen),
            Err(_) if worker.is_stopping() || worker.is_stopped() => Ok(Retired),
            Err(error) => Err(error.into()),
        }
    }
}
