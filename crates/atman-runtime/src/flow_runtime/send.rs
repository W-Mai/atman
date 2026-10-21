//! Owned delivery attempts from durable sender intent to receiver acknowledgement.

use std::io;

use atman_rt::TurnLease;
use uuid::Uuid;

use super::{FlowWorker, host::RuntimeHost};
use crate::event::AtmanUuid;
use crate::flow_store::FlowRef;
use crate::flow_store::inbox::DeliveryRequest;
use crate::flow_store::outbox::{
    DeliveryReceipt, RecipientRejected, SendAuthorization, SendRecord, SendStatus,
};
use crate::flow_store::waiter::ReplyWaiter;

/// A committed send and the outcome of its latest delivery attempt.
///
/// A pending record survives a failed attempt. Its recipient may already have accepted it when acknowledgement failed; retry the same request instead of creating another request identity.
#[derive(Debug)]
pub struct SendAttempt {
    pub record: SendRecord,
    pub pending_reason: Option<io::Error>,
}

/// A pending delivery's transient error; the original intent remains authoritative.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryIssue {
    pub request_id: Option<Uuid>,
    pub reason: String,
}

impl FlowWorker {
    pub fn subscribe_delivery_issues(&self) -> tokio::sync::watch::Receiver<Vec<DeliveryIssue>> {
        self.delivery_issues.subscribe()
    }

    /// Retry durable peer deliveries after an external storage or transport repair.
    pub fn retry_deliveries(&self) {
        self.observation_retry.send_modify(|_| {});
    }

    /// Authenticate the invoking flow before queueing; authorize only a new send.
    pub async fn queue_send(
        &self,
        lease: TurnLease<AtmanUuid>,
        request: DeliveryRequest,
        reply: Option<ReplyWaiter>,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<SendRecord> {
        self.queue_send_authorized(
            lease,
            request,
            reply,
            SendAuthorization::Host,
            authorize_new,
        )
        .await
    }

    pub(super) async fn queue_send_authorized(
        &self,
        lease: TurnLease<AtmanUuid>,
        request: DeliveryRequest,
        reply: Option<ReplyWaiter>,
        authorization: SendAuthorization,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<SendRecord> {
        let Some(scope) = request.scope.clone() else {
            return self
                .request(move |store| {
                    store.queue_send_authorized(
                        &lease,
                        request,
                        reply,
                        authorization,
                        authorize_new,
                    )
                })
                .await;
        };
        let lookup = request.clone();
        let expected_reply = reply.clone();
        if let Some(previous) = self
            .request(move |store| store.send_retry(&lookup, expected_reply.as_ref(), authorization))
            .await?
        {
            return Ok(previous);
        }
        let admission = self.resolve_message_scope(scope, None).await;
        let dependencies = admission.as_ref().ok().cloned().unwrap_or_default();
        self.request_scoped(dependencies, move |store, snapshot| {
            store.queue_send_authorized(&lease, request, reply, authorization, |request| {
                authorize_new(request)?;
                admission?.check(snapshot)
            })
        })
        .await
    }

    pub async fn send_record(&self, request_id: Uuid) -> io::Result<Option<SendRecord>> {
        self.request(move |store| Ok(store.send_record(request_id).cloned()))
            .await
    }

    pub async fn pending_sends(&self) -> io::Result<Vec<SendRecord>> {
        self.request(|store| Ok(store.pending_sends().cloned().collect()))
            .await
    }
}

/// Storage ownership, rather than a turn or frontend, retains this transport driver.
pub(super) async fn reconcile(sender: FlowWorker, directory: super::host::WeakRuntimeHost) {
    use crate::flow_store::access::FlowAction;
    use futures::FutureExt;

    let Ok(host) = directory.upgrade() else {
        return;
    };
    let mut directory_changes = host.changes();
    drop(host);
    let mut commits = sender.subscribe_commits();
    let mut retries = sender.observation_retry.subscribe();
    loop {
        directory_changes.borrow_and_update();
        commits.borrow_and_update();
        retries.borrow_and_update();
        let Ok(host) = directory.upgrade() else {
            return;
        };
        if sender.is_stopped() {
            return;
        }
        let mut issues = Vec::new();
        let records = match sender.pending_sends().await {
            Ok(records) => records
                .into_iter()
                .filter(|record| record.intent.authorization == SendAuthorization::PeerGrants)
                .collect::<Vec<_>>(),
            Err(error) => {
                issues.push(DeliveryIssue {
                    request_id: None,
                    reason: error.to_string(),
                });
                Vec::new()
            }
        };
        let mut scopes = sender
            .resolve_message_scopes(
                records
                    .iter()
                    .filter_map(|record| record.intent.request.scope.clone())
                    .collect(),
            )
            .await;
        let mut waits = Vec::new();
        let mut targets = std::collections::HashSet::new();
        for record in records {
            let target = record.intent.request.recipient.clone();
            // Subscribe before receiver admission, so activation, closure, and ACK writes cannot be missed.
            if targets.insert(target.clone()) {
                if let Ok(recipient) = host.flow(&target).await {
                    let mut changes = recipient.subscribe_commits();
                    waits.push(
                        async move {
                            let _ = changes.changed().await;
                        }
                        .boxed(),
                    );
                }
            }
            let authorizer = host.clone();
            let source = sender.address().clone();
            let request_id = record.intent.request.request_id;
            let result = attempt(&host, &sender, record, move |request| {
                let session = authorizer.session(source.session_id.0)?;
                if authorizer.allows_access(&session, &source, &request.recipient, FlowAction::Send)
                {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "flow action is outside the current access grants",
                    ))
                }
            })
            .await;
            if let Some(error) = result.pending_reason {
                issues.push(DeliveryIssue {
                    request_id: Some(request_id),
                    reason: error.to_string(),
                });
            }
        }
        sender.delivery_issues.send_if_modified(|previous| {
            if *previous == issues {
                false
            } else {
                *previous = issues;
                true
            }
        });
        drop(host);
        let has_targets = !waits.is_empty();
        tokio::select! {
            result = directory_changes.changed() => { if result.is_err() { return; } }
            result = commits.changed() => { if result.is_err() { return; } }
            result = retries.changed() => { if result.is_err() { return; } }
            _ = scopes.changed() => {}
            _ = async { futures::future::select_all(waits).await }, if has_targets => {}
        }
        tokio::task::yield_now().await;
    }
}

