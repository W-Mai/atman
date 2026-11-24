use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, Method, header},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    DeviceError, RemoteState, authorized, device_request,
    web_push::{PushSubscription, WebPushConfig},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Default)]
pub(super) struct Config {
    pub relay: Option<Relay>,
    pub web_push: Option<WebPushConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Relay {
    pub origin: String,
    pub certificate: Option<PathBuf>,
}

impl Config {
    pub(super) fn load(directory: &Path) -> Result<Self> {
        let path = directory.join("notifications.toml");
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).context("read notifications.toml"),
        };
        let mut config: Self = toml::from_str(&text).context("invalid notifications.toml")?;
        if let Some(relay) = &mut config.relay {
            relay.validate(directory)?;
        }
        Ok(config)
    }
}

impl Relay {
    fn validate(&mut self, directory: &Path) -> Result<()> {
        let origin =
            reqwest::Url::parse(&self.origin).context("invalid notification relay origin")?;
        anyhow::ensure!(
            self.origin.len() <= 320
                && origin.scheme() == "https"
                && origin.host().is_some()
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none(),
            "notification relay must be an HTTPS origin"
        );
        self.origin = origin.origin().ascii_serialization();
        self.certificate = self.certificate.take().map(|path| directory.join(path));
        Ok(())
    }
}

/// The relay capability can only be used at its configured issuer.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RelaySubscription {
    pub relay_origin: String,
    pub capability: String,
}

