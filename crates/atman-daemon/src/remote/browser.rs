use std::{io, sync::Arc};

use axum::{
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, header},
    response::{IntoResponse, Json, Response},
};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    DeviceError, DeviceView, IssuedCredentials, PairRequest, RefreshRequest, RemoteState,
    authorize_token, device_request, single_header,
};

const DEVICE_COOKIE: &str = "__Host-atman-device";
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

struct BrowserCredentials<'a> {
    access: &'a str,
    refresh: &'a str,
}

#[derive(Serialize)]
pub(super) struct BrowserSession {
    device: DeviceView,
    access_expires_at: DateTime<Utc>,
}

fn require_origin(headers: &HeaderMap, origin: &str, method: &Method) -> Result<(), DeviceError> {
    let confirmed = single_header(headers, header::ORIGIN)
        .is_some_and(|value| value.as_bytes() == origin.as_bytes());
    let same_origin_read = method == Method::GET
        && !headers.contains_key(header::ORIGIN)
        && single_header(headers, SEC_FETCH_SITE).is_some_and(|value| value == "same-origin");
    if confirmed || same_origin_read {
        Ok(())
    } else {
        Err(DeviceError::Origin)
    }
}

fn credentials<'a>(
    headers: &'a HeaderMap,
    origin: &str,
    method: &Method,
) -> Result<BrowserCredentials<'a>, DeviceError> {
    let mut pairs = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|line| line.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter_map(|(name, value)| (name == DEVICE_COOKIE).then_some(value));
    let (access, refresh) = pairs
        .next()
        .and_then(|pair| pair.split_once('~'))
        .filter(|_| pairs.next().is_none())
        .ok_or(DeviceError::Unauthorized)?;
    require_origin(headers, origin, method)?;
    Ok(BrowserCredentials { access, refresh })
}

pub(super) fn access_token<'a>(
    headers: &'a HeaderMap,
    origin: &str,
    method: &Method,
) -> Result<&'a str, DeviceError> {
    Ok(credentials(headers, origin, method)?.access)
}

fn issued_response(issued: IssuedCredentials) -> Result<Response, DeviceError> {
    let lifetime = (issued.device.expires_at - Utc::now()).num_seconds().max(0);
    let mut cookie = HeaderValue::from_str(&format!(
        "{DEVICE_COOKIE}={}~{}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={lifetime}",
        issued.access_token, issued.refresh_token,
    ))
    .map_err(io::Error::other)?;
    cookie.set_sensitive(true);
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(BrowserSession {
            device: issued.device,
            access_expires_at: issued.access_expires_at,
        }),
    )
        .into_response())
}

pub(super) async fn pair(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(request): Json<PairRequest>,
) -> Result<Response, DeviceError> {
    require_origin(&headers, &state.origin, &Method::POST)?;
    let issued = device_request(state, move |state| {
        state.devices.pair(&request.code, Utc::now())
    })
    .await?;
    issued_response(issued)
}

pub(super) async fn refresh(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(request): Json<RefreshRequest>,
) -> Result<Response, DeviceError> {
    let current = credentials(&headers, &state.origin, &Method::POST)?;
    let access = current.access.to_owned();
    let refresh = current.refresh.to_owned();
    let issued = device_request(state, move |state| {
        state
            .devices
            .refresh_browser(&access, &refresh, request.request_id, Utc::now())
    })
    .await?;
    issued_response(issued)
}

pub(super) async fn me(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
) -> Result<Json<BrowserSession>, DeviceError> {
    let access = access_token(&headers, &state.origin, &Method::GET)?;
    let grant = authorize_token(state, access).await?;
    Ok(Json(BrowserSession {
        device: grant.device,
        access_expires_at: grant.access_expires_at,
    }))
}
