use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::auth_store::{AuthCredentialCommit, ProviderKind, StoredProvider};
use crate::config_hub::{AuthTokenUpdate, ConfigHub};
use crate::provider::{DiscoveredModel, DiscoveredModelDetails, Provider};

const REFRESH_WINDOW_SECONDS: i64 = 300;
const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const REFRESH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(75);
#[cfg(not(test))]
const REFRESH_FAILURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const REFRESH_FAILURE_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(50);

type RefreshFuture = Pin<Box<dyn Future<Output = Result<TokenResult>> + Send>>;
type RefreshFn = dyn Fn(String) -> RefreshFuture + Send + Sync;
type CredentialResult = std::result::Result<OAuthCredential, OAuthCredentialError>;
type SharedRefreshFuture = Shared<BoxFuture<'static, SharedRefreshOutcome>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CredentialGateKey {
    auth_path: PathBuf,
    provider_id: String,
}

static CREDENTIAL_GATES: LazyLock<Mutex<HashMap<CredentialGateKey, CredentialGateEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct CredentialGate {
    key: CredentialGateKey,
    state: Mutex<CredentialGateState>,
}

struct CredentialGateEntry {
    weak: Weak<CredentialGate>,
    // Quarantined gates stay process-local so provider reconstruction cannot
    // bypass isolation. Healthy gates remain weak and release runtime state.
    quarantined: Option<Arc<CredentialGate>>,
}

#[derive(Default)]
struct CredentialGateState {
    in_flight: Option<SharedRefreshFuture>,
    pending_retry_active: bool,
    failure: Option<CachedRefreshFailure>,
    pending: Option<PendingCredentialCommit>,
    quarantine: Option<OAuthCredentialError>,
}

impl CredentialGate {
    fn ensure_not_quarantined(&self) -> std::result::Result<(), OAuthCredentialError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &state.quarantine {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn quarantine(self: &Arc<Self>) -> OAuthCredentialError {
        let mut gates = CREDENTIAL_GATES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = gates
            .entry(self.key.clone())
            .or_insert_with(|| CredentialGateEntry {
                weak: Arc::downgrade(self),
                quarantined: None,
            });
        entry.weak = Arc::downgrade(self);
        entry.quarantined = Some(self.clone());

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (error, newly_quarantined) = match &state.quarantine {
            Some(error) => (error.clone(), false),
            None => {
                let error = OAuthCredentialError::Quarantined(self.key.provider_id.clone());
                state.quarantine = Some(error.clone());
                (error, true)
            }
        };
        let in_flight = state.in_flight.take();
        let pending = state.pending.take();
        state.pending_retry_active = false;
        state.failure = None;
        drop(state);
        drop(gates);
        drop(in_flight);
        drop(pending);

        if newly_quarantined {
            // Keep notification sinks outside the shared refresh completion
            // path so a slow sink cannot strand credential waiters.
            let notification = error.clone();
            let _ = std::thread::Builder::new()
                .name("atman-oauth-quarantine-notify".into())
                .spawn(move || {
                    let _ =
                        crate::panic_capture::blocking(|| crate::notify!(error, "{notification}"));
                });
        }
        error
    }
}

struct CachedRefreshFailure {
    until: Instant,
    error: OAuthCredentialError,
}

#[derive(Clone)]
struct PendingCredentialCommit {
    snapshot: crate::auth_store::AuthProviderCredentialSnapshot,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
    account: Option<String>,
    _refresh_lock: Arc<RefreshFileLock>,
}

impl PendingCredentialCommit {
    fn update(&self) -> AuthTokenUpdate {
        AuthTokenUpdate {
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at: self.expires_at,
            account: self.account.clone(),
        }
    }
}

#[derive(Clone)]
struct RefreshFlightResult {
    result: CredentialResult,
    pending: Option<PendingCredentialCommit>,
}

#[derive(Clone)]
enum SharedRefreshOutcome {
    Complete(RefreshFlightResult),
    Quarantined(OAuthCredentialError),
}

impl RefreshFlightResult {
    fn complete(result: CredentialResult) -> Self {
        Self {
            result,
            pending: None,
        }
    }

    fn pending(error: OAuthCredentialError, pending: PendingCredentialCommit) -> Self {
        Self {
            result: Err(error),
            pending: Some(pending),
        }
    }
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let digest = hasher.finalize();
        let challenge = URL_SAFE_NO_PAD.encode(digest);

        Pkce {
            verifier,
            challenge,
        }
    }
}

pub struct TokenResult {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    pub account: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OAuthCredential {
    pub access_token: String,
    pub display_account: Option<String>,
}

impl std::fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthCredential")
            .field("access_token", &"[redacted]")
            .field(
                "display_account",
                &self.display_account.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum OAuthCredentialError {
    #[error("OAuth provider `{0}` is not configured")]
    Missing(String),
    #[error("OAuth provider `{0}` is disabled")]
    Disabled(String),
    #[error("OAuth provider `{0}` changed kind")]
    KindChanged(String),
    #[error("OAuth provider `{0}` must be authenticated again")]
    ReauthenticationRequired(String),
    #[error("OAuth credential snapshot for `{0}` expired; use a managed provider to refresh it")]
    SnapshotExpired(String),
    #[error("load OAuth credentials for `{provider_id}`: {message}")]
    Load {
        provider_id: String,
        message: String,
    },
    #[error("lock OAuth credential refresh for `{provider_id}`: {message}")]
    Lock {
        provider_id: String,
        message: String,
    },
    #[error("refresh OAuth credentials for `{provider_id}`: {message}")]
    Refresh {
        provider_id: String,
        message: String,
    },
    #[error("refresh OAuth credentials for `{0}` timed out")]
    RefreshTimeout(String),
    #[error("OAuth provider `{provider_id}` returned invalid credentials: {message}")]
    InvalidRefresh {
        provider_id: String,
        message: &'static str,
    },
    #[error("persist OAuth credentials for `{provider_id}`: {message}")]
    Persist {
        provider_id: String,
        message: String,
    },
    #[error("OAuth provider `{0}` changed while credentials were refreshing")]
    Changed(String),
    #[error("OAuth credential refresh task failed for `{provider_id}`: {message}")]
    Task {
        provider_id: String,
        message: String,
    },
    #[error(
        "OAuth provider `{0}` was quarantined after an internal credential failure; remove it and sign in again or restart atman"
    )]
    Quarantined(String),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

#[derive(Clone)]
pub(crate) struct OAuthCredentialLease {
    provider_id: String,
    expected_kind: ProviderKind,
    hub: ConfigHub,
    refresher: Arc<RefreshFn>,
    gate: Arc<CredentialGate>,
}

impl OAuthCredentialLease {
    pub(crate) fn new<P: OAuthProvider>(provider_id: impl Into<String>, hub: ConfigHub) -> Self {
        Self::with_refresher(provider_id, P::KIND.clone(), hub, |refresh_token| {
            P::refresh_token(&refresh_token)
        })
    }

    pub(crate) fn with_refresher(
        provider_id: impl Into<String>,
        expected_kind: ProviderKind,
        hub: ConfigHub,
        refresher: impl Fn(String) -> RefreshFuture + Send + Sync + 'static,
    ) -> Self {
        let provider_id = provider_id.into();
        let gate = credential_gate(&hub, &provider_id);
        Self {
            provider_id,
            expected_kind,
            hub,
            refresher: Arc::new(refresher),
            gate,
        }
    }

    pub(crate) async fn acquire(
        &self,
    ) -> std::result::Result<OAuthCredential, OAuthCredentialError> {
        match crate::panic_capture::future(self.acquire_inner()).await {
            Ok(result) => result,
            Err(_) => Err(self.gate.quarantine()),
        }
    }

    async fn acquire_inner(&self) -> std::result::Result<OAuthCredential, OAuthCredentialError> {
        self.gate.ensure_not_quarantined()?;
        let worker = self.refresh_worker();
        let (stored, snapshot) = worker.load_state_async().await?;
        self.discard_stale_pending(&snapshot);
        self.gate.ensure_not_quarantined()?;
        if !credentials_need_refresh(&stored)? {
            self.gate.ensure_not_quarantined()?;
            return Ok(credentials_from_provider(stored));
        }

        let result = match self.shared_refresh()?.await {
            SharedRefreshOutcome::Complete(outcome) => outcome.result,
            SharedRefreshOutcome::Quarantined(error) => Err(error),
        };
        self.gate.ensure_not_quarantined()?;
        result
    }

    fn shared_refresh(&self) -> std::result::Result<SharedRefreshFuture, OAuthCredentialError> {
        let gate = self.gate.clone();
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(error) = &state.quarantine {
            return Err(error.clone());
        }
        if state.pending_retry_active {
            return Err(state
                .failure
                .as_ref()
                .map(|failure| failure.error.clone())
                .unwrap_or_else(|| OAuthCredentialError::Changed(self.provider_id.clone())));
        }
        if let Some(in_flight) = &state.in_flight {
            return Ok(in_flight.clone());
        }
        if let Some(failure) = &state.failure
            && Instant::now() < failure.until
        {
            return Err(failure.error.clone());
        }
        state.failure = None;

        let pending = state.pending.clone();
        let worker = self.refresh_worker();
        let in_flight = shared_refresh_future(worker.clone(), pending);
        state.in_flight = Some(in_flight.clone());
        drop(state);

        // A driver keeps the shared future alive after every waiter is
        // cancelled and retries a rotated token until it is persisted.
        let driver = in_flight.clone();
        std::mem::drop(tokio::spawn(drive_shared_refresh(gate, worker, driver)));

        Ok(in_flight)
    }

