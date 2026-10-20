//! Confirmed removal of stopped legacy executors' original histories.

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{RuntimeStore, deletion, ensure_dir, invalid};

/// Exact directory contents reviewed by an operator. Migrated copies have independent identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacySessionRemoval {
    pub session_id: Uuid,
    pub directory: PathBuf,
    pub digest: String,
}

impl RuntimeStore {
    /// Inspect all original history files without opening an executor or writing metadata.
    pub fn review_legacy_removal(data: &Path, id: Uuid) -> io::Result<LegacySessionRemoval> {
        let source = data.join("sessions").join(id.to_string());
        let digest = directory_digest(&source)?;
        Ok(LegacySessionRemoval {
            session_id: id,
            directory: source.canonicalize()?,
            digest,
        })
    }

    pub fn pending_legacy_removals(data: &Path) -> io::Result<Vec<Uuid>> {
        deletion::pending_removals(
            &data.join(super::NAMESPACE),
            "legacy-sessions",
            &data.join("sessions"),
        )
    }

    pub fn inspect_legacy_removal(
        data: &Path,
        id: Uuid,
    ) -> io::Result<Option<LegacySessionRemoval>> {
        let marker = data
            .join(super::NAMESPACE)
            .join(".deletions/legacy-sessions")
            .join(id.to_string());
        if !marker.try_exists()? {
            return Ok(None);
        }
        let request: LegacySessionRemoval =
            serde_json::from_slice(&fs::read(marker.join("receipt.json"))?).map_err(invalid)?;
        if request.session_id != id {
            return Err(invalid(
                "legacy removal receipt belongs to another identity",
            ));
        }
        Ok(Some(request))
    }

    /// Delete only the reviewed original directory. The caller must explicitly confirm that its old executor stopped; old binaries do not honor persistent runtime owner locks. Retrying preserves the receipt and finishes interrupted cleanup.
    pub fn delete_legacy_session(
        &self,
        request: &LegacySessionRemoval,
        old_executor_stopped: bool,
    ) -> Result<()> {
        ensure!(
            old_executor_stopped,
            "confirm the legacy executor has stopped before deletion"
        );
        let _catalog = self.lock_catalog()?;
        let data = self.root.parent().unwrap();
        let source = data.join("sessions").join(request.session_id.to_string());
        let marker = self
            .root
            .join(".deletions/legacy-sessions")
            .join(request.session_id.to_string());
        let existing = Self::inspect_legacy_removal(data, request.session_id)?;
        if let Some(saved) = &existing {
            ensure!(saved == request, "legacy deletion request changed");
        }
        // Revalidate the original directory even when a receipt was committed before a crash.
        if existing.is_none() || source.try_exists()? {
            ensure!(
                Self::review_legacy_removal(data, request.session_id)? == *request,
                "legacy session changed; refresh before deletion"
            );
        }
        if existing.is_none() {
            ensure_dir(marker.parent().unwrap())?;
            super::initialize_dir(&marker, |dir| {
                super::write_json(&dir.join("receipt.json"), request)
            })?;
        }
        deletion::finish_removal(&source, &marker)
    }
}

