//! Reconcile a question's original disposal after its temporary worker has stopped.

use anyhow::{Context, Result, ensure};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::flow_store::{
    local_operator::LocalOperator,
    position::{FlowPosition, TransientPosition},
};

use super::{QuestionTarget, RuntimeHost};

pub enum QuestionDiscardState {
    Unseen,
    Draining,
    Retryable,
    Discarded,
    Retired,
}

pub(super) struct DiscardAttempt {
    id: Uuid,
    observed: FlowPosition,
    discarded: Mutex<bool>,
}

impl RuntimeHost {
    pub async fn discard_question(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        request_id: Uuid,
        observed: TransientPosition,
    ) -> Result<()> {
        let _host = self.retain_host_operation()?;
        target.validate_operation(request_id)?;
        let session = self
            .question_session(target)
            .context("Quick Ask owner retired")?;
        let _session = session.retain_operation().await?;
        let attempt = session
            .question_starts
            .find(&target.identity, operator.id())
            .context("Quick Ask identity is unavailable")?;
        let observed = FlowPosition::Transient {
            transient: observed,
        };
        let flow = crate::flow_store::FlowRef {
            session_id: target.identity.source.session_id,
            instance_id: atman_rt::FlowInstanceId(target.identity.question.into()),
        };
        if attempt.discard.get().is_none() {
            let validated = async {
                let (_, worker) = self
                    .question_owner(target, operator.id())
                    .await?
                    .ok()
                    .context("Quick Ask has no accepted temporary owner")?;
                let position = observed.clone();
                worker
                    .request(move |store| store.validate_transient_position(&position))
                    .await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if attempt.discard.get().is_none() {
                validated?;
            }
        }
        let discard = attempt.discard.get_or_init(|| DiscardAttempt {
            id: request_id,
            observed: observed.clone(),
            discarded: Mutex::new(false),
        });
        ensure!(
            discard.id == request_id && discard.observed == observed,
            "Quick Ask discard changed the original request"
        );
        let mut discarded = discard.discarded.lock().await;
        if !*discarded {
            self.discard_transient(flow, observed, |_| Ok(())).await?;
            *discarded = true;
        }
        Ok(())
    }

    pub fn observe_question_discard(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        request_id: Uuid,
    ) -> Result<QuestionDiscardState> {
        target.validate_operation(request_id)?;
        let Some(session) = self.question_session(target) else {
            return Ok(QuestionDiscardState::Retired);
        };
        let Some(attempt) = session
            .question_starts
            .find(&target.identity, operator.id())
        else {
            return Ok(QuestionDiscardState::Unseen);
        };
        let Some(discard) = attempt
            .discard
            .get()
            .filter(|discard| discard.id == request_id)
        else {
            return Ok(QuestionDiscardState::Unseen);
        };
        Ok(match discard.discarded.try_lock() {
            Err(_) => QuestionDiscardState::Draining,
            Ok(discarded) if *discarded => QuestionDiscardState::Discarded,
            Ok(_) => QuestionDiscardState::Retryable,
        })
    }
}
