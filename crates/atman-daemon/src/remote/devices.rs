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

const INVITE_LIFETIME: TimeDelta = TimeDelta::minutes(5);
const ACCESS_LIFETIME: TimeDelta = TimeDelta::minutes(15);
const DEVICE_LIFETIME: TimeDelta = TimeDelta::days(30);
const MAX_INVITES: usize = 32;
const MAX_DEVICES: usize = 128;
const MAX_STORE_BYTES: u64 = 256 * 1024;

type Digest = [u8; 32];

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Permission {
    Read,
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
}

pub(super) struct Registry {
    file: PathBuf,
    _writer: ExclusiveFileLease,
    state: Mutex<DeviceState>,
}

#[derive(thiserror::Error, Debug)]
pub(super) enum DeviceError {
    #[error("invalid device request")]
    Invalid,
    #[error("invalid or expired device credential")]
    Unauthorized,
    #[error("browser request must come from the device origin")]
    Origin,
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
}

impl DeviceGrant {
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
        let mut state = DeviceState::default();
        match File::open(&file) {
            Ok(opened) => {
                let mut bytes = Vec::new();
                opened.take(MAX_STORE_BYTES + 1).read_to_end(&mut bytes)?;
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
                state.devices = stored.devices;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            file,
            _writer: writer,
            state: Mutex::new(state),
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
        };
        let issued = issued(&device, access_expires_at, &access, &refresh, now);
        devices.insert(id, device);
        self.save(&mut state, devices)?;
        state.invitations.remove(&invite_id);
        if let Some(disposable) = disposable
            && let Some(observation) = state.observations.remove(&disposable)
        {
            observation.cancel();
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
        let device = view(device, now);
        let invalidated = state.observations.entry(id).or_default().clone();
        Ok(DeviceGrant {
            device,
            access_expires_at,
            invalidated,
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

    pub(super) fn revoke(&self, id: Uuid, now: DateTime<Utc>) -> Result<DeviceView, DeviceError> {
        let mut state = self.state.lock().unwrap();
        let mut devices = state.devices.clone();
        let device = devices.get_mut(&id).ok_or(DeviceError::Missing)?;
        let changed = device.credentials.take().is_some();
        let result = view(device, now);
        if changed {
            self.save(&mut state, devices)?;
            state.retire_observations(id);
        }
        Ok(result)
    }

    fn save(&self, state: &mut DeviceState, devices: BTreeMap<Uuid, Device>) -> io::Result<()> {
        let stored = DeviceFile {
            version: 1,
            devices,
        };
        let bytes = serde_json::to_vec(&stored)?;
        atomic_replace(&self.file, &bytes)?;
        state.devices = stored.devices;
        Ok(())
    }
}

impl DeviceState {
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
