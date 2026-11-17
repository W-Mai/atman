//! Private host discovery. Session writer locks remain the ownership authority.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use atman_runtime::platform::ExclusiveFileLease;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use atman_proto::hosts::{HostPresentation, HostRole};

/// An authenticated endpoint candidate, not proof of a live Session owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostRecord {
    pub generation: Uuid,
    pub process_id: u32,
    pub role: HostRole,
    pub presentation: HostPresentation,
    pub endpoint: SocketAddr,
    #[serde(default)]
    pub sessions: Vec<Uuid>,
}

impl HostRecord {
    fn validate(&self) -> io::Result<()> {
        if self.generation.is_nil()
            || self.process_id == 0
            || !self.endpoint.ip().is_loopback()
            || self.endpoint.port() == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid local host record",
            ));
        }
        Ok(())
    }
}

/// Retains one process registration. Dropping it never controls execution.
pub struct HostRegistration {
    path: PathBuf,
    _lease: ExclusiveFileLease,
    record: Mutex<HostRecord>,
}

/// Event-driven membership publication, independent of execution lifetime.
pub struct HostPublication {
    cancel: tokio_util::sync::CancellationToken,
    task: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl HostPublication {
    pub fn start(
        registration: Arc<HostRegistration>,
        host: atman_runtime::flow_runtime::host::RuntimeHost,
    ) -> Self {
        let mut changes = host.subscribe_changes();
        let cancel = tokio_util::sync::CancellationToken::new();
        let stopped = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                changes.borrow_and_update();
                let sessions = host.sessions();
                let retained = registration.clone();
                tokio::task::spawn_blocking(move || retained.publish_sessions(sessions))
                    .await
                    .map_err(io::Error::other)??;
                tokio::select! {
                    _ = stopped.cancelled() => return Ok(()),
                    changed = changes.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        });
        Self {
            cancel,
            task: Some(task),
        }
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(io::Error::other)??;
        }
        Ok(())
    }
}

impl Drop for HostPublication {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn directory(data: &Path) -> PathBuf {
    data.join("run").join("hosts")
}

fn lease_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

impl HostRegistration {
    /// Install a record while retaining its generation-specific private lease.
    pub fn register(data: &Path, record: HostRecord) -> io::Result<Self> {
        record.validate()?;
        let root = directory(data);
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let lease_path = root.join(format!("{}.lock", record.generation));
        let lease = ExclusiveFileLease::try_acquire(
            lease_options()
                .create(true)
                .truncate(false)
                .open(lease_path)?,
        )?;
        let path = root.join(format!("{}.json", record.generation));
        let bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
        // The containing directory is private before the first record is installed.
        atman_runtime::platform::atomic_replace(&path, &bytes)?;
        Ok(Self {
            path,
            _lease: lease,
            record: Mutex::new(record),
        })
    }

    pub fn record(&self) -> HostRecord {
        self.record.lock().unwrap().clone()
    }

    /// Publish membership observations from a host worker, never from a UI draw path.
    pub fn publish_sessions(&self, mut sessions: Vec<Uuid>) -> io::Result<()> {
        sessions.sort_unstable();
        sessions.dedup();
        let mut record = self.record.lock().unwrap();
        if record.sessions == sessions {
            return Ok(());
        }
        let mut changed = record.clone();
        changed.sessions = sessions;
        let bytes = serde_json::to_vec(&changed).map_err(io::Error::other)?;
        atman_runtime::platform::atomic_replace(&self.path, &bytes)?;
        *record = changed;
        Ok(())
    }
}

impl Drop for HostRegistration {
    fn drop(&mut self) {
        // Generation names cannot be reused while this lease is held.
        let _ = fs::remove_file(&self.path);
    }
}

impl HostRegistration {
    pub fn publish_presentation(&self, presentation: HostPresentation) -> io::Result<()> {
        let mut record = self.record.lock().unwrap();
        if record.presentation == presentation {
            return Ok(());
        }
        let mut changed = record.clone();
        changed.presentation = presentation;
        let bytes = serde_json::to_vec(&changed).map_err(io::Error::other)?;
        atman_runtime::platform::atomic_replace(&self.path, &bytes)?;
        *record = changed;
        Ok(())
    }
}

/// Observe retained registrations without opening Sessions or trusting process IDs.
pub fn discover(data: &Path) -> io::Result<Vec<HostRecord>> {
    let root = directory(data);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json")
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        let Some(generation) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            continue;
        };
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        const MAX_RECORD: u64 = 1024 * 1024;
        if !metadata.is_file() || metadata.len() > MAX_RECORD {
            continue;
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RECORD {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<HostRecord>(&bytes) else {
            continue;
        };
        if record.generation != generation || record.validate().is_err() {
            continue;
        }
        let lock = match lease_options().open(root.join(format!("{generation}.lock"))) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match ExclusiveFileLease::try_acquire(lock) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => records.push(record),
            Err(error) => return Err(error),
        }
    }
    records.sort_unstable_by_key(|record| record.generation);
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> HostRecord {
        HostRecord {
            generation: Uuid::now_v7(),
            process_id: std::process::id(),
            role: HostRole::Tui,
            presentation: HostPresentation::Attached,
            endpoint: "127.0.0.1:63123".parse().unwrap(),
            sessions: Vec::new(),
        }
    }

    #[test]
    fn discovery_requires_retained_lease_and_generation_and_does_not_keep_owner_alive() {
        let data = tempfile::tempdir().unwrap();
        let record = record();
        let registration = HostRegistration::register(data.path(), record.clone()).unwrap();
        assert_eq!(
            discover(data.path()).unwrap().as_slice(),
            std::slice::from_ref(&record)
        );
        assert!(HostRegistration::register(data.path(), record.clone()).is_err());
        let path = registration.path.clone();
        let original = fs::read(&path).unwrap();
        let mut wrong = record.clone();
        wrong.generation = Uuid::now_v7();
        fs::write(&path, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(discover(data.path()).unwrap().is_empty());
        fs::write(&path, original).unwrap();
        drop(registration);
        assert!(discover(data.path()).unwrap().is_empty());
        // A stale record with a live process ID still does not represent a host.
        fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(discover(data.path()).unwrap().is_empty());
    }

    #[test]
    fn public_and_unspecified_endpoints_cannot_be_registered() {
        let data = tempfile::tempdir().unwrap();
        for endpoint in ["0.0.0.0:63123", "192.0.2.1:63123", "127.0.0.1:0"] {
            let mut record = record();
            record.endpoint = endpoint.parse().unwrap();
            assert!(HostRegistration::register(data.path(), record).is_err());
        }
    }
}
