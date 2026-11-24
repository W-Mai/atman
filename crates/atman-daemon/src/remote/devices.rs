use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Mutex,
};

use atman_runtime::platform::{ExclusiveFileLease, atomic_replace};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    notification_delivery::Channels,
    notification_subscription::{
        Binding, Channel, Enrollment, Subscription, SubscriptionStatus, Update,
    },
};

const INVITE_LIFETIME: TimeDelta = TimeDelta::minutes(5);
const ACCESS_LIFETIME: TimeDelta = TimeDelta::minutes(15);
const DEVICE_LIFETIME: TimeDelta = TimeDelta::days(30);
const MAX_INVITES: usize = 32;
const MAX_DEVICES: usize = 128;
const MAX_STORE_BYTES: u64 = 256 * 1024;

type Digest = [u8; 32];

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Permission {
    Read,
    Send,
    Approve,
    SendAndApprove,
}

impl Permission {
    pub(super) fn allows(self, required: Self) -> bool {
        match required {
            Self::Read => true,
            Self::Send => matches!(self, Self::Send | Self::SendAndApprove),
            Self::Approve => matches!(self, Self::Approve | Self::SendAndApprove),
            Self::SendAndApprove => self == Self::SendAndApprove,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    pub session_id: Uuid,
    pub permission: Permission,
}

#[derive(Clone, Serialize, Deserialize)]
struct Device {
    id: Uuid,
    label: String,
    scope: Scope,
    expires_at: DateTime<Utc>,
    credentials: Option<Credentials>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notification_subscription: Option<Binding>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Credentials {
    access: Digest,
    access_expires_at: DateTime<Utc>,
    refresh: Digest,
    last_rotation: Option<Rotation>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Rotation {
    request_id: Uuid,
    previous_refresh: Digest,
}

#[derive(Serialize, Deserialize)]
struct DeviceFile {
    version: u32,
    devices: BTreeMap<Uuid, Device>,
}

struct Invitation {
    label: String,
    scope: Scope,
    digest: Digest,
    expires_at: DateTime<Utc>,
}

#[derive(Default)]
struct DeviceState {
    invitations: BTreeMap<Uuid, Invitation>,
    devices: BTreeMap<Uuid, Device>,
    observations: BTreeMap<Uuid, CancellationToken>,
    notification_deliveries: BTreeMap<Uuid, CancellationToken>,
}

pub(super) struct Registry {
    file: PathBuf,
    _writer: ExclusiveFileLease,
    state: Mutex<DeviceState>,
}

#[derive(Clone, Copy)]
pub(super) struct NotificationRecipient {
    pub device_id: Uuid,
    pub scope: Scope,
    pub enrollment: Enrollment,
}

pub(super) struct NotificationDelivery {
    pub subscription: Subscription,
    pub revision: Uuid,
    pub retired: CancellationToken,
    pub expires_at: DateTime<Utc>,
}

#[derive(thiserror::Error, Debug)]
pub(super) enum DeviceError {
    #[error("invalid device request")]
    Invalid,
    #[error("invalid or expired device credential")]
    Unauthorized,
    #[error("browser request must come from the device origin")]
    Origin,
    #[error("browser pairing has changed")]
    DeviceChanged,
    #[error("notification subscription has changed")]
    SubscriptionChanged,
    #[error("device not found")]
    Missing,
    #[error("device capacity reached")]
    Capacity,
    #[error("device store unavailable")]
    Storage(#[from] io::Error),
}

pub(super) struct IssuedInvite {
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub(super) struct DeviceView {
    pub id: Uuid,
    pub label: String,
    pub scope: Scope,
    pub expires_at: DateTime<Utc>,
    pub status: &'static str,
}

#[derive(Serialize)]
pub(super) struct IssuedCredentials {
    pub(super) device: DeviceView,
    pub(super) access_token: String,
    pub(super) access_expires_at: DateTime<Utc>,
    pub(super) refresh_token: String,
}

pub(super) struct DeviceGrant {
    pub device: DeviceView,
    pub access_expires_at: DateTime<Utc>,
    invalidated: CancellationToken,
    access_digest: Digest,
}

/// A private host-routing proof bound to the current stored device generation.
#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) struct DeviceAuthority {
    pub(super) device_id: Uuid,
    pub(super) session_id: Uuid,
    access_digest: Digest,
}

impl DeviceAuthority {
    fn permits(&self, device: &Device, required: Permission, now: DateTime<Utc>) -> bool {
        device.id == self.device_id
            && device.scope.session_id == self.session_id
            && device.scope.permission.allows(required)
            && device.expires_at > now
            && device.credentials.as_ref().is_some_and(|credentials| {
                credentials.access_expires_at > now
                    && crate::http::constant_time_eq(&credentials.access, &self.access_digest)
            })
    }

    pub(super) fn check(
        self,
        data: &Path,
        session_id: Uuid,
        required: Permission,
        now: DateTime<Utc>,
    ) -> io::Result<()> {
        let denied = || {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "device authorization is no longer valid",
            )
        };
        if session_id != self.session_id {
            return Err(denied());
        }
        let file = data.join("remote-devices").join("devices.json");
        let stored = read_devices(&file)?;
        let valid = stored
            .get(&self.device_id)
            .is_some_and(|device| self.permits(device, required, now));
        if valid { Ok(()) } else { Err(denied()) }
    }
}

impl DeviceGrant {
    pub(super) fn authority(&self) -> DeviceAuthority {
        DeviceAuthority {
            device_id: self.device.id,
            session_id: self.device.scope.session_id,
            access_digest: self.access_digest,
        }
    }
    pub(super) fn retirement(&self) -> crate::http::ObservationRetirement {
        let remaining = (self.access_expires_at - Utc::now())
            .to_std()
            .unwrap_or_default();
        crate::http::ObservationRetirement {
            invalidated: self.invalidated.clone(),
            deadline: tokio::time::Instant::now() + remaining,
        }
    }

    pub async fn retired(&self) {
        self.retirement().wait().await;
    }
}

impl Registry {
    pub(super) fn directory(&self) -> &Path {
        self.file
            .parent()
            .expect("registry file has a private directory")
    }

    pub(super) fn open(data_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let directory = data_dir.join("remote-devices");
        #[cfg(unix)]
        let created = {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&directory)
        };
        #[cfg(not(unix))]
        let created = fs::create_dir(&directory);
        if let Err(error) = created
            && error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(error);
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() {
            return Err(io::Error::other("device storage is not a directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "device storage must be private to the daemon user",
                ));
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = directory.join("writer.lock");
        ensure_regular(&lock)?;
        let writer = ExclusiveFileLease::try_acquire(options.open(lock)?)?;
        let file = directory.join("devices.json");
        ensure_regular(&file)?;
        let devices = match read_devices(&file) {
            Ok(devices) => devices,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            file,
            _writer: writer,
            state: Mutex::new(DeviceState {
                devices,
                ..DeviceState::default()
            }),
        })
    }

    pub(super) fn invite(
        &self,
        label: String,
        scope: Scope,
        now: DateTime<Utc>,
    ) -> Result<IssuedInvite, DeviceError> {
        let label = label.trim();
        if label.is_empty()
            || label.len() > 80
            || label.chars().any(char::is_control)
            || scope.session_id.is_nil()
        {
            return Err(DeviceError::Invalid);
        }
        let mut state = self.state.lock().unwrap();
        state.invitations.retain(|_, item| item.expires_at > now);
        if state.invitations.len() >= MAX_INVITES {
            return Err(DeviceError::Capacity);
        }
        let id = Uuid::new_v4();
        let secret = secret()?;
        let expires_at = now + INVITE_LIFETIME;
        state.invitations.insert(
            id,
            Invitation {
                label: label.to_owned(),
                scope,
                digest: digest(&secret),
                expires_at,
            },
        );
        Ok(IssuedInvite {
            code: encode("pair-v1", id, &secret),
            expires_at,
        })
    }

    pub(super) fn pair(
        &self,
        code: &str,
        now: DateTime<Utc>,
    ) -> Result<IssuedCredentials, DeviceError> {
        let (invite_id, input) = decode("pair-v1", code)?;
        let mut state = self.state.lock().unwrap();
        let invite = state
            .invitations
            .get(&invite_id)
            .filter(|invite| invite.expires_at > now && matches_digest(&invite.digest, &input))
            .ok_or(DeviceError::Unauthorized)?;
        let mut devices = state.devices.clone();
        let disposable = if devices.len() >= MAX_DEVICES {
            let disposable = devices
                .iter()
                .find(|(_, device)| device.credentials.is_none() || device.expires_at <= now)
                .map(|(id, _)| *id)
                .ok_or(DeviceError::Capacity)?;
            devices.remove(&disposable);
            Some(disposable)
        } else {
            None
        };
        let id = Uuid::new_v4();
        let access = secret()?;
        let refresh = secret()?;
        let access_expires_at = now + ACCESS_LIFETIME;
        let device = Device {
            id,
            label: invite.label.clone(),
            scope: invite.scope,
            expires_at: now + DEVICE_LIFETIME,
            credentials: Some(Credentials {
                access: digest(&access),
                refresh: digest(&refresh),
                access_expires_at,
                last_rotation: None,
            }),
            notification_subscription: None,
        };
        let issued = issued(&device, access_expires_at, &access, &refresh, now);
        devices.insert(id, device);
        self.save(&mut state, devices)?;
        state.invitations.remove(&invite_id);
        if let Some(disposable) = disposable {
            state.retire_observations(disposable);
            state.retire_notification_delivery(disposable);
        }
        Ok(issued)
    }

    pub(super) fn refresh(
        &self,
        token: &str,
        request_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<IssuedCredentials, DeviceError> {
        self.rotate(token, None, request_id, now)
    }

    pub(super) fn refresh_browser(
        &self,
        access: &str,
        refresh: &str,
        request_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<IssuedCredentials, DeviceError> {
        self.rotate(refresh, Some(decode("access-v1", access)?), request_id, now)
    }

    fn rotate(
        &self,
        token: &str,
        browser_access: Option<(Uuid, [u8; 32])>,
        request_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<IssuedCredentials, DeviceError> {
        if request_id.is_nil() {
            return Err(DeviceError::Invalid);
        }
        let (id, old_secret) = decode("refresh-v1", token)?;
        if browser_access.is_some_and(|(access_id, _)| access_id != id) {
            return Err(DeviceError::Unauthorized);
        }
        let mut state = self.state.lock().unwrap();
        let mut device = state
            .devices
            .get(&id)
            .filter(|device| device.expires_at > now)
            .cloned()
            .ok_or(DeviceError::Unauthorized)?;
        let credentials = device
            .credentials
            .as_mut()
            .ok_or(DeviceError::Unauthorized)?;
        let access = rotate_secret(&old_secret, b"atman-device-access\0", request_id);
        let refresh = rotate_secret(&old_secret, b"atman-device-refresh\0", request_id);
        if !matches_digest(&credentials.refresh, &old_secret) {
            if credentials.last_rotation.as_ref().is_some_and(|previous| {
                previous.request_id == request_id
                    && matches_digest(&previous.previous_refresh, &old_secret)
                    && matches_digest(&credentials.refresh, &refresh)
                    && matches_digest(&credentials.access, &access)
            }) {
                let access_expires_at = credentials.access_expires_at;
                return Ok(issued(&device, access_expires_at, &access, &refresh, now));
            }
            return Err(DeviceError::Unauthorized);
        }
        if browser_access.is_some_and(|(_, access)| !matches_digest(&credentials.access, &access)) {
            return Err(DeviceError::Unauthorized);
        }
        if credentials
            .last_rotation
            .as_ref()
            .is_some_and(|previous| previous.request_id == request_id)
        {
            if let Some((_, access)) = browser_access {
                let access_expires_at = credentials.access_expires_at;
                return Ok(issued(
                    &device,
                    access_expires_at,
                    &access,
                    &old_secret,
                    now,
                ));
            }
            return Err(DeviceError::Invalid);
        }
        let access_expires_at = (now + ACCESS_LIFETIME).min(device.expires_at);
        *credentials = Credentials {
            access: digest(&access),
            access_expires_at,
            refresh: digest(&refresh),
            last_rotation: Some(Rotation {
                request_id,
                previous_refresh: digest(&old_secret),
            }),
        };
        let issued = issued(&device, access_expires_at, &access, &refresh, now);
        let mut devices = state.devices.clone();
        devices.insert(id, device);
        self.save(&mut state, devices)?;
        state.retire_observations(id);
        Ok(issued)
    }

    pub(super) fn authorize(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<DeviceGrant, DeviceError> {
        let (id, secret) = decode("access-v1", token)?;
        let mut state = self.state.lock().unwrap();
        let device = state
            .devices
            .get(&id)
            .filter(|device| device.expires_at > now)
            .ok_or(DeviceError::Unauthorized)?;
        let credentials = device
            .credentials
            .as_ref()
            .filter(|credentials| {
                credentials.access_expires_at > now && matches_digest(&credentials.access, &secret)
            })
            .ok_or(DeviceError::Unauthorized)?;
        let access_expires_at = credentials.access_expires_at;
        let access_digest = credentials.access;
        let device = view(device, now);
        let invalidated = state.observations.entry(id).or_default().clone();
        Ok(DeviceGrant {
            device,
            access_expires_at,
            invalidated,
            access_digest,
        })
    }

    pub(super) fn list(&self, now: DateTime<Utc>) -> Vec<DeviceView> {
        self.state
            .lock()
            .unwrap()
            .devices
            .values()
            .map(|device| view(device, now))
            .collect()
    }

    pub(super) fn notification_subscription(
        &self,
        authority: DeviceAuthority,
        channel: Channel<'_>,
        now: DateTime<Utc>,
    ) -> Result<SubscriptionStatus, DeviceError> {
        let state = self.state.lock().unwrap();
        let device = state.authorized_device(authority, now)?;
        Ok(Binding::status(
            device.notification_subscription.as_ref(),
            channel,
        ))
    }

    pub(super) fn notification_recipients(
        &self,
        channels: &Channels,
        now: DateTime<Utc>,
    ) -> Vec<NotificationRecipient> {
        self.with_notification_recipients(channels, now, |recipients| recipients)
    }

    pub(super) fn with_notification_recipients<T>(
        &self,
        channels: &Channels,
        now: DateTime<Utc>,
        read: impl FnOnce(Vec<NotificationRecipient>) -> T,
    ) -> T {
        let state = self.state.lock().unwrap();
        let recipients = state
            .devices
            .values()
            .filter(|device| device.expires_at > now && device.credentials.is_some())
            .filter_map(|device| {
                let (enrollment, subscription) =
                    device.notification_subscription.as_ref()?.active()?;
                if !channels.supports(subscription.channel()) {
                    return None;
                }
                Some(NotificationRecipient {
                    device_id: device.id,
                    scope: device.scope,
                    enrollment,
                })
            })
            .collect();
        read(recipients)
    }

    pub(super) fn read_notification<T>(
        &self,
        authority: DeviceAuthority,
        now: DateTime<Utc>,
        read: impl FnOnce(Scope) -> Result<T, DeviceError>,
    ) -> Result<T, DeviceError> {
        let state = self.state.lock().unwrap();
        let device = state.authorized_device(authority, now)?;
        read(device.scope)
    }

    pub(super) fn notification_delivery(
        &self,
        channels: &Channels,
        device_id: Uuid,
        session: Uuid,
        enrollment: Uuid,
        required: Permission,
        now: DateTime<Utc>,
    ) -> Option<NotificationDelivery> {
        let mut state = self.state.lock().unwrap();
        let device = state.devices.get(&device_id).filter(|device| {
            device.expires_at > now
                && device.credentials.is_some()
                && device.scope.session_id == session
                && device.scope.permission.allows(required)
        })?;
        let binding = device.notification_subscription.as_ref()?;
        let (registered, subscription) = binding.active()?;
        if registered.id != enrollment || !channels.supports(subscription.channel()) {
            return None;
        }
        let expires_at = device.expires_at;
        let revision = binding.revision();
        let subscription = subscription.clone();
        let retired = state
            .notification_deliveries
            .entry(device_id)
            .or_default()
            .clone();
        Some(NotificationDelivery {
            subscription,
            revision,
            retired,
            expires_at,
        })
    }

    pub(super) fn update_notification_subscription(
        &self,
        authority: DeviceAuthority,
        request: Update,
        channel: Channel<'_>,
        now: DateTime<Utc>,
    ) -> Result<SubscriptionStatus, DeviceError> {
        let mut state = self.state.lock().unwrap();
        let device = state.authorized_device(authority, now)?;
        let current = device.notification_subscription.as_ref();
        let Some(change) = Binding::change(current, request, channel, now)? else {
            return Ok(Binding::status(current, channel));
        };
        let result = Binding::status(Some(&change), channel);
        let mut device = device.clone();
        device.notification_subscription = Some(change);
        let mut devices = state.devices.clone();
        devices.insert(device.id, device);
        self.save(&mut state, devices)?;
        state.retire_notification_delivery(authority.device_id);
        Ok(result)
    }

    pub(super) fn retire_notification_subscription(
        &self,
        id: Uuid,
        revision: Uuid,
    ) -> io::Result<bool> {
        let mut state = self.state.lock().unwrap();
        let Some(device) = state.devices.get(&id).filter(|device| {
            device
                .notification_subscription
                .as_ref()
                .is_some_and(|binding| binding.revision() == revision)
        }) else {
            return Ok(false);
        };
        let mut device = device.clone();
        device.notification_subscription = Some(Binding::retire(revision));
        let mut devices = state.devices.clone();
        devices.insert(id, device);
        self.save(&mut state, devices)?;
        state.retire_notification_delivery(id);
        Ok(true)
    }

    pub(super) fn revoke(&self, id: Uuid, now: DateTime<Utc>) -> Result<DeviceView, DeviceError> {
        let mut state = self.state.lock().unwrap();
        let mut devices = state.devices.clone();
        let device = devices.get_mut(&id).ok_or(DeviceError::Missing)?;
        let changed = device.credentials.take().is_some();
        let retired_subscription = device.notification_subscription.take();
        let result = view(device, now);
        if changed || retired_subscription.is_some() {
            self.save(&mut state, devices)?;
            state.retire_observations(id);
            state.retire_notification_delivery(id);
        }
        Ok(result)
    }

    fn save(&self, state: &mut DeviceState, devices: BTreeMap<Uuid, Device>) -> io::Result<()> {
        let stored = DeviceFile {
            version: 1,
            devices,
        };
        let bytes = serde_json::to_vec(&stored)?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err(io::Error::other("device storage is too large"));
        }
        atomic_replace(&self.file, &bytes)?;
        state.devices = stored.devices;
        Ok(())
    }
}

impl DeviceState {
    fn retire_notification_delivery(&mut self, id: Uuid) {
        if let Some(token) = self.notification_deliveries.remove(&id) {
            token.cancel();
        }
    }

    fn authorized_device(
        &self,
        authority: DeviceAuthority,
        now: DateTime<Utc>,
    ) -> Result<&Device, DeviceError> {
        self.devices
            .get(&authority.device_id)
            .filter(|device| authority.permits(device, Permission::Read, now))
            .ok_or(DeviceError::Unauthorized)
    }

    fn retire_observations(&mut self, id: Uuid) {
        if let Some(token) = self.observations.remove(&id) {
            token.cancel();
        }
    }
}

fn ensure_regular(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(io::Error::other("device storage is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_devices(file: &Path) -> io::Result<BTreeMap<Uuid, Device>> {
    ensure_regular(file)?;
    let mut bytes = Vec::new();
    File::open(file)?
        .take(MAX_STORE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(io::Error::other("device storage is too large"));
    }
    let stored: DeviceFile = serde_json::from_slice(&bytes)?;
    if stored.version != 1
        || stored.devices.len() > MAX_DEVICES
        || stored.devices.iter().any(|(id, device)| *id != device.id)
    {
        return Err(io::Error::other("unsupported device store"));
    }
    Ok(stored.devices)
}

fn view(device: &Device, now: DateTime<Utc>) -> DeviceView {
    let status = if device.credentials.is_none() {
        "revoked"
    } else if device.expires_at <= now {
        "expired"
    } else {
        "active"
    };
    DeviceView {
        id: device.id,
        label: device.label.clone(),
        scope: device.scope,
        expires_at: device.expires_at,
        status,
    }
}

fn issued(
    device: &Device,
    access_expires_at: DateTime<Utc>,
    access: &[u8; 32],
    refresh: &[u8; 32],
    now: DateTime<Utc>,
) -> IssuedCredentials {
    IssuedCredentials {
        device: view(device, now),
        access_token: encode("access-v1", device.id, access),
        access_expires_at,
        refresh_token: encode("refresh-v1", device.id, refresh),
    }
}

fn secret() -> io::Result<[u8; 32]> {
    let mut secret = [0; 32];
    OsRng
        .try_fill_bytes(&mut secret)
        .map_err(io::Error::other)?;
    Ok(secret)
}

fn digest(secret: &[u8; 32]) -> Digest {
    blake3::derive_key("atman remote device credential digest v1", secret)
}

fn matches_digest(expected: &Digest, secret: &[u8; 32]) -> bool {
    crate::http::constant_time_eq(expected, &digest(secret))
}

fn rotate_secret(previous: &[u8; 32], purpose: &[u8], request_id: Uuid) -> [u8; 32] {
    let mut derive = blake3::Hasher::new_keyed(previous);
    derive.update(purpose);
    derive.update(request_id.as_bytes());
    *derive.finalize().as_bytes()
}

fn encode(kind: &str, id: Uuid, secret: &[u8; 32]) -> String {
    format!("{kind}.{id}.{}", URL_SAFE_NO_PAD.encode(secret))
}

pub(super) fn browser_cookie_device_id(access: &str, refresh: &str) -> Result<Uuid, DeviceError> {
    let (id, _) = decode("access-v1", access)?;
    let (refresh_id, _) = decode("refresh-v1", refresh)?;
    if id != refresh_id {
        return Err(DeviceError::Unauthorized);
    }
    Ok(id)
}

fn decode(kind: &str, encoded: &str) -> Result<(Uuid, [u8; 32]), DeviceError> {
    let (version, tail) = encoded.split_once('.').ok_or(DeviceError::Unauthorized)?;
    let (id, secret) = tail.split_once('.').ok_or(DeviceError::Unauthorized)?;
    if version != kind || id.len() != 36 || secret.len() != 43 {
        return Err(DeviceError::Unauthorized);
    }
    let id = id.parse().map_err(|_| DeviceError::Unauthorized)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(secret)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(DeviceError::Unauthorized)?;
    Ok((id, decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expiry_and_revocation_fence_the_original_device_grant() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::open(root.path()).unwrap();
        let now = Utc::now();
        let scope = Scope {
            session_id: Uuid::new_v4(),
            permission: Permission::Read,
        };
        let ticket = registry.invite("Desktop".into(), scope, now).unwrap();
        assert!(matches!(
            registry.pair(&ticket.code, ticket.expires_at),
            Err(DeviceError::Unauthorized)
        ));
        let ticket = registry.invite("Phone".into(), scope, now).unwrap();
        let paired = registry.pair(&ticket.code, now).unwrap();
        assert!(matches!(
            registry.authorize(&paired.access_token, paired.access_expires_at),
            Err(DeviceError::Unauthorized)
        ));
        assert!(matches!(
            registry.refresh(
                &paired.refresh_token,
                Uuid::new_v4(),
                paired.device.expires_at
            ),
            Err(DeviceError::Unauthorized)
        ));
        let grant = registry.authorize(&paired.access_token, now).unwrap();
        assert!(!grant.invalidated.is_cancelled());
        let mut expired = registry.authorize(&paired.access_token, now).unwrap();
        expired.access_expires_at = Utc::now();
        tokio::time::timeout(std::time::Duration::from_secs(1), expired.retired())
            .await
            .unwrap();
        assert!(!grant.invalidated.is_cancelled());

        let rotation = Uuid::new_v4();
        let rotated = registry
            .refresh(&paired.refresh_token, rotation, now)
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), grant.retired())
            .await
            .unwrap();
        let refreshed = registry.authorize(&rotated.access_token, now).unwrap();
        registry
            .refresh(&paired.refresh_token, rotation, now)
            .unwrap();
        assert!(!refreshed.invalidated.is_cancelled());
        registry.revoke(paired.device.id, now).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), refreshed.retired())
            .await
            .unwrap();
        assert!(matches!(
            registry.authorize(&rotated.access_token, now),
            Err(DeviceError::Unauthorized)
        ));
    }

    #[test]
    fn browser_rotation_reconciles_both_cookie_generations_without_a_second_write() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::open(root.path()).unwrap();
        let now = Utc::now();
        let scope = Scope {
            session_id: Uuid::new_v4(),
            permission: Permission::Read,
        };
        let ticket = registry.invite("Browser".into(), scope, now).unwrap();
        let first = registry.pair(&ticket.code, now).unwrap();
        let now = first.access_expires_at;
        let id = Uuid::new_v4();
        let rotated = registry
            .refresh_browser(&first.access_token, &first.refresh_token, id, now)
            .unwrap();
        let original = fs::read(&registry.file).unwrap();
        drop(registry);

        let registry = Registry::open(root.path()).unwrap();
        for pair in [&first, &rotated] {
            let recovered = registry
                .refresh_browser(&pair.access_token, &pair.refresh_token, id, now)
                .unwrap();
            assert_eq!(recovered.access_token, rotated.access_token);
            assert_eq!(recovered.refresh_token, rotated.refresh_token);
            assert_eq!(recovered.access_expires_at, rotated.access_expires_at);
        }
        assert!(matches!(
            registry.refresh_browser(&first.access_token, &rotated.refresh_token, id, now),
            Err(DeviceError::Unauthorized)
        ));
        assert!(matches!(
            registry.refresh_browser(
                &first.access_token,
                &first.refresh_token,
                Uuid::new_v4(),
                now
            ),
            Err(DeviceError::Unauthorized)
        ));
        assert_eq!(fs::read(&registry.file).unwrap(), original);
    }

    #[test]
    fn input_proof_observes_rotation_expiry_and_revocation_from_the_shared_store() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let registry = Registry::open(root).unwrap();
        let now = Utc::now();
        let session_id = Uuid::new_v4();
        let scope = Scope {
            session_id,
            permission: Permission::Send,
        };
        let invite = registry.invite("Send device".into(), scope, now).unwrap();
        let issued = registry.pair(&invite.code, now).unwrap();
        let proof = registry
            .authorize(&issued.access_token, now)
            .unwrap()
            .authority();
        proof
            .check(root, session_id, Permission::Send, now)
            .unwrap();
        assert!(
            proof
                .check(root, session_id, Permission::Approve, now)
                .is_err()
        );
        assert!(
            proof
                .check(root, Uuid::new_v4(), Permission::Send, now)
                .is_err()
        );
        assert!(
            proof
                .check(root, session_id, Permission::Send, issued.access_expires_at)
                .is_err()
        );
        let rotated = registry
            .refresh(&issued.refresh_token, Uuid::new_v4(), now)
            .unwrap();
        assert!(
            proof
                .check(root, session_id, Permission::Send, now)
                .is_err()
        );
        let fresh = registry
            .authorize(&rotated.access_token, now)
            .unwrap()
            .authority();
        fresh
            .check(root, session_id, Permission::Send, now)
            .unwrap();
        registry.revoke(issued.device.id, now).unwrap();
        assert!(
            fresh
                .check(root, session_id, Permission::Send, now)
                .is_err()
        );
    }

    #[test]
    fn unsuccessful_pair_write_keeps_the_invitation_available() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::open(root.path()).unwrap();
        assert!(Registry::open(root.path()).is_err());
        let now = Utc::now();
        let scope = Scope {
            session_id: Uuid::new_v4(),
            permission: Permission::Read,
        };
        let ticket = registry.invite("Phone".into(), scope, now).unwrap();
        fs::create_dir(&registry.file).unwrap();
        assert!(matches!(
            registry.pair(&ticket.code, now),
            Err(DeviceError::Storage(_))
        ));
        assert!(registry.list(now).is_empty());
        fs::remove_dir(&registry.file).unwrap();
        assert!(registry.pair(&ticket.code, now).is_ok());
        assert!(matches!(
            registry.pair(&ticket.code, now),
            Err(DeviceError::Unauthorized)
        ));
    }
}
