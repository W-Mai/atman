//! Native notification providers for direct delivery and the push relay.

mod apns;
mod fcm;

use std::{path::Path, time::Duration};

use anyhow::Result;
use reqwest::Response;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::notification_delivery::{Envelope, SendResult};

pub(super) use self::{apns::ApnsConfig, fcm::FcmConfig};

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Platform {
    Apns,
    Fcm,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Subscription {
    pub platform: Platform,
    pub channel: String,
    pub device_token: String,
}

impl Subscription {
    pub(super) fn valid(&self) -> bool {
        self.platform.valid_token(&self.device_token)
    }
}

impl Platform {
    pub(super) fn valid_token(self, token: &str) -> bool {
        match self {
            Self::Apns => {
                (32..=256).contains(&token.len())
                    && token.len() % 2 == 0
                    && token
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }
            Self::Fcm => {
                (16..=512).contains(&token.len())
                    && token.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-' | b'.')
                    })
            }
        }
    }
}

#[derive(Default)]
pub(super) struct NativePush {
    apns: Option<apns::Apns>,
    fcm: Option<fcm::Fcm>,
}

impl NativePush {
    pub(super) fn load(
        apns: Option<ApnsConfig>,
        fcm: Option<FcmConfig>,
        directory: &Path,
    ) -> Result<Self> {
        Ok(Self {
            apns: apns
                .map(|config| apns::Apns::new(config, directory))
                .transpose()?,
            fcm: fcm
                .map(|config| fcm::Fcm::new(config, directory))
                .transpose()?,
        })
    }

    pub(super) fn enabled(&self) -> bool {
        self.apns.is_some() || self.fcm.is_some()
    }

    pub(super) fn channel(&self, platform: Platform) -> Option<&str> {
        match platform {
            Platform::Apns => self.apns.as_ref().map(apns::Apns::channel),
            Platform::Fcm => self.fcm.as_ref().map(fcm::Fcm::channel),
        }
    }

    pub(super) async fn send(
        &self,
        subscription: &Subscription,
        notice: &Envelope,
        remaining: Duration,
    ) -> SendResult {
        if self.channel(subscription.platform) != Some(subscription.channel.as_str())
            || !subscription.valid()
        {
            return SendResult::InvalidSubscription;
        }
        match subscription.platform {
            Platform::Apns => {
                let Some(provider) = &self.apns else {
                    return SendResult::InvalidSubscription;
                };
                provider
                    .send(&subscription.device_token, notice, remaining)
                    .await
            }
            Platform::Fcm => {
                let Some(provider) = &self.fcm else {
                    return SendResult::InvalidSubscription;
                };
                provider
                    .send(&subscription.device_token, notice, remaining)
                    .await
            }
        }
    }
}

async fn response_json<T: DeserializeOwned>(mut response: Response) -> Option<T> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len().saturating_add(chunk.len()) > 8192 {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

fn message(notice: &Envelope) -> &'static str {
    use super::notification_queue::NoticeKind;
    match notice.kind {
        NoticeKind::TurnCompleted => "A Flow finished.",
        NoticeKind::ApprovalRequested => "A tool is waiting for approval.",
    }
}
