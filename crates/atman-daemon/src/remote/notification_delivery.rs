//! Send queued notices to the configured HTTPS relay, independently of Flow observation.

use std::{fs, io, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use atman_proto::{
    JsonRpcRequest,
    approvals::{FlowApprovalQueueResponse, ReadFlowApprovalQueueRequest},
    methods,
};
use atman_runtime::flow_store::{RuntimeStore, notification::FlowNotificationKind};
use chrono::Utc;
use futures::{StreamExt, stream};
use reqwest::{Certificate, Client, StatusCode, Url};
use serde::Serialize;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    DeviceError, Permission, RemoteState,
    notification_queue::{Attempt, NoticeKind, PendingDelivery, Queue},
    notification_subscription::Relay,
    run,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const IN_FLIGHT: usize = 4;

pub(super) struct RelayDelivery {
    client: Client,
    endpoint: Url,
}

#[derive(Serialize)]
struct Envelope {
    notification_id: Uuid,
    kind: NoticeKind,
}

impl RelayDelivery {
    pub(super) fn new(relay: &Relay) -> Result<Self> {
        let endpoint = Url::parse(&relay.origin)?.join("/v1/notifications")?;
        let mut client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(3))
            .timeout(REQUEST_TIMEOUT);
        if let Some(path) = &relay.certificate {
            let pem = fs::read(path).context("read notification relay CA")?;
            let roots =
                Certificate::from_pem_bundle(&pem).context("parse notification relay CA")?;
            anyhow::ensure!(
                !roots.is_empty(),
                "notification relay CA contains no certificates"
            );
            for root in roots {
                client = client.add_root_certificate(root);
            }
        }
        Ok(Self {
            client: client.build().context("build notification relay client")?,
            endpoint,
        })
    }

    async fn send(&self, capability: &str, envelope: Envelope) -> Attempt {
        let result = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(capability)
            .header("Idempotency-Key", envelope.notification_id.to_string())
            .json(&envelope)
            .send()
            .await;
        match result.map(|response| response.status()) {
            Ok(status) if status.is_success() => Attempt::Accepted,
            Ok(StatusCode::GONE) => Attempt::Obsolete,
            _ => Attempt::Retry,
        }
    }
}

fn queue(state: &RemoteState) -> Result<&Queue, DeviceError> {
    state
        .notification_queue
        .as_ref()
        .ok_or(DeviceError::Missing)
}

pub(super) async fn deliver(state: Arc<RemoteState>, shutdown: CancellationToken) {
    let (Some(relay), Some(delivery)) = (&state.notification_relay, &state.notification_delivery)
    else {
        return;
    };
    let mut interval = time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut reported = false;
    loop {
        let batch = async {
            interval.tick().await;
            let pending = run(
                state.clone(),
                |state| Ok(queue(state)?.pending(Utc::now())?),
            )
            .await?;
            stream::iter(pending)
                .map(|pending| async {
                    let attempt = send_pending(&state, relay, delivery, &pending).await;
                    run(state.clone(), move |state| {
                        Ok(queue(state)?.complete(&pending, attempt, Utc::now())?)
                    })
                    .await
                })
                .buffer_unordered(IN_FLIGHT)
                .fold(Ok(()), |result, current| async { result.and(current) })
                .await
        };
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            result = batch => result,
        };
        if result.is_err() && !reported {
            eprintln!("[atman-daemon] notification delivery queue unavailable; will retry");
        }
        reported = result.is_err();
    }
}

async fn send_pending(
    state: &Arc<RemoteState>,
    relay: &Relay,
    delivery: &RelayDelivery,
    pending: &PendingDelivery,
) -> Attempt {
    let kind = NoticeKind::from(&pending.kind);
    let permission = match kind {
        NoticeKind::TurnCompleted => Permission::Read,
        NoticeKind::ApprovalRequested => Permission::Approve,
    };
    let flow = pending.flow.clone();
    let id = pending.device;
    let enrollment = pending.enrollment;
    let relay = relay.clone();
    let registered = run(state.clone(), move |state| {
        let session = flow.session_id.0;
        match RuntimeStore::inspect_session(state.daemon.data_dir(), session) {
            Ok(catalog) => {
                if !catalog
                    .projection
                    .flows
                    .iter()
                    .any(|entry| entry.id.0.0 == flow.instance_id)
                {
                    return Ok(None);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        Ok(state.devices.notification_delivery(
            &relay,
            id,
            session,
            enrollment,
            permission,
            Utc::now(),
        ))
    })
    .await;
    let registration = match registered {
        Ok(Some(registration)) => registration,
        Ok(None) => return Attempt::Obsolete,
        Err(_) => return Attempt::Retry,
    };
    let expires_at = registration.expires_at.min(pending.expires_at);
    let Ok(remaining) = (expires_at - Utc::now()).to_std() else {
        return Attempt::Obsolete;
    };
    let admission = async {
        if let FlowNotificationKind::ApprovalRequested {
            request_id,
            revision,
        } = &pending.kind
        {
            match time::timeout(
                REQUEST_TIMEOUT,
                approval_pending(state, pending, request_id.0, *revision),
            )
            .await
            {
                Ok(Some(true)) => {}
                Ok(Some(false)) => return Attempt::Obsolete,
                Ok(None) | Err(_) => return Attempt::Retry,
            }
        }
        if registration.retired.is_cancelled() {
            return Attempt::Retry;
        }
        if Utc::now() >= expires_at {
            return Attempt::Obsolete;
        }
        delivery
            .send(
                &registration.subscription.capability,
                Envelope {
                    notification_id: pending.id,
                    kind,
                },
            )
            .await
    };
    tokio::select! {
        biased;
        _ = registration.retired.cancelled() => Attempt::Retry,
        _ = time::sleep(remaining) => Attempt::Obsolete,
        attempt = admission => attempt,
    }
}

async fn approval_pending(
    state: &Arc<RemoteState>,
    pending: &PendingDelivery,
    request_id: Uuid,
    revision: u64,
) -> Option<bool> {
    let flow = pending.flow.clone();
    let params = serde_json::to_value(ReadFlowApprovalQueueRequest { flow: flow.clone() }).ok()?;
    let request = JsonRpcRequest::new(
        "notification-approval",
        methods::FLOW_APPROVAL_QUEUE,
        params,
    );
    let response = crate::dispatch(state.daemon.clone(), request).await;
    match serde_json::from_value(response.result?).ok()? {
        FlowApprovalQueueResponse::Ready {
            flow: source,
            requests,
            ..
        } if source == flow => Some(
            requests
                .iter()
                .any(|row| row.request_id == request_id && row.revision == revision),
        ),
        // The owner may have changed. A missing snapshot cannot authorize delivery.
        _ => None,
    }
}
