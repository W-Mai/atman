//! Scoped device access on the standalone daemon's separate HTTPS listener.

mod browser;
pub(crate) mod device_rpc;
mod devices;
mod native_push;
mod notification_delivery;
mod notification_observer;
mod notification_queue;
pub(crate) mod notification_source;
mod notification_subscription;
mod tls;
mod web_push;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use atman_proto::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    conversation::ReadConversationRequest,
    flows::{ListFlowsRequest, ReadFlowEventsRequest},
    methods::Method,
};
use atman_runtime::{flow_runtime::catalog::SessionCatalog, flow_store::RuntimeStore};
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Extension, Path as RoutePath, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method as HttpMethod, StatusCode, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
};
use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    DaemonState,
    http::{ListenerShutdown, SseQuery, flow_events},
};
use devices::{
    DeviceError, DeviceGrant, DeviceView, IssuedCredentials, Permission, Registry, Scope,
};

const MAX_DEVICE_REQUESTS: usize = 16;
const MAX_DEVICE_OBSERVATIONS: usize = 32;
const MAX_DEVICE_RPC_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteConfig {
    listen: SocketAddr,
    origin: String,
    certificate: PathBuf,
    private_key: PathBuf,
}

pub struct RemoteAccess {
    listener: tls::TlsListener,
    state: Arc<RemoteState>,
}

struct RemoteState {
    daemon: Arc<DaemonState>,
    origin: String,
    devices: Registry,
    requests: Arc<Semaphore>,
    observations: Arc<Semaphore>,
    notification_queue: Option<notification_queue::Queue>,
    notifications: notification_delivery::Channels,
}

#[derive(Serialize)]
struct SharedSession {
    session_id: Uuid,
    title: Option<String>,
    project: Option<String>,
    default_flow: Option<Uuid>,
}

impl RemoteAccess {
    /// Start a remote listener only for an explicitly configured HTTPS origin.
    pub async fn prepare(daemon: Arc<DaemonState>, config_path: &Path) -> Result<Option<Self>> {
        let directory = config_path.parent().unwrap_or_else(|| Path::new("."));
        let config = match std::fs::read_to_string(directory.join("remote.toml")) {
            Ok(text) => toml::from_str::<RemoteConfig>(&text).context("invalid remote.toml")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read remote.toml"),
        };
        let origin = reqwest::Url::parse(&config.origin).context("invalid remote HTTPS origin")?;
        anyhow::ensure!(
            origin.scheme() == "https"
                && origin.host().is_some()
                && origin.path() == "/"
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.query().is_none()
                && origin.fragment().is_none(),
            "remote origin must be an HTTPS origin"
        );
        let listener = tls::TlsListener::bind(
            config.listen,
            &directory.join(config.certificate),
            &directory.join(config.private_key),
        )
        .await?;
        let data = daemon.data_dir().to_owned();
        let notifications = notification_delivery::Channels::load(directory)?;
        let enabled = notifications.enabled();
        let (devices, notification_queue) = tokio::task::spawn_blocking(move || {
            let devices = Registry::open(&data)?;
            let queue = enabled
                .then(|| notification_queue::Queue::open(devices.directory()))
                .transpose()?;
            Ok::<_, std::io::Error>((devices, queue))
        })
        .await??;
        let origin = origin.origin().ascii_serialization();
        Ok(Some(Self {
            listener,
            state: Arc::new(RemoteState {
                daemon,
                origin,
                devices,
                requests: Arc::new(Semaphore::new(MAX_DEVICE_REQUESTS)),
                observations: Arc::new(Semaphore::new(MAX_DEVICE_OBSERVATIONS)),
                notification_queue,
                notifications,
            }),
        }))
    }

    /// Device management uses the existing daemon's loopback operator credential.
    pub fn operator_routes(&self, auth_token: String) -> Router {
        Router::new()
            .route("/devices", get(list))
            .route("/devices/invite", post(invite))
            .route("/devices/{id}", delete(revoke))
            .route_layer(middleware::from_fn_with_state(auth_token, require_operator))
            .layer(DefaultBodyLimit::max(8192))
            .layer(middleware::from_fn(no_store))
            .with_state(self.state.clone())
    }

