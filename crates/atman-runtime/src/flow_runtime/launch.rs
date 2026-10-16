//! Owned creation, result registration, and initial activation across flow writers.

use std::{io, sync::Arc};

use super::scheduler::{CompletedTurn, PrepareTurn};
use super::{FlowWorker, host::RuntimeHost};
use crate::flow_store::launch::{LaunchInput, LaunchRecord};
use crate::flow_store::subscription::{FlowSubscription, SubscriptionPurpose};
use crate::flow_store::{FlowDescriptor, invalid};

pub struct LaunchedFlow {
    pub worker: FlowWorker,
    pub record: LaunchRecord,
}

/// Trusted host settings for a retained entry, independent of who requested its creation. The catalog cursor protects creation; authority only narrows the recorded entry ceiling.
pub struct EntryExecution {
    pub executor: crate::Executor,
    pub authority: crate::flow_authority::EffectiveAuthority,
    pub trust: crate::trust::TrustConfig,
    pub yield_interval: std::num::NonZeroUsize,
    pub catalog: crate::event_log::journal::CommitReceipt,
}

pub(crate) struct LaunchExecution {
    pub executor: crate::Executor,
    pub prepare: PrepareTurn,
    pub completed: CompletedTurn,
    pub trust: crate::trust::TrustConfig,
}

impl FlowWorker {
    pub async fn descriptor(&self) -> io::Result<FlowDescriptor> {
        self.request(|store| Ok(store.descriptor().clone())).await
    }

    pub async fn launch_record(&self) -> io::Result<Option<LaunchRecord>> {
        self.request(|store| Ok(store.launch_record())).await
    }
}