impl RelaySubscription {
    fn valid(&self) -> bool {
        (16..=512).contains(&self.capability.len())
            && self.capability.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
            })
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(super) enum Subscription {
    Relay(RelaySubscription),
    WebPush(PushSubscription),
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Channel<'a> {
    Relay(&'a str),
    WebPush(&'a str),
}

impl Subscription {
    pub(super) fn channel(&self) -> Channel<'_> {
        match self {
            Self::Relay(subscription) => Channel::Relay(&subscription.relay_origin),
            Self::WebPush(subscription) => Channel::WebPush(&subscription.application_server_key),
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Relay(subscription) => subscription.valid(),
            Self::WebPush(subscription) => subscription.valid(),
        }
    }
}

/// Subscription changes persist alongside the original pairing and rotate independently of access credentials.
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Binding {
    revision: Uuid,
    enrollment: Option<Enrollment>,
    request: Update,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) struct Enrollment {
    pub id: Uuid,
    pub enabled_at: DateTime<Utc>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Update {
    request_id: Uuid,
    expected_revision: Option<Uuid>,
    subscription: Option<Subscription>,
}

#[derive(Serialize)]
pub(super) struct SubscriptionStatus {
    revision: Option<Uuid>,
    enabled: bool,
}

impl Binding {
    pub(super) fn revision(&self) -> Uuid {
        self.revision
    }

    pub(super) fn retire(revision: Uuid) -> Self {
        Self {
            revision: Uuid::new_v4(),
            enrollment: None,
            request: Update {
                request_id: Uuid::new_v4(),
                expected_revision: Some(revision),
                subscription: None,
            },
        }
    }

    pub(super) fn change(
        current: Option<&Self>,
        request: Update,
        channel: Channel<'_>,
        now: DateTime<Utc>,
    ) -> Result<Option<Self>, DeviceError> {
        if request.request_id.is_nil()
            || request.subscription.as_ref().is_some_and(|subscription| {
                subscription.channel() != channel || !subscription.valid()
            })
        {
            return Err(DeviceError::Invalid);
        }
        if let Some(current) = current
            && current.request.request_id == request.request_id
        {
            return if current.request == request {
                Ok(None)
            } else {
                Err(DeviceError::SubscriptionChanged)
            };
        }
        if current.map(|binding| binding.revision) != request.expected_revision {
            return Err(DeviceError::SubscriptionChanged);
        }
        if request.subscription.is_none()
            && current
                .and_then(Self::active)
                .is_some_and(|(_, subscription)| !subscription.channel().same_transport(channel))
        {
            return Err(DeviceError::SubscriptionChanged);
        }
        let enrollment = request.subscription.as_ref().map(|_| {
            current
                .and_then(Self::active)
                .filter(|(_, subscription)| subscription.channel() == channel)
                .map(|(enrollment, _)| enrollment)
                .unwrap_or_else(|| Enrollment {
                    id: Uuid::new_v4(),
                    enabled_at: now,
                })
        });
        Ok(Some(Self {
            revision: Uuid::new_v4(),
            enrollment,
            request,
        }))
    }

    pub(super) fn status(current: Option<&Self>, channel: Channel<'_>) -> SubscriptionStatus {
        SubscriptionStatus {
            revision: current.map(|binding| binding.revision),
            enabled: current
                .and_then(Self::active)
                .is_some_and(|(_, subscription)| subscription.channel() == channel),
        }
    }

    pub(super) fn active(&self) -> Option<(Enrollment, &Subscription)> {
        Some((self.enrollment?, self.request.subscription.as_ref()?))
    }
}

impl Channel<'_> {
    fn same_transport(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}

#[derive(Serialize)]
pub(super) struct RelayStatus {
    #[serde(flatten)]
    status: SubscriptionStatus,
    relay_origin: String,
}

#[derive(Serialize)]
pub(super) struct WebPushStatus {
    #[serde(flatten)]
    status: SubscriptionStatus,
    application_server_key: String,
}

pub(super) async fn status(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
) -> Result<Json<RelayStatus>, DeviceError> {
    let relay_origin = state
        .notifications
        .relay_origin()
        .ok_or(DeviceError::Missing)?
        .to_owned();
    let grant = authorized(state.clone(), &headers, &Method::GET, true).await?;
    device_request(state, move |state| {
        let status = state.devices.notification_subscription(
            grant.authority(),
            Channel::Relay(&relay_origin),
            Utc::now(),
        )?;
        Ok(Json(RelayStatus {
            status,
            relay_origin,
        }))
    })
    .await
}

pub(super) async fn update(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(request): Json<Update>,
) -> Result<Json<RelayStatus>, DeviceError> {
    let relay_origin = state
        .notifications
        .relay_origin()
        .ok_or(DeviceError::Missing)?
        .to_owned();
    let grant = authorized(state.clone(), &headers, &Method::POST, true).await?;
    device_request(state, move |state| {
        let status = state.devices.update_notification_subscription(
            grant.authority(),
            request,
            Channel::Relay(&relay_origin),
            Utc::now(),
        )?;
        Ok(Json(RelayStatus {
            status,
            relay_origin,
        }))
    })
    .await
}

pub(super) async fn web_status(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
) -> Result<Json<WebPushStatus>, DeviceError> {
    web_subscription(state, headers, None).await
}

pub(super) async fn web_update(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(request): Json<Update>,
) -> Result<Json<WebPushStatus>, DeviceError> {
    web_subscription(state, headers, Some(request)).await
}

async fn web_subscription(
    state: Arc<RemoteState>,
    headers: HeaderMap,
    request: Option<Update>,
) -> Result<Json<WebPushStatus>, DeviceError> {
    let application_server_key = state
        .notifications
        .web_key()
        .ok_or(DeviceError::Missing)?
        .to_owned();
    if headers.contains_key(header::AUTHORIZATION) {
        return Err(DeviceError::Unauthorized);
    }
    let method = if request.is_some() {
        Method::POST
    } else {
        Method::GET
    };
    let grant = authorized(state.clone(), &headers, &method, true).await?;
    device_request(state, move |state| {
        let channel = Channel::WebPush(&application_server_key);
        let authority = grant.authority();
        let now = Utc::now();
        let status = match request {
            Some(request) => state
                .devices
                .update_notification_subscription(authority, request, channel, now)?,
            None => state
                .devices
                .notification_subscription(authority, channel, now)?,
        };
        Ok(Json(WebPushStatus {
            status,
            application_server_key,
        }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_revisions_preserve_the_enrollment_only_during_continuous_opt_in() {
        let now = Utc::now();
        let origin = "https://relay.example";
        let channel = Channel::Relay(origin);
        let change = |expected_revision, capability: Option<&str>| Update {
            request_id: Uuid::new_v4(),
            expected_revision,
            subscription: capability.map(|capability| {
                Subscription::Relay(RelaySubscription {
                    relay_origin: origin.into(),
                    capability: capability.into(),
                })
            }),
        };
        let first = Binding::change(
            None,
            change(None, Some("fixture-capability-1")),
            channel,
            now,
        )
        .unwrap()
        .unwrap();
        let tomorrow = now + chrono::TimeDelta::days(1);
        let renewed = Binding::change(
            Some(&first),
            change(Some(first.revision), Some("fixture-capability-2")),
            channel,
            tomorrow,
        )
        .unwrap()
        .unwrap();
        assert_ne!(first.revision, renewed.revision);
        assert_eq!(first.active().unwrap().0.id, renewed.active().unwrap().0.id);
        assert_eq!(renewed.active().unwrap().0.enabled_at, now);
        let disabled = Binding::change(
            Some(&renewed),
            change(Some(renewed.revision), None),
            channel,
            tomorrow,
        )
        .unwrap()
        .unwrap();
        let reenabled = Binding::change(
            Some(&disabled),
            change(Some(disabled.revision), Some("fixture-capability-2")),
            channel,
            tomorrow,
        )
        .unwrap()
        .unwrap();
        assert_ne!(
            first.active().unwrap().0.id,
            reenabled.active().unwrap().0.id
        );
        assert_eq!(reenabled.active().unwrap().0.enabled_at, tomorrow);
    }
}
