//! Owned continuation pickup from the same durable inbox as model input.

use std::io;

use atman_rt::TurnOutcome;

use super::FlowWorker;
use crate::event::TurnId;
use crate::flow_store::completion::{CommittedCompletion, InvocationResult};
use crate::flow_store::inbox::InputContent;
use crate::flow_store::waiter::{ReplyStatus, ReplyWaiter};
use crate::flow_store::{FlowStore, invalid};
use crate::{RuntimeError, Value};

/// Dropping a wait releases only its continuation claim, never the accepted input or source task.
pub struct ReplyWait {
    worker: FlowWorker,
    waiter: ReplyWaiter,
    settled: bool,
}

impl ReplyWait {
    pub(super) fn new(worker: FlowWorker, waiter: ReplyWaiter) -> Self {
        Self {
            worker,
            waiter,
            settled: false,
        }
    }

    pub fn waiter(&self) -> &ReplyWaiter {
        &self.waiter
    }

    pub async fn completion(mut self, turn: TurnId) -> Result<Value, RuntimeError> {
        let mut changes = self.worker.subscribe_commits();
        loop {
            changes.borrow_and_update();
            let waiter = self.waiter.clone();
            let expected = turn.clone();
            let result = self
                .worker
                .request(move |store| receive(store, &waiter, &expected))
                .await
                .map_err(failed)?;
            if let Some(completion) = result {
                self.settled = true;
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
            changes.changed().await.map_err(failed)?;
        }
    }
}

impl Drop for ReplyWait {
    fn drop(&mut self) {
        if !self.settled
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let worker = self.worker.clone();
            let waiter = self.waiter.clone();
            runtime.spawn(async move {
                let _ = worker
                    .request(move |store| {
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
    let Some(reserved) = store.reserve_reply_input(&waiter.lease, waiter.correlation)? else {
        return Ok(None);
    };
    let envelope = store
        .pending_inputs()
        .find(|envelope| reserved.message_ids().contains(&envelope.id))
        .ok_or_else(|| invalid("reserved reply disappeared"))?;
    let completion = crate::flow_store::subscription::completion_notification(envelope)?;
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