impl RuntimeHost {
    /// Launch and retain scheduling through the same owner path used by peer flow creation.
    ///
    /// Authenticate the sender before calling. The authorization callback is rechecked at each new-effect boundary. Invalid entry bindings fail before catalog mutation. A matching retry only returns the committed activation; it does not resume paused work or restart a stopped scheduler. Use `SessionHandle::bind_entry` to explicitly restore scheduling.
    pub async fn launch_entry(
        &self,
        descriptor: FlowDescriptor,
        input: LaunchInput,
        execution: EntryExecution,
        authorize_new: impl Fn(&FlowDescriptor, &LaunchInput) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<LaunchedFlow> {
        let creation = descriptor.clone();
        let requested = input.clone();
        let snapshot = self
            .session(descriptor.address.session_id.0)?
            .snapshot()
            .await?;
        let session = snapshot.catalog.descriptor;
        self.launch_owned(
            descriptor,
            input,
            authorize_new,
            Some(execution.catalog),
            move || {
                let recipe = super::entry::EntryRecipe::from_launch(
                    &session,
                    &creation,
                    &requested,
                    &execution.authority,
                    execution.yield_interval,
                )?;
                Ok(Some(LaunchExecution {
                    executor: execution.executor,
                    prepare: Box::new(move |snapshot| recipe.prepare(snapshot)),
                    completed: Box::new(|_| {}),
                    trust: execution.trust,
                }))
            },
        )
        .await
    }

    /// Trusted host operation. Authenticate the caller, sender, source access, and inherited context
    /// before entry. The callback authorizes new effects at each owner boundary. Matching committed
    /// retries return the original receipt even after consumption, pause, or close.
    ///
    /// The host owns an accepted attempt even if its waiting caller disappears. Failure before
    /// activation leaves the instance Prepared; retry the same descriptor and initial input.
    pub async fn launch_flow(
        &self,
        descriptor: FlowDescriptor,
        input: LaunchInput,
        authorize_new: impl Fn(&FlowDescriptor, &LaunchInput) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<LaunchedFlow> {
        self.launch_owned(descriptor, input, authorize_new, None, || Ok(None))
            .await
    }

    pub(crate) async fn launch_owned(
        &self,
        descriptor: FlowDescriptor,
        input: LaunchInput,
        authorize_new: impl Fn(&FlowDescriptor, &LaunchInput) -> io::Result<()> + Send + Sync + 'static,
        catalog: Option<crate::event_log::journal::CommitReceipt>,
        prepare_execution: impl FnOnce() -> io::Result<Option<LaunchExecution>> + Send + 'static,
    ) -> io::Result<LaunchedFlow> {
        let host = self.clone();
        let authorize = Arc::new(authorize_new);
        tokio::spawn(async move {
            if input.request.recipient != descriptor.address
                || input.request.request_id != descriptor.creation_request
            {
                return Err(invalid("launch input does not match creation identity"));
            }
            input.request.validate_source()?;
            let session = host.session(descriptor.address.session_id.0)?;
            let snapshot = session.snapshot().await?;
            let startup = input
                .execution
                .as_ref()
                .is_some_and(|binding| binding.session_start);
            if input
                .execution
                .as_ref()
                .is_some_and(|binding| binding.session_start)
                && (descriptor.created_by.is_some()
                    || matches!(
                        input.request.sender,
                        crate::flow_store::inbox::SenderIdentity::Flow(_)
                    )
                    || snapshot
                        .catalog
                        .flows
                        .first()
                        .is_some_and(|flow| flow.id != descriptor.address.instance_id))
            {
                return Err(invalid(
                    "session startup requires the container's initial host entry",
                ));
            }
            let catalog = if startup {
                Some(catalog.unwrap_or_else(|| snapshot.cursor.clone()))
            } else {
                catalog
            };
            // Existing catalog entries reconcile through their unique worker before new authorization.
            let existing = snapshot
                .catalog
                .flows
                .iter()
                .any(|flow| flow.id == descriptor.address.instance_id);
            let existing = if existing {
                let worker = session
                    .create_flow_at(descriptor.clone(), catalog.clone())
                    .await?;
                if let Some(record) = worker.launch_record().await? {
                    return if record.input == input {
                        Ok(LaunchedFlow { worker, record })
                    } else {
                        Err(invalid(
                            "launch input conflicts with the committed activation",
                        ))
                    };
                }
                Some(worker)
            } else {
                authorize(&descriptor, &input)?;
                None
            };
            let execution = tokio::task::spawn_blocking(prepare_execution)
                .await
                .map_err(io::Error::other)??;
            let worker = match existing {
                Some(worker) => worker,
                None => session.create_flow_at(descriptor.clone(), catalog).await?,
            };
            // One activation owns scheduler adoption. Retried launches wait for its durable result instead of racing for scheduling ownership.
            let _launch = worker.launch_gate.clone().lock_owned().await;
            // Creation can race with another accepted attempt. Reconcile again before new effects.
            if let Some(record) = worker.launch_record().await? {
                return if record.input == input {
                    Ok(LaunchedFlow { worker, record })
                } else {
                    Err(invalid(
                        "launch input conflicts with the committed activation",
                    ))
                };
            }
            let requested = input.clone();
            let mut controller = None;
            let attempt = async {
                authorize(&descriptor, &input)?;
                if let Some(workspace) = input.workspace.clone() {
                    let authorize = authorize.clone();
                    let creation = descriptor.clone();
                    let requested = input.clone();
                    worker
                        .request(move |store| {
                            authorize(&creation, &requested)?;
                            if store.state().state() != &atman_rt::InstanceState::Prepared {
                                return Err(invalid(
                                    "only a prepared instance can acquire launch resources",
                                ));
                            }
                            store.request_workspace(workspace.clone())?;
                            workspace.acquire(store.state().address())
                        })
                        .await?;
                }
                if let Some(execution) = execution {
                    // The launch owns scheduling until activation commits. Failed activation
                    // releases it before returning, so a Prepared instance remains retryable.
                    controller = Some(worker.schedule_observed(
                        execution.executor,
                        execution.prepare,
                        execution.completed,
                    )?);
                    worker
                        .context()
                        .update_trust(execution.trust, |_| Ok(()))
                        .map_err(io::Error::other)?;
                }
                let anchor = worker
                    .request(|store| {
                        if store.state().state() != &atman_rt::InstanceState::Prepared {
                            return Err(invalid("instance is no longer prepared for launch"));
                        }
                        Ok(store.observation_anchor())
                    })
                    .await?;
                let proof = if let Some(route) = &input.result_route {
                    host.authorize_observation(&route.recipient, &descriptor.address)?;
                    let recipient = host.flow(&route.recipient).await?;
                    let subscription = FlowSubscription {
                        id: route.subscription,
                        source: descriptor.address.clone(),
                        filter: route.filter(),
                        once: false,
                        purpose: SubscriptionPurpose::SpawnResult,
                        expires_at: None,
                        reply: None,
                    };
                    let auth = authorize.clone();
                    let creation = descriptor.clone();
                    let requested = input.clone();
                    let access = host.clone();
                    Some(
                        recipient
                            .request(move |store| {
                                auth(&creation, &requested)?;
                                access.authorize_observation(
                                    store.state().address(),
                                    &creation.address,
                                )?;
                                // A retry after registration keeps the original anchor even if unrelated source
                                // commits advanced the current journal. Registration remains idempotent.
                                let existing = store
                                    .flow_subscriptions()
                                    .find(|r| r.subscription.id == subscription.id)
                                    .cloned();
                                if let Some(record) = existing {
                                    if record.subscription != subscription {
                                        return Err(invalid(
                                            "result subscription identity changed",
                                        ));
                                    }
                                } else {
                                    store.subscribe_events(subscription.clone(), &anchor)?;
                                }
                                let completion =
                                    requested.result_route.as_ref().and_then(|route| {
                                        route.completion_subscription(&creation.address)
                                    });
                                if let Some(completion) = &completion {
                                    store.subscribe_events(completion.clone(), &anchor)?;
                                }
                                store.result_subscription_proof(
                                    subscription.id,
                                    completion.map(|s| s.id),
                                )
                            })
                            .await?,
                    )
                } else {
                    None
                };
                let context = worker.context();
                let access = host.clone();
                worker
                    .request(move |store| {
                        let new = store.launch_record().is_none();
                        let record = store.activate_with_input(input, proof, |input| {
                            authorize(&descriptor, input)?;
                            if let Some(route) = &input.result_route {
                                access
                                    .authorize_observation(&route.recipient, &descriptor.address)?;
                            }
                            Ok(())
                        })?;
                        if new {
                            context.project_instance_goal(store.goal().cloned());
                            for message in &record.input.inherited_context {
                                context.append_message(message.clone(), None);
                            }
                        }
                        Ok(record)
                    })
                    .await
            }
            .await;
            let record = match attempt {
                Ok(record) => record,
                Err(error) => {
                    // Another attempt may have committed while this one was between owners.
                    // Reconcile the authoritative activation; never rerun an effect to infer success.
                    match worker.launch_record().await? {
                        Some(record) if record.input == requested => record,
                        _ => {
                            if let Some(controller) = controller.take() {
                                let _ = controller.stop().await;
                            }
                            return Err(error);
                        }
                    }
                }
            };
            if let Some(controller) = controller {
                session.adopt_scheduler(worker.clone(), controller).await?;
            }
            Ok(LaunchedFlow { worker, record })
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_runtime::execution::TurnRequest;
    use crate::flow_store::inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity};
    use crate::flow_store::{
        EntrySnapshot, FlowInstanceId, FlowRef, RuntimeStore, SessionDescriptor,
    };
    use atman_rt::{InstanceState, RunId, TurnId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    #[tokio::test]
    async fn failed_launch_releases_scheduling_and_retry_adopts_one_controller() {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let id = Uuid::new_v4();
        let host = RuntimeHost::default();
        let session = host
            .attach_session(
                root.create_session(SessionDescriptor {
                    configuration: Default::default(),
                    id,
                    title: "Launch".into(),
                    project: None,
                })
                .unwrap(),
            )
            .unwrap();
        let descriptor = FlowDescriptor {
            address: FlowRef {
                session_id: id.into(),
                instance_id: FlowInstanceId::now(),
            },
            creation_request: Uuid::new_v4(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: crate::source_program::RevisionBundle {
                    entry: "entry:main.at".into(),
                    edges: Default::default(),
                    sources: [(
                        "entry:main.at".into(),
                        "flow main() -> int { return 42 }".into(),
                    )]
                    .into(),
                },
            }
            .into(),
        };
        let input = LaunchInput {
            workspace: None,
            initial_goal: None,
            execution: None,
            request: DeliveryRequest {
                request_id: descriptor.creation_request,
                sender: SenderIdentity::User(Uuid::new_v4()),
                recipient: descriptor.address.clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Invocation {
                    user: None,
                    arguments: Default::default(),
                    invocation: Default::default(),
                },
            },
            inherited_context: vec![],
            result_route: None,
        };
        let catalog = session.snapshot().await.unwrap().cursor;
        let execution = |completed: CompletedTurn| LaunchExecution {
            executor: crate::Executor::new(),
            trust: Default::default(),
            completed,
            prepare: Box::new(|snapshot| {
                Ok(snapshot.pending.first().map(|input| TurnRequest {
                    turn: TurnId::now(),
                    run: RunId::now(),
                    messages: vec![input.id],
                    arguments: Default::default(),
                    invocation: Default::default(),
                    context: vec![],
                    authority: crate::flow_authority::EffectiveAuthority::root(
                        &Default::default(),
                        false,
                        None,
                    ),
                    yield_interval: std::num::NonZeroUsize::new(16).unwrap(),
                }))
            }),
        };
        let checks = AtomicUsize::new(0);
        assert!(
            host.launch_owned(
                descriptor.clone(),
                input.clone(),
                move |_, _| {
                    if checks.fetch_add(1, Ordering::SeqCst) == 2 {
                        Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "activation denied",
                        ))
                    } else {
                        Ok(())
                    }
                },
                Some(catalog.clone()),
                move || Ok(Some(execution(Box::new(|_| panic!(
                    "unactivated entry executed"
                )))))
            )
            .await
            .is_err()
        );
        let worker = host.flow(&descriptor.address).await.unwrap();
        assert_eq!(
            worker.state().await.unwrap().state(),
            &InstanceState::Prepared
        );
        assert!(worker.launch_record().await.unwrap().is_none());
        assert!(
            session
                .scheduling_status(descriptor.address.instance_id.clone())
                .await
                .unwrap()
                .is_none()
        );
        let (sent, received) = tokio::sync::oneshot::channel();
        let mut sent = Some(sent);
        host.launch_owned(
            descriptor.clone(),
            input,
            |_, _| Ok(()),
            Some(catalog),
            move || {
                Ok(Some(execution(Box::new(move |completion| {
                    sent.take().unwrap().send(completion).ok();
                }))))
            },
        )
        .await
        .unwrap();
        let completion = tokio::time::timeout(std::time::Duration::from_secs(5), received)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(completion.result, Ok(crate::Value::Int(42))));
        assert!(
            session
                .scheduling_status(descriptor.address.instance_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(worker.snapshot().await.unwrap().pending.is_empty());
    }
}
