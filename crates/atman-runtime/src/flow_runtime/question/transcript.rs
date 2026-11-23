//! Bounded conversation observation using the original temporary journal.

use std::{io, num::NonZeroUsize, time::Duration};

use anyhow::Result;
use uuid::Uuid;

use crate::flow_store::{
    branches::ConversationItem,
    position::{FlowPage, FlowPosition, TransientPosition},
};

use super::{FlowWorker, QuestionIdentity, QuestionTarget, QuestionUnavailable, RuntimeHost};

const SOURCE_BYTES: usize = 2 * 1024 * 1024;
const OBSERVATION_WAIT: Duration = Duration::from_secs(5);

pub struct QuestionTranscriptRequest {
    pub identity: QuestionIdentity,
    pub user: Uuid,
    pub owner_generation: Uuid,
    pub host_generation: Uuid,
    pub after: Option<TransientPosition>,
    pub limit: NonZeroUsize,
    pub wait: bool,
}

pub enum QuestionTranscriptState {
    Ready(FlowPage<ConversationItem>),
    Capacity {
        required_at_least: usize,
        allowed: usize,
    },
    Unavailable,
    Retired,
}

impl RuntimeHost {
    /// Read or wait for a bounded page without opening or pinning an execution owner.
    pub async fn read_question_transcript(
        &self,
        request: QuestionTranscriptRequest,
    ) -> Result<QuestionTranscriptState> {
        let target = QuestionTarget {
            identity: request.identity,
            owner_generation: request.owner_generation,
            host_generation: request.host_generation,
        };
        let worker = match self.question_owner(&target, request.user).await? {
            Ok((_, worker)) => worker,
            Err(QuestionUnavailable::Unavailable) => {
                return Ok(QuestionTranscriptState::Unavailable);
            }
            Err(QuestionUnavailable::Retired) => return Ok(QuestionTranscriptState::Retired),
        };
        let after = request
            .after
            .map(|transient| FlowPosition::Transient { transient });
        let limit = request.limit.min(NonZeroUsize::new(32).unwrap());
        let mut commits = worker.subscribe_commits();
        commits.borrow_and_update();
        let mut state = read_page(&worker, after.clone(), limit).await?;
        if request.wait
            && after.is_some()
            && matches!(&state, QuestionTranscriptState::Ready(page) if page.batches.is_empty())
            && tokio::time::timeout(OBSERVATION_WAIT, commits.changed())
                .await
                .is_ok()
        {
            state = read_page(&worker, after, limit).await?;
        }
        Ok(state)
    }
}

async fn read_page(
    worker: &FlowWorker,
    after: Option<FlowPosition>,
    limit: NonZeroUsize,
) -> Result<QuestionTranscriptState> {
    let captured = worker
        .request(move |store| {
            store.capture_transient_conversation(after.as_ref(), limit, SOURCE_BYTES)
        })
        .await;
    let capture = match captured {
        Ok(Some(Ok(capture))) => capture,
        Ok(Some(Err(required_at_least))) => {
            return Ok(QuestionTranscriptState::Capacity {
                required_at_least,
                allowed: SOURCE_BYTES,
            });
        }
        Ok(None) => return Ok(QuestionTranscriptState::Retired),
        Err(_) if worker.is_stopping() || worker.is_stopped() => {
            return Ok(QuestionTranscriptState::Retired);
        }
        Err(error) => return Err(error.into()),
    };
    let page = crate::read_work::capture(move || capture.project())
        .await
        .map_err(io::Error::other)??;
    Ok(QuestionTranscriptState::Ready(page))
}
