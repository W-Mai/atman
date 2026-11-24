//! Send queued notices through the selected channel, independently of Flow observation.

use std::{fs, io, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use atman_proto::{
    JsonRpcRequest,
    approvals::{FlowApprovalQueueResponse, ReadFlowApprovalQueueRequest},
    methods,
};
use atman_runtime::flow_store::{RuntimeStore, notification::FlowNotificationKind};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use futures::{StreamExt, stream};
use reqwest::{Certificate, Client, ClientBuilder, StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    DeviceError, Permission, RemoteState,
    native_push::{NativePush, Platform},
    notification_queue::{Attempt, NoticeKind, PendingDelivery, Queue},
    notification_subscription::{Channel, Config, Relay, Subscription},
    run,
    web_push::WebPushDelivery,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const IN_FLIGHT: usize = 4;

struct RelayDelivery {
    client: Client,
    endpoint: Url,
    origin: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    pub(super) notification_id: Uuid,
    pub(super) kind: NoticeKind,
}

#[derive(Debug)]
pub(super) enum SendResult {
    Accepted,
    InvalidSubscription,
    Retry,
}

#[derive(Default)]
pub(super) struct Channels {
    relay: Option<RelayDelivery>,
    web_push: Option<WebPushDelivery>,
    native: NativePush,
}

impl Channels {
    pub(super) fn load(directory: &std::path::Path) -> Result<Self> {
        let config = Config::load(directory)?;
        Ok(Self {
            relay: config.relay.map(RelayDelivery::new).transpose()?,
            web_push: config
                .web_push
                .map(|config| WebPushDelivery::new(config, directory))
                .transpose()?,
            native: NativePush::load(config.apns, config.fcm, directory)?,
        })
    }

    pub(super) fn enabled(&self) -> bool {
        self.relay.is_some() || self.web_push.is_some() || self.native.enabled()
    }

    pub(super) fn relay_origin(&self) -> Option<&str> {
        self.relay.as_ref().map(|relay| relay.origin.as_str())
    }

    pub(super) fn web_key(&self) -> Option<&str> {
        self.web_push.as_ref().map(WebPushDelivery::public_key)
    }

    pub(super) fn native_channel(&self, platform: Platform) -> Option<&str> {
        self.native.channel(platform)
    }

    pub(super) fn supports(&self, channel: Channel<'_>) -> bool {
        match channel {
            Channel::Relay(origin) => self.relay_origin() == Some(origin),
            Channel::WebPush(public_key) => self.web_key() == Some(public_key),
            Channel::Native(platform, key) => self.native_channel(platform) == Some(key),
        }
    }

    async fn send(
        &self,
        subscription: &Subscription,
        envelope: &Envelope,
        remaining: Duration,
    ) -> SendResult {
        match subscription {
            Subscription::Relay(subscription) => match &self.relay {
                Some(relay) => {
                    relay
                        .send(&subscription.capability, envelope, remaining)
                        .await
                }
                None => SendResult::InvalidSubscription,
            },
            Subscription::WebPush(subscription) => match &self.web_push {
                Some(push) => push.send(subscription, envelope, remaining).await,
                None => SendResult::InvalidSubscription,
            },
            Subscription::Native(subscription) => {
                self.native.send(subscription, envelope, remaining).await
            }
        }
    }
}

pub(super) fn https_client() -> ClientBuilder {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(3))
        .timeout(REQUEST_TIMEOUT)
}

pub(super) fn compact_jwt(
    header: impl Serialize,
    claims: impl Serialize,
    sign: impl FnOnce(&[u8]) -> Option<Vec<u8>>,
) -> Option<String> {
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).ok()?);
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).ok()?);
    let message = format!("{header}.{claims}");
    let signature = URL_SAFE_NO_PAD.encode(sign(message.as_bytes())?);
    Some(format!("{message}.{signature}"))
}

impl RelayDelivery {
    fn new(relay: Relay) -> Result<Self> {
        let endpoint = Url::parse(&relay.origin)?.join("/v1/notifications")?;
        let mut client = https_client();
        if let Some(path) = relay.certificate {
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
            origin: relay.origin,
        })
    }

    async fn send(&self, capability: &str, envelope: &Envelope, remaining: Duration) -> SendResult {
        let result = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(capability)
            .header("Idempotency-Key", envelope.notification_id.to_string())
            .header("TTL", remaining.as_secs().min(86_400).to_string())
            .json(envelope)
            .send()
            .await;
        match result.map(|response| response.status()) {
            Ok(status) if status.is_success() => SendResult::Accepted,
            Ok(StatusCode::GONE) => SendResult::InvalidSubscription,
            _ => SendResult::Retry,
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
    if !state.notifications.enabled() {
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
                    let attempt = send_pending(&state, &pending).await;
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

async fn send_pending(state: &Arc<RemoteState>, pending: &PendingDelivery) -> Attempt {
    let kind = NoticeKind::from(&pending.kind);
    let permission = match kind {
        NoticeKind::TurnCompleted => Permission::Read,
        NoticeKind::ApprovalRequested => Permission::Approve,
    };
    let flow = pending.flow.clone();
    let id = pending.device;
    let enrollment = pending.enrollment;
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
            &state.notifications,
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
                Ok(Some(false)) => return Err(Attempt::Obsolete),
                Ok(None) | Err(_) => return Err(Attempt::Retry),
            }
        }
        if registration.retired.is_cancelled() {
            return Err(Attempt::Retry);
        }
        let Ok(remaining) = (expires_at - Utc::now()).to_std() else {
            return Err(Attempt::Obsolete);
        };
        Ok(state
            .notifications
            .send(
                &registration.subscription,
                &Envelope {
                    notification_id: pending.id,
                    kind,
                },
                remaining,
            )
            .await)
    };
    let result = tokio::select! {
        biased;
        _ = registration.retired.cancelled() => return Attempt::Retry,
        _ = time::sleep(remaining) => return Attempt::Obsolete,
        result = admission => result,
    };
    match result {
        Ok(SendResult::Accepted) => Attempt::Accepted,
        Ok(SendResult::Retry) => Attempt::Retry,
        Ok(SendResult::InvalidSubscription) => {
            let revision = registration.revision;
            match run(state.clone(), move |state| {
                Ok(state
                    .devices
                    .retire_notification_subscription(id, revision)?)
            })
            .await
            {
                Ok(true) => Attempt::Obsolete,
                Ok(false) | Err(_) => Attempt::Retry,
            }
        }
        Err(attempt) => attempt,
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
