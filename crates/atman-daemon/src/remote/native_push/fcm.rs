use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use reqwest::{Client, RequestBuilder, StatusCode, Url, header};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::Mutex, time::Instant};
use tokio_rustls::rustls::pki_types::{PrivatePkcs8KeyDer, pem::PemObject};

use super::{message, response_json};
use crate::remote::notification_delivery::{Envelope, SendResult, compact_jwt, https_client};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const FCM_ERROR: &str = "type.googleapis.com/google.firebase.fcm.v1.FcmError";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::remote) struct FcmConfig {
    service_account_file: PathBuf,
}

#[derive(Deserialize)]
struct Account {
    #[serde(rename = "type")]
    kind: String,
    project_id: String,
    client_email: String,
    private_key_id: String,
    private_key: String,
    token_uri: String,
}

#[derive(Deserialize)]
struct AccessToken {
    access_token: String,
    token_type: String,
    expires_in: u64,
}

pub(super) struct Fcm {
    client: Client,
    endpoint: Url,
    channel: String,
    account: String,
    key_id: String,
    key: RsaKeyPair,
    token: Mutex<Option<(String, Instant)>>,
}

impl Fcm {
    pub(super) fn new(config: FcmConfig, directory: &Path) -> Result<Self> {
        let bytes = fs::read(directory.join(config.service_account_file))
            .context("read FCM service account")?;
        let account: Account =
            serde_json::from_slice(&bytes).context("parse FCM service account")?;
        ensure!(
            account.kind == "service_account"
                && account.token_uri == TOKEN_URL
                && (6..=30).contains(&account.project_id.len())
                && account
                    .project_id
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase)
                && account
                    .project_id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !account.project_id.ends_with('-')
                && account.client_email.len() <= 254
                && account.client_email.contains('@')
                && account
                    .client_email
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic())
                && (1..=128).contains(&account.private_key_id.len())
                && account
                    .private_key_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric()),
            "invalid FCM service account or token origin",
        );
        let private_key = PrivatePkcs8KeyDer::from_pem_slice(account.private_key.as_bytes())
            .context("decode FCM private key")?;
        let key = RsaKeyPair::from_pkcs8(private_key.secret_pkcs8_der())
            .map_err(|_| anyhow::anyhow!("invalid FCM RSA signing key"))?;
        let endpoint = Url::parse(&format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            account.project_id
        ))?;
        Ok(Self {
            client: https_client().build().context("build FCM HTTPS client")?,
            endpoint,
            channel: format!("fcm:{}", account.project_id),
            account: account.client_email,
            key_id: account.private_key_id,
            key,
            token: Mutex::new(None),
        })
    }

    pub(super) fn channel(&self) -> &str {
        &self.channel
    }

    fn assertion(&self, iat: i64) -> Option<String> {
        compact_jwt(
            json!({ "typ": "JWT", "alg": "RS256", "kid": self.key_id }),
            json!({ "iss": self.account, "scope": SCOPE, "aud": TOKEN_URL, "iat": iat, "exp": iat + 3600 }),
            |message| {
                let mut signature = vec![0; self.key.public().modulus_len()];
                self.key
                    .sign(
                        &RSA_PKCS1_SHA256,
                        &SystemRandom::new(),
                        message,
                        &mut signature,
                    )
                    .ok()?;
                Some(signature)
            },
        )
    }

    async fn bearer(&self) -> Option<String> {
        let mut cache = self.token.lock().await;
        let now = Instant::now();
        if let Some((token, valid_until)) = &*cache
            && *valid_until > now
        {
            return Some(token.clone());
        }
        let assertion = self.assertion(Utc::now().timestamp())?;
        let response = self
            .client
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ])
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let token = response_json::<AccessToken>(response).await?;
        if !token.token_type.eq_ignore_ascii_case("bearer")
            || token.expires_in == 0
            || token.access_token.is_empty()
            || token.access_token.len() > 4096
            || !token
                .access_token
                .bytes()
                .all(|byte| byte.is_ascii_graphic())
        {
            return None;
        }
        let valid_for = Duration::from_secs(token.expires_in.min(3600).saturating_sub(30));
        *cache = Some((token.access_token.clone(), now + valid_for));
        Some(token.access_token)
    }

    fn request(
        &self,
        device: &str,
        notice: &Envelope,
        remaining: Duration,
        bearer: &str,
    ) -> RequestBuilder {
        let id = notice.notification_id.to_string();
        self.client
            .post(self.endpoint.clone())
            .bearer_auth(bearer)
            .header(header::ACCEPT, "application/json")
            .json(&json!({
                "message": {
                    "token": device,
                    "notification": { "title": "atman", "body": message(notice) },
                    "data": { "notification_id": id },
                    "android": {
                        "priority": "HIGH",
                        "ttl": format!("{}s", remaining.as_secs().min(86_400)),
                        "notification": { "tag": id },
                    },
                },
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
        match response.status() {
            status if status.is_success() => SendResult::Accepted,
            StatusCode::UNAUTHORIZED => {
                let mut cached = self.token.lock().await;
                if cached.as_ref().is_some_and(|(token, _)| token == &bearer) {
                    *cached = None;
                }
                SendResult::Retry
            }
            StatusCode::NOT_FOUND if unregistered(response_json(response).await) => {
                SendResult::InvalidSubscription
            }
            _ => SendResult::Retry,
        }
    }
}

fn unregistered(body: Option<Value>) -> bool {
    body.as_ref()
        .and_then(|value| value["error"]["details"].as_array())
        .is_some_and(|details| {
            details
                .iter()
                .any(|detail| detail["@type"] == FCM_ERROR && detail["errorCode"] == "UNREGISTERED")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_fcm_token_retirement_invalidates_a_subscription() {
        assert!(!unregistered(Some(
            json!({ "error": { "status": "NOT_FOUND" } })
        )));
        assert!(!unregistered(Some(json!({ "error": { "details": [
            { "@type": "other", "errorCode": "UNREGISTERED" },
            { "@type": FCM_ERROR, "errorCode": "SENDER_ID_MISMATCH" }
        ] } }))));
        assert!(unregistered(Some(json!({ "error": { "details": [
            { "@type": FCM_ERROR, "errorCode": "UNREGISTERED" }
        ] } }))));
    }
}
