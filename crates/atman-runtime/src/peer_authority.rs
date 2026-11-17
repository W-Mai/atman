//! Same-machine coordination gates for peer effects and authority mutations.
//!
//! These gates provide exclusion, not authorization. Trusted callers must derive the complete participant plan, validate the original authority while holding it, and retain the guard through the original writer's commit and acknowledgement. Generations never affect lock keys.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::file_lock::FileLock;

/// The profile precedes all Session participants; Session identities use UUID ordering.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AuthorityKey {
    Profile,
    Session(Uuid),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AuthorityMode {
    Shared,
    Exclusive,
}

/// One canonical, immutable lock set. Duplicate keys use their strongest requested mode.
#[derive(Clone, Debug)]
pub struct AuthorityPlan(Arc<[(AuthorityKey, AuthorityMode)]>);

impl AuthorityPlan {
    pub fn new(
        participants: impl IntoIterator<Item = (AuthorityKey, AuthorityMode)>,
    ) -> io::Result<Self> {
        let mut plan = BTreeMap::<AuthorityKey, AuthorityMode>::new();
        for (key, mode) in participants {
            if matches!(key, AuthorityKey::Session(id) if id.is_nil()) {
                return Err(invalid("authority Session identity is nil"));
            }
            plan.entry(key)
                .and_modify(|current| *current = (*current).max(mode))
                .or_insert(mode);
        }
        if plan.is_empty() {
            return Err(invalid("authority participant plan is empty"));
        }
        Ok(Self(plan.into_iter().collect()))
    }

    pub fn participants(&self) -> &[(AuthorityKey, AuthorityMode)] {
        &self.0
    }

    pub fn covers(&self, key: AuthorityKey, mode: AuthorityMode) -> bool {
        self.0
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .is_some_and(|index| self.0[index].1 >= mode)
    }
}

struct Domain {
    data_root: PathBuf,
    profile_id: Uuid,
    attempts: tokio::sync::Semaphore,
    #[cfg(test)]
    attempted: std::sync::atomic::AtomicUsize,
}

/// A trusted, same-filesystem authority domain. Construction performs no filesystem work.
#[derive(Clone)]
pub struct PeerAuthority(Arc<Domain>);

impl PeerAuthority {
    pub fn new(data_root: PathBuf, profile_id: Uuid) -> io::Result<Self> {
        if !data_root.is_absolute() || profile_id.is_nil() {
            return Err(invalid(
                "authority requires an absolute data root and profile identity",
            ));
        }
        Ok(Self(Arc::new(Domain {
            data_root,
            profile_id,
            attempts: tokio::sync::Semaphore::new(1),
            #[cfg(test)]
            attempted: Default::default(),
        })))
    }

    pub fn profile_id(&self) -> Uuid {
        self.0.profile_id
    }

    pub fn data_root(&self) -> &Path {
        &self.0.data_root
    }

    /// Try once without waiting for contended gates. Handoff must release its local freeze on contention.
    pub async fn try_acquire(&self, plan: AuthorityPlan) -> io::Result<Option<AuthorityGuard>> {
        let authority = self.clone();
        crate::read_work::coordinate(move || {
            authority.try_all(
                plan,
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(5),
            )
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Acquire the entire plan or retain none. Cancellation never waits on a blocking OS lock.
    ///
    /// Each attempt is a short nonblocking task on the existing storage workers. Contention releases the whole partial set before an asynchronous bounded backoff. OS lock scheduling does not guarantee FIFO fairness; the deadline bounds preparation, not a held effect.
    pub async fn acquire(
        &self,
        plan: AuthorityPlan,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> io::Result<AuthorityGuard> {
        static TICKET: AtomicU64 = AtomicU64::new(0);
        let ticket = TICKET.fetch_add(1, Ordering::Relaxed) ^ u64::from(std::process::id());
        let mut round = 0u32;
        loop {
            check(&cancel, deadline)?;
            let permit = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                _ = tokio::time::sleep_until(deadline.into()) => return Err(timed_out()),
                permit = self.0.attempts.acquire() => permit.map_err(io::Error::other)?,
            };
            let authority = self.clone();
            let participants = plan.clone();
            let worker_cancel = cancel.clone();
            let attempt = crate::read_work::coordinate(move || {
                authority.try_all(participants, &worker_cancel, deadline)
            });
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                _ = tokio::time::sleep_until(deadline.into()) => return Err(timed_out()),
                result = attempt => result.map_err(io::Error::other)??,
            };
            drop(permit);
            if let Some(guard) = result {
                check(&cancel, deadline)?;
                return Ok(guard);
            }
            let delay = Duration::from_millis(
                (1u64 << round.min(5)) + ticket.wrapping_add(u64::from(round)) % 4,
            );
            round = round.saturating_add(1);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                _ = tokio::time::sleep_until(deadline.into()) => return Err(timed_out()),
                _ = tokio::time::sleep(delay) => {},
            }
        }
    }

    fn try_all(
        &self,
        plan: AuthorityPlan,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> io::Result<Option<AuthorityGuard>> {
        check(cancel, deadline)?;
        #[cfg(test)]
        self.0.attempted.fetch_add(1, Ordering::Release);
        let data_root = self.0.data_root.canonicalize()?;
        let run = data_root.join("run");
        ensure_directory(&run, false)?;
        let root = run.join("authority");
        ensure_directory(&root, true)?;
        let sessions = root.join("sessions");
        ensure_directory(&sessions, true)?;
        let mut locks = Vec::with_capacity(plan.0.len());
        for (key, mode) in plan.participants() {
            check(cancel, deadline)?;
            let path = match key {
                AuthorityKey::Profile => root.join("profile.lock"),
                AuthorityKey::Session(id) => sessions.join(format!("{id}.lock")),
            };
            let file = open_gate(&path)?;
            let identity = file_identity(&file)?;
            let locked = match mode {
                AuthorityMode::Shared => FileLock::try_shared(file),
                AuthorityMode::Exclusive => FileLock::try_exclusive(file),
            };
            let locked = match locked {
                Ok(locked) => locked,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error),
            };
            validate_gate(&path)?;
            if file_identity(&File::open(&path)?)? != identity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "authority gate identity changed",
                ));
            }
            locks.push(locked);
        }
        check(cancel, deadline)?;
        Ok(Some(AuthorityGuard {
            domain: self.0.clone(),
            data_root,
            plan,
            locks: Some(locks),
        }))
    }
}

