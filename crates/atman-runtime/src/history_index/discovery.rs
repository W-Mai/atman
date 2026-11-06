//! Disposable index metadata. Original-history availability is checked on every read.

use super::*;
use std::{sync::Mutex, time::SystemTime};

#[derive(PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

fn stamp(path: &Path) -> std::io::Result<Option<FileStamp>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(FileStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
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
    }))
}

type Stamp = (Option<FileStamp>, Option<FileStamp>);

fn source_stamp(directory: &Path) -> std::io::Result<Stamp> {
    // A cached result never bypasses current filesystem readability.
    let _file = fs::File::open(directory.join("index.db"))?;
    Ok((
        stamp(&directory.join("index.db"))?,
        stamp(&directory.join("index.db-wal"))?,
    ))
}

/// Process-local, read-only directory cache. Call on a storage worker; it never authorizes recovery or execution.
#[derive(Default)]
pub struct DiscoveryCache {
    entries: Mutex<BTreeMap<PathBuf, (Stamp, Vec<IndexedSession>)>>,
}

impl DiscoveryCache {
    pub fn discover(&self, data: &Path) -> Result<Vec<IndexedSession>> {
        let directories = directories(data)?;
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("history discovery cache poisoned"))?;
        entries.retain(|directory, _| directories.contains_key(directory));
        let mut sessions = Vec::new();
        for (directory, project) in directories {
            let read = (|| -> Result<Vec<IndexedSession>> {
                let before = source_stamp(&directory)?;
                let rows = match entries.get(&directory) {
                    Some(entry) if entry.0 == before => entry.1.clone(),
                    _ => {
                        let rows = discover_directory(&directory, None)?;
                        if source_stamp(&directory)? == before {
                            entries.insert(directory.clone(), (before, rows.clone()));
                        }
                        rows
                    }
                };
                let mut eligible = Vec::new();
                for mut row in rows {
                    if !original_exists(data, row.source.session)? {
                        row.project = project.clone();
                        eligible.push(row);
                    }
                }
                Ok(eligible)
            })();
            match read {
                Ok(rows) => sessions.extend(rows),
                Err(error) => crate::notify!(
                    warn,
                    location = Toast,
                    stack = dedupe(format!("history-index:{}", directory.display()), 30_000),
                    "Cannot read indexed history {}: {error:#}. Other session sources remain available.",
                    directory.join("index.db").display()
                ),
            }
        }
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ProjectEventInsert;

    #[test]
    fn discovery_cache_tracks_wal_and_rechecks_original_history_availability() {
        let data = tempfile::tempdir().unwrap();
        let directory = data.path().join("projects/example");
        let index = AnchorIndex::open_project(&directory).unwrap();
        let session = Uuid::now_v7();
        let payload = r#"{"message":{"parts":[{"text":"retained question"}]}}"#;
        let insert = |seq| {
            index
                .insert_project_event_raw(ProjectEventInsert {
                    session_id: &session.to_string(),
                    seq,
                    ts: "2026-10-01T00:00:00Z",
                    kind: "user_msg",
                    turn_id: None,
                    flow_run_id: None,
                    text_content: "retained question",
                    payload_json: payload,
                })
                .unwrap()
        };
        insert(1);
        let cache = DiscoveryCache::default();
        assert_eq!(cache.discover(data.path()).unwrap()[0].user_messages, 1);
        assert_eq!(cache.discover(data.path()).unwrap()[0].user_messages, 1);
        let original = data
            .path()
            .join("sessions")
            .join(session.to_string())
            .join("events.jsonl");
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        fs::write(&original, "original").unwrap();
        assert!(cache.discover(data.path()).unwrap().is_empty());
        fs::remove_file(original).unwrap();
        assert_eq!(cache.discover(data.path()).unwrap().len(), 1);
        insert(2);
        assert_eq!(cache.discover(data.path()).unwrap()[0].user_messages, 2);
        drop(index);
        fs::remove_file(directory.join("index.db")).unwrap();
        assert!(cache.discover(data.path()).unwrap().is_empty());
        assert!(!data.path().join("runtime-v1").exists());
    }
}
