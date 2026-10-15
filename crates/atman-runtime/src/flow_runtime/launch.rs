//! Owned creation, result registration, and initial activation across flow writers.

use std::{io, sync::Arc};

use super::{FlowWorker, host::RuntimeHost};
use crate::flow_store::launch::{LaunchInput, LaunchRecord};
use crate::flow_store::subscription::{AnswerFilter, AnswerSubscription, SubscriptionPurpose};
use crate::flow_store::{FlowDescriptor, invalid};

pub struct LaunchedFlow {
    pub worker: FlowWorker,
    pub record: LaunchRecord,
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
            let worker = session.create_flow(descriptor.clone()).await?;
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
            let attempt = async {
                authorize(&descriptor, &input)?;
                let anchor = worker
                    .request(|store| {
                        if store.state().state() != &atman_rt::InstanceState::Prepared {
                            return Err(invalid("instance is no longer prepared for launch"));
                        }
                        Ok(store.publication_anchor())
                    })
                    .await?;
                let proof = if let Some(route) = &input.result_route {
                    host.authorize_observation(&route.recipient, &descriptor.address)?;
                    let recipient = host.flow(&route.recipient).await?;
                    let subscription = AnswerSubscription {
                        id: route.subscription,
                        source: descriptor.address.clone(),
                        filter: AnswerFilter::All,
                        once: false,
                        purpose: SubscriptionPurpose::SpawnResult,
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
                                    .answer_subscriptions()
                                    .find(|r| r.subscription.id == subscription.id)
                                    .cloned();
                                if let Some(record) = existing {
                                    if record.subscription != subscription {
                                        return Err(invalid(
                                            "result subscription identity changed",
                                        ));
                                    }
                                } else {
                                    store.subscribe_answers(subscription.clone(), &anchor)?;
                                }
                                store.result_subscription_proof(subscription.id)
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
                        _ => return Err(error),
                    }
                }
            };
            Ok(LaunchedFlow { worker, record })
        })
        .await
        .map_err(io::Error::other)?
    }
}
