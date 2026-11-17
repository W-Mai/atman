//! Crash recovery metadata for original host handoffs; conversation journals remain authoritative.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use atman_runtime::{flow_runtime::host::SessionHandoffReceipt, platform::ExclusiveFileLease};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::peer_transport::{HandoffBinding, credential_directory, private_file};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Prepared,
    Releasing,
    Released,
    Installed,
    Completed,
    Aborted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffRecord {
    pub revision: u32,
    pub binding: HandoffBinding,
    pub phase: Phase,
    pub receipt: Option<SessionHandoffReceipt>,
    pub supersedes: Option<Uuid>,
    pub replacement: Option<Uuid>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn root(data: &Path) -> io::Result<PathBuf> {
    let run = data.join("run");
    credential_directory(&run, false)?;
    let peer = run.join("peer");
    credential_directory(&peer, true)?;
    let root = peer.join("handoffs");
    credential_directory(&root, true)?;
    Ok(root)
}

pub(crate) async fn update(data: PathBuf, record: HandoffRecord) -> io::Result<HandoffRecord> {
    let record = Arc::new(record);
    loop {
        let data = data.clone();
        let record = record.clone();
        let result = tokio::task::spawn_blocking(move || update_once(&data, (*record).clone()))
            .await
            .map_err(io::Error::other)?;
        match result {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await
            }
            result => return result,
        }
    }
}

fn update_once(data: &Path, mut record: HandoffRecord) -> io::Result<HandoffRecord> {
    let root = root(data)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let gate = root.join(format!("{}.lock", record.binding.request));
    let file = options.open(&gate)?;
    private_file(&gate, &file)?;
    let _owner = ExclusiveFileLease::try_acquire(file)?;
    let path = root.join(format!("{}.json", record.binding.request));
    if let Some(previous) = read(&path)? {
        if previous.binding != record.binding || previous.supersedes != record.supersedes {
            return Err(invalid("handoff recovery operation identity changed"));
        }
        match (previous.phase, record.phase) {
            (Phase::Aborted, Phase::Aborted) => {}
            (Phase::Aborted, _) => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "the original handoff preparation was cancelled",
                ));
            }
            (Phase::Prepared, Phase::Aborted) => {}
            (_, Phase::Aborted) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the original handoff already accepted writer release",
                ));
            }
            _ => {}
        }
        if previous.replacement.is_some()
            && record.replacement.is_some()
            && previous.replacement != record.replacement
        {
            return Err(invalid("handoff replacement identity changed"));
        }
        if record.replacement.is_none() {
            record.replacement = previous.replacement;
        }
        if previous.receipt.is_some()
            && record.receipt.is_some()
            && previous.receipt != record.receipt
        {
            return Err(invalid("handoff recovery receipt changed"));
        }
        if record.receipt.is_none() {
            record.receipt = previous.receipt;
        }
        record.phase = previous.phase.max(record.phase);
    } else if record.phase == Phase::Aborted {
        return Err(invalid("handoff preparation is not recorded"));
    }
    validate(data, &record)?;
    let bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(invalid("handoff recovery metadata exceeds its size bound"));
    }
    atman_runtime::platform::atomic_replace(&path, &bytes)?;
    #[cfg(unix)]
    fs::File::open(&root)?.sync_all()?;
    Ok(record)
}

fn read(path: &Path) -> io::Result<Option<HandoffRecord>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16 * 1024 * 1024
    {
        return Err(invalid("invalid handoff recovery file"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(invalid("handoff recovery metadata exceeds its size bound"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

fn validate(data: &Path, record: &HandoffRecord) -> io::Result<()> {
    let binding = &record.binding;
    if record.supersedes == Some(binding.request) || record.replacement == Some(binding.request) {
        return Err(invalid("handoff recovery cannot replace itself"));
    }
    if record.phase == Phase::Aborted && (record.receipt.is_some() || record.replacement.is_some())
    {
        return Err(invalid(
            "aborted handoff cannot contain an installation receipt",
        ));
    }
    if record.revision != 1
        || binding.request.is_nil()
        || binding.session.is_nil()
        || binding.source.profile_id.is_nil()
        || binding.source.profile_id != binding.destination.profile_id
        || binding.source.canonical_data_root != data
        || binding.destination.canonical_data_root != data
        || (binding.source.runtime_generation == binding.destination.runtime_generation
            && record.supersedes.is_none())
    {
        return Err(invalid("invalid handoff recovery domain or identity"));
    }
    if let Some(receipt) = &record.receipt {
        if receipt.request_id != binding.request
            || receipt.session_id != binding.session
            || receipt.source_generation != binding.source.runtime_generation
            || receipt.catalog.journal_id != binding.session
        {
            return Err(invalid(
                "handoff recovery receipt belongs to another operation",
            ));
        }
    }
    Ok(())
}

pub(crate) async fn load(data: PathBuf) -> io::Result<Vec<HandoffRecord>> {
    tokio::task::spawn_blocking(move || {
        let root = root(&data)?;
        let mut records = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| Uuid::parse_str(stem).ok())
            else {
                continue;
            };
            let Some(record) = read(&path)? else {
                continue;
            };
            validate(&data, &record)?;
            if record.binding.request != id {
                return Err(invalid("handoff recovery filename identity changed"));
            }
            records.push(record);
        }
        Ok(records)
    })
    .await
    .map_err(io::Error::other)?
}
