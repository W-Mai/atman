//! Retain the temporary transcript until its durable replacement is installed.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::{
    event_log::journal::CommitReceipt,
    flow_runtime::host::RetentionPending,
    flow_store::{
        local_operator::LocalOperator,
        position::{FlowPosition, TransientPosition},
        retention::RetentionReceipt,
    },
};

use super::{FlowWorker, QuestionTarget, RuntimeHost};

#[derive(Clone)]
pub enum QuestionKeepState {
    Unseen,
    Pending,
    Published(Box<RetentionReceipt>),
    Installed(Box<RetentionReceipt>),
    Retired,
}

pub(super) struct KeepAttempt {
    id: Uuid,
    expected: TransientPosition,
    gate: AsyncMutex<()>,
    presentation: Mutex<KeepPresentation>,
}

struct KeepPresentation {
    state: QuestionKeepState,
    /// Holds the original observation through catalog publication and client installation.
    memory: Option<FlowWorker>,
}

impl RuntimeHost {
    pub async fn keep_question(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        id: Uuid,
        expected: TransientPosition,
    ) -> Result<QuestionKeepState> {
        let _host = self.retain_host_operation()?;
        target.validate_operation(id)?;
        let session = self
            .question_session(target)
            .context("Quick Ask owner retired")?;
        let _session = session.retain_operation().await?;
        let attempt = session
            .question_starts
            .find(&target.identity, operator.id())
            .context("Quick Ask identity is unavailable")?;
        let existing = attempt.keep.lock().unwrap().clone();
        let keep = if let Some(existing) = existing {
            existing
        } else {
            let owner = self.question_owner(target, operator.id()).await;
            let mut slot = attempt.keep.lock().unwrap();
            if let Some(existing) = slot.as_ref() {
                existing.clone()
            } else {
                let (_, worker) = owner?
                    .ok()
                    .context("Quick Ask has no accepted temporary owner")?;
                let keep = Arc::new(KeepAttempt {
                    id,
                    expected: expected.clone(),
                    gate: AsyncMutex::new(()),
                    presentation: Mutex::new(KeepPresentation {
                        state: QuestionKeepState::Pending,
                        memory: Some(worker),
                    }),
                });
                *slot = Some(keep.clone());
                keep
            }
        };
        ensure!(
            keep.id == id && keep.expected == expected,
            "Quick Ask Keep changed the original request"
        );
        let _gate = keep.gate.lock().await;
        ensure!(
            attempt
                .keep
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &keep)),
            "Quick Ask Keep must be reviewed again"
        );
        let state = keep.presentation.lock().unwrap().state.clone();
        if matches!(
            state,
            QuestionKeepState::Published(_) | QuestionKeepState::Installed(_)
        ) {
            return Ok(state);
        }
        let flow = target.address();
        let result = self
            .keep_transient(
                flow,
                id,
                FlowPosition::Transient {
                    transient: expected,
                },
                |_| Ok(()),
            )
            .await;
        match result {
            Ok(kept) => {
                let state = QuestionKeepState::Published(Box::new(kept.receipt));
                keep.presentation.lock().unwrap().state = state.clone();
                Ok(state)
            }
            Err(error) => {
                let memory = keep.presentation.lock().unwrap().memory.clone();
                let frozen = if let Some(memory) = memory {
                    memory
                        .request(|store| Ok(store.transient_frozen()))
                        .await
                        .unwrap_or(false)
                } else {
                    false
                };
                if frozen
                    || error
                        .get_ref()
                        .is_some_and(|cause| cause.is::<RetentionPending>())
                {
                    return Ok(QuestionKeepState::Pending);
                }
                attempt.keep.lock().unwrap().take();
                Err(error.into())
            }
        }
    }

    pub fn observe_question_keep(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        id: Uuid,
    ) -> Result<QuestionKeepState> {
        target.validate_operation(id)?;
        let Some(session) = self.question_session(target) else {
            return Ok(QuestionKeepState::Retired);
        };
        let state = session
            .question_starts
            .find(&target.identity, operator.id())
            .and_then(|attempt| attempt.keep.lock().unwrap().clone())
            .filter(|keep| keep.id == id)
            .map(|keep| keep.presentation.lock().unwrap().state.clone())
            .unwrap_or(QuestionKeepState::Unseen);
        Ok(state)
    }

    pub async fn install_kept_question(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        id: Uuid,
        installed: CommitReceipt,
    ) -> Result<QuestionKeepState> {
        let _host = self.retain_host_operation()?;
        target.validate_operation(id)?;
        let session = self
            .question_session(target)
            .context("Quick Ask owner retired")?;
        let _session = session.retain_operation().await?;
        let keep = session
            .question_starts
            .find(&target.identity, operator.id())
            .and_then(|attempt| attempt.keep.lock().unwrap().clone())
            .filter(|keep| keep.id == id)
            .context("Quick Ask Keep request is unavailable")?;
        let _gate = keep.gate.lock().await;
        let state = keep.presentation.lock().unwrap().state.clone();
        let receipt = match &state {
            QuestionKeepState::Published(receipt) | QuestionKeepState::Installed(receipt) => {
                receipt
            }
            _ => anyhow::bail!("Quick Ask Keep has not published a durable Flow"),
        };
        ensure!(
            receipt.destination == installed,
            "Quick Ask installation differs from the published destination"
        );
        if matches!(state, QuestionKeepState::Installed(_)) {
            return Ok(state);
        }
        self.acknowledge_retention((**receipt).clone(), |_| Ok(()))
            .await?;
        let state = QuestionKeepState::Installed(receipt.clone());
        let mut presentation = keep.presentation.lock().unwrap();
        presentation.memory.take();
        presentation.state = state.clone();
        Ok(state)
    }

    pub(super) fn kept_question_presentation(
        &self,
        target: &QuestionTarget,
        user: Uuid,
    ) -> Option<FlowWorker> {
        let session = self.question_session(target)?;
        let attempt = session.question_starts.find(&target.identity, user)?;
        let keep = attempt.keep.lock().unwrap().clone()?;
        let memory = keep.presentation.lock().unwrap().memory.clone();
        memory.filter(|worker| !worker.is_stopped())
    }
}