    fn discard_stale_pending(&self, snapshot: &crate::auth_store::AuthProviderCredentialSnapshot) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| &pending.snapshot != snapshot)
        {
            state.pending = None;
            state.failure = None;
        }
    }

    fn refresh_worker(&self) -> OAuthCredentialRefresh {
        OAuthCredentialRefresh {
            provider_id: self.provider_id.clone(),
            expected_kind: self.expected_kind.clone(),
            hub: self.hub.clone(),
            refresher: self.refresher.clone(),
            gate: self.gate.clone(),
        }
    }
}

fn shared_refresh_future(
    worker: OAuthCredentialRefresh,
    pending: Option<PendingCredentialCommit>,
) -> SharedRefreshFuture {
    let gate = worker.gate.clone();
    crate::panic_capture::future(async move { worker.refresh_serialized(pending).await })
        .map(move |result| match result {
            Ok(result) => match &result.result {
                Err(error @ OAuthCredentialError::Quarantined(_)) => {
                    SharedRefreshOutcome::Quarantined(error.clone())
                }
                _ => SharedRefreshOutcome::Complete(result),
            },
            Err(_) => SharedRefreshOutcome::Quarantined(gate.quarantine()),
        })
        .boxed()
        .shared()
}

async fn drive_shared_refresh(
    gate: Arc<CredentialGate>,
    worker: OAuthCredentialRefresh,
    in_flight: SharedRefreshFuture,
) {
    let mut outcome = match in_flight.await {
        SharedRefreshOutcome::Complete(outcome) => outcome,
        SharedRefreshOutcome::Quarantined(_) => return,
    };
    loop {
        let retry_pending = outcome.pending.is_some();
        let failure = outcome.result.as_ref().err().cloned();
        {
            let mut state = gate
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.quarantine.is_some() {
                state.in_flight = None;
                state.pending_retry_active = false;
                state.pending = None;
                state.failure = None;
                return;
            }
            state.in_flight = None;
            state.pending_retry_active = false;
            state.pending = outcome.pending;
            state.failure = failure.map(|error| CachedRefreshFailure {
                until: Instant::now() + REFRESH_FAILURE_COOLDOWN,
                error,
            });
        }
        if !retry_pending {
            return;
        }

        tokio::time::sleep(REFRESH_FAILURE_COOLDOWN).await;
        let pending = {
            let mut state = gate
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.in_flight.is_some() {
                return;
            }
            let Some(pending) = state.pending.clone() else {
                return;
            };
            state.pending_retry_active = true;
            pending
        };
        outcome = match shared_refresh_future(worker.clone(), Some(pending)).await {
            SharedRefreshOutcome::Complete(outcome) => outcome,
            SharedRefreshOutcome::Quarantined(_) => return,
        };
    }
}

#[derive(Clone)]
struct OAuthCredentialRefresh {
    provider_id: String,
    expected_kind: ProviderKind,
    hub: ConfigHub,
    refresher: Arc<RefreshFn>,
    gate: Arc<CredentialGate>,
}

enum BlockingOutcome<T> {
    Complete(T),
    Quarantined(OAuthCredentialError),
}

impl OAuthCredentialRefresh {
    async fn run_blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> std::result::Result<T, OAuthCredentialError> {
        self.gate.ensure_not_quarantined()?;
        let gate = self.gate.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            if let Err(error) = gate.ensure_not_quarantined() {
                return BlockingOutcome::Quarantined(error);
            }
            match crate::panic_capture::blocking(operation) {
                Ok(value) => BlockingOutcome::Complete(value),
                Err(_) => BlockingOutcome::Quarantined(gate.quarantine()),
            }
        })
        .await;
        let value = match outcome {
            Ok(BlockingOutcome::Complete(value)) => value,
            Ok(BlockingOutcome::Quarantined(error)) => return Err(error),
            Err(error) if error.is_panic() => {
                return Err(self.gate.quarantine());
            }
            Err(error) => {
                return Err(OAuthCredentialError::Task {
                    provider_id: self.provider_id.clone(),
                    message: error.to_string(),
                });
            }
        };
        self.gate.ensure_not_quarantined()?;
        Ok(value)
    }

    fn load_state(
        &self,
    ) -> std::result::Result<
        (
            StoredProvider,
            crate::auth_store::AuthProviderCredentialSnapshot,
        ),
        OAuthCredentialError,
    > {
        let Some(state) = self
            .hub
            .load_or_create_auth_provider_credential_state(&self.provider_id)
            .map_err(|error| OAuthCredentialError::Load {
                provider_id: self.provider_id.clone(),
                message: error.to_string(),
            })?
        else {
            return Err(OAuthCredentialError::Missing(self.provider_id.clone()));
        };
        if !state.0.enabled {
            return Err(OAuthCredentialError::Disabled(self.provider_id.clone()));
        }
        if state.0.kind != self.expected_kind {
            return Err(OAuthCredentialError::KindChanged(self.provider_id.clone()));
        }
        Ok(state)
    }

    async fn load_state_async(
        &self,
    ) -> std::result::Result<
        (
            StoredProvider,
            crate::auth_store::AuthProviderCredentialSnapshot,
        ),
        OAuthCredentialError,
    > {
        let worker = self.clone();
        self.run_blocking(move || worker.load_state()).await?
    }

    async fn persist_pending(
        &self,
        pending: &PendingCredentialCommit,
    ) -> std::result::Result<AuthCredentialCommit, OAuthCredentialError> {
        let hub = self.hub.clone();
        let provider_id = self.provider_id.clone();
        let snapshot = pending.snapshot.clone();
        let update = pending.update();
        self.run_blocking(move || {
            hub.update_auth_tokens_if_current(&provider_id, &snapshot, update)
        })
        .await?
        .map_err(|error| OAuthCredentialError::Persist {
            provider_id: self.provider_id.clone(),
            message: error.to_string(),
        })
    }

    async fn acquire_refresh_file_lock(
        &self,
    ) -> std::result::Result<RefreshFileLock, OAuthCredentialError> {
        let path = refresh_lock_path(&self.hub, &self.provider_id);
        let provider_id = self.provider_id.clone();
        self.run_blocking(move || open_refresh_file_lock(&path))
            .await?
            .map(RefreshFileLock)
            .map_err(|error| OAuthCredentialError::Lock {
                provider_id,
                message: error.to_string(),
            })
    }

    async fn commit_pending(&self, pending: PendingCredentialCommit) -> RefreshFlightResult {
        let commit = match self.persist_pending(&pending).await {
            Ok(commit) => commit,
            Err(error) => return RefreshFlightResult::pending(error, pending),
        };
        match commit {
            AuthCredentialCommit::Updated { provider, .. } => {
                if provider.enabled {
                    RefreshFlightResult::complete(Ok(credentials_from_provider(provider)))
                } else {
                    RefreshFlightResult::complete(Err(OAuthCredentialError::Disabled(
                        self.provider_id.clone(),
                    )))
                }
            }
            AuthCredentialCommit::Missing => RefreshFlightResult::complete(Err(
                OAuthCredentialError::Missing(self.provider_id.clone()),
            )),
            AuthCredentialCommit::Changed => {
                // A concurrent login or refresh may already have installed a
                // usable token; adopt it instead of failing this request.
                let (provider, _) = match self.load_state_async().await {
                    Ok(state) => state,
                    Err(error) => return RefreshFlightResult::complete(Err(error)),
                };
                match credentials_need_refresh(&provider) {
                    Ok(false) => {
                        RefreshFlightResult::complete(Ok(credentials_from_provider(provider)))
                    }
                    Ok(true) => RefreshFlightResult::complete(Err(OAuthCredentialError::Changed(
                        self.provider_id.clone(),
                    ))),
                    Err(error) => RefreshFlightResult::complete(Err(error)),
                }
            }
        }
    }

    async fn refresh_serialized(
        &self,
        pending: Option<PendingCredentialCommit>,
    ) -> RefreshFlightResult {
        if let Some(pending) = pending {
            // The pending commit owns the lock acquired before token rotation.
            // Reacquiring it here would deadlock against our own file handle.
            return self.commit_pending(pending).await;
        }

        // OAuth refresh tokens may be single-use. Hold the cross-process lock
        // until the replacement token is durably committed. The process-local
        // shared future admits only one leader before this point.
        let refresh_lock = match self.acquire_refresh_file_lock().await {
            Ok(guard) => Arc::new(guard),
            Err(error) => {
                return RefreshFlightResult::complete(Err(error));
            }
        };

        let (stored, snapshot) = match self.load_state_async().await {
            Ok(state) => state,
            Err(error) => return RefreshFlightResult::complete(Err(error)),
        };
        match credentials_need_refresh(&stored) {
            Ok(false) => {
                return RefreshFlightResult::complete(Ok(credentials_from_provider(stored)));
            }
            Ok(true) => {}
            Err(error) => return RefreshFlightResult::complete(Err(error)),
        }
        let Some(refresh_token) = stored
            .refresh_token
            .clone()
            .filter(|token| !token.trim().is_empty())
        else {
            return RefreshFlightResult::complete(Err(
                OAuthCredentialError::ReauthenticationRequired(self.provider_id.clone()),
            ));
        };
        let tokens =
            match tokio::time::timeout(REFRESH_TIMEOUT, (self.refresher)(refresh_token)).await {
                Ok(Ok(tokens)) => tokens,
                Ok(Err(error)) => {
                    return RefreshFlightResult::complete(Err(OAuthCredentialError::Refresh {
                        provider_id: self.provider_id.clone(),
                        message: error.to_string(),
                    }));
                }
                Err(_) => {
                    return RefreshFlightResult::complete(Err(
                        OAuthCredentialError::RefreshTimeout(self.provider_id.clone()),
                    ));
                }
            };
        if let Err(error) = validate_refreshed_tokens(&self.provider_id, &tokens) {
            return RefreshFlightResult::complete(Err(error));
        }
        let TokenResult {
            access_token,
            refresh_token,
            expires_at,
            account,
        } = tokens;
        self.commit_pending(PendingCredentialCommit {
            snapshot,
            access_token,
            refresh_token: refresh_token.filter(|token| !token.trim().is_empty()),
            expires_at,
            account: account.filter(|account| !account.trim().is_empty()),
            _refresh_lock: refresh_lock,
        })
        .await
    }
}