/// A non-clonable, non-serializable exclusion bundle. It must outlive the actual target commit.
pub struct AuthorityGuard {
    domain: Arc<Domain>,
    data_root: PathBuf,
    plan: AuthorityPlan,
    locks: Option<Vec<FileLock>>,
}

impl AuthorityGuard {
    pub fn profile_id(&self) -> Uuid {
        self.domain.profile_id
    }
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }
    pub fn covers(&self, key: AuthorityKey, mode: AuthorityMode) -> bool {
        self.plan.covers(key, mode)
    }

    pub fn belongs_to(&self, authority: &PeerAuthority) -> bool {
        self.domain.profile_id == authority.0.profile_id
            && self.domain.data_root == authority.0.data_root
    }

    /// Confirm background unlock completion when a caller needs to observe handoff readiness.
    pub async fn release(mut self) -> io::Result<()> {
        if let Some(locks) = self.locks.take() {
            crate::read_work::coordinate(move || drop(locks))
                .await
                .map_err(io::Error::other)?;
        }
        Ok(())
    }
}

impl Drop for AuthorityGuard {
    fn drop(&mut self) {
        if let Some(locks) = self.locks.take() {
            // Closing file handles and dropping unread acquired bundles stay off UI callers.
            crate::read_work::retire_coordination(locks);
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn cancelled() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "authority preparation cancelled",
    )
}
fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "authority preparation timed out")
}

fn check(cancel: &CancellationToken, deadline: Instant) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(cancelled())
    } else if Instant::now() >= deadline {
        Err(timed_out())
    } else {
        Ok(())
    }
}

fn ensure_directory(path: &Path, private: bool) -> io::Result<()> {
    #[cfg(unix)]
    let mut builder = fs::DirBuilder::new();
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authority directory is not a regular directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let forbidden = if private { 0o077 } else { 0o022 };
        if metadata.permissions().mode() & forbidden != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authority directory is not private",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    Ok(())
}

fn validate_gate(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authority gate is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.nlink() != 1 || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authority gate is not private",
            ));
        }
    }
    Ok(())
}

fn open_gate(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.create_new(true).open(path) {
        Ok(file) => {
            validate_gate(path)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_gate(path)?;
            options.create_new(false).open(path)
        }
        Err(error) => Err(error),
    }
}

fn reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(unix)]
type FileIdentity = (u64, u64);
#[cfg(windows)]
type FileIdentity = (u32, u32, u32);
#[cfg(not(any(unix, windows)))]
type FileIdentity = (u64, u64);

