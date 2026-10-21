//! Owned continuation pickup from the same durable inbox as model input.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, Weak},
};

use atman_rt::TurnOutcome;
use uuid::Uuid;

use super::FlowWorker;
use crate::event::TurnId;
use crate::flow_store::completion::{CommittedCompletion, InvocationResult};
use crate::flow_store::inbox::InputContent;
use crate::flow_store::waiter::{ReplyStatus, ReplyWaiter};
use crate::flow_store::{FlowStore, invalid};
use crate::{RuntimeError, Value};

#[derive(Default)]
pub(super) struct ReplyClaims {
    entries: Mutex<HashMap<Uuid, (Uuid, Weak<ReplyClaim>)>>,
}

struct ReplyClaim {
    identity: Uuid,
    worker: FlowWorker,
    waiter: ReplyWaiter,
}

/// Identical retries share continuation ownership. Dropping the last wait releases that claim, never the accepted input or source task.
pub struct ReplyWait {
    claim: Arc<ReplyClaim>,
}

impl ReplyWait {
    pub(super) fn new(worker: FlowWorker, waiter: ReplyWaiter) -> Self {
        let mut entries = worker.reply_claims.entries.lock().unwrap();
        if let Some(claim) = entries
            .get(&waiter.correlation)
            .and_then(|(_, claim)| claim.upgrade())
        {
            return Self { claim };
        }
        let claim = Arc::new(ReplyClaim {
            identity: Uuid::new_v4(),
            worker: worker.clone(),
            waiter,
        });
        entries.insert(
            claim.waiter.correlation,
            (claim.identity, Arc::downgrade(&claim)),
        );
        Self { claim }
    }

    pub fn waiter(&self) -> &ReplyWaiter {
        &self.claim.waiter
    }

    pub async fn completion(self, turn: TurnId) -> Result<Value, RuntimeError> {
        let mut changes = self.claim.worker.subscribe_commits();
        loop {
            changes.borrow_and_update();
            let waiter = self.claim.waiter.clone();
            let expected = turn.clone();
            let worker = &self.claim.worker;
            let references = worker.pending_message_scopes().await.map_err(failed)?;
            let mut scopes = worker.resolve_message_scopes(references).await;
            let checked = scopes.clone();
            let result = self
                .claim
                .worker
                .request_scoped(scopes.dependencies.clone(), move |store, snapshot| {
                    checked.invalidate(store, snapshot)?;
                    let eligible_wait =
                        checked.eligible_scope(snapshot, store.reply_scope(waiter.correlation));
                    receive(store, &waiter, &expected, eligible_wait, |input| {
                        checked.eligible(snapshot, &input.request)
                    })
                })
                .await
                .map_err(failed)?;
            if let Some(completion) = result {
                return match (completion.completion.outcome, completion.completion.result) {
                    (_, InvocationResult::Failed(error)) => Err(error),
                    (TurnOutcome::Completed, InvocationResult::Returned(value)) => value.restore(),
                    (TurnOutcome::Completed, InvocationResult::Unobserved(_)) => {
                        Err(failed("entry return was not observed"))
                    }
                    (TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled), _) => Err(
                        RuntimeError::Cancelled("instance turn was cancelled".into()),
                    ),
                    (outcome, _) => Err(failed(format!("instance turn ended with {outcome:?}"))),
                };
            }
            tokio::select! {
                changed = changes.changed() => { changed.map_err(failed)?; }
                _ = scopes.changed() => {}
            }
        }
    }
}

impl Drop for ReplyClaim {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let worker = self.worker.clone();
            let waiter = self.waiter.clone();
            let claims = worker.reply_claims.clone();
            let identity = self.identity;
            runtime.spawn(async move {
                let _ = worker
                    .request(move |store| {
                        let mut entries = claims.entries.lock().unwrap();
                        if entries
                            .get(&waiter.correlation)
                            .is_none_or(|(current, _)| *current != identity)
                        {
                            return Ok(());
                        }
                        entries.remove(&waiter.correlation);
                        store
                            .cancel_reply_waiter(&waiter.lease, waiter.correlation)
                            .map(|_| ())
                    })
                    .await;
            });
        }
    }
}

fn receive(
    store: &mut FlowStore,
    waiter: &ReplyWaiter,
    turn: &TurnId,
    eligible_wait: bool,
    eligible: impl FnMut(&crate::flow_store::inbox::AcceptedEnvelope) -> bool,
) -> io::Result<Option<CommittedCompletion>> {
    let registered = store
        .reply_waiters()
        .iter()
        .find(|record| record.waiter.correlation == waiter.correlation)
        .ok_or_else(|| invalid("reply waiter is not registered"))?;
    if registered.waiter != *waiter {
        return Err(invalid("reply waiter identity changed"));
    }
    if matches!(registered.status, ReplyStatus::Cancelled { .. }) {
        return Err(invalid("reply wait was cancelled"));
    }
    if !store
        .state()
        .can_receive_reply(&waiter.lease)
        .map_err(invalid)?
    {
        return Ok(None);
    }
    if matches!(registered.status, ReplyStatus::Delivered { .. }) {
        let saved = store
            .committed_inputs()
            .iter()
            .find_map(|record| match &record.input.content {
                InputContent::WaitingContinuation { correlation, value }
                    if *correlation == waiter.correlation =>
                {
                    Some(value)
                }
                _ => None,
            })
            .ok_or_else(|| invalid("delivered reply has no committed input"))?;
        let completion: CommittedCompletion =
            serde_json::from_value(saved.clone()).map_err(invalid)?;
        return if &completion.completion.lease.turn == turn {
            Ok(Some(completion))
        } else {
            Err(invalid("committed reply belongs to another source turn"))
        };
    }
    if !eligible_wait {
        return Ok(None);
    }
    let Some(reserved) = store.reserve_reply_input(&waiter.lease, waiter.correlation, eligible)?
    else {
        return Ok(None);
    };
    let entry = store
        .inbox_entry(reserved.message_ids()[0])?
        .ok_or_else(|| invalid("reserved reply disappeared"))?;
    let completion = crate::flow_store::subscription::completion_notification(&entry.envelope)?;
    if &completion.completion.lease.turn != turn {
        return Err(invalid("reply belongs to another source turn"));
    }
    store.commit_input(
        &reserved,
        InputContent::WaitingContinuation {
            correlation: waiter.correlation,
            value: serde_json::to_value(&completion).map_err(invalid)?,
        },
    )?;
    Ok(Some(completion))
}

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("flow reply: {error}"))
}
