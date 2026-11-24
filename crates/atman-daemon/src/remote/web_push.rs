//! Encrypted Web Push to the HTTPS endpoints used by paired browsers.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use reqwest::{Client, RequestBuilder, StatusCode, Url, header};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair},
};
use serde::{Deserialize, Serialize};
use web_push_native::{
    Auth, WebPushBuilder,
    p256::{PublicKey, SecretKey},
};

use super::notification_delivery::{Envelope, SendResult, https_client};

const MAX_TTL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WebPushConfig {
    key_file: PathBuf,
    contact: String,
}

pub(super) struct WebPushDelivery {
    client: Client,
    signing_key: EcdsaKeyPair,
    public_key: String,
    contact: String,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PushSubscription {
    endpoint: String,
    p256dh: String,
    auth: String,
    pub application_server_key: String,
}

impl PushSubscription {
    pub(super) fn valid(&self) -> bool {
        self.endpoint().is_some() && self.keys().is_some()
    }

    fn endpoint(&self) -> Option<Url> {
        if self.endpoint.len() > 1024 {
            return None;
        }
        let endpoint = Url::parse(&self.endpoint).ok()?;
        let path = endpoint.path();
        let supported_path = match endpoint.host_str()? {
            "fcm.googleapis.com" => path
                .strip_prefix("/fcm/send/")
                .or_else(|| path.strip_prefix("/wp/")),
            "updates.push.services.mozilla.com" => path.strip_prefix("/wpush/v2/"),
            "web.push.apple.com" => path.strip_prefix('/'),
            _ => None,
        };
        (endpoint.scheme() == "https"
            && endpoint.as_str() == self.endpoint
            && endpoint.port().is_none()
            && endpoint.username().is_empty()
            && endpoint.password().is_none()
            && endpoint.query().is_none()
            && endpoint.fragment().is_none()
            && supported_path.is_some_and(|suffix| {
                !suffix.is_empty()
                    && suffix.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
                    })
            }))
        .then_some(endpoint)
    }

    fn keys(&self) -> Option<(PublicKey, Auth)> {
        if self.p256dh.len() != 87 || self.auth.len() != 22 {
            return None;
        }
        let key = URL_SAFE_NO_PAD.decode(&self.p256dh).ok()?;
        if key.first() != Some(&4) {
            return None;
        }
        let auth: [u8; 16] = URL_SAFE_NO_PAD.decode(&self.auth).ok()?.try_into().ok()?;
        Some((PublicKey::from_sec1_bytes(&key).ok()?, Auth::from(auth)))
    }
}

impl WebPushDelivery {
    pub(super) fn new(config: WebPushConfig, directory: &Path) -> Result<Self> {
        let contact = Url::parse(&config.contact).context("invalid Web Push VAPID contact")?;
        ensure!(
            config.contact.len() <= 300
                && (contact.scheme() == "mailto" && !contact.path().is_empty()
                    || contact.scheme() == "https" && contact.host().is_some())
                && contact.username().is_empty()
                && contact.password().is_none()
                && contact.fragment().is_none()
                && contact.query().is_none(),
            "Web Push VAPID contact must be a mailto or HTTPS URL",
        );
        let key = fs::read_to_string(directory.join(config.key_file))
            .context("read Web Push VAPID key")?;
        let key = URL_SAFE_NO_PAD
            .decode(key.trim())
            .context("invalid Web Push VAPID key encoding")?;
        ensure!(key.len() == 32, "Web Push VAPID key must contain 32 bytes");
        let secret = SecretKey::from_slice(&key)
            .map_err(|_| anyhow::anyhow!("invalid Web Push VAPID key"))?;
        let public = secret.public_key().to_sec1_bytes();
        let signing_key = EcdsaKeyPair::from_private_key_and_public_key(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &key,
            &public,
            &SystemRandom::new(),
        )
        .map_err(|_| anyhow::anyhow!("invalid Web Push VAPID key pair"))?;
        let public_key = URL_SAFE_NO_PAD.encode(public);
        Ok(Self {
            client: https_client().build().context("build Web Push client")?,
            signing_key,
            public_key,
            contact: config.contact,
        })
    }

    pub(super) fn public_key(&self) -> &str {
        &self.public_key
    }