impl RuntimeHost {
    /// Authenticate the sender first. Commit intent, attempt delivery, and record the receiver's ACK.
    ///
    /// The owned task continues if its waiting caller is cancelled. Send and delivery authorization are separate commit-boundary checks; an already accepted retry does not become new work after a pause or revocation.
    pub async fn send(
        &self,
        lease: TurnLease<AtmanUuid>,
        request: DeliveryRequest,
        reply: Option<ReplyWaiter>,
        authorize_send: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
        authorize_delivery: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<SendAttempt> {
        self.send_authorized(
            lease,
            request,
            reply,
            SendAuthorization::Host,
            authorize_send,
            authorize_delivery,
        )
        .await
    }

    pub(super) async fn send_authorized(
        &self,
        lease: TurnLease<AtmanUuid>,
        request: DeliveryRequest,
        reply: Option<ReplyWaiter>,
        authorization: SendAuthorization,
        authorize_send: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
        authorize_delivery: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<SendAttempt> {
        let host = self.clone();
        tokio::spawn(async move {
            let sender = host.flow(&lease.flow).await?;
            let record = sender
                .queue_send_authorized(lease, request, reply, authorization, authorize_send)
                .await?;
            Ok(attempt(&host, &sender, record, authorize_delivery).await)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Reconcile a recorded send after authenticating access to its sender; never synthesize an intent from retry parameters.
    pub async fn retry_send(
        &self,
        source: FlowRef,
        request_id: Uuid,
        authorize_delivery: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<SendAttempt> {
        let host = self.clone();
        tokio::spawn(async move {
            let sender = host.flow(&source).await?;
            let record = sender.send_record(request_id).await?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "send intent does not exist")
            })?;
            Ok(attempt(&host, &sender, record, authorize_delivery).await)
        })
        .await
        .map_err(io::Error::other)?
    }
}

async fn attempt(
    host: &RuntimeHost,
    sender: &FlowWorker,
    record: SendRecord,
    authorize_delivery: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
) -> SendAttempt {
    if !matches!(record.status, SendStatus::PendingDelivery) {
        return SendAttempt {
            record,
            pending_reason: None,
        };
    }
    let accepted = match host
        .deliver(record.intent.request.clone(), authorize_delivery)
        .await
    {
        Ok(accepted) => accepted,
        Err(error) => {
            if let Some(rejection) = error
                .get_ref()
                .and_then(|cause| cause.downcast_ref::<RecipientRejected>())
            {
                let failure = rejection.0.clone();
                let request_id = record.intent.request.request_id;
                return match sender
                    .request(move |store| store.fail_delivery(request_id, failure))
                    .await
                {
                    Ok(record) => SendAttempt {
                        record,
                        pending_reason: None,
                    },
                    Err(error) => SendAttempt {
                        record,
                        pending_reason: Some(error),
                    },
                };
            }
            return SendAttempt {
                record,
                pending_reason: Some(error),
            };
        }
    };
    let request_id = record.intent.request.request_id;
    let delivery = DeliveryReceipt::from(&accepted);
    match sender
        .request(move |store| store.confirm_delivery(request_id, delivery))
        .await
    {
        Ok(record) => SendAttempt {
            record,
            pending_reason: None,
        },
        Err(error) => SendAttempt {
            record,
            pending_reason: Some(error),
        },
    }
}