fn file_identity(file: &File) -> io::Result<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut metadata = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut metadata) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if metadata.nNumberOfLinks != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authority gate has multiple links",
            ));
        }
        Ok((
            metadata.dwVolumeSerialNumber,
            metadata.nFileIndexHigh,
            metadata.nFileIndexLow,
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "authority file identity is unavailable",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    fn plan(entries: &[(AuthorityKey, AuthorityMode)]) -> AuthorityPlan {
        AuthorityPlan::new(entries.iter().copied()).unwrap()
    }

    async fn acquire(authority: &PeerAuthority, plan: AuthorityPlan) -> AuthorityGuard {
        authority
            .acquire(
                plan,
                CancellationToken::new(),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap()
    }

    #[test]
    fn plans_coalesce_the_strongest_mode_and_order_all_participants() {
        let a = AuthorityKey::Session(Uuid::from_u128(1));
        let b = AuthorityKey::Session(Uuid::from_u128(2));
        let plan = plan(&[
            (b, AuthorityMode::Shared),
            (a, AuthorityMode::Shared),
            (AuthorityKey::Profile, AuthorityMode::Shared),
            (a, AuthorityMode::Exclusive),
        ]);
        assert_eq!(
            plan.participants(),
            &[
                (AuthorityKey::Profile, AuthorityMode::Shared),
                (a, AuthorityMode::Exclusive),
                (b, AuthorityMode::Shared)
            ]
        );
        assert!(plan.covers(a, AuthorityMode::Shared));
        assert!(plan.covers(a, AuthorityMode::Exclusive));
        assert!(!plan.covers(b, AuthorityMode::Exclusive));
        assert!(AuthorityPlan::new([]).is_err());
        assert!(
            AuthorityPlan::new([(AuthorityKey::Session(Uuid::nil()), AuthorityMode::Shared)])
                .is_err()
        );
    }

    #[tokio::test]
    async fn contention_releases_partial_groups_and_cancelled_waits_leave_workers_available() {
        let root = tempfile::tempdir().unwrap();
        let authority = PeerAuthority::new(root.path().to_owned(), Uuid::now_v7()).unwrap();
        let a = AuthorityKey::Session(Uuid::from_u128(1));
        let b = AuthorityKey::Session(Uuid::from_u128(2));
        let held = acquire(&authority, plan(&[(b, AuthorityMode::Exclusive)])).await;
        let attempts = authority.0.attempted.load(Ordering::Acquire);
        let cancel = CancellationToken::new();
        let waiting_authority = authority.clone();
        let waiting_cancel = cancel.clone();
        let waiting = tokio::spawn(async move {
            waiting_authority
                .acquire(
                    plan(&[(a, AuthorityMode::Shared), (b, AuthorityMode::Exclusive)]),
                    waiting_cancel,
                    Instant::now() + Duration::from_secs(30),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while authority.0.attempted.load(Ordering::Acquire) <= attempts {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), crate::read_work::coordinate(|| ()))
            .await
            .unwrap()
            .unwrap();
        let independent = acquire(&authority, plan(&[(a, AuthorityMode::Exclusive)])).await;
        independent.release().await.unwrap();
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let error = authority
            .acquire(
                plan(&[(b, AuthorityMode::Exclusive)]),
                CancellationToken::new(),
                Instant::now() + Duration::from_millis(20),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        let runtime_authority = authority.clone();
        let started = authority.0.attempted.load(Ordering::Acquire);
        let (stopped, observed) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let pending = runtime_authority.clone();
                tokio::spawn(async move {
                    pending
                        .acquire(
                            plan(&[(b, AuthorityMode::Exclusive)]),
                            CancellationToken::new(),
                            Instant::now() + Duration::from_secs(30),
                        )
                        .await
                });
                while runtime_authority.0.attempted.load(Ordering::Acquire) <= started {
                    tokio::task::yield_now().await;
                }
            });
            drop(runtime);
            let _ = stopped.send(());
        });
        tokio::time::timeout(Duration::from_secs(5), observed)
            .await
            .unwrap()
            .unwrap();
        thread.join().unwrap();
        held.release().await.unwrap();
        acquire(
            &authority,
            plan(&[(a, AuthorityMode::Exclusive), (b, AuthorityMode::Exclusive)]),
        )
        .await
        .release()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn guard_release_preserves_the_same_private_gate_inode() {
        let root = tempfile::tempdir().unwrap();
        let profile = Uuid::now_v7();
        let authority = PeerAuthority::new(root.path().to_owned(), profile).unwrap();
        let session = Uuid::now_v7();
        let participants = plan(&[
            (AuthorityKey::Profile, AuthorityMode::Shared),
            (AuthorityKey::Session(session), AuthorityMode::Exclusive),
        ]);
        let guard = acquire(&authority, participants.clone()).await;
        assert!(guard.belongs_to(&authority));
        assert_eq!(guard.profile_id(), profile);
        assert_eq!(guard.data_root(), root.path().canonicalize().unwrap());
        let path = root
            .path()
            .join("run/authority/sessions")
            .join(format!("{session}.lock"));
        let before = file_identity(&File::open(&path).unwrap()).unwrap();
        guard.release().await.unwrap();
        let recreated = PeerAuthority::new(root.path().to_owned(), profile).unwrap();
        let guard = acquire(&recreated, participants).await;
        assert!(guard.covers(AuthorityKey::Session(session), AuthorityMode::Exclusive));
        assert_eq!(file_identity(&File::open(&path).unwrap()).unwrap(), before);
        assert_eq!(File::open(&path).unwrap().metadata().unwrap().len(), 0);
        guard.release().await.unwrap();
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(root.path().join("outside"), &path).unwrap();
            let error = authority
                .acquire(
                    plan(&[(AuthorityKey::Session(session), AuthorityMode::Shared)]),
                    CancellationToken::new(),
                    Instant::now() + Duration::from_secs(5),
                )
                .await
                .err()
                .unwrap();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::InvalidData | io::ErrorKind::NotFound
            ));
        }
    }

    async fn child(
        root: &Path,
        profile: Uuid,
        a: Uuid,
        b: Uuid,
        mode: &str,
    ) -> (
        tokio::process::Child,
        tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    ) {
        let mut process = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "peer_authority::tests::child_process_lock",
                "--nocapture",
            ])
            .env("ATMAN_PEER_GATE_FIXTURE", root)
            .env("ATMAN_PEER_GATE_PROFILE", profile.to_string())
            .env("ATMAN_PEER_GATE_A", a.to_string())
            .env("ATMAN_PEER_GATE_B", b.to_string())
            .env("ATMAN_PEER_GATE_MODE", mode)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(process.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("child exited before acquiring its gate");
                if line.contains("PEER_GATE_READY") {
                    break;
                }
            }
        })
        .await
        .unwrap();
        (process, lines)
    }

    #[tokio::test]
    async fn processes_share_readers_and_opposite_plans_complete_without_lock_cycles() {
        let root = tempfile::tempdir().unwrap();
        let profile = Uuid::now_v7();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let authority = PeerAuthority::new(root.path().to_owned(), profile).unwrap();
        let (mut process, _output) = child(root.path(), profile, a, b, "hold-shared").await;
        acquire(
            &authority,
            plan(&[(AuthorityKey::Session(a), AuthorityMode::Shared)]),
        )
        .await
        .release()
        .await
        .unwrap();
        let error = authority
            .acquire(
                plan(&[(AuthorityKey::Session(a), AuthorityMode::Exclusive)]),
                CancellationToken::new(),
                Instant::now() + Duration::from_millis(20),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        process.stdin.take().unwrap().write_all(b"x").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), process.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        let (mut process, _output) = child(root.path(), profile, a, b, "opposite").await;
        process.stdin.take().unwrap().write_all(b"x").await.unwrap();
        for _ in 0..12 {
            acquire(
                &authority,
                plan(&[
                    (AuthorityKey::Session(a), AuthorityMode::Shared),
                    (AuthorityKey::Session(b), AuthorityMode::Exclusive),
                ]),
            )
            .await
            .release()
            .await
            .unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(15), process.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn child_process_lock() {
        let Some(root) = std::env::var_os("ATMAN_PEER_GATE_FIXTURE") else {
            return;
        };
        let profile = std::env::var("ATMAN_PEER_GATE_PROFILE")
            .unwrap()
            .parse()
            .unwrap();
        let a = std::env::var("ATMAN_PEER_GATE_A").unwrap().parse().unwrap();
        let b = std::env::var("ATMAN_PEER_GATE_B").unwrap().parse().unwrap();
        let mode = std::env::var("ATMAN_PEER_GATE_MODE").unwrap();
        let authority = PeerAuthority::new(root.into(), profile).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let held = if mode == "hold-shared" {
                Some(
                    acquire(
                        &authority,
                        plan(&[(AuthorityKey::Session(a), AuthorityMode::Shared)]),
                    )
                    .await,
                )
            } else {
                None
            };
            println!("PEER_GATE_READY");
            std::io::stdout().flush().unwrap();
            std::io::stdin().read_exact(&mut [0]).unwrap();
            if let Some(held) = held {
                held.release().await.unwrap();
            } else {
                assert_eq!(mode, "opposite");
                for _ in 0..12 {
                    acquire(
                        &authority,
                        plan(&[
                            (AuthorityKey::Session(b), AuthorityMode::Shared),
                            (AuthorityKey::Session(a), AuthorityMode::Exclusive),
                        ]),
                    )
                    .await
                    .release()
                    .await
                    .unwrap();
                }
            }
        });
    }
}
