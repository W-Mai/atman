//! Owner handoff from a frozen memory conversation to a registered durable writer.

use super::*;
use crate::flow_runtime::scheduler::SchedulerBinding;

#[path = "host_retention/resume.rs"]
mod resume;
use crate::flow_store::retention::{RetentionCapture, RetentionReceipt};

pub(super) type Authorize = Arc<dyn Fn(&FlowRef) -> io::Result<()> + Send + Sync>;

#[derive(Clone)]
pub struct KeptFlow {
    pub worker: FlowWorker,
    pub receipt: RetentionReceipt,
}

/// Publication may have completed. Keep the original view readable and retry this exact request before sending or discarding.
#[derive(Debug)]
pub struct RetentionPending {
    pub request: Uuid,
    pub source: FlowPosition,
    pub reason: String,
}

impl std::fmt::Display for RetentionPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Keep is pending; retry the same request: {}",
            self.reason
        )
    }
}

impl std::error::Error for RetentionPending {}

pub(super) struct KeepRequest {
    target: FlowRef,
    id: Uuid,
    expected: FlowPosition,
    authorize: Authorize,
}

pub(super) struct ResumeRequest {
    target: FlowRef,
    id: Uuid,
    accepted: Acceptance,
    authorize: Authorize,
}

struct FrozenOwner {
    memory: FlowWorker,
    capture: Arc<RetentionCapture>,
    scheduling: Option<SchedulerBinding>,
}

struct RetiredOwner {
    receipt: RetentionReceipt,
    memory: Option<FlowWorker>,
    capture: Option<Arc<RetentionCapture>>,
    scheduling: Option<SchedulerBinding>,
    awaiting_submission: bool,
}

#[derive(Default)]
pub(super) struct CatalogRetention {
    pending: HashMap<FlowInstanceId, FrozenOwner>,
    retired: HashMap<FlowInstanceId, RetiredOwner>,
}

pub(super) struct CatalogBindings<'a> {
    pub flows: &'a FlowDirectory,
    pub access: &'a Arc<RwLock<SessionAccess>>,
    pub observers: &'a mut HashMap<FlowInstanceId, AbortOnDropHandle<()>>,
    pub schedulers: &'a mut HashMap<FlowInstanceId, FlowScheduler>,
    pub transient_requests: &'a mut HashMap<FlowInstanceId, Uuid>,
    pub host: &'a WeakRuntimeHost,
    pub services: &'a HostServices,
}

