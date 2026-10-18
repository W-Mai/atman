//! Host-owned execution of durably admitted instance turns.

use std::{collections::BTreeMap, future::Future, io, num::NonZeroUsize};

use atman_rt::{InstanceActivity, InstanceState, RunId, TurnId, TurnLease, TurnOutcome, TurnPhase};
use futures::FutureExt;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::FlowWorker;
use crate::error::RuntimeError;
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;
use crate::flow_authority::EffectiveAuthority;
use crate::flow_store::inbox::{Acceptance, DeliveryRequest, InputContent};
use crate::flow_store::{
    completion::{CommittedCompletion, InvocationResult},
    invalid,
};
use crate::message::Message;
use crate::{Executor, InstanceInvocation, RootInvocation, Value, ValueJson};

/// Host-prepared entry input and explicit authority; scheduling does not infer agent policy.
pub struct TurnRequest {
    pub turn: TurnId<AtmanUuid>,
    pub run: RunId<AtmanUuid>,
    /// The exact ordered pending envelope IDs selected by the host.
    pub messages: Vec<Uuid>,
    pub arguments: BTreeMap<String, serde_json::Value>,
    pub invocation: BTreeMap<String, serde_json::Value>,
    pub context: Vec<Message>,
    pub authority: EffectiveAuthority,
    /// Cooperative scheduling only; this does not impose an operation limit.
    pub yield_interval: NonZeroUsize,
}

pub struct TurnCompletion {
    pub lease: TurnLease<AtmanUuid>,
    pub outcome: TurnOutcome,
    pub result: Result<Value, RuntimeError>,
    pub receipt: CommitReceipt,
}

/// Dropping this observation handle does not cancel execution or its terminal commit.
pub struct TurnTicket {
    pub lease: TurnLease<AtmanUuid>,
    completed: oneshot::Receiver<io::Result<TurnCompletion>>,
}

impl TurnTicket {
    pub async fn wait(self) -> io::Result<TurnCompletion> {
        self.completed.await.map_err(|_| {
            io::Error::other("instance execution stopped without a terminal acknowledgement")
        })?
    }
}

impl FlowWorker {
    /// Read a recorded entry result without rerunning the entry or publishing an answer.
    pub async fn completion(
        &self,
        turn: Option<crate::event::TurnId>,
    ) -> io::Result<Option<CommittedCompletion>> {
        self.request(move |store| Ok(store.completion(turn.as_ref()).cloned()))
            .await
    }

    /// Authenticate the sender before admission; the callback authorizes only new delivery.
    pub async fn accept_input(
        &self,
        request: DeliveryRequest,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Acceptance> {
        self.request(move |store| store.accept_input(request, authorize_new))
            .await
    }

    /// Compile the recorded entry, atomically admit actual input, and start an owned execution task.
    pub async fn start_turn(
        &self,
        executor: Executor,
        request: TurnRequest,
    ) -> io::Result<TurnTicket> {
        self.admit_turn(executor, request, None)
            .await?
            .ok_or_else(|| invalid("unconditional admission did not return a ticket"))
    }

    /// Admit a host-prepared batch only if its scheduling snapshot is still current.
    pub(super) async fn admit_turn(
        &self,
        executor: Executor,
        request: TurnRequest,
        expected: Option<CommitReceipt>,
    ) -> io::Result<Option<TurnTicket>> {
        let worker = self.clone();
        let runtime = tokio::runtime::Handle::current();
        self.request(move |store| {
            if worker.is_stopping() {
                return if expected.is_some() {
                    Ok(None)
                } else {
                    Err(invalid("instance host is stopping"))
                };
            }
            if expected
                .as_ref()
                .is_some_and(|cursor| cursor != store.cursor())
            {
                return Ok(None);
            }
            if request.messages.is_empty() {
                return Err(invalid("a turn requires selected pending input"));
            }
            let workspace = if let Some(workspace) = store.workspace_request() {
                workspace.validate_live(store.state().address())?;
                if request.authority.workspace_root.as_ref() != Some(&workspace.binding.path) {
                    return Err(invalid(
                        "execution authority does not match its workspace resource",
                    ));
                }
                Some(workspace.binding.clone())
            } else {
                None
            };
            worker.context().bind_executor(&executor).map_err(invalid)?;
            let selected_command = store.command_for_input(&request.messages)?;
            let command = selected_command.map(|(id, _)| id);
            let entry = selected_command
                .map_or_else(|| store.descriptor().entry(), |(_, entry)| Ok(entry))?;
            let program = crate::source_program::load_program_from_bundle(
                &entry.bundle,
                &entry.path,
                &store.session_descriptor().source_roots(),
            )
            .map_err(invalid)?;
            let flow_name = entry.name.clone();
            let entry = program
                .entry_flow(&flow_name)
                .and_then(|id| program.flow(&id))
                .ok_or_else(|| invalid("recorded entry is unavailable"))?;
            let selected: std::collections::HashSet<_> = request.messages.iter().copied().collect();
            let reserved = store
                .reserve_invocation_input(request.turn, request.run, |envelope| {
                    selected.contains(&envelope.id)
                })?
                .ok_or_else(|| invalid("selected input is not available"))?;
            if reserved.message_ids() != request.messages.as_slice() {
                return Err(invalid("selected input order or ownership changed"));
            }
            let lease = reserved.lease().clone();
            let args = request
                .arguments
                .iter()
                .map(|(key, value)| {
                    let decoded = match entry
                        .params
                        .iter()
                        .find(|parameter| &parameter.name.name == key)
                    {
                        Some(parameter) => {
                            super::entry::decode_argument(value.clone(), &parameter.ty)
                        }
                        None => Value::from_json(value.clone()),
                    };
                    (key.clone(), decoded)
                })
                .collect();
            let env = crate::InvocationEnv::from_values(
                request
                    .invocation
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::from_json(value.clone()))),
            );
            let binding =
                InstanceInvocation::new(lease.clone(), request.authority, CancellationToken::new())
                    .with_workspace(workspace)
                    .with_session_start(
                        store.last_invocation_input().is_none()
                            && store.launch_record().is_some_and(|record| {
                                record
                                    .input
                                    .execution
                                    .as_ref()
                                    .is_some_and(|binding| binding.session_start)
                                    && reserved
                                        .message_ids()
                                        .contains(&record.acceptance.message_id)
                            }),
                    )
                    .with_vm_options(
                        atman_rt::VmRunOptions::measure()
                            .with_yield_interval(request.yield_interval),
                    )
                    .with_storage(worker.clone())
                    .map_err(invalid)?;
            store.commit_input(
                &reserved,
                InputContent::InvocationInput {
                    command,
                    arguments: request.arguments,
                    invocation: request.invocation,
                    messages: request.context.clone(),
                },
            )?;
            let context = worker.context();
            let ctx = crate::tool::ToolCtx::new().with_session_runtime(context);
            for message in request.context {
                crate::tools::session::append_message_to_context(&ctx, message).map_err(invalid)?;
            }
            let (completed, observed) = oneshot::channel();
            let owned_lease = lease.clone();
            runtime.spawn(async move {
                let result = std::panic::AssertUnwindSafe(executor.run_linked_with_invocation(
                    &program,
                    &flow_name,
                    args,
                    RootInvocation {
                        instance: Some(binding),
                        env,
                        ..Default::default()
                    },
                ))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(RuntimeError::ToolFailed(
                        "instance execution panicked".into(),
                    ))
                });
                let end = worker
                    .request(move |store| {
                        let record = store
                            .finish_turn(owned_lease.clone(), InvocationResult::capture(&result))?;
                        Ok(TurnCompletion {
                            lease: owned_lease,
                            outcome: record.completion.outcome,
                            result,
                            receipt: record.receipt,
                        })
                    })
                    .await;
                let _ = completed.send(end);
            });
            Ok(Some(TurnTicket {
                lease,
                completed: observed,
            }))
        })
        .await
    }
}

