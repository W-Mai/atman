use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use reqwest::{Client, RequestBuilder, StatusCode};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair},
};
use serde::Deserialize;
use serde_json::json;
use tokio::{sync::Mutex, time::Instant};
use tokio_rustls::rustls::pki_types::{PrivatePkcs8KeyDer, pem::PemObject};

use super::{message, response_json};
use crate::remote::notification_delivery::{Envelope, SendResult, compact_jwt, https_client};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::remote) struct ApnsConfig {
    environment: Environment,
    topic: String,
    team_id: String,
    key_id: String,
    key_file: PathBuf,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Environment {
    Production,
    Sandbox,
}

pub(super) struct Apns {
    client: Client,
    host: &'static str,
    channel: String,
    topic: String,
    team_id: String,
    key_id: String,
    key: EcdsaKeyPair,
    token: Mutex<Option<(String, Instant)>>,
}

#[derive(Deserialize)]
struct ApnsError {
    reason: String,
}

impl Apns {
    pub(super) fn new(config: ApnsConfig, directory: &Path) -> Result<Self> {
        let valid_id = |value: &str| {
            value.len() == 10
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        };
        ensure!(
            valid_id(&config.team_id)
                && valid_id(&config.key_id)
                && config.topic.len() <= 255
                && config.topic.contains('.')
                && config.topic.split('.').all(|part| !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| { byte.is_ascii_alphanumeric() || byte == b'-' })),
            "invalid APNs team, key ID, or App topic",
        );
        let key_file = PrivatePkcs8KeyDer::from_pem_file(directory.join(config.key_file))
            .context("read APNs signing key")?;
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            key_file.secret_pkcs8_der(),
            &SystemRandom::new(),
        )
        .map_err(|_| anyhow::anyhow!("invalid APNs P-256 signing key"))?;
        let (environment, host) = match config.environment {
            Environment::Production => ("production", "https://api.push.apple.com"),
            Environment::Sandbox => ("sandbox", "https://api.sandbox.push.apple.com"),
        };
        Ok(Self {
            client: https_client()
                .http2_prior_knowledge()
                .build()
                .context("build APNs HTTPS client")?,
            host,
            channel: format!("apns:{environment}:{}", config.topic),
            topic: config.topic,
            team_id: config.team_id,
            key_id: config.key_id,
            key,
            token: Mutex::new(None),
        })
    }

    pub(super) fn channel(&self) -> &str {
        &self.channel
    }

    async fn bearer(&self) -> Option<String> {
        let mut cache = self.token.lock().await;
        let now = Instant::now();
        if let Some((token, valid_until)) = &*cache
            && *valid_until > now
        {
            return Some(token.clone());
        }
        let token = compact_jwt(
            json!({ "alg": "ES256", "kid": self.key_id }),
            json!({ "iss": self.team_id, "iat": Utc::now().timestamp() }),
            |body| {
                Some(
                    self.key
                        .sign(&SystemRandom::new(), body)
                        .ok()?
                        .as_ref()
                        .to_vec(),
                )
            },
        )?;
        *cache = Some((token.clone(), now + Duration::from_secs(45 * 60)));
        Some(token)
    }

    fn request(
        &self,
        device: &str,
        notice: &Envelope,
        remaining: Duration,
        bearer: &str,
    ) -> RequestBuilder {
        let id = notice.notification_id.to_string();
        let expiration = Utc::now()
            .timestamp()
            .saturating_add(i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX));
        self.client
            .post(format!("{}/3/device/{device}", self.host))
            .bearer_auth(bearer)
            .header("apns-topic", &self.topic)
            .header("apns-push-type", "alert")
            .header("apns-id", &id)
            .header("apns-collapse-id", &id)
            .header("apns-expiration", expiration.to_string())
            .json(&json!({
                "aps": { "alert": { "title": "atman", "body": message(notice) } },
                "notification_id": id,
            }))
    }

    pub(super) async fn send(
        &self,
        device: &str,
        notice: &Envelope,
        remaining: Duration,
    ) -> SendResult {
        let Some(bearer) = self.bearer().await else {
            return SendResult::Retry;
        };
        let Ok(response) = self
            .request(device, notice, remaining, &bearer)
            .send()
            .await
        else {
            return SendResult::Retry;
        };
        let status = response.status();
        if status.is_success() {
            return SendResult::Accepted;
        }
        if status == StatusCode::GONE {
            return SendResult::InvalidSubscription;
        }
        let reason = response_json::<ApnsError>(response)
            .await
            .map(|error| error.reason);
        if status == StatusCode::BAD_REQUEST && reason.as_deref() == Some("BadDeviceToken") {
            return SendResult::InvalidSubscription;
        }
        if status == StatusCode::FORBIDDEN && reason.as_deref() == Some("ExpiredProviderToken") {
            let mut cached = self.token.lock().await;
            if cached.as_ref().is_some_and(|(token, _)| token == &bearer) {
                *cached = None;
            }
        }
        SendResult::Retry
    }
}
