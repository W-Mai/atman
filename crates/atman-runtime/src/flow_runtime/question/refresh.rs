//! Publish a reference change through the question's existing idle checkpoint.

use std::{future::Future, io};

use anyhow::{Result, bail, ensure};
use uuid::Uuid;

use crate::flow_store::{
    FlowStore,
    branches::HistoryHead,
    context::{ContextCheckpointPurpose, QuestionRefreshCommit},
    invalid,
    local_operator::LocalOperator,
    position::FlowPosition,
    source_capture::SourceSelection,
};

use super::{
    QuestionPreparation, QuestionRefreshRequest, QuestionTarget, QuestionUnavailable, RuntimeHost,
};

pub struct QuestionRefreshReceipt {
    pub source: SourceSelection,
    pub applied: FlowPosition,
}

pub enum QuestionRefreshState {
    Unseen,
    Unavailable,
    Retired,
    Applied(Box<QuestionRefreshReceipt>),
}

impl RuntimeHost {
    pub async fn refresh_observed_question(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        request: QuestionRefreshRequest,
        source_head: HistoryHead,
        prepare: impl Future<Output = Result<QuestionPreparation>> + Send,
    ) -> Result<QuestionRefreshReceipt> {
        let _host = self.retain_host_operation()?;
        target.validate_operation(request.id)?;
        ensure!(
            request.question == target.address() && request.source.flow == target.identity.source,
            "Quick Ask refresh changed its source or question"
        );
        let (session, worker) = match self.question_owner(target, operator.id()).await? {
            Ok(owner) => owner,
            Err(QuestionUnavailable::Unavailable) => bail!("Quick Ask has no accepted owner"),
            Err(QuestionUnavailable::Retired) => bail!("Quick Ask owner retired"),
        };
        let _session = session.retain_operation().await?;
        let mut digest = blake3::Hasher::new();
        serde_json::to_writer(&mut digest, &(&request, &source_head, operator.id()))?;
        let authored_digest = digest.finalize().to_hex().to_string();
        let id = request.id;
        let reconcile = || {
            worker.request({
                let authored = authored_digest.clone();
                move |store| observed_receipt(store, id, Some(&authored))
            })
        };
        if let Some(receipt) = reconcile().await? {
            return Ok(receipt);
        }
        let result = async {
            let _capacity = session.question_starts.reserve_preparation()?;
            let source = request.source.flow.clone();
            let question = request.question.clone();
            let authorize = move |candidate: &_| {
                if candidate != &source && candidate != &question {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Quick Ask refresh target changed",
                    ));
                }
                Ok(())
            };
            let prepared = self
                .prepare_question_refresh_inner(
                    &worker,
                    request,
                    Some(source_head),
                    async { prepare.await.map_err(io::Error::other) },
                    authorize.clone(),
                )
                .await?;
            let committed = worker
                .append_idle_context_with_refresh(
                    prepared.update,
                    prepared.outputs,
                    Some(Box::new(QuestionRefreshCommit {
                        authored_digest: authored_digest.clone(),
                        source: prepared.source,
                    })),
                    authorize,
                )
                .await?;
            match committed.checkpoint.purpose {
                ContextCheckpointPurpose::Append {
                    question_refresh: Some(original),
                    ..
                } => Ok(QuestionRefreshReceipt {
                    source: original.source,
                    applied: committed.receipt,
                }),
                _ => bail!("Quick Ask refresh commit has no receipt metadata"),
            }
        }
        .await;
        match result {
            Ok(receipt) => Ok(receipt),
            Err(error) => reconcile().await?.ok_or(error),
        }
    }

    pub async fn observe_question_refresh(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        id: Uuid,
    ) -> Result<QuestionRefreshState> {
        target.validate_operation(id)?;
        let worker = match self.question_owner(target, operator.id()).await? {
            Ok((_, worker)) => worker,
            Err(QuestionUnavailable::Unavailable) => return Ok(QuestionRefreshState::Unavailable),
            Err(QuestionUnavailable::Retired) => return Ok(QuestionRefreshState::Retired),
        };
        match worker
            .request(move |store| observed_receipt(store, id, None))
            .await
        {
            Ok(Some(receipt)) => Ok(QuestionRefreshState::Applied(Box::new(receipt))),
            Ok(None) => Ok(QuestionRefreshState::Unseen),
            Err(_) if worker.is_stopping() || worker.is_stopped() => {
                Ok(QuestionRefreshState::Retired)
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn observed_receipt(
    store: &FlowStore,
    id: Uuid,
    authored: Option<&str>,
) -> io::Result<Option<QuestionRefreshReceipt>> {
    let Some((applied, original)) = store.question_refresh_receipt(id) else {
        return Ok(None);
    };
    if authored.is_some_and(|digest| digest != original.authored_digest) {
        return Err(invalid("Quick Ask refresh changed the original request"));
    }
    Ok(Some(QuestionRefreshReceipt {
        source: original.source.clone(),
        applied: applied.clone(),
    }))
}