impl FlowWorker {
    /// Stop admission under the writer, unwind active execution, then relinquish storage.
    pub(super) async fn stop_execution(self) -> io::Result<()> {
        let stopping = self.stopping.clone();
        self.request(move |store| {
            stopping.store(true, std::sync::atomic::Ordering::Release);
            if let InstanceState::Open(InstanceActivity::Active(active)) = store.state().state()
                && active.phase != TurnPhase::Cancelling
            {
                store.transition(
                    store.state().revision(),
                    crate::flow_store::FlowTransition::PhaseChanged {
                        lease: active.lease.clone(),
                        phase: TurnPhase::Cancelling,
                    },
                )?;
            }
            Ok(())
        })
        .await?;
        self.wait_execution_stopped().await?;
        let closing = matches!(self.state().await?.state(), InstanceState::Closing { .. });
        let cleanup = if closing {
            self.finish_close().await
        } else {
            Ok(())
        };
        // A failed workspace release must not retain the journal across host shutdown.
        let stopped = self.shutdown().await.map(drop);
        cleanup.and(stopped)
    }
}

/// Pause polling at suspension boundaries while preserving the actual executor future.
///
/// Host-selected VM cooperative yields also make CPU-only code reach these boundaries. Cancellation always permits polling so the executor can unwind its invocation resources.
pub(crate) async fn drive<T>(
    worker: &FlowWorker,
    lease: &TurnLease<AtmanUuid>,
    cancellation: &CancellationToken,
    execution: impl Future<Output = T>,
) -> Result<T, RuntimeError> {
    let mut state = worker.subscribe_state();
    tokio::pin!(execution);
    loop {
        let paused = {
            let snapshot = state.borrow_and_update();
            match snapshot.state() {
                InstanceState::Open(InstanceActivity::Active(active)) if &active.lease == lease => {
                    if active.phase == TurnPhase::Cancelling {
                        cancellation.cancel();
                    }
                    Some(!snapshot.pauses().is_empty() && active.phase != TurnPhase::Finishing)
                }
                InstanceState::Closing {
                    active: Some(active),
                } if &active.lease == lease => {
                    cancellation.cancel();
                    Some(false)
                }
                _ => None,
            }
        };
        let Some(paused) = paused else {
            cancellation.cancel();
            let _ = execution.await;
            return Err(RuntimeError::Cancelled(
                "instance turn lease is no longer active".into(),
            ));
        };
        if cancellation.is_cancelled() {
            return Ok(execution.await);
        }
        tokio::select! {
            biased;
            changed = state.changed() => {
                if changed.is_err() {
                    cancellation.cancel();
                    return Ok(execution.await);
                }
            }
            _ = cancellation.cancelled() => return Ok(execution.await),
            output = std::future::poll_fn(|cx| {
                if worker.trace.poll_capacity(cx).is_pending() {
                    return std::task::Poll::Pending;
                }
                execution.as_mut().poll(cx)
            }), if !paused => return Ok(output),
        }
    }
}
