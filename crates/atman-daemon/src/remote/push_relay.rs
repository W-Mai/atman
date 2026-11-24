//! Standalone notification relay. Its store has no Session or Flow identifiers.

mod store;

use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, HeaderName, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    serve::Listener,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::{sync::Semaphore, time};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    bearer,
    native_push::{ApnsConfig, FcmConfig, NativePush, Platform},
    no_store,
    notification_delivery::{Envelope, SendResult},
    tls::TlsListener,
};
use store::{Job, Store};

const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
const TTL: HeaderName = HeaderName::from_static("ttl");
const SEND_PREFIX: &str = "push-v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    certificate: PathBuf,
    private_key: PathBuf,
    state_directory: PathBuf,
    apns: Option<ApnsConfig>,
    fcm: Option<FcmConfig>,
}

struct Relay {
    store: Mutex<Store>,
    providers: NativePush,
    permits: Arc<Semaphore>,
}

#[derive(Debug)]
enum Error {
    Invalid,
    Unauthorized,
    Missing,
    Conflict,
    Retired,
    Busy,
    Storage,
}

impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}

impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Storage
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Invalid => (StatusCode::BAD_REQUEST, "invalid notification request"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "invalid notification credential"),
            Self::Missing => (
                StatusCode::NOT_FOUND,
                "notification registration unavailable",
            ),
            Self::Conflict => (StatusCode::CONFLICT, "notification revision changed"),
            Self::Retired => (StatusCode::GONE, "notification registration retired"),
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "notification capacity reached",
            ),
            Self::Storage => (
                StatusCode::SERVICE_UNAVAILABLE,
                "notification store unavailable",
            ),
        };
        (status, message).into_response()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Update {
    request_id: Uuid,
    expected_revision: Option<Uuid>,
    platform: Platform,
    device_token: Option<String>,
}

#[derive(Serialize)]
struct Registration {
    revision: Uuid,
    capability: Option<String>,
    expires_at_unix: i64,
}

/// Run the relay on its own TLS socket and storage directory.
pub async fn serve(config_path: &Path) -> Result<()> {
    let directory = config_path.parent().unwrap_or_else(|| Path::new("."));
    let config: Config =
        toml::from_str(&fs::read_to_string(config_path).context("read push relay configuration")?)
            .context("invalid push relay configuration")?;
    let providers = NativePush::load(config.apns, config.fcm, directory)?;
    ensure!(
        providers.enabled(),
        "configure at least one native push provider"
    );
    let path = directory.join(config.state_directory);
    let store = tokio::task::spawn_blocking(move || Store::open(&path)).await??;
    let listener = TlsListener::bind(
        config.listen,
        &directory.join(config.certificate),
        &directory.join(config.private_key),
    )
    .await?;
    println!(
        "[atman-push-relay] HTTPS listening on {}",
        listener.local_addr()?
    );
    let relay = Arc::new(Relay {
        store: Mutex::new(store),
        providers,
        permits: Arc::new(Semaphore::new(32)),
    });
    let shutdown = CancellationToken::new();
    let app = router(relay.clone());
    let worker = deliver(relay, shutdown.clone());
    let server = async {
        let result = crate::http::serve(listener, app, shutdown.clone()).await;
        shutdown.cancel();
        result
    };
    let signals = async {
        #[cfg(unix)]
        let terminate = async {
            if let Ok(mut signal) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                signal.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate => {}
        }
        shutdown.cancel();
    };
    let (result, (), ()) = tokio::join!(server, worker, signals);
    result.context("serve push relay")
}

fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route("/v1/registrations/{id}", get(registration).post(update))
        .route("/v1/notifications", post(publish))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn(no_store))
        .with_state(relay)
}

fn owner_secret(headers: &HeaderMap) -> Result<[u8; 32], Error> {
    decode_secret(bearer(headers).ok_or(Error::Unauthorized)?).ok_or(Error::Unauthorized)
}

fn decode_secret(value: &str) -> Option<[u8; 32]> {
    if value.len() != 43 {
        return None;
    }
    URL_SAFE_NO_PAD.decode(value).ok()?.try_into().ok()
}

async fn blocking<T: Send + 'static>(
    relay: Arc<Relay>,
    action: impl FnOnce(&mut Store) -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    let permit = relay
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut store = relay.store.lock().map_err(|_| Error::Storage)?;
        action(&mut store)
    })
    .await
    .map_err(|_| Error::Storage)?
}