fn directory_digest(root: &Path) -> io::Result<String> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return Err(invalid("legacy session must be a directory, not a link"));
    }
    fn visit(root: &Path, directory: &Path, digest: &mut Sha256) -> io::Result<()> {
        let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            let relative = path.strip_prefix(root).map_err(invalid)?;
            let (kind, size) = if meta.is_dir() {
                ("directory", 0)
            } else if meta.is_file() {
                ("file", meta.len())
            } else if meta.is_symlink() {
                ("link", 0)
            } else {
                return Err(invalid("legacy session contains an unsupported file type"));
            };
            digest.update(serde_json::to_vec(&(relative, kind, size)).map_err(invalid)?);
            digest.update(b"\n");
            if meta.is_dir() {
                visit(root, &path, digest)?;
            } else if meta.is_file() {
                let mut file = fs::File::open(&path)?;
                let copied = io::copy(&mut (&mut file).take(size), digest)?;
                if copied != size || file.metadata()?.len() != size {
                    return Err(invalid("legacy session changed during review"));
                }
            } else {
                digest.update(serde_json::to_vec(&fs::read_link(&path)?).map_err(invalid)?);
            }
        }
        Ok(())
    }
    let mut digest = Sha256::new();
    visit(root, root, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(data: &Path, id: Uuid) -> PathBuf {
        let path = data.join("sessions").join(id.to_string());
        fs::create_dir_all(path.join("images")).unwrap();
        fs::write(path.join("events.jsonl"), b"original history\n").unwrap();
        fs::write(path.join("session_meta.json"), b"{}").unwrap();
        path
    }

    #[test]
    fn removal_requires_stopped_confirmation_and_exact_directory_contents() {
        let data = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let id = Uuid::now_v7();
        let source = history(data.path(), id);
        let other = history(data.path(), Uuid::now_v7());
        // Persistent identities and copies remain independent of the legacy source.
        let native = root
            .create_session(super::super::SessionDescriptor {
                id,
                project: None,
                configuration: Default::default(),
                title: "Native copy".into(),
            })
            .unwrap();
        let request = RuntimeStore::review_legacy_removal(data.path(), id).unwrap();
        assert!(root.delete_legacy_session(&request, false).is_err());
        assert!(
            RuntimeStore::inspect_legacy_removal(data.path(), id)
                .unwrap()
                .is_none()
        );
        fs::write(source.join("images/image.bin"), b"image").unwrap();
        assert!(root.delete_legacy_session(&request, true).is_err());
        assert!(source.join("events.jsonl").is_file());
        fs::remove_file(source.join("images/image.bin")).unwrap();
        fs::write(source.join("session_meta.json"), b"{ }").unwrap();
        assert!(root.delete_legacy_session(&request, true).is_err());
        fs::write(source.join("session_meta.json"), b"{}").unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        assert!(root.delete_legacy_session(&request, true).is_err());
        fs::remove_dir(source.join("empty")).unwrap();
        let wrong = LegacySessionRemoval {
            directory: other.clone(),
            ..request.clone()
        };
        assert!(root.delete_legacy_session(&wrong, true).is_err());
        root.delete_legacy_session(&request, true).unwrap();
        root.delete_legacy_session(&request, true).unwrap();
        assert!(root.delete_legacy_session(&wrong, true).is_err());
        assert!(!source.exists());
        assert!(other.join("events.jsonl").exists());
        assert!(native.dir().exists());
        assert!(
            RuntimeStore::pending_legacy_removals(data.path())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            RuntimeStore::inspect_legacy_removal(data.path(), id).unwrap(),
            Some(request)
        );
    }

    #[test]
    fn cleanup_resumes_before_and_after_move_without_deleting_changed_sources() {
        for moved in [false, true] {
            let data = tempfile::tempdir().unwrap();
            let root = RuntimeStore::open(data.path()).unwrap();
            let id = Uuid::now_v7();
            let source = history(data.path(), id);
            let request = RuntimeStore::review_legacy_removal(data.path(), id).unwrap();
            let marker = root
                .root
                .join(".deletions/legacy-sessions")
                .join(id.to_string());
            fs::create_dir_all(&marker).unwrap();
            super::super::write_json(&marker.join("receipt.json"), &request).unwrap();
            if moved {
                fs::rename(&source, marker.join("data")).unwrap();
                fs::remove_file(marker.join("data/events.jsonl")).unwrap();
            } else {
                fs::write(source.join("session_meta.json"), b"changed").unwrap();
                assert!(root.delete_legacy_session(&request, true).is_err());
                assert_eq!(
                    fs::read(source.join("session_meta.json")).unwrap(),
                    b"changed"
                );
                fs::write(source.join("session_meta.json"), b"{}").unwrap();
            }
            assert_eq!(
                RuntimeStore::pending_legacy_removals(data.path()).unwrap(),
                vec![id]
            );
            root.delete_legacy_session(&request, true).unwrap();
            assert!(!source.exists());
            assert!(!marker.join("data").exists());
            assert!(marker.join("receipt.json").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_are_reviewed_and_removed_without_following_their_targets() {
        use std::os::unix::fs::symlink;
        let data = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let id = Uuid::now_v7();
        let source = history(data.path(), id);
        let outside = data.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"keep").unwrap();
        symlink(&outside, source.join("directory-link")).unwrap();
        symlink(outside.join("keep"), source.join("file-link")).unwrap();
        let request = RuntimeStore::review_legacy_removal(data.path(), id).unwrap();
        root.delete_legacy_session(&request, true).unwrap();
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
        symlink(&outside, &source).unwrap();
        assert!(RuntimeStore::review_legacy_removal(data.path(), id).is_err());
        assert!(root.delete_legacy_session(&request, true).is_err());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
    }
}