    pub async fn serve(self, shutdown: CancellationToken) -> std::io::Result<()> {
        let observer_shutdown = shutdown.child_token();
        let observer =
            notification_observer::observe(self.state.clone(), observer_shutdown.clone());
        let delivery =
            notification_delivery::deliver(self.state.clone(), observer_shutdown.clone());
        let app = Router::new()
            .route("/devices/pair", post(pair))
            .route("/devices/refresh", post(refresh))
            .route("/devices/me", get(me))
            .route("/devices/session", get(session))
            .route("/devices/browser/pair", post(browser::pair))
            .route("/devices/browser/refresh", post(browser::refresh))
            .route("/devices/browser/me", get(browser::me))
            .route("/devices/browser/session", get(browser::session))
            .route("/devices/browser/sign-out", post(browser::sign_out))
            .route(
                "/devices/notifications/subscription",
                get(notification_subscription::status).post(notification_subscription::update),
            )
            .route(
                "/devices/notifications/web-push",
                get(notification_subscription::web_status)
                    .post(notification_subscription::web_update),
            )
            .route(
                "/devices/notifications/native/{platform}",
                get(notification_subscription::native_status)
                    .post(notification_subscription::native_update),
            )
            .route(
                "/devices/notifications/{id}",
                get(notification_observer::destination),
            )
            .route(
                "/rpc",
                post(rpc).layer(DefaultBodyLimit::max(MAX_DEVICE_RPC_BYTES)),
            )
            .route("/events", get(events))
            .merge(crate::web::paired_router())
            .layer(DefaultBodyLimit::max(8192))
            .layer(middleware::from_fn_with_state(
                self.state.clone(),
                origin_boundary,
            ))
            .layer(middleware::from_fn(no_store))
            .with_state(self.state);
        let serve = async {
            let result = crate::http::serve(self.listener, app, shutdown).await;
            observer_shutdown.cancel();
            result
        };
        let (result, (), ()) = tokio::join!(serve, observer, delivery);
        result
    }

    pub fn origin(&self) -> &str {
        &self.state.origin
    }
}

async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    single_header(headers, header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn single_header(headers: &HeaderMap, name: axum::http::HeaderName) -> Option<&HeaderValue> {
    let values = headers.get_all(name);
    let mut values = values.iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

async fn require_operator(
    State(expected): State<String>,
    request: Request,
    next: Next,
) -> Response {
    if !expected.is_empty()
        && bearer(request.headers()).is_some_and(|token| {
            crate::http::constant_time_eq(expected.as_bytes(), token.as_bytes())
        })
    {
        next.run(request).await
    } else {
        DeviceError::Unauthorized.into_response()
    }
}

async fn origin_boundary(
    State(state): State<Arc<RemoteState>>,
    request: Request,
    next: Next,
) -> Response {
    let host = single_header(request.headers(), header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<Authority>().ok());
    let same_host = host
        .and_then(|authority| reqwest::Url::parse(&format!("https://{authority}")).ok())
        .is_some_and(|url| {
            url.username().is_empty()
                && url.password().is_none()
                && url.origin().ascii_serialization() == state.origin
        });
    let same_origin = !request.headers().contains_key(header::ORIGIN)
        || single_header(request.headers(), header::ORIGIN)
            .is_some_and(|origin| origin.to_str().ok() == Some(&state.origin));
    if !same_host || !same_origin {
        return (StatusCode::FORBIDDEN, "remote origin mismatch").into_response();
    }
    next.run(request).await
}

async fn run<T: Send + 'static>(
    state: Arc<RemoteState>,
    action: impl FnOnce(&RemoteState) -> Result<T, DeviceError> + Send + 'static,
) -> Result<T, DeviceError> {
    tokio::task::spawn_blocking(move || action(&state))
        .await
        .map_err(std::io::Error::other)?
}

async fn device_request<T: Send + 'static>(
    state: Arc<RemoteState>,
    action: impl FnOnce(&RemoteState) -> Result<T, DeviceError> + Send + 'static,
) -> Result<T, DeviceError> {
    let permit = state
        .requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| DeviceError::Capacity)?;
    run(state, move |state| {
        let _permit = permit;
        action(state)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InviteRequest {
    label: String,
    scope: Scope,
}

#[derive(Serialize)]
struct InviteResponse {
    url: String,
    expires_at: chrono::DateTime<Utc>,
}

async fn invite(
    State(state): State<Arc<RemoteState>>,
    Json(request): Json<InviteRequest>,
) -> Result<Json<InviteResponse>, DeviceError> {
    run(state, move |state| {
        if request.scope.session_id.is_nil() {
            return Err(DeviceError::Invalid);
        }
        RuntimeStore::inspect_session(state.daemon.data_dir(), request.scope.session_id).map_err(
            |error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    DeviceError::Missing
                } else {
                    DeviceError::Storage(error)
                }
            },
        )?;
        let invite = state
            .devices
            .invite(request.label, request.scope, Utc::now())?;
        Ok(Json(InviteResponse {
            url: format!("{}/#pair={}", state.origin, invite.code),
            expires_at: invite.expires_at,
        }))
    })
    .await
}

