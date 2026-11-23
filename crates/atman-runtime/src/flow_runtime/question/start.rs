//! Reconcile the first question against its original Session owner.

use std::{
    collections::HashMap,
    future::Future,
    io,
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context, Result, ensure};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::flow_store::{
    FlowRef,
    branches::HistoryHead,
    launch::LaunchRecord,
    position::{FlowPosition, TransientPosition},
    source_capture::SourceSelection,
};

use super::{
    PreparedQuestion, QuestionPreparation, QuestionRequest, RuntimeHost, discard::DiscardAttempt,
};

const MAX_REQUESTS: usize = 256;
const MAX_PREPARATIONS: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionIdentity {
    pub source: FlowRef,
    pub question: Uuid,
    pub request: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionStartReceipt {
    pub source: SourceSelection,
    pub launch: TransientPosition,
    pub accepted: TransientPosition,
    pub message: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuestionStartState {
    Unseen,
    Preparing,
    Retryable,
    Accepted(Box<QuestionStartReceipt>),
    Retired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionStartObservation {
    /// Changes when the Session owner is reopened, even on the same host.
    pub owner_generation: Option<Uuid>,
    pub state: QuestionStartState,
}

pub(in crate::flow_runtime) struct QuestionStarts {
    generation: Uuid,
    attempts: Mutex<HashMap<Uuid, Arc<Attempt>>>,
    capacity: Arc<Semaphore>,
}

impl Default for QuestionStarts {
    fn default() -> Self {
        Self {
            generation: Uuid::now_v7(),
            attempts: Mutex::default(),
            capacity: Arc::new(Semaphore::new(MAX_PREPARATIONS)),
        }
    }
}

#[derive(Default)]
enum Stage {
    #[default]
    Unprepared,
    Prepared(Box<PreparedQuestion>, OwnedSemaphorePermit),
    Accepted(Box<QuestionStartReceipt>),
}

pub(super) struct Attempt {
    identity: QuestionIdentity,
    user: Uuid,
    intent: blake3::Hash,
    stage: AsyncMutex<Stage>,
    pub(super) discard: OnceLock<DiscardAttempt>,
}

impl QuestionStarts {
    pub(super) fn reserve_preparation(&self) -> Result<OwnedSemaphorePermit> {
        self.capacity
            .clone()
            .try_acquire_owned()
            .context("Quick Ask preparation capacity reached")
    }

    pub(super) fn owns(&self, generation: Uuid) -> bool {
        self.generation == generation
    }

    fn reserve(
        &self,
        identity: QuestionIdentity,
        user: Uuid,
        intent: blake3::Hash,
    ) -> Result<Arc<Attempt>> {
        let mut attempts = self.attempts.lock().unwrap();
        if let Some(attempt) = attempts.get(&identity.question) {
            ensure!(
                attempt.identity == identity && attempt.user == user && attempt.intent == intent,
                "Quick Ask retry changed the original request"
            );
            return Ok(attempt.clone());
        }
        ensure!(
            !attempts
                .values()
                .any(|attempt| attempt.identity.request == identity.request),
            "Quick Ask request already belongs to another question"
        );
        ensure!(
            attempts.len() < MAX_REQUESTS,
            "Quick Ask request history is full for this Session owner"
        );
        let attempt = Arc::new(Attempt {
            identity,
            user,
            intent,
            stage: AsyncMutex::default(),
            discard: OnceLock::new(),
        });
        attempts.insert(attempt.identity.question, attempt.clone());
        Ok(attempt)
    }

    pub(super) fn find(&self, identity: &QuestionIdentity, user: Uuid) -> Option<Arc<Attempt>> {
        self.attempts
            .lock()
            .unwrap()
            .get(&identity.question)
            .filter(|attempt| &attempt.identity == identity && attempt.user == user)
            .cloned()
    }
}

impl RuntimeHost {
    /// Start or reconcile the exact original request on the Session owner observed by the client.
    pub async fn start_observed_question(
        &self,
        owner_generation: Uuid,
        request: QuestionRequest,
        head: HistoryHead,
        prepare: impl Future<Output = Result<QuestionPreparation>> + Send,
        authorize_source: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + Clone + 'static,
    ) -> Result<QuestionStartReceipt> {
        let _host = self.retain_host_operation()?;
        authorize_source(&request.source.flow)?;
        let session = self.session(request.source.flow.session_id.0)?;
        let _session = session.retain_operation().await?;
        let attempts = &session.question_starts;
        ensure!(
            attempts.generation == owner_generation,
            "Quick Ask Session owner changed"
        );
        let identity = QuestionIdentity {
            source: request.source.flow.clone(),
            question: request.id,
            request: request.request_id,
        };
        let mut signature = blake3::Hasher::new();
        serde_json::to_writer(&mut signature, &(&request, &head))?;
        let attempt = attempts.reserve(identity, request.user, signature.finalize())?;
        let mut stage = attempt.stage.lock().await;
        if let Stage::Accepted(receipt) = &*stage {
            ensure!(
                session
                    .loaded_flow(&atman_rt::FlowInstanceId(request.id.into()))
                    .is_some(),
                "Quick Ask owner retired"
            );
            return Ok((**receipt).clone());
        }
        if matches!(&*stage, Stage::Unprepared) {
            let permit = attempts.reserve_preparation()?;
            let preparation = prepare.await?;
            let frozen = self
                .prepare_question_from_observation(
                    request,
                    head,
                    preparation,
                    authorize_source.clone(),
                )
                .await?;
            *stage = Stage::Prepared(Box::new(frozen), permit);
        }
        let Stage::Prepared(frozen, _permit) = &*stage else {
            unreachable!()
        };
        let source_check = frozen.source().flow.clone();
        let record = match frozen
            .launch(self, move |_, _| authorize_source(&source_check))
            .await
        {
            Ok(launched) => launched.flow.record,
            Err(error) => {
                let Some(worker) = session.loaded_flow(&frozen.address().instance_id) else {
                    return Err(error.into());
                };
                let Some(record) = worker.launch_record().await? else {
                    return Err(error.into());
                };
                record
            }
        };
        let accepted = start_receipt(frozen, &record)?;
        *stage = Stage::Accepted(Box::new(accepted.clone()));
        Ok(accepted)
    }

    /// Observe an original request without preparing an entry, opening a Flow, or taking ownership.
    pub async fn observe_question_start(
        &self,
        identity: &QuestionIdentity,
        user: Uuid,
        expected_owner: Option<Uuid>,
    ) -> Result<QuestionStartObservation> {
        let session = self.session(identity.source.session_id.0).ok();
        let starts = session.as_ref().map(|session| &session.question_starts);
        let generation = starts.map(|starts| starts.generation);
        let state = if expected_owner.is_some() && expected_owner != generation {
            QuestionStartState::Retired
        } else if let Some(attempt) = starts.and_then(|starts| starts.find(identity, user)) {
            match attempt.stage.try_lock() {
                Err(_) => QuestionStartState::Preparing,
                Ok(mut stage) => {
                    let worker = session.as_ref().and_then(|session| {
                        session.loaded_flow(&atman_rt::FlowInstanceId(identity.question.into()))
                    });
                    match &*stage {
                        Stage::Unprepared => QuestionStartState::Unseen,
                        Stage::Prepared(frozen, _) => match worker {
                            Some(worker) => match worker.launch_record().await? {
                                Some(record) => {
                                    let receipt = Box::new(start_receipt(frozen, &record)?);
                                    *stage = Stage::Accepted(receipt.clone());
                                    QuestionStartState::Accepted(receipt)
                                }
                                None => QuestionStartState::Retryable,
                            },
                            None => QuestionStartState::Retryable,
                        },
                        Stage::Accepted(receipt) if worker.is_some() => {
                            QuestionStartState::Accepted(receipt.clone())
                        }
                        Stage::Accepted(_) => QuestionStartState::Retired,
                    }
                }
            }
        } else {
            QuestionStartState::Unseen
        };
        Ok(QuestionStartObservation {
            owner_generation: generation,
            state,
        })
    }
}

fn start_receipt(frozen: &PreparedQuestion, record: &LaunchRecord) -> Result<QuestionStartReceipt> {
    let acceptance = record
        .acceptance
        .as_ref()
        .context("Quick Ask has no original input acceptance")?;
    let FlowPosition::Transient { transient: launch } = &record.receipt else {
        anyhow::bail!("Quick Ask launch is outside the temporary owner");
    };
    let FlowPosition::Transient {
        transient: accepted,
    } = &acceptance.receipt
    else {
        anyhow::bail!("Quick Ask input is outside the temporary owner");
    };
    ensure!(
        record.input == frozen.input
            && launch.flow_id == frozen.address().instance_id.0.0
            && record.receipt.same_history(&acceptance.receipt),
        "Quick Ask receipt identity changed"
    );
    Ok(QuestionStartReceipt {
        source: frozen.source().clone(),
        launch: launch.clone(),
        accepted: accepted.clone(),
        message: acceptance.message_id,
    })
}