struct RefreshFileLock(std::fs::File);

impl Drop for RefreshFileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn credential_gate(hub: &ConfigHub, provider_id: &str) -> Arc<CredentialGate> {
    let key = CredentialGateKey {
        auth_path: normalized_auth_path(hub),
        provider_id: provider_id.to_string(),
    };
    let mut gates = CREDENTIAL_GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    gates.retain(|_, entry| entry.quarantined.is_some() || entry.weak.strong_count() > 0);
    if let Some(gate) = gates
        .get(&key)
        .and_then(|entry| entry.quarantined.clone().or_else(|| entry.weak.upgrade()))
    {
        return gate;
    }
    let gate = Arc::new(CredentialGate {
        key: key.clone(),
        state: Mutex::new(CredentialGateState::default()),
    });
    gates.insert(
        key,
        CredentialGateEntry {
            weak: Arc::downgrade(&gate),
            quarantined: None,
        },
    );
    gate
}

fn normalized_auth_path(hub: &ConfigHub) -> PathBuf {
    let auth_path =
        std::path::absolute(hub.auth_path()).unwrap_or_else(|_| hub.auth_path().to_path_buf());
    let Some(parent) = auth_path.parent() else {
        return auth_path;
    };
    match std::fs::canonicalize(parent) {
        Ok(parent) => parent.join(
            auth_path
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("auth.json")),
        ),
        Err(_) => auth_path,
    }
}

fn refresh_lock_path(hub: &ConfigHub, provider_id: &str) -> PathBuf {
    let auth_path = normalized_auth_path(hub);
    let mut hasher = Sha256::new();
    hasher.update(auth_path.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(provider_id.as_bytes());
    let digest = hasher.finalize();
    let suffix = URL_SAFE_NO_PAD.encode(&digest[..16]);
    hub.auth_path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!(".oauth-refresh-{suffix}.lock"))
}

fn open_refresh_file_lock(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    open_refresh_file_lock_with_timeout(path, REFRESH_LOCK_TIMEOUT)
}

fn open_refresh_file_lock_with_timeout(
    path: &std::path::Path,
    timeout: std::time::Duration,
) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "OAuth refresh lock timed out",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(file)
}

fn credentials_need_refresh(
    provider: &StoredProvider,
) -> std::result::Result<bool, OAuthCredentialError> {
    let now = unix_timestamp()?;
    if provider.expires_at > now.saturating_add(REFRESH_WINDOW_SECONDS) {
        return Ok(false);
    }
    if provider
        .refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
    {
        return Ok(true);
    }
    if provider.expires_at > now {
        return Ok(false);
    }
    Err(OAuthCredentialError::ReauthenticationRequired(
        provider.id.clone(),
    ))
}

fn validate_refreshed_tokens(
    provider_id: &str,
    tokens: &TokenResult,
) -> std::result::Result<(), OAuthCredentialError> {
    if tokens.access_token.trim().is_empty() {
        return Err(OAuthCredentialError::InvalidRefresh {
            provider_id: provider_id.to_string(),
            message: "access token is empty",
        });
    }
    if tokens.expires_at <= unix_timestamp()? {
        return Err(OAuthCredentialError::InvalidRefresh {
            provider_id: provider_id.to_string(),
            message: "access token is already expired",
        });
    }
    Ok(())
}

fn unix_timestamp() -> std::result::Result<i64, OAuthCredentialError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| OAuthCredentialError::Clock)?
        .as_secs() as i64)
}

fn credentials_from_provider(provider: StoredProvider) -> OAuthCredential {
    OAuthCredential {
        access_token: provider.access_token,
        display_account: provider.account,
    }
}

pub trait OAuthProvider: Provider {
    const KIND: ProviderKind = ProviderKind::Custom;

    fn authorize_url() -> (String, Pkce, String);
    fn exchange_code(
        code: &str,
        verifier: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenResult>> + Send>>;
    fn refresh_token(
        token: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenResult>> + Send>>;
    fn from_stored(stored: &StoredProvider) -> Self;
    fn from_managed_stored(_stored: &StoredProvider, _hub: ConfigHub) -> Option<Self>
    where
        Self: Sized,
    {
        None
    }
}

pub fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_jwt_exp(token: &str) -> Option<i64> {
    jwt_payload(token)?.get("exp")?.as_i64()
}

pub fn extract_account_from_id_token(id_token: &str) -> Option<String> {
    jwt_payload(id_token)?
        .get("email")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}

pub fn extract_chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = jwt_payload(access_token)?;
    payload
        .get("chatgpt_account_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            payload
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned)
}

fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(payload.as_bytes()).ok()?;
    serde_json::from_slice(&payload).ok()
}

/// Create an in-memory provider from one credential snapshot and discover models.
/// The snapshot is never refreshed or persisted.
pub async fn create_oauth_provider<P: OAuthProvider>(
    stored: &StoredProvider,
) -> Result<(Arc<P>, Vec<DiscoveredModel>)> {
    let provider = create_oauth_provider_from_snapshot_impl::<P>(stored).await?;
    let models = provider.discover_models().await;
    Ok((provider, models))
}

/// Create a provider backed by the authoritative credentials in `hub`.
pub async fn create_oauth_provider_with_hub<P: OAuthProvider>(
    stored: &StoredProvider,
    hub: ConfigHub,
) -> Result<(Arc<P>, Vec<DiscoveredModel>)> {
    let gate = credential_gate(&hub, &stored.id);
    let provider = create_oauth_provider_impl::<P>(stored, hub).await?;
    let models = provider.discover_models().await;
    gate.ensure_not_quarantined()?;
    Ok((provider, models))
}

/// Create an in-memory provider and discover capability metadata.
/// The snapshot is never refreshed or persisted.
pub async fn create_oauth_provider_with_details<P: OAuthProvider>(
    stored: &StoredProvider,
) -> Result<(Arc<P>, Vec<DiscoveredModelDetails>)> {
    let provider = create_oauth_provider_from_snapshot_impl::<P>(stored).await?;
    let models = provider
        .try_discover_models()
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok((provider, models))
}

/// Create a managed provider and discover capability metadata.
pub async fn create_oauth_provider_with_details_and_hub<P: OAuthProvider>(
    stored: &StoredProvider,
    hub: ConfigHub,
) -> Result<(Arc<P>, Vec<DiscoveredModelDetails>)> {
    let gate = credential_gate(&hub, &stored.id);
    let provider = create_oauth_provider_impl::<P>(stored, hub).await?;
    let models = provider
        .try_discover_models()
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    gate.ensure_not_quarantined()?;
    Ok((provider, models))
}

/// Create an in-memory provider without model discovery.
/// The snapshot is never refreshed or persisted.
pub async fn create_oauth_provider_no_discover<P: OAuthProvider>(
    stored: &StoredProvider,
) -> Result<Arc<P>> {
    create_oauth_provider_from_snapshot_impl::<P>(stored).await
}

/// Create a managed provider without model discovery.
pub async fn create_oauth_provider_no_discover_with_hub<P: OAuthProvider>(
    stored: &StoredProvider,
    hub: ConfigHub,
) -> Result<Arc<P>> {
    create_oauth_provider_impl::<P>(stored, hub).await
}

/// Constructs a managed provider without loading or refreshing its credentials.
///
/// Disabled and expired snapshots remain valid until the provider acquires a
/// credential at a request boundary. A provider quarantined by a credential
/// panic cannot be reconstructed in the same process.
pub fn create_managed_oauth_provider_from_stored<P: OAuthProvider>(
    stored: &StoredProvider,
    hub: ConfigHub,
) -> Result<Arc<P>> {
    ensure_provider_kind::<P>(stored)?;
    let gate = credential_gate(&hub, &stored.id);
    gate.ensure_not_quarantined()?;
    let provider = P::from_managed_stored(stored, hub).ok_or_else(|| {
        anyhow::anyhow!(
            "provider `{}` does not support managed OAuth credentials",
            stored.id
        )
    })?;
    gate.ensure_not_quarantined()?;
    Ok(Arc::new(provider))
}

/// Constructs the managed runtime provider supported by a stored auth record.
///
/// Construction does not load or refresh credentials. The provider resolves
/// its credential at a request boundary. Quarantined provider identities are rejected.
pub fn create_supported_managed_oauth_provider(
    stored: &StoredProvider,
    hub: ConfigHub,
) -> Result<Arc<dyn Provider>> {
    match stored.kind {
        ProviderKind::Codex => Ok(create_managed_oauth_provider_from_stored::<
            crate::providers::codex::CodexProvider,
        >(stored, hub)?),
        ref kind => anyhow::bail!("managed OAuth provider kind `{kind:?}` is not supported"),
    }
}

