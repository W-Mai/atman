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

pub(crate) struct LaunchExecution {
    pub executor: crate::Executor,
    pub prepare: PrepareTurn,
    pub completed: CompletedTurn,
    pub trust: crate::trust::TrustConfig,
    pub catalog: crate::event_log::journal::CommitReceipt,
}

impl FlowWorker {
    pub async fn launch_record(&self) -> io::Result<Option<LaunchRecord>> {
        self.request(|store| Ok(store.launch_record())).await
    }
}

impl RuntimeHost {
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
        self.launch_owned(descriptor, input, authorize_new, None)
            .await
    }

    pub(crate) async fn launch_owned(
        &self,
        descriptor: FlowDescriptor,
        input: LaunchInput,
        authorize_new: impl Fn(&FlowDescriptor, &LaunchInput) -> io::Result<()> + Send + Sync + 'static,
        execution: Option<LaunchExecution>,
    ) -> io::Result<LaunchedFlow> {
        let host = self.clone();
        let authorize = Arc::new(authorize_new);
        tokio::spawn(async move {
            if input.request.recipient != descriptor.address
                || input.request.request_id != descriptor.creation_request
            {
                return Err(invalid("launch input does not match creation identity"));
            }
            let session = host.session(descriptor.address.session_id.0)?;
            // Existing catalog entries reconcile through their unique worker before new authorization.
            let existing = session
                .snapshot()
                .await?
                .catalog
                .flows
                .iter()
                .any(|flow| flow.id == descriptor.address.instance_id);
            if !existing {
                authorize(&descriptor, &input)?;
            }
            let worker = session
                .create_flow_at(
                    descriptor.clone(),
                    execution.as_ref().map(|binding| binding.catalog.clone()),
                )
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
            let requested = input.clone();
            let mut controller = None;
            let attempt = async {
                authorize(&descriptor, &input)?;
                if let Some(execution) = execution {
                    worker
                        .context()
                        .update_trust(execution.trust, |_| Ok(()))
                        .map_err(io::Error::other)?;
                    // The launch owns scheduling until activation commits. Failed activation
                    // releases it before returning, so a Prepared instance remains retryable.
                    controller = Some(worker.schedule_observed(
                        execution.executor,
                        execution.prepare,
                        execution.completed,
                    )?);
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
                    id,
                    title: "Launch".into(),
                    project_root: None,
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
            entry: EntrySnapshot {
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
            },
        };
        let input = LaunchInput {
            request: DeliveryRequest {
                request_id: descriptor.creation_request,
                sender: SenderIdentity::User(Uuid::new_v4()),
                recipient: descriptor.address.clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Invocation {
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
            catalog: catalog.clone(),
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
                Some(execution(Box::new(|_| panic!(
                    "unactivated entry executed"
                ))))
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
            Some(execution(Box::new(move |completion| {
                sent.take().unwrap().send(completion).ok();
            }))),
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