impl RuntimeHost {
    /// Transfer an idle temporary owner without executing its pending inputs. Accepted work survives caller disconnection.
    pub async fn keep_transient(
        &self,
        target: FlowRef,
        request_id: Uuid,
        expected: FlowPosition,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<KeptFlow> {
        let host = self.clone();
        tokio::spawn(async move {
            authorize(&target)?;
            let session = host.session(target.session_id.0)?;
            let _source = session.retain_source().await?;
            let worker = session
                .loaded_flow(&target.instance_id)
                .ok_or_else(|| invalid("temporary conversation is not loaded"))?;
            let _launch = worker.launch_gate.clone().lock_owned().await;
            session
                .request(|reply| CatalogCommand::Keep {
                    request: KeepRequest {
                        target,
                        id: request_id,
                        expected,
                        authorize: Arc::new(authorize),
                    },
                    reply,
                })
                .await
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Release the original memory owner only after a client has installed the durable document.
    pub async fn acknowledge_retention(
        &self,
        receipt: RetentionReceipt,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<()> {
        authorize(&receipt.flow)?;
        self.session(receipt.flow.session_id.0)?
            .request(|reply| CatalogCommand::AcknowledgeRetention {
                receipt,
                authorize: Arc::new(authorize),
                reply,
            })
            .await
    }

    /// Restore the retained scheduling policy after an explicit user submission has been accepted. Opening or acknowledging a view does not call this operation.
    pub async fn resume_retained_scheduling(
        &self,
        target: FlowRef,
        request_id: Uuid,
        accepted: Acceptance,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<()> {
        authorize(&target)?;
        self.session(target.session_id.0)?
            .request(|reply| CatalogCommand::ResumeRetained {
                request: ResumeRequest {
                    target,
                    id: request_id,
                    accepted,
                    authorize: Arc::new(authorize),
                },
                reply,
            })
            .await
    }
}

impl CatalogRetention {
    pub(super) fn blocks_scheduling(&self, store: &SessionStore, id: &FlowInstanceId) -> bool {
        self.pending.contains_key(id)
            || store.projection().flows.iter().any(|flow| {
                &flow.id == id && flow.retention.is_some() && flow.retention_resume.is_none()
            })
            || self
                .retired
                .get(id)
                .is_some_and(|owner| owner.awaiting_submission)
    }

    pub(super) fn has_retired_memory(&self) -> bool {
        self.retired.values().any(|owner| owner.memory.is_some())
    }

    pub(super) async fn release_unobserved(&mut self) {
        for owner in self.retired.values_mut() {
            let unobserved = owner
                .memory
                .as_ref()
                .is_some_and(|memory| memory.commands.strong_count() == 1);
            if unobserved {
                if let Some(memory) = owner.memory.take() {
                    let _ = memory.stop_execution().await;
                }
                owner.capture = None;
            }
        }
        self.retired.retain(|_, owner| {
            owner.memory.is_some() || owner.awaiting_submission || owner.scheduling.is_some()
        });
    }

    pub(super) async fn keep(
        &mut self,
        mut store: SessionStore,
        request: KeepRequest,
        bindings: CatalogBindings<'_>,
    ) -> Result<(SessionStore, io::Result<KeptFlow>), tokio::task::JoinError> {
        let id = request.target.instance_id.clone();
        if let Err(error) = (request.authorize)(&request.target) {
            return Ok((store, Err(error)));
        }
        let worker = bindings.flows.lock().unwrap().get(&id).cloned();
        let Some(worker) = worker.filter(|worker| !worker.is_stopped()) else {
            return Ok((store, Err(invalid("retention owner is unavailable"))));
        };
        if request.id.is_nil() || request.target != *worker.address() {
            return Ok((store, Err(invalid("retention identity changed"))));
        }
        // A retry after canonical handoff must not acquire the already owned durable journal.
        if !worker.is_transient() {
            let expected = request.expected;
            let requested = request.id;
            let (returned, receipt) = tokio::task::spawn_blocking(move || {
                let result = store.retention_receipt(requested, &expected);
                (store, result)
            })
            .await?;
            store = returned;
            *bindings.access.write().unwrap() = store.projection().access.clone();
            let result = receipt
                .and_then(|receipt| {
                    receipt.ok_or_else(|| invalid("Flow was not retained by this request"))
                })
                .map(|receipt| KeptFlow { worker, receipt });
            return Ok((store, result));
        }
        if let Some(pending) = self.pending.get(&id) {
            if pending.capture.request() != request.id
                || (FlowPosition::Transient {
                    transient: pending.capture.source().clone(),
                }) != request.expected
                || !pending.memory.commands.same_channel(&worker.commands)
            {
                return Ok((
                    store,
                    Err(invalid("retry the original pending Keep request")),
                ));
            }
        } else {
            let authorize = request.authorize.clone();
            let expected = request.expected.clone();
            let checked = worker
                .request(move |store| {
                    authorize(store.state().address())?;
                    if store.cursor() != &expected
                        || store.transient_frozen()
                        || matches!(
                            store.state().state(),
                            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_))
                                | atman_rt::InstanceState::Closing { .. }
                        )
                    {
                        return Err(invalid("Keep requires the inspected idle conversation"));
                    }
                    Ok(())
                })
                .await;
            if let Err(error) = checked {
                return Ok((store, Err(error)));
            }
            let controller = bindings.schedulers.remove(&id);
            if controller.is_none()
                && worker
                    .scheduler_owned
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return Ok((
                    store,
                    Err(invalid("retention requires host-owned scheduling")),
                ));
            }
            let scheduling = match controller {
                Some(controller) => match controller.suspend().await {
                    Ok(binding) => Some(binding),
                    Err(error) => return Ok((store, Err(error))),
                },
                None => None,
            };
            if let Some(observer) = bindings.observers.remove(&id) {
                observer.abort();
                let _ = observer.await;
            }
            let expected = request.expected.clone();
            let authorize = request.authorize.clone();
            let resources = worker.resources.clone();
            let frozen = worker
                .request(move |store| {
                    authorize(store.state().address())?;
                    if store.cursor() != &expected || !resources.is_stopped() {
                        return Err(invalid("Keep requires completed process resources"));
                    }
                    store.freeze_transient()
                })
                .await;
            let capture = match frozen {
                Err(error) => Err(error),
                Ok(frozen) => {
                    // A sealed idle writer cannot admit another invocation. Drain completion-owned producers before capturing output assets.
                    worker.resources.executions.close();
                    worker.resources.executions.wait().await;
                    let ticket = frozen.id;
                    let authorize = request.authorize.clone();
                    let expected = request.expected.clone();
                    let requested = request.id;
                    let trace = worker.trace.clone();
                    let captured = worker
                        .request(move |store| {
                            // No other storage command can interleave this thaw, late trace flush and final freeze.
                            store.thaw_transient(frozen.id)?;
                            trace.flush(store)?;
                            authorize(store.state().address())?;
                            store.capture_retention(requested, &expected)
                        })
                        .await;
                    if captured.is_err() {
                        let _ = worker
                            .request(move |store| {
                                if store.transient_frozen() {
                                    store.thaw_transient(ticket)?;
                                }
                                Ok(())
                            })
                            .await;
                        worker.resources.executions.reopen();
                    }
                    captured
                }
            };
            let capture = match capture {
                Ok(capture) => Arc::new(capture),
                Err(error) => {
                    bindings.observers.insert(
                        id.clone(),
                        super::super::observation::start(worker.clone(), bindings.host.clone()),
                    );
                    if let Some(binding) = scheduling {
                        match binding.restore(&worker) {
                            Ok(controller) => {
                                bindings.schedulers.insert(id, controller);
                            }
                            Err(restore) => {
                                return Ok((
                                    store,
                                    Err(io::Error::other(format!(
                                        "{error}; scheduler restoration failed: {restore}"
                                    ))),
                                ));
                            }
                        }
                    }
                    return Ok((store, Err(error)));
                }
            };
            self.pending.insert(
                id.clone(),
                FrozenOwner {
                    memory: worker.clone(),
                    capture,
                    scheduling,
                },
            );
        }
        let pending = &self.pending[&id];
        let capture = pending.capture.clone();
        let expected = store.cursor().clone();
        let (returned, saved) = tokio::task::spawn_blocking(move || {
            let result = store.retain_flow(&capture, &expected);
            (store, result)
        })
        .await?;
        store = returned;
        *bindings.access.write().unwrap() = store.projection().access.clone();
        let saved = match saved {
            Ok(saved) => saved,
            Err(error) => return Ok((store, Err(pending_error(&request, error)))),
        };
        let receipt = saved.receipt;
        let durable = match FlowWorker::start_owned(
            saved.store,
            Some(bindings.host.clone()),
            bindings.services.redactor.clone(),
        )
        .await
        {
            Ok(durable) => durable,
            Err(error) => return Ok((store, Err(pending_error(&request, error)))),
        };
        let context = durable.context();
        let trust = worker.context().trust_config();
        let configured =
            tokio::task::spawn_blocking(move || context.update_trust(trust, |_| Ok(()))).await;
        let configured = configured
            .map_err(io::Error::other)
            .and_then(|result| result.map_err(io::Error::other));
        if let Err(error) = configured {
            let _ = durable.stop_execution().await;
            return Ok((store, Err(pending_error(&request, error))));
        }
        if let Some(clients) = bindings.services.permission_clients.upgrade() {
            clients.register(
                durable.address().clone(),
                &durable.context().permission_broker(),
            );
        }
        bindings
            .flows
            .lock()
            .unwrap()
            .insert(id.clone(), durable.clone());
        bindings.observers.insert(
            id.clone(),
            super::super::observation::start(durable.clone(), bindings.host.clone()),
        );
        bindings.transient_requests.remove(&id);
        let pending = self
            .pending
            .remove(&id)
            .expect("frozen owner is retained through handoff");
        self.retired.insert(
            id,
            RetiredOwner {
                receipt: receipt.clone(),
                memory: Some(pending.memory),
                capture: Some(pending.capture),
                scheduling: pending.scheduling,
                awaiting_submission: true,
            },
        );
        if let Ok(host) = bindings.host.upgrade() {
            host.changed();
        }
        Ok((
            store,
            Ok(KeptFlow {
                worker: durable,
                receipt,
            }),
        ))
    }

    pub(super) async fn acknowledge(
        &mut self,
        receipt: RetentionReceipt,
        authorize: Authorize,
        store: &SessionStore,
    ) -> io::Result<()> {
        authorize(&receipt.flow)?;
        let registered = store
            .projection()
            .flows
            .iter()
            .find(|flow| flow.id == receipt.flow.instance_id)
            .and_then(|flow| flow.retention.as_ref());
        if registered != Some(&receipt) {
            return Err(invalid(
                "retention acknowledgement does not match the catalog",
            ));
        }
        if let Some(retired) = self.retired.get_mut(&receipt.flow.instance_id) {
            if retired.receipt != receipt {
                return Err(invalid("retention handoff changed"));
            }
            if let Some(memory) = retired.memory.take() {
                memory.stop_execution().await?;
            }
            retired.capture = None;
            if retired.scheduling.is_none() && !retired.awaiting_submission {
                self.retired.remove(&receipt.flow.instance_id);
            }
        }
        Ok(())
    }

    pub(super) fn resume(
        &mut self,
        target: &FlowRef,
        request: Uuid,
        authorize: Authorize,
        store: &SessionStore,
        bindings: CatalogBindings<'_>,
    ) -> io::Result<()> {
        authorize(target)?;
        if self.pending.contains_key(&target.instance_id) {
            return Err(invalid(
                "Keep must be reconciled before restoring scheduling",
            ));
        }
        let registered = store
            .projection()
            .flows
            .iter()
            .find(|flow| flow.id == target.instance_id)
            .and_then(|flow| flow.retention.as_ref());
        if registered.is_none_or(|receipt| receipt.flow != *target || receipt.request != request) {
            return Err(invalid(
                "retained scheduling request does not match the catalog",
            ));
        }
        let Some(retired) = self.retired.get_mut(&target.instance_id) else {
            return Ok(());
        };
        if retired.receipt.request != request || retired.receipt.flow != *target {
            return Err(invalid("retained scheduling identity changed"));
        }
        if !retired.awaiting_submission {
            return Ok(());
        }
        let worker = bindings
            .flows
            .lock()
            .unwrap()
            .get(&target.instance_id)
            .cloned()
            .ok_or_else(|| invalid("retained owner is unavailable"))?;
        if worker.is_transient() || worker.is_stopped() {
            return Err(invalid("durable handoff is not installed"));
        }
        if bindings.schedulers.contains_key(&target.instance_id) {
            return Err(invalid("retained owner already has a scheduling policy"));
        }
        if let Some(binding) = retired.scheduling.as_mut() {
            bindings
                .schedulers
                .insert(target.instance_id.clone(), binding.start(&worker)?);
            retired.scheduling = None;
        }
        retired.awaiting_submission = false;
        if retired.memory.is_none() {
            self.retired.remove(&target.instance_id);
        }
        Ok(())
    }

    pub(super) async fn shutdown(&mut self) {
        let workers = self
            .pending
            .drain()
            .map(|(_, pending)| pending.memory)
            .chain(
                self.retired
                    .drain()
                    .filter_map(|(_, retired)| retired.memory),
            )
            .collect::<Vec<_>>();
        for worker in workers {
            if !worker.is_stopped() {
                let _ = worker.stop_execution().await;
            }
        }
    }
}

fn pending_error(request: &KeepRequest, error: io::Error) -> io::Error {
    io::Error::other(RetentionPending {
        request: request.id,
        source: request.expected.clone(),
        reason: error.to_string(),
    })
}

#[cfg(test)]
#[path = "host_retention/tests.rs"]
mod tests;