    fn authorization(&self, endpoint: &Url) -> Option<header::HeaderValue> {
        #[derive(Serialize)]
        struct Claims<'a> {
            aud: String,
            exp: i64,
            sub: &'a str,
        }
        let claims = Claims {
            aud: endpoint.origin().ascii_serialization(),
            exp: Utc::now().timestamp() + MAX_TTL.as_secs() as i64,
            sub: &self.contact,
        };
        let protected = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).ok()?);
        let message = format!("{protected}.{claims}");
        let signature = self
            .signing_key
            .sign(&SystemRandom::new(), message.as_bytes())
            .ok()?;
        let signature = URL_SAFE_NO_PAD.encode(signature.as_ref());
        let mut auth: header::HeaderValue =
            format!("vapid t={message}.{signature}, k={}", self.public_key)
                .parse()
                .ok()?;
        auth.set_sensitive(true);
        Some(auth)
    }

    fn request(
        &self,
        subscription: &PushSubscription,
        envelope: &Envelope,
        remaining: Duration,
    ) -> Result<RequestBuilder, SendResult> {
        if subscription.application_server_key != self.public_key {
            return Err(SendResult::InvalidSubscription);
        }
        let endpoint = subscription
            .endpoint()
            .ok_or(SendResult::InvalidSubscription)?;
        let uri = endpoint
            .as_str()
            .parse()
            .map_err(|_| SendResult::InvalidSubscription)?;
        let (key, auth) = subscription.keys().ok_or(SendResult::InvalidSubscription)?;
        let data = serde_json::to_vec(envelope).map_err(|_| SendResult::Retry)?;
        let wire = WebPushBuilder::new(uri, key, auth)
            .with_valid_duration(remaining.min(MAX_TTL))
            .build(data)
            .map_err(|_| SendResult::Retry)?;
        let (parts, body) = wire.into_parts();
        let authorization = self.authorization(&endpoint).ok_or(SendResult::Retry)?;
        Ok(self
            .client
            .post(endpoint)
            .headers(parts.headers)
            .header(header::AUTHORIZATION, authorization)
            .header(
                "Topic",
                URL_SAFE_NO_PAD.encode(envelope.notification_id.as_bytes()),
            )
            .body(body))
    }

    pub(super) async fn send(
        &self,
        subscription: &PushSubscription,
        envelope: &Envelope,
        remaining: Duration,
    ) -> SendResult {
        let request = match self.request(subscription, envelope, remaining) {
            Ok(request) => request,
            Err(error) => return error,
        };
        match request.send().await.map(|response| response.status()) {
            Ok(status) if status.is_success() => SendResult::Accepted,
            Ok(StatusCode::NOT_FOUND | StatusCode::GONE) => SendResult::InvalidSubscription,
            _ => SendResult::Retry,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::notification_queue::NoticeKind;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn provider_scope_and_encrypted_payload_share_the_notice_identity() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("private.key"),
            URL_SAFE_NO_PAD.encode([12; 32]),
        )
        .unwrap();
        let push = WebPushDelivery::new(
            WebPushConfig {
                key_file: "private.key".into(),
                contact: "mailto:notifications@example.com".into(),
            },
            directory.path(),
        )
        .unwrap();
        let browser = SecretKey::from_slice(&[34; 32]).unwrap();
        let mut subscription = PushSubscription {
            endpoint: "https://fcm.googleapis.com/fcm/send/opaque-browser-token".to_owned(),
            p256dh: URL_SAFE_NO_PAD.encode(browser.public_key().to_sec1_bytes()),
            auth: URL_SAFE_NO_PAD.encode([56; 16]),
            application_server_key: push.public_key().into(),
        };
        let envelope = Envelope {
            notification_id: Uuid::new_v4(),
            kind: NoticeKind::ApprovalRequested,
        };
        let request = push
            .request(&subscription, &envelope, Duration::from_secs(23))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.headers()["ttl"], "23");
        assert_eq!(request.headers()["content-encoding"], "aes128gcm");
        assert_eq!(
            request.headers()["topic"],
            URL_SAFE_NO_PAD.encode(envelope.notification_id.as_bytes())
        );
        let body = request.body().unwrap().as_bytes().unwrap();
        let plaintext =
            web_push_native::decrypt(body.to_vec(), &browser, &Auth::from([56; 16])).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&plaintext).unwrap(),
            json!({
                "notification_id": envelope.notification_id,
                "kind": "approval_requested",
            })
        );
        let authorization = &request.headers()[header::AUTHORIZATION];
        assert!(authorization.is_sensitive());
        let (jwt, key) = authorization
            .to_str()
            .unwrap()
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(key, push.public_key());
        let (message, signature) = jwt.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(
            &ECDSA_P256_SHA256_FIXED,
            URL_SAFE_NO_PAD.decode(key).unwrap(),
        )
        .verify(
            message.as_bytes(),
            &URL_SAFE_NO_PAD.decode(signature).unwrap(),
        )
        .unwrap();
        let (jwt_header, claims) = message.split_once('.').unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &URL_SAFE_NO_PAD.decode(jwt_header).unwrap()
            )
            .unwrap(),
            json!({"alg": "ES256", "typ": "JWT"})
        );
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "mailto:notifications@example.com");
        assert!(claims["exp"].as_i64().unwrap() > Utc::now().timestamp());

        for endpoint in [
            "https://fcm.googleapis.com.evil.example/fcm/send/token",
            "https://user@fcm.googleapis.com/fcm/send/token",
            "https://fcm.googleapis.com:8443/fcm/send/token",
            "https://fcm.googleapis.com/v1/projects",
            "https://localhost/fcm/send/token",
        ] {
            subscription.endpoint = endpoint.into();
            assert!(!subscription.valid());
            assert!(matches!(
                push.request(&subscription, &envelope, Duration::from_secs(23)),
                Err(SendResult::InvalidSubscription)
            ));
        }
    }
}