async fn create_oauth_provider_impl<P: OAuthProvider>(
    stored: &StoredProvider,
    hub: ConfigHub,
) -> Result<Arc<P>> {
    ensure_provider_kind::<P>(stored)?;
    let gate = credential_gate(&hub, &stored.id);
    gate.ensure_not_quarantined()?;
    let validator = OAuthCredentialRefresh {
        provider_id: stored.id.clone(),
        expected_kind: P::KIND.clone(),
        hub: hub.clone(),
        refresher: Arc::new(|refresh_token| P::refresh_token(&refresh_token)),
        gate,
    };
    let (authoritative, _) = validator.load_state_async().await?;
    validator.gate.ensure_not_quarantined()?;
    let provider = Arc::new(P::from_managed_stored(&authoritative, hub).ok_or_else(|| {
        anyhow::anyhow!(
            "provider `{}` does not support managed OAuth credentials",
            stored.id
        )
    })?);
    validator.gate.ensure_not_quarantined()?;
    Ok(provider)
}

async fn create_oauth_provider_from_snapshot_impl<P: OAuthProvider>(
    stored: &StoredProvider,
) -> Result<Arc<P>> {
    ensure_provider_kind::<P>(stored)?;
    if stored.expires_at <= unix_timestamp()? {
        return Err(OAuthCredentialError::SnapshotExpired(stored.id.clone()).into());
    }
    Ok(Arc::new(P::from_stored(stored)))
}

fn ensure_provider_kind<P: OAuthProvider>(stored: &StoredProvider) -> Result<()> {
    if stored.kind != P::KIND {
        return Err(OAuthCredentialError::KindChanged(stored.id.clone()).into());
    }
    Ok(())
}

