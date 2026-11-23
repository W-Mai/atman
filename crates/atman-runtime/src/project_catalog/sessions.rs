//! Read-only Session metadata for project navigation.

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    flow_store::{FlowInstanceId, RuntimeStore, session_activity_time},
    session_meta::SessionMeta,
};

const MAX_SESSIONS: usize = 32_768;
const MAX_LEGACY_METADATA_BYTES: u64 = 256 * 1024;
static DIRECTORY_READER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DirectorySessionKind {
    Native,
    Legacy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectorySession {
    pub id: Uuid,
    pub kind: DirectorySessionKind,
    pub title: String,
    pub project_root: Option<PathBuf>,
    pub project_fingerprint: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub available: bool,
}

#[derive(PartialEq, Eq)]
struct MetadataStamp {
    bytes: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

fn stamp(path: &Path) -> Option<MetadataStamp> {
    let metadata = path.symlink_metadata().ok()?;
    metadata.is_file().then(|| MetadataStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        },
    })
}

struct CachedSession {
    stamp: Option<MetadataStamp>,
    row: DirectorySession,
    flows: Vec<FlowInstanceId>,
}

/// Disposable observations for one storage Profile. Opening and removing Sessions use their own authority checks.
pub struct SessionDirectoryCache {
    data: PathBuf,
    cached: Arc<Mutex<HashMap<(DirectorySessionKind, Uuid), CachedSession>>>,
}

impl SessionDirectoryCache {
    pub fn new(data: PathBuf) -> Self {
        Self {
            data,
            cached: Default::default(),
        }
    }

    pub async fn observe(&self) -> io::Result<Vec<DirectorySession>> {
        let permit = DIRECTORY_READER.try_acquire().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "Session directory reader is busy",
            )
        })?;
        let data = self.data.clone();
        let cached = self.cached.clone();
        crate::read_work::capture(move || {
            let _permit = permit;
            let mut cached = cached
                .lock()
                .map_err(|_| io::Error::other("Session directory cache is unavailable"))?;
            scan(&data, &mut cached)
        })
        .await
        .map_err(io::Error::other)?
    }
}

fn scan(
    data: &Path,
    cached: &mut HashMap<(DirectorySessionKind, Uuid), CachedSession>,
) -> io::Result<Vec<DirectorySession>> {
    use DirectorySessionKind::{Legacy, Native};
    let mut paths = RuntimeStore::session_ids(data)?
        .into_iter()
        .map(|id| (Native, id, RuntimeStore::session_directory(data, id)))
        .collect::<Vec<_>>();
    let legacy = match fs::read_dir(data.join("sessions")) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    for entry in legacy.into_iter().flatten() {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
        {
            paths.push((Legacy, id, entry.path()));
        }
        if paths.len() > MAX_SESSIONS {
            break;
        }
    }
    if paths.len() > MAX_SESSIONS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Session directory exceeds its metadata budget",
        ));
    }
    paths.sort_unstable();
    paths.dedup_by_key(|(kind, id, _)| (*kind, *id));
    cached.retain(|&(kind, id), _| {
        paths
            .binary_search_by_key(&(kind, id), |(source, session, _)| (*source, *session))
            .is_ok()
    });
    let mut rows = Vec::with_capacity(paths.len());
    for (kind, id, dir) in paths {
        let metadata = dir.join(match kind {
            Native => "events.jsonl",
            Legacy => "meta.json",
        });
        let observed = stamp(&metadata);
        let current = cached
            .get(&(kind, id))
            .filter(|entry| entry.stamp == observed);
        let fresh;
        let entry = if let Some(entry) = current {
            entry
        } else {
            fresh = read(data, id, kind, &metadata, observed);
            if fresh.stamp == stamp(&metadata) {
                cached.entry((kind, id)).insert_entry(fresh).into_mut()
            } else {
                &fresh
            }
        };
        let mut row = entry.row.clone();
        row.updated_at = session_activity_time(&dir, &entry.flows)
            .or_else(|| dir.metadata().ok()?.modified().ok())
            .map(DateTime::<Utc>::from)
            .or(row.updated_at);
        rows.push(row);
    }
    Ok(rows)
}

fn read(
    data: &Path,
    id: Uuid,
    kind: DirectorySessionKind,
    metadata: &Path,
    stamp: Option<MetadataStamp>,
) -> CachedSession {
    let mut row = DirectorySession {
        id,
        kind,
        title: String::new(),
        project_root: None,
        project_fingerprint: None,
        updated_at: None,
        available: true,
    };
    let mut flows = Vec::new();
    match kind {
        DirectorySessionKind::Native => {
            if let Some(inspected) = stamp
                .as_ref()
                .and_then(|_| RuntimeStore::inspect_session(data, id).ok())
            {
                row.title = inspected.projection.title.unwrap_or_default();
                row.project_root = inspected
                    .projection
                    .project
                    .as_ref()
                    .map(|project| project.root().to_owned());
                flows.extend(inspected.projection.flows.into_iter().map(|flow| flow.id));
            } else {
                row.title = "Unavailable Session".into();
                row.available = false;
            }
        }
        DirectorySessionKind::Legacy => {
            if let Some(meta) = stamp
                .as_ref()
                .filter(|source| source.bytes <= MAX_LEGACY_METADATA_BYTES)
                .and_then(|_| File::open(metadata).ok())
                .and_then(|file| {
                    serde_json::from_reader::<_, SessionMeta>(
                        file.take(MAX_LEGACY_METADATA_BYTES + 1),
                    )
                    .ok()
                })
            {
                row.title = meta.title.unwrap_or_default();
                row.project_root = meta.project_root;
                row.project_fingerprint = meta.project_fingerprint;
                row.updated_at = meta.created_at;
            }
        }
    }
    CachedSession { stamp, row, flows }
}