async fn registration(
    State(relay): State<Arc<Relay>>,
    RoutePath(id): RoutePath<Uuid>,
    headers: HeaderMap,
) -> Result<Json<Registration>, Error> {
    let owner = owner_secret(&headers)?;
    blocking(relay, move |store| {
        store
            .registration(id, &owner, Utc::now().timestamp())
            .map(Json)
    })
    .await
}

async fn update(
    State(relay): State<Arc<Relay>>,
    RoutePath(id): RoutePath<Uuid>,
    headers: HeaderMap,
    Json(update): Json<Update>,
) -> Result<Json<Registration>, Error> {
    let owner = owner_secret(&headers)?;
    let channel = relay
        .providers
        .channel(update.platform)
        .ok_or(Error::Missing)?
        .to_owned();
    if id.is_nil()
        || update.request_id.is_nil()
        || update
            .device_token
            .as_ref()
            .is_some_and(|token| !update.platform.valid_token(token))
    {
        return Err(Error::Invalid);
    }
    blocking(relay, move |store| {
        store
            .update(id, &owner, update, channel, Utc::now().timestamp())
            .map(Json)
    })
    .await
}

async fn publish(
    State(relay): State<Arc<Relay>>,
    headers: HeaderMap,
    Json(notice): Json<Envelope>,
) -> Result<StatusCode, Error> {
    let value = bearer(&headers).ok_or(Error::Unauthorized)?;
    let (id, value) = value
        .strip_prefix("push-v1.")
        .and_then(|value| value.split_once('.'))
        .ok_or(Error::Unauthorized)?;
    let id = Uuid::parse_str(id).map_err(|_| Error::Unauthorized)?;
    let sending = decode_secret(value).ok_or(Error::Unauthorized)?;
    if id.is_nil()
        || notice.notification_id.is_nil()
        || super::single_header(&headers, IDEMPOTENCY_KEY)
            .and_then(|header| header.to_str().ok())
            .and_then(|value| Uuid::parse_str(value).ok())
            != Some(notice.notification_id)
    {
        return Err(Error::Invalid);
    }
    let ttl = if headers.contains_key(&TTL) {
        super::single_header(&headers, TTL)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|seconds| *seconds <= 86_400)
            .ok_or(Error::Invalid)?
    } else {
        86_400
    };
    blocking(relay.clone(), move |store| {
        store.publish(
            id,
            &sending,
            notice,
            Utc::now().timestamp(),
            i64::from(ttl),
            |subscription| {
                relay.providers.channel(subscription.platform)
                    == Some(subscription.channel.as_str())
            },
        )
    })
    .await?;
    Ok(StatusCode::ACCEPTED)
}

async fn deliver(relay: Arc<Relay>, shutdown: CancellationToken) {
    let mut interval = time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let work = async {
        loop {
            interval.tick().await;
            let now = Utc::now().timestamp();
            let Ok(due) = blocking(relay.clone(), move |store| store.pending(now)).await else {
                continue;
            };
            stream::iter(due)
                .for_each_concurrent(4, |job| send(&relay, job))
                .await;
        }
    };
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {}
        () = work => {}
    }
}

async fn send(relay: &Arc<Relay>, job: Job) {
    let id = job.id;
    let epoch = job.epoch;
    let Ok(subscribed) = blocking(relay.clone(), move |store| {
        store.admit(id, epoch, Utc::now().timestamp())
    })
    .await
    else {
        return;
    };
    let (result, revision) = if let Some(grant) = subscribed {
        let seconds = job
            .expires
            .min(grant.expires)
            .saturating_sub(Utc::now().timestamp());
        let Ok(seconds) = u64::try_from(seconds).map(Duration::from_secs) else {
            return;
        };
        let result = tokio::select! {
            biased;
            _ = grant.changed.cancelled() => SendResult::Retry,
            _ = time::sleep(seconds) => return,
            result = relay.providers.send(&grant.subscription, &job.notice, seconds) => result,
        };
        (result, Some(grant.revision))
    } else {
        (SendResult::InvalidSubscription, None)
    };
    let _ = blocking(relay.clone(), move |store| {
        store.finish(job, result, revision, Utc::now().timestamp())
    })
    .await;
}