pub fn callback_page(ok: bool, title: &str, message: &str) -> String {
    let icon = if ok { "✓" } else { "✗" };
    let color = if ok { "#0078a0" } else { "#c0392b" };
    let acetate = if ok {
        "rgba(0,120,160,0.10)"
    } else {
        "rgba(192,57,43,0.10)"
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title} — atman</title>
<style>
  * {{ margin:0; padding:0; box-sizing:border-box; }}
  body {{
    font-family: "JetBrains Mono","Fira Code",Menlo,Consolas,monospace;
    background: linear-gradient(180deg,#f0f0f0 0%,#e8e8ec 100%);
    min-height: 100vh; display:flex; align-items:center; justify-content:center;
  }}
  .card {{
    background: #fff; border-radius: 12px; padding: 36px 44px;
    box-shadow: 0 2px 8px rgba(0,0,0,.06);
    text-align: center; max-width: 520px;
    border-top: 3px solid {color};
  }}
  .logo {{ margin-bottom: 20px; }}
  .logo pre {{
    font-size: 6.5px; line-height: 1.15; color: #0078a0;
    font-family: "JetBrains Mono","Fira Code",Menlo,Consolas,monospace;
  }}
  .icon {{
    font-size: 36px; color: {color}; margin-bottom: 16px;
    display: inline-block; width: 56px; height: 56px; line-height: 56px;
    border-radius: 50%; background: {acetate};
  }}
  h1 {{ font-size: 18px; font-weight: 600; color: #1e1e1e; margin-bottom: 8px; }}
  p  {{ font-size: 13px; color: #606060; line-height: 1.6; }}
</style>
</head>
<body>
<div class="card">
  <div class="logo"><pre>
      ⢀⡤⣾⢿⡿⢿⡿⣷⢤⡀                                           
     ⢠⢯⢎⠞⡵⠚⠓⢮⠳⡱⡽⡄                                          
     ⡟⡏⡏⣀⣳⣀⣀⣞⣀⡰⢹⢻    ████████╗███╗   ███╗ █████╗ ███╗   ██╗
  ⢀⣠⡄⣧⣇⡇⠻⠿⠿⠿⠿⠿⢿⡿⣷⣦⣄⡀ ╚══██╔══╝████╗ ████║██╔══██╗████╗  ██║
⢀⡴⡫⡪⠕⠹⡼⡜⡄    ⢠⢢⢮⠍⠺⢗⢝⢦⡀  ██║   ██╔████╔██║███████║██╔██╗ ██║
⡞⡞⡞   ⠙⣝⢞⢦⡀⢀⡴⡳⣫⠋   ⢳⢳⢳  ██║   ██║╚██╔╝██║██╔══██║██║╚██╗██║
⢧⢧⡣⡀   ⠈⣓⡡⣔⣽⡪⢞⠁   ⢀⢜⡼⡼  ██║   ██║     ██║██║  ██║██║ ╚████║
⠈⠓⠿⣾⣿⣿⣿⣿⡿⠿⠛⠙⠾⢷⣿⣿⣿⣿⣷⠿⠚⠁  ╚═╝   ╚═╝     ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝
</pre></div>
  <div class="icon">{icon}</div>
  <h1>{title}</h1>
  <p>{message}</p>
</div>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    struct SnapshotOAuthProvider {
        access_token: String,
        display_account: Option<String>,
    }

    impl Provider for SnapshotOAuthProvider {
        fn name(&self) -> &str {
            "snapshot-oauth"
        }

        fn call<'a>(
            &'a self,
            _req: crate::provider::LlmRequest,
        ) -> crate::tool::BoxFut<
            'a,
            std::result::Result<crate::provider::AssistantMessage, crate::error::RuntimeError>,
        > {
            Box::pin(async {
                Err(crate::error::RuntimeError::ToolFailed(
                    "unused test provider".into(),
                ))
            })
        }

        fn call_streaming(
            &self,
            _req: crate::provider::LlmRequest,
        ) -> crate::event::Observable<crate::provider::AssistantMessage> {
            panic!("unused test provider")
        }
    }

    impl OAuthProvider for SnapshotOAuthProvider {
        const KIND: ProviderKind = ProviderKind::Codex;

        fn authorize_url() -> (String, Pkce, String) {
            panic!("unused test provider")
        }

        fn exchange_code(_code: &str, _verifier: &str) -> RefreshFuture {
            Box::pin(async { panic!("unused test provider") })
        }

        fn refresh_token(_token: &str) -> RefreshFuture {
            Box::pin(async { panic!("snapshot provider must not refresh credentials") })
        }

        fn from_stored(stored: &StoredProvider) -> Self {
            Self {
                access_token: stored.access_token.clone(),
                display_account: stored.account.clone(),
            }
        }
    }

    static MANAGED_REFRESH_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct ManagedOAuthProvider {
        lease: OAuthCredentialLease,
    }

    impl ManagedOAuthProvider {
        async fn acquire(&self) -> std::result::Result<OAuthCredential, OAuthCredentialError> {
            self.lease.acquire().await
        }
    }

    impl Provider for ManagedOAuthProvider {
        fn name(&self) -> &str {
            "managed-oauth"
        }

        fn call<'a>(
            &'a self,
            _req: crate::provider::LlmRequest,
        ) -> crate::tool::BoxFut<
            'a,
            std::result::Result<crate::provider::AssistantMessage, crate::error::RuntimeError>,
        > {
            Box::pin(async {
                Err(crate::error::RuntimeError::ToolFailed(
                    "unused test provider".into(),
                ))
            })
        }

        fn call_streaming(
            &self,
            _req: crate::provider::LlmRequest,
        ) -> crate::event::Observable<crate::provider::AssistantMessage> {
            panic!("unused test provider")
        }
    }

    impl OAuthProvider for ManagedOAuthProvider {
        const KIND: ProviderKind = ProviderKind::Codex;

        fn authorize_url() -> (String, Pkce, String) {
            panic!("unused test provider")
        }

        fn exchange_code(_code: &str, _verifier: &str) -> RefreshFuture {
            Box::pin(async { panic!("unused test provider") })
        }

        fn refresh_token(token: &str) -> RefreshFuture {
            assert_eq!(token, "refresh-v1");
            MANAGED_REFRESH_CALLS.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(refreshed_tokens()) })
        }

        fn from_stored(_stored: &StoredProvider) -> Self {
            panic!("managed test provider requires a config hub")
        }

        fn from_managed_stored(stored: &StoredProvider, hub: ConfigHub) -> Option<Self> {
            Some(Self {
                lease: OAuthCredentialLease::new::<Self>(&stored.id, hub),
            })
        }
    }

    struct QuarantiningProvider {
        provider_id: String,
        hub: ConfigHub,
    }

    impl Provider for QuarantiningProvider {
        fn name(&self) -> &str {
            &self.provider_id
        }

        fn call<'a>(
            &'a self,
            _req: crate::provider::LlmRequest,
        ) -> crate::tool::BoxFut<
            'a,
            std::result::Result<crate::provider::AssistantMessage, crate::error::RuntimeError>,
        > {
            Box::pin(async {
                Err(crate::error::RuntimeError::ToolFailed(
                    "unused test provider".into(),
                ))
            })
        }

        fn call_streaming(
            &self,
            _req: crate::provider::LlmRequest,
        ) -> crate::event::Observable<crate::provider::AssistantMessage> {
            panic!("unused test provider")
        }

        fn discover_models(&self) -> crate::tool::BoxFut<'static, Vec<DiscoveredModel>> {
            let provider_id = self.provider_id.clone();
            let hub = self.hub.clone();
            Box::pin(async move {
                credential_gate(&hub, &provider_id).quarantine();
                Vec::new()
            })
        }

        fn try_discover_models(
            &self,
        ) -> crate::tool::BoxFut<
            'static,
            std::result::Result<Vec<DiscoveredModelDetails>, crate::provider::ModelDiscoveryError>,
        > {
            let provider_id = self.provider_id.clone();
            let hub = self.hub.clone();
            Box::pin(async move {
                credential_gate(&hub, &provider_id).quarantine();
                Ok(Vec::new())
            })
        }
    }

    impl OAuthProvider for QuarantiningProvider {
        const KIND: ProviderKind = ProviderKind::Codex;

        fn authorize_url() -> (String, Pkce, String) {
            panic!("unused test provider")
        }

        fn exchange_code(_code: &str, _verifier: &str) -> RefreshFuture {
            Box::pin(async { panic!("unused test provider") })
        }

        fn refresh_token(_token: &str) -> RefreshFuture {
            Box::pin(async { panic!("unused test provider") })
        }

        fn from_stored(_stored: &StoredProvider) -> Self {
            panic!("unused test provider")
        }

        fn from_managed_stored(stored: &StoredProvider, hub: ConfigHub) -> Option<Self> {
            if stored.id.ends_with("construction") {
                credential_gate(&hub, &stored.id).quarantine();
            }
            Some(Self {
                provider_id: stored.id.clone(),
                hub,
            })
        }
    }

    fn expired_provider(id: &str) -> StoredProvider {
        StoredProvider {
            id: id.into(),
            name: "OAuth account".into(),
            kind: ProviderKind::Codex,
            access_token: "access-v1".into(),
            refresh_token: Some("refresh-v1".into()),
            expires_at: chrono::Utc::now().timestamp() - 1,
            account: Some("display-v1@example.test".into()),
            enabled: true,
            model_cache: None,
        }
    }

    fn hub_with_provider(provider: StoredProvider) -> (tempfile::TempDir, ConfigHub) {
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        hub.add_auth_provider(provider).unwrap();
        (dir, hub)
    }

    fn refreshed_tokens() -> TokenResult {
        TokenResult {
            access_token: "access-v2".into(),
            refresh_token: Some("refresh-v2".into()),
            expires_at: chrono::Utc::now().timestamp() + 3_600,
            account: Some("display-v2@example.test".into()),
        }
    }

    #[test]
    fn oauth_credential_debug_redacts_secrets_and_account() {
        let credential = OAuthCredential {
            access_token: "secret-access-token".into(),
            display_account: Some("person@example.test".into()),
        };

        let debug = format!("{credential:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("secret-access-token"));
        assert!(!debug.contains("person@example.test"));
    }

    #[test]
    fn pkce_generates_43char_verifier_and_valid_challenge() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier.len(), 43);
        assert!(!pkce.challenge.is_empty());
        let mut hasher = Sha256::new();
        hasher.update(pkce.verifier.as_bytes());
        let digest = hasher.finalize();
        let expected = URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn generate_state_is_32_hex_chars() {
        let state = generate_state();
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_jwt_exp_works() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"exp":123456789,"other":"data"}"#.as_bytes());
        let token = format!("header.{payload}.sig");
        assert_eq!(parse_jwt_exp(&token), Some(123456789));
    }

    #[test]
    fn parse_jwt_exp_returns_none_when_missing() {
        assert_eq!(parse_jwt_exp("not.a.jwt"), None);
        assert_eq!(parse_jwt_exp(""), None);
    }

    #[test]
    fn extract_account_prefers_email() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"email":"a@b.com","sub":"123"}"#.as_bytes());
        let token = format!("h.{payload}.sig");
        assert_eq!(
            extract_account_from_id_token(&token),
            Some("a@b.com".to_string())
        );
    }

    #[test]
    fn extract_account_returns_none_when_missing() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"name":"John"}"#.as_bytes());
        let token = format!("h.{payload}.sig");
        assert_eq!(extract_account_from_id_token(&token), None);
    }

    #[test]
    fn extracts_chatgpt_account_id_from_access_token_claims() {
        let nested = URL_SAFE_NO_PAD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-nested"}}"#);
        let top_level = URL_SAFE_NO_PAD.encode(
            r#"{"chatgpt_account_id":"account-top","https://api.openai.com/auth":{"chatgpt_account_id":"account-nested"}}"#,
        );

        assert_eq!(
            extract_chatgpt_account_id(&format!("h.{nested}.s")),
            Some("account-nested".into())
        );
        assert_eq!(
            extract_chatgpt_account_id(&format!("h.{top_level}.s")),
            Some("account-top".into())
        );
    }

    #[test]
    fn refresh_file_lock_serializes_independent_openers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refresh.lock");
        let first = RefreshFileLock(
            open_refresh_file_lock_with_timeout(&path, std::time::Duration::from_millis(50))
                .unwrap(),
        );

        let error =
            open_refresh_file_lock_with_timeout(&path, std::time::Duration::from_millis(25))
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(first);
        let _second = RefreshFileLock(
            open_refresh_file_lock_with_timeout(&path, std::time::Duration::from_millis(50))
                .unwrap(),
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn concurrent_acquire_refreshes_once_and_persists_rotation() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let calls = calls.clone();
                let started = started.clone();
                let proceed = proceed.clone();
                move |refresh_token| {
                    let calls = calls.clone();
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        assert_eq!(refresh_token, "refresh-v1");
                        calls.fetch_add(1, Ordering::SeqCst);
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        Ok(refreshed_tokens())
                    })
                }
            },
        );

        let first = tokio::spawn({
            let lease = lease.clone();
            async move { lease.acquire().await }
        });
        started.acquire().await.unwrap().forget();
        let mut rest = Vec::new();
        for _ in 0..7 {
            let lease = lease.clone();
            rest.push(tokio::spawn(async move { lease.acquire().await }));
        }
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        proceed.add_permits(8);

        let mut credentials = vec![first.await.unwrap().unwrap()];
        for task in rest {
            credentials.push(task.await.unwrap().unwrap());
        }
        assert!(credentials.iter().all(|credential| {
            credential.access_token == "access-v2"
                && credential.display_account.as_deref() == Some("display-v2@example.test")
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let stored = hub.load_auth().unwrap().providers.remove(0);
        assert_eq!(stored.access_token, "access-v2");
        assert_eq!(stored.refresh_token.as_deref(), Some("refresh-v2"));
        assert_eq!(stored.account.as_deref(), Some("display-v2@example.test"));
    }

    #[tokio::test]
    async fn concurrent_refresh_failure_is_shared_and_cooled_down() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease =
            OAuthCredentialLease::with_refresher("oauth-account", ProviderKind::Codex, hub, {
                let calls = calls.clone();
                let started = started.clone();
                let proceed = proceed.clone();
                move |_| {
                    let calls = calls.clone();
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        anyhow::bail!("refresh service unavailable")
                    })
                }
            });

        let first = tokio::spawn({
            let lease = lease.clone();
            async move { lease.acquire().await }
        });
        started.acquire().await.unwrap().forget();
        let mut rest = Vec::new();
        for _ in 0..7 {
            let lease = lease.clone();
            rest.push(tokio::spawn(async move { lease.acquire().await }));
        }
        tokio::task::yield_now().await;
        proceed.add_permits(1);

        let expected = first.await.unwrap().unwrap_err().to_string();
        assert!(expected.contains("refresh service unavailable"));
        for task in rest {
            assert_eq!(task.await.unwrap().unwrap_err().to_string(), expected);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let cooldown_error = lease.acquire().await.unwrap_err().to_string();
        assert_eq!(cooldown_error, expected);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_refresh_panic_quarantines_before_shared_completion() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease =
            OAuthCredentialLease::with_refresher("oauth-account", ProviderKind::Codex, hub, {
                let calls = calls.clone();
                let started = started.clone();
                let proceed = proceed.clone();
                move |_| {
                    let calls = calls.clone();
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        panic!("refresh panic fixture")
                    })
                }
            });

        let flight = lease.shared_refresh().unwrap();
        let joined = Arc::new(Semaphore::new(0));
        let mut waiters = Vec::new();
        for _ in 0..8 {
            let joined = joined.clone();
            let mut flight = Box::pin(flight.clone());
            waiters.push(tokio::spawn(async move {
                let mut announced = false;
                std::future::poll_fn(move |context| {
                    let result = flight.as_mut().poll(context);
                    if !announced && result.is_pending() {
                        announced = true;
                        joined.add_permits(1);
                    }
                    result
                })
                .await
            }));
        }
        started.acquire().await.unwrap().forget();
        joined.acquire_many(8).await.unwrap().forget();
        proceed.add_permits(1);

        let first = waiters.remove(0);
        let first_result = tokio::time::timeout(std::time::Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            first_result,
            SharedRefreshOutcome::Quarantined(OAuthCredentialError::Quarantined(provider))
                if provider == "oauth-account"
        ));
        assert!(matches!(
            lease.acquire().await,
            Err(OAuthCredentialError::Quarantined(provider)) if provider == "oauth-account"
        ));
        for task in waiters {
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap(),
                SharedRefreshOutcome::Quarantined(OAuthCredentialError::Quarantined(provider))
                    if provider == "oauth-account"
            ));
        }
        tokio::time::sleep(REFRESH_FAILURE_COOLDOWN * 2).await;
        assert!(matches!(
            lease.acquire().await,
            Err(OAuthCredentialError::Quarantined(provider)) if provider == "oauth-account"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn synchronous_refresher_panic_quarantines_provider() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let lease =
            OAuthCredentialLease::with_refresher("oauth-account", ProviderKind::Codex, hub, |_| {
                panic!("synchronous refresher panic fixture")
            });

        assert!(matches!(
            lease.acquire().await,
            Err(OAuthCredentialError::Quarantined(provider)) if provider == "oauth-account"
        ));
    }

    #[tokio::test]
    async fn cancelled_waiter_cannot_drop_refresh_panic_quarantine() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let calls = calls.clone();
                let started = started.clone();
                let proceed = proceed.clone();
                move |_| {
                    let calls = calls.clone();
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        panic!("detached refresh panic fixture")
                    })
                }
            },
        );

        let waiter = tokio::spawn({
            let lease = lease.clone();
            async move { lease.acquire().await }
        });
        started.acquire().await.unwrap().forget();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        proceed.add_permits(1);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if lease.gate.ensure_not_quarantined().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(lease);
        let rebuilt =
            OAuthCredentialLease::with_refresher("oauth-account", ProviderKind::Codex, hub, |_| {
                Box::pin(async { Ok(refreshed_tokens()) })
            });
        assert!(matches!(
            rebuilt.acquire().await,
            Err(OAuthCredentialError::Quarantined(provider)) if provider == "oauth-account"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_blocking_waiter_cannot_drop_quarantine() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            |_| Box::pin(async { Ok(refreshed_tokens()) }),
        );
        let worker = lease.refresh_worker();
        let gate = lease.gate.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(async move {
            let result: std::result::Result<(), OAuthCredentialError> = worker
                .run_blocking(move || {
                    let _ = started_tx.send(());
                    proceed_rx.recv().unwrap();
                    panic!("blocking credential panic fixture")
                })
                .await;
            result
        });
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        proceed_tx.send(()).unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if gate.ensure_not_quarantined().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let rebuilt =
            OAuthCredentialLease::with_refresher("oauth-account", ProviderKind::Codex, hub, |_| {
                Box::pin(async { Ok(refreshed_tokens()) })
            });
        assert!(matches!(
            rebuilt.acquire().await,
            Err(OAuthCredentialError::Quarantined(provider)) if provider == "oauth-account"
        ));
    }

    #[tokio::test]
    async fn quarantine_survives_fresh_credentials_and_is_scoped_by_path_and_id() {
        let (dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            |_| Box::pin(async { Ok(refreshed_tokens()) }),
        );
        lease.gate.quarantine();
        assert!(
            hub.update_auth_tokens(
                "oauth-account",
                AuthTokenUpdate {
                    access_token: "fresh-access".into(),
                    refresh_token: Some("fresh-refresh".into()),
                    expires_at: chrono::Utc::now().timestamp() + 3_600,
                    account: None,
                },
            )
            .unwrap()
        );
        drop(lease);

        let reloaded_hub = ConfigHub::from_config_dir(dir.path());
        let rebuilt = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            reloaded_hub.clone(),
            |_| panic!("quarantined refresher must not run"),
        );
        assert!(matches!(
            rebuilt.acquire().await,
            Err(OAuthCredentialError::Quarantined(provider)) if provider == "oauth-account"
        ));

        let mut peer = expired_provider("peer-account");
        peer.expires_at = chrono::Utc::now().timestamp() + 3_600;
        reloaded_hub.add_auth_provider(peer).unwrap();
        let peer = OAuthCredentialLease::with_refresher(
            "peer-account",
            ProviderKind::Codex,
            reloaded_hub,
            |_| panic!("fresh peer credentials must not refresh"),
        );
        assert_eq!(peer.acquire().await.unwrap().access_token, "access-v1");

        let mut independent = expired_provider("oauth-account");
        independent.expires_at = chrono::Utc::now().timestamp() + 3_600;
        let (_other_dir, other_hub) = hub_with_provider(independent);
        let independent = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            other_hub,
            |_| panic!("fresh independent credentials must not refresh"),
        );
        assert_eq!(
            independent.acquire().await.unwrap().access_token,
            "access-v1"
        );
    }

    #[test]
    fn healthy_gate_is_not_pinned_by_the_global_registry() {
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        let gate = credential_gate(&hub, "healthy-account");
        let weak = Arc::downgrade(&gate);

        drop(gate);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn managed_factories_reject_quarantined_provider() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        credential_gate(&hub, "oauth-account").quarantine();
        let stored = hub.load_auth().unwrap().providers.remove(0);

        let sync_error =
            create_managed_oauth_provider_from_stored::<ManagedOAuthProvider>(&stored, hub.clone())
                .err()
                .expect("quarantined sync factory succeeded");
        assert!(sync_error.to_string().contains("was quarantined"));

        let async_error =
            create_oauth_provider_no_discover_with_hub::<ManagedOAuthProvider>(&stored, hub)
                .await
                .err()
                .expect("quarantined async factory succeeded");
        assert!(async_error.to_string().contains("was quarantined"));
    }

    #[tokio::test]
    async fn managed_factories_recheck_quarantine_after_construction() {
        let (_sync_dir, sync_hub) = hub_with_provider(expired_provider("sync-construction"));
        let sync_stored = sync_hub.load_auth().unwrap().providers.remove(0);
        let sync_error = create_managed_oauth_provider_from_stored::<QuarantiningProvider>(
            &sync_stored,
            sync_hub,
        )
        .err()
        .expect("sync factory ignored construction-time quarantine");
        assert!(sync_error.to_string().contains("was quarantined"));

        let (_async_dir, async_hub) = hub_with_provider(expired_provider("async-construction"));
        let async_stored = async_hub.load_auth().unwrap().providers.remove(0);
        let async_error = create_oauth_provider_no_discover_with_hub::<QuarantiningProvider>(
            &async_stored,
            async_hub,
        )
        .await
        .err()
        .expect("async factory ignored construction-time quarantine");
        assert!(async_error.to_string().contains("was quarantined"));
    }

    #[tokio::test]
    async fn managed_discovery_factories_recheck_quarantine_before_returning() {
        let (_legacy_dir, legacy_hub) = hub_with_provider(expired_provider("legacy-discovery"));
        let legacy_stored = legacy_hub.load_auth().unwrap().providers.remove(0);
        let legacy_error =
            create_oauth_provider_with_hub::<QuarantiningProvider>(&legacy_stored, legacy_hub)
                .await
                .err()
                .expect("legacy discovery returned a quarantined provider");
        assert!(legacy_error.to_string().contains("was quarantined"));

        let (_typed_dir, typed_hub) = hub_with_provider(expired_provider("typed-discovery"));
        let typed_stored = typed_hub.load_auth().unwrap().providers.remove(0);
        let typed_error = create_oauth_provider_with_details_and_hub::<QuarantiningProvider>(
            &typed_stored,
            typed_hub,
        )
        .await
        .err()
        .expect("typed discovery returned a quarantined provider");
        assert!(typed_error.to_string().contains("was quarantined"));
    }

    #[test]
    fn quarantine_discards_retry_state_and_releases_refresh_lock() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let gate = credential_gate(&hub, "oauth-account");
        let (_, snapshot) = OAuthCredentialRefresh {
            provider_id: "oauth-account".into(),
            expected_kind: ProviderKind::Codex,
            hub: hub.clone(),
            refresher: Arc::new(|_| Box::pin(async { Ok(refreshed_tokens()) })),
            gate: gate.clone(),
        }
        .load_state()
        .unwrap();
        let refresh_lock_path = refresh_lock_path(&hub, "oauth-account");
        let pending = PendingCredentialCommit {
            snapshot,
            access_token: "pending-access".into(),
            refresh_token: Some("pending-refresh".into()),
            expires_at: chrono::Utc::now().timestamp() + 3_600,
            account: None,
            _refresh_lock: Arc::new(RefreshFileLock(
                open_refresh_file_lock_with_timeout(
                    &refresh_lock_path,
                    std::time::Duration::from_millis(50),
                )
                .unwrap(),
            )),
        };
        let in_flight = futures::future::ready(SharedRefreshOutcome::Complete(
            RefreshFlightResult::complete(Err(OAuthCredentialError::Changed(
                "oauth-account".into(),
            ))),
        ))
        .boxed()
        .shared();
        {
            let mut state = gate
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.in_flight = Some(in_flight);
            state.pending_retry_active = true;
            state.failure = Some(CachedRefreshFailure {
                until: Instant::now() + REFRESH_FAILURE_COOLDOWN,
                error: OAuthCredentialError::Changed("oauth-account".into()),
            });
            state.pending = Some(pending);
        }

        gate.quarantine();
        let state = gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.in_flight.is_none());
        assert!(!state.pending_retry_active);
        assert!(state.failure.is_none());
        assert!(state.pending.is_none());
        assert!(matches!(
            state.quarantine,
            Some(OAuthCredentialError::Quarantined(ref provider))
                if provider == "oauth-account"
        ));
        drop(state);
        let _reacquired = RefreshFileLock(
            open_refresh_file_lock_with_timeout(
                &refresh_lock_path,
                std::time::Duration::from_millis(50),
            )
            .unwrap(),
        );
    }

    #[tokio::test]
    async fn refresh_persistence_survives_waiter_cancellation() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let started = started.clone();
                let proceed = proceed.clone();
                move |_| {
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        Ok(refreshed_tokens())
                    })
                }
            },
        );

        let waiter = tokio::spawn(async move { lease.acquire().await });
        started.acquire().await.unwrap().forget();
        waiter.abort();
        proceed.add_permits(1);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if hub.load_auth().unwrap().providers[0].access_token == "access-v2" {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn in_flight_refresh_cannot_resurrect_removed_provider() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let started = started.clone();
                let proceed = proceed.clone();
                move |_| {
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        Ok(refreshed_tokens())
                    })
                }
            },
        );

        let acquire = tokio::spawn(async move { lease.acquire().await });
        started.acquire().await.unwrap().forget();
        assert!(hub.remove_auth_provider("oauth-account").unwrap());
        proceed.add_permits(1);

        assert!(matches!(
            acquire.await.unwrap(),
            Err(OAuthCredentialError::Missing(provider)) if provider == "oauth-account"
        ));
        assert!(hub.load_auth().unwrap().providers.is_empty());
    }

    #[tokio::test]
    async fn in_flight_refresh_rejects_disabled_provider() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let started = started.clone();
                let proceed = proceed.clone();
                move |_| {
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        Ok(refreshed_tokens())
                    })
                }
            },
        );

        let acquire = tokio::spawn(async move { lease.acquire().await });
        started.acquire().await.unwrap().forget();
        assert!(
            hub.set_auth_provider_enabled("oauth-account", false)
                .unwrap()
        );
        proceed.add_permits(1);

        assert!(matches!(
            acquire.await.unwrap(),
            Err(OAuthCredentialError::Disabled(provider)) if provider == "oauth-account"
        ));
        let stored = hub.load_auth().unwrap().providers.remove(0);
        assert!(!stored.enabled);
        assert_eq!(stored.access_token, "access-v2");
        assert_eq!(stored.refresh_token.as_deref(), Some("refresh-v2"));
    }

    #[tokio::test]
    async fn in_flight_refresh_preserves_concurrent_catalog_update() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let started = started.clone();
                let proceed = proceed.clone();
                move |_| {
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        Ok(refreshed_tokens())
                    })
                }
            },
        );

        let acquire = tokio::spawn(async move { lease.acquire().await });
        started.acquire().await.unwrap().forget();
        assert!(
            hub.update_auth_model_cache_details(
                "oauth-account",
                "oauth-account@account",
                10,
                &[crate::provider::DiscoveredModelDetails {
                    slug: "cached-model".into(),
                    context_budget: Some(16_384),
                    capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                        crate::provider::ModelCapabilities::default(),
                    ),
                }],
            )
            .unwrap()
        );
        proceed.add_permits(1);

        assert_eq!(acquire.await.unwrap().unwrap().access_token, "access-v2");
        let stored = hub.load_auth().unwrap().providers.remove(0);
        assert_eq!(stored.access_token, "access-v2");
        assert_eq!(stored.model_cache.unwrap().models[0].slug, "cached-model");
    }

    #[tokio::test]
    async fn rotated_refresh_token_is_reused_and_preserved_when_omitted() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let refresh_inputs = Arc::new(Mutex::new(Vec::new()));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let refresh_inputs = refresh_inputs.clone();
                move |refresh_token| {
                    let call = {
                        let mut inputs = refresh_inputs
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        inputs.push(refresh_token);
                        inputs.len()
                    };
                    Box::pin(async move {
                        Ok(TokenResult {
                            access_token: format!("access-v{}", call + 1),
                            refresh_token: (call == 1).then(|| "refresh-v2".into()),
                            expires_at: chrono::Utc::now().timestamp() + 3_600,
                            account: None,
                        })
                    })
                }
            },
        );

        assert_eq!(lease.acquire().await.unwrap().access_token, "access-v2");
        assert!(
            hub.update_auth_tokens(
                "oauth-account",
                AuthTokenUpdate {
                    access_token: "access-v2".into(),
                    refresh_token: None,
                    expires_at: chrono::Utc::now().timestamp() - 1,
                    account: None,
                },
            )
            .unwrap()
        );
        assert_eq!(lease.acquire().await.unwrap().access_token, "access-v3");

        let stored = hub.load_auth().unwrap().providers.remove(0);
        assert_eq!(stored.refresh_token.as_deref(), Some("refresh-v2"));
        assert_eq!(
            *refresh_inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["refresh-v1", "refresh-v2"]
        );
    }

    #[tokio::test]
    async fn in_flight_refresh_adopts_newer_authoritative_credentials() {
        let (_dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let started = Arc::new(Semaphore::new(0));
        let proceed = Arc::new(Semaphore::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let started = started.clone();
                let proceed = proceed.clone();
                move |_| {
                    let started = started.clone();
                    let proceed = proceed.clone();
                    Box::pin(async move {
                        started.add_permits(1);
                        proceed.acquire().await.unwrap().forget();
                        Ok(refreshed_tokens())
                    })
                }
            },
        );

        let acquire = tokio::spawn(async move { lease.acquire().await });
        started.acquire().await.unwrap().forget();
        assert!(
            hub.update_auth_tokens(
                "oauth-account",
                AuthTokenUpdate {
                    access_token: "authoritative-access".into(),
                    refresh_token: Some("authoritative-refresh".into()),
                    expires_at: chrono::Utc::now().timestamp() + 3_600,
                    account: Some("authoritative@example.test".into()),
                },
            )
            .unwrap()
        );
        proceed.add_permits(1);

        let credential = acquire.await.unwrap().unwrap();
        assert_eq!(credential.access_token, "authoritative-access");
        assert_eq!(
            credential.display_account.as_deref(),
            Some("authoritative@example.test")
        );
        assert_eq!(
            hub.load_auth().unwrap().providers[0].access_token,
            "authoritative-access"
        );
    }

    #[tokio::test]
    async fn persistence_failure_holds_refresh_lock_until_detached_retry_commits() {
        let (dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let refresh_inputs = Arc::new(Mutex::new(Vec::new()));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let refresh_inputs = refresh_inputs.clone();
                move |refresh_token| {
                    refresh_inputs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(refresh_token);
                    Box::pin(async { Ok(refreshed_tokens()) })
                }
            },
        );
        let auth_lock = dir.path().join(".auth.json.lock");
        std::fs::remove_file(&auth_lock).unwrap();
        std::fs::create_dir(&auth_lock).unwrap();

        assert!(matches!(
            lease.acquire().await,
            Err(OAuthCredentialError::Persist { .. })
        ));
        let refresh_lock = refresh_lock_path(&hub, "oauth-account");
        let lock_error = open_refresh_file_lock_with_timeout(
            &refresh_lock,
            std::time::Duration::from_millis(25),
        )
        .unwrap_err();
        assert_eq!(lock_error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            hub.load_auth().unwrap().providers[0].access_token,
            "access-v1"
        );
        assert!(matches!(
            lease.acquire().await,
            Err(OAuthCredentialError::Persist { .. })
        ));
        assert_eq!(
            refresh_inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );

        drop(lease);
        std::fs::remove_dir(&auth_lock).unwrap();
        let stored = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let stored = hub.load_auth().unwrap().providers.remove(0);
                if stored.access_token == "access-v2" {
                    break stored;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(stored.access_token, "access-v2");
        assert_eq!(stored.refresh_token.as_deref(), Some("refresh-v2"));
        let _next_opener = RefreshFileLock(
            open_refresh_file_lock_with_timeout(
                &refresh_lock,
                std::time::Duration::from_millis(50),
            )
            .unwrap(),
        );
        assert_eq!(
            *refresh_inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["refresh-v1"]
        );
    }

    #[tokio::test]
    async fn active_detached_persistence_retry_returns_cached_error_to_callers() {
        let (dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let calls = Arc::new(AtomicUsize::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(refreshed_tokens()) })
                }
            },
        );
        let auth_lock_path = dir.path().join(".auth.json.lock");
        std::fs::remove_file(&auth_lock_path).unwrap();
        std::fs::create_dir(&auth_lock_path).unwrap();
        assert!(matches!(
            lease.acquire().await,
            Err(OAuthCredentialError::Persist { .. })
        ));

        std::fs::remove_dir(&auth_lock_path).unwrap();
        let auth_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&auth_lock_path)
            .unwrap();
        fs2::FileExt::lock_exclusive(&auth_lock).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let active = lease
                    .gate
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending_retry_active;
                if active {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(100), lease.acquire())
                .await
                .unwrap(),
            Err(OAuthCredentialError::Persist { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        fs2::FileExt::unlock(&auth_lock).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if hub.load_auth().unwrap().providers[0].access_token == "access-v2" {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn persistence_retry_discards_pending_after_credential_change() {
        let (dir, hub) = hub_with_provider(expired_provider("oauth-account"));
        let calls = Arc::new(AtomicUsize::new(0));
        let lease = OAuthCredentialLease::with_refresher(
            "oauth-account",
            ProviderKind::Codex,
            hub.clone(),
            {
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(refreshed_tokens()) })
                }
            },
        );
        let auth_lock = dir.path().join(".auth.json.lock");
        std::fs::remove_file(&auth_lock).unwrap();
        std::fs::create_dir(&auth_lock).unwrap();

        assert!(matches!(
            lease.acquire().await,
            Err(OAuthCredentialError::Persist { .. })
        ));
        std::fs::remove_dir(&auth_lock).unwrap();
        assert!(
            hub.update_auth_tokens(
                "oauth-account",
                AuthTokenUpdate {
                    access_token: "authoritative-access".into(),
                    refresh_token: Some("authoritative-refresh".into()),
                    expires_at: chrono::Utc::now().timestamp() + 3_600,
                    account: Some("authoritative@example.test".into()),
                },
            )
            .unwrap()
        );

        let credential = lease.acquire().await.unwrap();
        assert_eq!(credential.access_token, "authoritative-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let stored = hub.load_auth().unwrap().providers.remove(0);
        assert_eq!(stored.access_token, "authoritative-access");
        assert_eq!(
            stored.refresh_token.as_deref(),
            Some("authoritative-refresh")
        );
    }

    #[tokio::test]
    async fn expired_credentials_without_refresh_token_require_authentication() {
        let mut provider = expired_provider("oauth-account");
        provider.refresh_token = None;
        let (_dir, hub) = hub_with_provider(provider);
        let lease =
            OAuthCredentialLease::with_refresher("oauth-account", ProviderKind::Codex, hub, |_| {
                Box::pin(async { panic!("refresher must not run") })
            });

        assert!(matches!(
            lease.acquire().await,
            Err(OAuthCredentialError::ReauthenticationRequired(provider))
                if provider == "oauth-account"
        ));
    }

    #[tokio::test]
    async fn usable_access_token_without_refresh_token_is_not_rejected_early() {
        let mut provider = expired_provider("oauth-account");
        provider.refresh_token = None;
        provider.expires_at = chrono::Utc::now().timestamp() + 60;
        let (_dir, hub) = hub_with_provider(provider);
        let lease =
            OAuthCredentialLease::with_refresher("oauth-account", ProviderKind::Codex, hub, |_| {
                Box::pin(async { panic!("refresher must not run") })
            });

        assert_eq!(lease.acquire().await.unwrap().access_token, "access-v1");
    }

    #[tokio::test]
    async fn snapshot_factory_uses_valid_credentials_without_refreshing() {
        let mut stored = expired_provider("in-memory-oauth");
        stored.expires_at = chrono::Utc::now().timestamp() + 60;
        let provider = create_oauth_provider_no_discover::<SnapshotOAuthProvider>(&stored)
            .await
            .unwrap();

        assert_eq!(provider.access_token, "access-v1");
        assert_eq!(
            provider.display_account.as_deref(),
            Some("display-v1@example.test")
        );
    }

    #[tokio::test]
    async fn snapshot_factory_rejects_expired_credentials_without_refreshing() {
        let error = create_oauth_provider_no_discover::<SnapshotOAuthProvider>(&expired_provider(
            "in-memory-oauth",
        ))
        .await
        .err()
        .unwrap();

        assert!(error.to_string().contains("use a managed provider"));
    }

    #[tokio::test]
    async fn inert_managed_factory_does_not_refresh_or_require_an_enabled_provider() {
        MANAGED_REFRESH_CALLS.store(0, Ordering::SeqCst);
        let mut stored = expired_provider("inert-managed-oauth");
        stored.enabled = false;
        let (_dir, hub) = hub_with_provider(stored.clone());

        let provider =
            create_managed_oauth_provider_from_stored::<ManagedOAuthProvider>(&stored, hub)
                .unwrap();

        assert_eq!(MANAGED_REFRESH_CALLS.load(Ordering::SeqCst), 0);
        assert!(matches!(
            provider.acquire().await,
            Err(OAuthCredentialError::Disabled(provider)) if provider == "inert-managed-oauth"
        ));
        assert_eq!(MANAGED_REFRESH_CALLS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn factories_reject_wrong_provider_kind_before_construction() {
        let mut stored = expired_provider("wrong-kind");
        stored.kind = ProviderKind::AnthropicOauth;
        stored.expires_at = chrono::Utc::now().timestamp() + 3_600;

        let snapshot_error = create_oauth_provider_no_discover::<SnapshotOAuthProvider>(&stored)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            snapshot_error.downcast_ref::<OAuthCredentialError>(),
            Some(OAuthCredentialError::KindChanged(provider)) if provider == "wrong-kind"
        ));

        let (_dir, hub) = hub_with_provider(stored.clone());
        let managed_error =
            create_oauth_provider_no_discover_with_hub::<SnapshotOAuthProvider>(&stored, hub)
                .await
                .err()
                .unwrap();
        assert!(matches!(
            managed_error.downcast_ref::<OAuthCredentialError>(),
            Some(OAuthCredentialError::KindChanged(provider)) if provider == "wrong-kind"
        ));

        let mut requested = expired_provider("authoritative-wrong-kind");
        requested.expires_at = chrono::Utc::now().timestamp() + 3_600;
        let mut authoritative = requested.clone();
        authoritative.kind = ProviderKind::AnthropicOauth;
        let (_dir, hub) = hub_with_provider(authoritative);
        let authoritative_error =
            create_oauth_provider_no_discover_with_hub::<ManagedOAuthProvider>(&requested, hub)
                .await
                .err()
                .unwrap();
        assert!(matches!(
            authoritative_error.downcast_ref::<OAuthCredentialError>(),
            Some(OAuthCredentialError::KindChanged(provider))
                if provider == "authoritative-wrong-kind"
        ));
    }

    #[tokio::test]
    async fn managed_factory_preserves_pending_rotation_after_persistence_failure() {
        MANAGED_REFRESH_CALLS.store(0, Ordering::SeqCst);
        let stored = expired_provider("managed-oauth");
        let (dir, hub) = hub_with_provider(stored.clone());
        let auth_lock = dir.path().join(".auth.json.lock");
        std::fs::remove_file(&auth_lock).unwrap();
        std::fs::create_dir(&auth_lock).unwrap();

        let provider = create_oauth_provider_no_discover_with_hub::<ManagedOAuthProvider>(
            &stored,
            hub.clone(),
        )
        .await
        .unwrap();
        assert_eq!(MANAGED_REFRESH_CALLS.load(Ordering::SeqCst), 0);

        assert!(matches!(
            provider.acquire().await,
            Err(OAuthCredentialError::Persist { .. })
        ));
        assert_eq!(MANAGED_REFRESH_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(
            hub.load_auth().unwrap().providers[0].access_token,
            "access-v1"
        );

        std::fs::remove_dir(&auth_lock).unwrap();
        let credential = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match provider.acquire().await {
                    Ok(credential) => break credential,
                    Err(OAuthCredentialError::Persist { .. }) => {
                        tokio::time::sleep(
                            REFRESH_FAILURE_COOLDOWN + std::time::Duration::from_millis(25),
                        )
                        .await;
                    }
                    Err(error) => panic!("unexpected credential error: {error}"),
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(credential.access_token, "access-v2");
        assert_eq!(MANAGED_REFRESH_CALLS.load(Ordering::SeqCst), 1);
        let persisted = hub.load_auth().unwrap().providers.remove(0);
        assert_eq!(persisted.access_token, "access-v2");
        assert_eq!(persisted.refresh_token.as_deref(), Some("refresh-v2"));
    }

    #[tokio::test]
    async fn managed_factory_does_not_hide_missing_or_disabled_provider() {
        let mut stored = expired_provider("managed-oauth");
        stored.expires_at = chrono::Utc::now().timestamp() + 3_600;
        let missing_dir = tempfile::tempdir().unwrap();
        let missing_hub = ConfigHub::from_config_dir(missing_dir.path());
        let missing = create_oauth_provider_no_discover_with_hub::<ManagedOAuthProvider>(
            &stored,
            missing_hub,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(
            missing.downcast_ref::<OAuthCredentialError>(),
            Some(OAuthCredentialError::Missing(provider)) if provider == "managed-oauth"
        ));

        stored.enabled = false;
        let (_dir, disabled_hub) = hub_with_provider(stored.clone());
        let disabled = create_oauth_provider_no_discover_with_hub::<ManagedOAuthProvider>(
            &stored,
            disabled_hub,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(
            disabled.downcast_ref::<OAuthCredentialError>(),
            Some(OAuthCredentialError::Disabled(provider)) if provider == "managed-oauth"
        ));
    }

    #[tokio::test]
    async fn managed_provider_rechecks_authoritative_state_at_request_boundary() {
        let mut stored = expired_provider("managed-oauth");
        stored.expires_at = chrono::Utc::now().timestamp() + 3_600;

        let (_missing_dir, missing_hub) = hub_with_provider(stored.clone());
        let missing_provider = create_oauth_provider_no_discover_with_hub::<ManagedOAuthProvider>(
            &stored,
            missing_hub.clone(),
        )
        .await
        .unwrap();
        assert!(missing_hub.remove_auth_provider("managed-oauth").unwrap());
        assert!(matches!(
            missing_provider.acquire().await,
            Err(OAuthCredentialError::Missing(provider)) if provider == "managed-oauth"
        ));

        let (_disabled_dir, disabled_hub) = hub_with_provider(stored.clone());
        let disabled_provider = create_oauth_provider_no_discover_with_hub::<ManagedOAuthProvider>(
            &stored,
            disabled_hub.clone(),
        )
        .await
        .unwrap();
        assert!(
            disabled_hub
                .set_auth_provider_enabled("managed-oauth", false)
                .unwrap()
        );
        assert!(matches!(
            disabled_provider.acquire().await,
            Err(OAuthCredentialError::Disabled(provider)) if provider == "managed-oauth"
        ));
    }

    #[tokio::test]
    async fn managed_factory_rejects_provider_without_lease_support() {
        let mut stored = expired_provider("snapshot-only-oauth");
        stored.expires_at = chrono::Utc::now().timestamp() + 3_600;
        let (_dir, hub) = hub_with_provider(stored.clone());

        let error =
            create_oauth_provider_no_discover_with_hub::<SnapshotOAuthProvider>(&stored, hub)
                .await
                .err()
                .unwrap();
        assert!(
            error
                .to_string()
                .contains("does not support managed OAuth credentials")
        );
    }

    #[test]
    fn callback_page_ok_has_right_icon_and_color() {
        let html = callback_page(true, "OK", "done");
        assert!(html.contains("✓"));
        assert!(html.contains("#0078a0"));
    }

    #[test]
    fn callback_page_err_has_right_icon_and_color() {
        let html = callback_page(false, "Err", "fail");
        assert!(html.contains("✗"));
        assert!(html.contains("#c0392b"));
    }
}