async fn list(State(state): State<Arc<RemoteState>>) -> Result<Json<Vec<DeviceView>>, DeviceError> {
    run(state, |state| Ok(Json(state.devices.list(Utc::now())))).await
}

async fn revoke(
    State(state): State<Arc<RemoteState>>,
    RoutePath(id): RoutePath<Uuid>,
) -> Result<Json<DeviceView>, DeviceError> {
    run(state, move |state| {
        state.devices.revoke(id, Utc::now()).map(Json)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairRequest {
    code: String,
}

async fn pair(
    State(state): State<Arc<RemoteState>>,
    Json(request): Json<PairRequest>,
) -> Result<Json<IssuedCredentials>, DeviceError> {
    device_request(state, move |state| {
        state.devices.pair(&request.code, Utc::now()).map(Json)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRequest {
    request_id: Uuid,
}

async fn refresh(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(request): Json<RefreshRequest>,
) -> Result<Json<IssuedCredentials>, DeviceError> {
    let token = bearer(&headers)
        .ok_or(DeviceError::Unauthorized)?
        .to_owned();
    device_request(state, move |state| {
        state
            .devices
            .refresh(&token, request.request_id, Utc::now())
            .map(Json)
    })
    .await
}

async fn authorized(
    state: Arc<RemoteState>,
    headers: &HeaderMap,
    method: &HttpMethod,
    require_device_id: bool,
) -> Result<DeviceGrant, DeviceError> {
    if headers.contains_key(header::AUTHORIZATION) {
        return authorize_token(state, bearer(headers).ok_or(DeviceError::Unauthorized)?).await;
    }
    let token = browser::access_token(headers, &state.origin, method)?;
    let grant = authorize_token(state, token).await?;
    browser::check_device(headers, grant.device.id, require_device_id)?;
    Ok(grant)
}

async fn authorize_token(state: Arc<RemoteState>, token: &str) -> Result<DeviceGrant, DeviceError> {
    let token = token.to_owned();
    device_request(state, move |state| {
        state.devices.authorize(&token, Utc::now())
    })
    .await
}

async fn me(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
) -> Result<Json<DeviceView>, DeviceError> {
    let token = bearer(&headers).ok_or(DeviceError::Unauthorized)?;
    let grant = authorize_token(state, token).await?;
    Ok(Json(grant.device))
}

async fn session(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
) -> Result<Json<SharedSession>, DeviceError> {
    let token = bearer(&headers).ok_or(DeviceError::Unauthorized)?;
    let grant = authorize_token(state.clone(), token).await?;
    shared_session(&state, grant).await
}

async fn shared_session(
    state: &RemoteState,
    grant: DeviceGrant,
) -> Result<Json<SharedSession>, DeviceError> {
    let id = grant.device.scope.session_id;
    let catalog = SessionCatalog::new(
        Some(state.daemon.runtime_host()),
        Some(state.daemon.data_dir().to_owned()),
    );
    let summary = tokio::select! {
        biased;
        _ = grant.retired() => return Err(DeviceError::Unauthorized),
        summary = catalog.overview(id) => summary.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound { DeviceError::Missing } else { error.into() }
        })?.summary,
    };
    Ok(Json(SharedSession {
        session_id: id,
        title: summary.title,
        project: summary.project_root.and_then(|root| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        }),
        default_flow: summary.default_flow.map(|id| id.0.0),
    }))
}

fn permits(permission: Permission, method: Method) -> bool {
    matches!(
        method,
        Method::Capabilities | Method::SessionFlows | Method::FlowConversation | Method::FlowEvents
    ) || device_rpc::required_permission(method).is_some_and(|required| permission.allows(required))
}

async fn rpc(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Result<Json<JsonRpcResponse>, DeviceError> {
    let method = Method::from_name(&request.method);
    let bound = method.and_then(device_rpc::required_permission).is_some();
    let grant = authorized(state.clone(), &headers, &HttpMethod::POST, bound).await?;
    let id = request.id.clone();
    let reject = || {
        JsonRpcResponse::err(
            id.clone(),
            JsonRpcError::application("method or target is outside the device scope"),
        )
    };
    let allow = |method| permits(grant.device.scope.permission, method);
    let Some(method) = method.filter(|method| allow(*method)) else {
        return Ok(Json(reject()));
    };
    if request.jsonrpc != atman_proto::JSONRPC_VERSION {
        return Ok(Json(JsonRpcResponse::err(
            id,
            JsonRpcError::invalid_params("expected JSON-RPC 2.0"),
        )));
    }
    let params = request
        .params
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    match method {
        Method::Capabilities => {
            let response = crate::capabilities::query_scoped(&state.daemon, params, allow)
                .and_then(|response| {
                    serde_json::to_value(response)
                        .map_err(|_| JsonRpcError::internal("encode capabilities"))
                });
            return Ok(Json(match response {
                Ok(result) => JsonRpcResponse::ok(id, result),
                Err(error) => JsonRpcResponse::err(id, error),
            }));
        }
        Method::SessionFlows => {
            let target = serde_json::from_value::<ListFlowsRequest>(params).ok();
            if target.is_none_or(|target| target.session_id.0 != grant.device.scope.session_id) {
                return Ok(Json(reject()));
            }
        }
        Method::FlowConversation => {
            let target = serde_json::from_value::<ReadConversationRequest>(params).ok();
            if target.is_none_or(|target| {
                target.flow.session_id.0 != grant.device.scope.session_id
                    || target
                        .before
                        .is_some_and(|before| before.source.flow != target.flow)
            }) {
                return Ok(Json(reject()));
            }
        }
        Method::FlowEvents => {
            let target = serde_json::from_value::<ReadFlowEventsRequest>(params).ok();
            if target.is_none_or(|target| target.flow.session_id.0 != grant.device.scope.session_id)
            {
                return Ok(Json(reject()));
            }
            return tokio::select! {
                biased;
                _ = grant.retired() => Err(DeviceError::Unauthorized),
                response = flow_events::receipt_rpc(&state.daemon, request, false) => Ok(Json(response)),
            };
        }
        Method::FlowSubmit
        | Method::FlowInputStatus
        | Method::FlowApprovalQueue
        | Method::FlowApprovalApply
        | Method::FlowApprovalStatus => {
            let _permit = state
                .requests
                .try_acquire()
                .map_err(|_| DeviceError::Capacity)?;
            return Ok(Json(
                device_rpc::dispatch(state.daemon.clone(), grant.authority(), request).await,
            ));
        }
        _ => return Ok(Json(reject())),
    }
    tokio::select! {
        biased;
        _ = grant.retired() => Err(DeviceError::Unauthorized),
        response = crate::dispatch(state.daemon.clone(), request) => Ok(Json(response)),
    }
}

async fn events(
    State(state): State<Arc<RemoteState>>,
    Extension(shutdown): Extension<ListenerShutdown>,
    Query(query): Query<SseQuery>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let grant = authorized(state.clone(), &headers, &HttpMethod::GET, false)
        .await
        .map_err(IntoResponse::into_response)?;
    if query.session_id.0 != grant.device.scope.session_id
        || !permits(grant.device.scope.permission, Method::FlowEvents)
    {
        return Err((
            StatusCode::FORBIDDEN,
            "event target is outside the device scope",
        )
            .into_response());
    }
    // Device callers supply only the durable continuation. Host routing headers are private to the gateway.
    let mut forwarded = HeaderMap::new();
    let last_id = axum::http::HeaderName::from_static("last-event-id");
    if headers.contains_key(&last_id) {
        let last = single_header(&headers, last_id.clone())
            .ok_or_else(|| DeviceError::Invalid.into_response())?;
        forwarded.insert(last_id, last.clone());
    }
    let permit = state
        .observations
        .clone()
        .try_acquire_owned()
        .map_err(|_| DeviceError::Capacity.into_response())?;
    let retirement = grant.retirement();
    let response = tokio::select! {
        biased;
        _ = retirement.clone().wait() => return Err(DeviceError::Unauthorized.into_response()),
        response = flow_events::receipt_response(&state.daemon, query, &forwarded, shutdown.0) => {
            response.map_err(|(status, _)| {
                let message = if status == StatusCode::BAD_REQUEST {
                    "invalid Flow address or receipt"
                } else {
                    "Flow receipts are unavailable"
                };
                (status, message).into_response()
            })?
        }
    };
    let mut response = response.map(|body| {
        Body::from_stream(
            body.into_data_stream()
                .take_until(retirement.clone().wait())
                .map(move |chunk| {
                    let _permit = &permit;
                    chunk
                }),
        )
    });
    response.extensions_mut().insert(retirement);
    Ok(response)
}

impl IntoResponse for DeviceError {
    fn into_response(self) -> Response {
        let code = match self {
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Origin => StatusCode::FORBIDDEN,
            Self::DeviceChanged => StatusCode::CONFLICT,
            Self::SubscriptionChanged => StatusCode::CONFLICT,
            Self::Missing => StatusCode::NOT_FOUND,
            Self::Capacity => StatusCode::TOO_MANY_REQUESTS,
            Self::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
        };
        (code, self.to_string()).into_response()
    }
}
