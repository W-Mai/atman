//! Owned delivery attempts from durable sender intent to receiver acknowledgement.

use std::io;

use atman_rt::TurnLease;
use uuid::Uuid;

use super::{FlowWorker, host::RuntimeHost};
use crate::event::AtmanUuid;
use crate::flow_store::FlowRef;
use crate::flow_store::inbox::DeliveryRequest;
use crate::flow_store::outbox::{DeliveryReceipt, SendRecord, SendStatus};
use crate::flow_store::waiter::ReplyWaiter;

/// A committed send and the outcome of its latest delivery attempt.
///
/// A pending record survives a failed attempt. Its recipient may already have accepted it when acknowledgement failed; retry the same request instead of creating another request identity.
#[derive(Debug)]
pub struct SendAttempt {
    pub record: SendRecord,
    pub pending_reason: Option<io::Error>,
}

impl FlowWorker {
    /// Authenticate the invoking flow before queueing; authorize only a new send.
    pub async fn queue_send(
        &self,
        lease: TurnLease<AtmanUuid>,
        request: DeliveryRequest,
        reply: Option<ReplyWaiter>,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<SendRecord> {
        self.request(move |store| store.queue_send(&lease, request, reply, authorize_new))
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
        let host = self.clone();
        tokio::spawn(async move {
            let sender = host.flow(&lease.flow).await?;
            let record = sender
                .queue_send(lease, request, reply, authorize_send)
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
    if matches!(record.status, SendStatus::Accepted { .. }) {
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
