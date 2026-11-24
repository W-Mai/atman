use std::{io, path::Path, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, Method},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{DeviceError, RemoteState, authorized, device_request};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    relay: Relay,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Relay {
    pub origin: String,
}

impl Relay {
    pub fn load(directory: &Path) -> Result<Option<Self>> {
        let path = directory.join("notifications.toml");
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read notifications.toml"),
        };
        let Config { mut relay } = toml::from_str(&text).context("invalid notifications.toml")?;
        let origin =
            reqwest::Url::parse(&relay.origin).context("invalid notification relay origin")?;
        anyhow::ensure!(
            relay.origin.len() <= 320
                && origin.scheme() == "https"
                && origin.host().is_some()
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none(),
            "notification relay must be an HTTPS origin"
        );
        relay.origin = origin.origin().ascii_serialization();
        Ok(Some(relay))
    }
}

/// The relay capability can only be used at its configured issuer.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Subscription {
    pub relay_origin: String,
    pub capability: String,
}

impl Subscription {
    fn matches(&self, relay: &Relay) -> bool {
        self.relay_origin == relay.origin
            && (16..=512).contains(&self.capability.len())
            && self.capability.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
            })
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
    relay_origin: String,
}

impl Binding {
    pub(super) fn change(
        current: Option<&Self>,
        request: Update,
        relay: &Relay,
        now: DateTime<Utc>,
    ) -> Result<Option<Self>, DeviceError> {
        if request.request_id.is_nil()
            || request
                .subscription
                .as_ref()
                .is_some_and(|subscription| !subscription.matches(relay))
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
        let enrollment = request.subscription.as_ref().map(|_| {
            current
                .and_then(|current| current.active(relay).map(|(enrollment, _)| enrollment))
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

    pub(super) fn status(current: Option<&Self>, relay: &Relay) -> SubscriptionStatus {
        SubscriptionStatus {
            revision: current.map(|binding| binding.revision),
            enabled: current.and_then(|binding| binding.active(relay)).is_some(),
            relay_origin: relay.origin.clone(),
        }
    }

    pub(super) fn active(&self, relay: &Relay) -> Option<(Enrollment, &Subscription)> {
        let subscription = self
            .request
            .subscription
            .as_ref()
            .filter(|subscription| subscription.matches(relay))?;
        Some((self.enrollment?, subscription))
    }
}

pub(super) async fn status(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
) -> Result<Json<SubscriptionStatus>, DeviceError> {
    let relay = state
        .notification_relay
        .clone()
        .ok_or(DeviceError::Missing)?;
    let grant = authorized(state.clone(), &headers, &Method::GET, true).await?;
    device_request(state, move |state| {
        state
            .devices
            .notification_subscription(grant.authority(), &relay, Utc::now())
            .map(Json)
    })
    .await
}

pub(super) async fn update(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(request): Json<Update>,
) -> Result<Json<SubscriptionStatus>, DeviceError> {
    let relay = state
        .notification_relay
        .clone()
        .ok_or(DeviceError::Missing)?;
    let grant = authorized(state.clone(), &headers, &Method::POST, true).await?;
    device_request(state, move |state| {
        state
            .devices
            .update_notification_subscription(grant.authority(), request, &relay, Utc::now())
            .map(Json)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_revisions_preserve_the_enrollment_only_during_continuous_opt_in() {
        let now = Utc::now();
        let relay = Relay {
            origin: "https://relay.example".into(),
        };
        let change = |expected_revision, capability: Option<&str>| Update {
            request_id: Uuid::new_v4(),
            expected_revision,
            subscription: capability.map(|capability| Subscription {
                relay_origin: relay.origin.clone(),
                capability: capability.into(),
            }),
        };
        let first = Binding::change(
            None,
            change(None, Some("fixture-capability-1")),
            &relay,
            now,
        )
        .unwrap()
        .unwrap();
        let tomorrow = now + chrono::TimeDelta::days(1);
        let renewed = Binding::change(
            Some(&first),
            change(Some(first.revision), Some("fixture-capability-2")),
            &relay,
            tomorrow,
        )
        .unwrap()
        .unwrap();
        assert_ne!(first.revision, renewed.revision);
        assert_eq!(
            first.active(&relay).unwrap().0.id,
            renewed.active(&relay).unwrap().0.id
        );
        assert_eq!(renewed.active(&relay).unwrap().0.enabled_at, now);
        let disabled = Binding::change(
            Some(&renewed),
            change(Some(renewed.revision), None),
            &relay,
            tomorrow,
        )
        .unwrap()
        .unwrap();
        let reenabled = Binding::change(
            Some(&disabled),
            change(Some(disabled.revision), Some("fixture-capability-2")),
            &relay,
            tomorrow,
        )
        .unwrap()
        .unwrap();
        assert_ne!(
            first.active(&relay).unwrap().0.id,
            reenabled.active(&relay).unwrap().0.id
        );
        assert_eq!(reenabled.active(&relay).unwrap().0.enabled_at, tomorrow);
    }
}
