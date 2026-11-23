//! Bounded conversation observation using the original temporary journal.

use std::{io, num::NonZeroUsize, time::Duration};

use anyhow::Result;
use uuid::Uuid;

use crate::flow_store::{
    branches::ConversationItem,
    position::{FlowPage, FlowPosition, TransientPosition},
};

use super::{
    super::live::{DraftObservation, DraftObservationReader},
    FlowWorker, QuestionIdentity, QuestionTarget, QuestionUnavailable, RuntimeHost,
};

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

pub struct QuestionDraftRead {
    pub known: Option<(Uuid, u64)>,
}

impl RuntimeHost {
    /// Read or wait for a bounded page without opening or pinning an execution owner.
    pub async fn read_question_transcript(
        &self,
        request: QuestionTranscriptRequest,
    ) -> Result<QuestionTranscriptState> {
        let (page, _) = self.read_question_observation(request, None).await?;
        Ok(page)
    }

    /// Pair committed pages and live witnesses at one temporary owner boundary.
    pub async fn read_question_observation(
        &self,
        request: QuestionTranscriptRequest,
        drafts: Option<QuestionDraftRead>,
    ) -> Result<(QuestionTranscriptState, Option<DraftObservation>)> {
        let target = QuestionTarget {
            identity: request.identity,
            owner_generation: request.owner_generation,
            host_generation: request.host_generation,
        };
        let (worker, reader) =
            if let Some(memory) = self.kept_question_presentation(&target, request.user) {
                (memory, None)
            } else {
                match self.question_owner(&target, request.user).await? {
                    Ok((_, worker)) => {
                        let reader = drafts.as_ref().and_then(|_| worker.draft_observer());
                        (worker, reader)
                    }
                    Err(QuestionUnavailable::Unavailable) => {
                        return Ok((QuestionTranscriptState::Unavailable, None));
                    }
                    Err(QuestionUnavailable::Retired) => {
                        return Ok((QuestionTranscriptState::Retired, None));
                    }
                }
            };
        let after = request
            .after
            .map(|transient| FlowPosition::Transient { transient });
        let limit = request.limit.min(NonZeroUsize::new(32).unwrap());
        let mut commits = worker.subscribe_commits();
        commits.borrow_and_update();
        let (mut state, mut reader, mut snapshot) =
            read_page(&worker, after.clone(), limit, reader).await?;
        let known = drafts.and_then(|read| read.known);
        let has_new_drafts = snapshot
            .as_ref()
            .is_some_and(|s| known != Some((s.source.worker_incarnation, s.revision)));
        if request.wait
            && after.is_some()
            && !has_new_drafts
            && matches!(&state, QuestionTranscriptState::Ready(page) if page.batches.is_empty())
        {
            let change = async {
                if let Some(reader) = &mut reader {
                    tokio::select! {
                        _ = commits.changed() => {}
                        _ = reader.changed() => {}
                    }
                } else {
                    let _ = commits.changed().await;
                }
            };
            if tokio::time::timeout(OBSERVATION_WAIT, change).await.is_ok() {
                (state, _, snapshot) = read_page(&worker, after, limit, reader).await?;
            }
        }
        if !matches!(&state, QuestionTranscriptState::Ready(_))
            || snapshot
                .as_ref()
                .is_some_and(|s| known == Some((s.source.worker_incarnation, s.revision)))
        {
            snapshot = None;
        }
        Ok((state, snapshot))
    }
}

async fn read_page(
    worker: &FlowWorker,
    after: Option<FlowPosition>,
    limit: NonZeroUsize,
    mut reader: Option<DraftObservationReader>,
) -> Result<(
    QuestionTranscriptState,
    Option<DraftObservationReader>,
    Option<DraftObservation>,
)> {
    let captured = worker
        .request(move |store| {
            let page = store.capture_transient_conversation(after.as_ref(), limit, SOURCE_BYTES)?;
            let snapshot = reader.as_mut().and_then(DraftObservationReader::snapshot);
            Ok((page, reader, snapshot))
        })
        .await;
    let (capture, reader, snapshot) = match captured {
        Ok((Some(Ok(capture)), reader, snapshot)) => (capture, reader, snapshot),
        Ok((Some(Err(required_at_least)), _, _)) => {
            return Ok((
                QuestionTranscriptState::Capacity {
                    required_at_least,
                    allowed: SOURCE_BYTES,
                },
                None,
                None,
            ));
        }
        Ok((None, _, _)) => return Ok((QuestionTranscriptState::Retired, None, None)),
        Err(_) if worker.is_stopping() || worker.is_stopped() => {
            return Ok((QuestionTranscriptState::Retired, None, None));
        }
        Err(error) => return Err(error.into()),
    };
    let page = crate::read_work::capture(move || capture.project())
        .await
        .map_err(io::Error::other)??;
    Ok((QuestionTranscriptState::Ready(page), reader, snapshot))
}
