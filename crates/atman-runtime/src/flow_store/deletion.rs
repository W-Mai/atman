//! Removal requires exclusive catalog access and closed, detached instances.

use std::{fs, fs::File, io, path::Path};

use anyhow::{Context, Result, ensure};
use atman_rt::InstanceState;

use super::{
    FlowContent, RuntimeStore, SessionInspection, SessionOwner, ensure_dir, profile::ProfileStore,
};
use crate::{event_log::journal::sync_parent, session_meta::fingerprint_from_root};

/// A confirmed catalog version for deleting one Session and its recorded flows.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRemoval {
    pub session_id: uuid::Uuid,
    pub expected: crate::event_log::journal::CommitReceipt,
}

impl RuntimeStore {
    /// Discover unfinished deletion cleanup without opening Session owners or changing data.
    pub fn pending_session_removals(data: &Path) -> io::Result<Vec<uuid::Uuid>> {
        let root = data.join(super::NAMESPACE);
        let entries = match fs::read_dir(root.join(".deletions/sessions")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
                && let Ok(id) = uuid::Uuid::parse_str(name)
                && id.to_string() == name
                && (entry.path().join("data").try_exists()?
                    || root.join("sessions").join(name).try_exists()?)
            {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Read the original deletion request, including after cleanup completed, without mutation.
    pub fn inspect_session_removal(
        data: &Path,
        id: uuid::Uuid,
    ) -> io::Result<Option<SessionRemoval>> {
        let root = Self {
            root: data.join(super::NAMESPACE),
        };
        let marker = root.deletion_path(id);
        if !marker.try_exists()? {
            return Ok(None);
        }
        let saved: SessionRemoval = serde_json::from_slice(&fs::read(marker.join("receipt.json"))?)
            .map_err(super::invalid)?;
        if saved.session_id != id || saved.expected.journal_id != id {
            return Err(super::invalid(
                "session deletion receipt belongs to another identity",
            ));
        }
        Ok(Some(saved))
    }

    pub(super) fn deletion_path(&self, id: uuid::Uuid) -> std::path::PathBuf {
        self.root
            .join(".deletions")
            .join("sessions")
            .join(id.to_string())
    }

    pub(super) fn ensure_session_available(&self, id: uuid::Uuid) -> io::Result<()> {
        if self.deletion_path(id).try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session was deleted",
            ));
        }
        Ok(())
    }

    /// Delete a closed, detached Session under catalog ownership. Retrying the same request finishes interrupted cleanup; the retired identity cannot be created or migrated again. This blocking host operation never stops execution or releases live workspaces.
    pub fn delete_session(&self, request: &SessionRemoval) -> Result<()> {
        let _catalog = self.lock_catalog()?;
        let marker = self.deletion_path(request.session_id);
        let receipt = marker.join("receipt.json");
        match fs::read(&receipt) {
            Ok(bytes) => {
                let saved: SessionRemoval = serde_json::from_slice(&bytes)?;
                ensure!(saved == *request, "session deletion request changed");
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.ensure_session_available(request.session_id)?;
                ensure!(
                    ProfileStore::protected_session(self)? != Some(request.session_id),
                    "session is the management session; select another management session before deletion"
                );
                let dir = self
                    .root
                    .join("sessions")
                    .join(request.session_id.to_string());
                let owner = SessionOwner::lock(&dir)
                    .context("session still has an owner; detach it before deletion")?;
                let inspected =
                    Self::inspect_session(self.root.parent().unwrap(), request.session_id)?;
                ensure!(
                    inspected.journal.cursor.as_ref() == Some(&request.expected),
                    "session changed; refresh before deletion"
                );
                validate_closed_session(&inspected)?;
                ensure_dir(marker.parent().unwrap())?;
                super::initialize_dir(&marker, |dir| {
                    super::write_json(&dir.join("receipt.json"), request)
                })?;
                // The durable receipt retires the identity before any data is removed.
                drop(owner);
            }
            Err(error) => return Err(error.into()),
        }
        self.finish_session_deletion(request.session_id)
    }

    fn finish_session_deletion(&self, id: uuid::Uuid) -> Result<()> {
        let source = self.root.join("sessions").join(id.to_string());
        let trash = self.deletion_path(id).join("data");
        if source.try_exists()? {
            ensure!(
                !trash.try_exists()?,
                "session deletion has conflicting data directories"
            );
            fs::rename(&source, &trash)?;
            sync_parent(&source)?;
            sync_parent(&trash)?;
        }
        match fs::remove_dir_all(&trash) {
            Ok(()) => sync_parent(&trash)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("session deleted; retry to finish data cleanup");
            }
        }
        Ok(())
    }
}

fn validate_closed_session(inspected: &SessionInspection) -> Result<()> {
    ensure!(
        inspected.journal.trailing_bytes == 0,
        "session {} has an incomplete journal",
        inspected.projection.descriptor.id
    );
    for registered in &inspected.projection.flows {
        let flow = inspected.inspect_flow(&registered.id)?;
        ensure!(
            flow.journal.trailing_bytes == 0,
            "flow {} has an incomplete journal",
            registered.id
        );
        ensure!(
            matches!(flow.descriptor.content, FlowContent::History(_))
                || matches!(flow.state.state(), InstanceState::Closed),
            "flow {} is not closed; close it before deletion",
            registered.id
        );
    }
    Ok(())
}

/// Keep membership stable until shared data and project catalog records have been updated.
pub(crate) struct ProjectDeletion {
    _catalog: File,
}

impl ProjectDeletion {
    pub(crate) fn purge_closed_sessions(
        runtime: &RuntimeStore,
        project_root: &Path,
    ) -> Result<Self> {
        let guard = Self {
            _catalog: runtime.lock_catalog()?,
        };
        let data_dir = runtime.root.parent().unwrap();
        let fingerprint = fingerprint_from_root(project_root);
        let management = ProfileStore::protected_session(runtime)?;
        let mut sessions = Vec::new();
        let mut owners = Vec::new();
        for id in RuntimeStore::session_ids(data_dir)? {
            let inspected = RuntimeStore::inspect_session(data_dir, id)
                .with_context(|| format!("inspect session {id} before deleting project"))?;
            if inspected
                .projection
                .descriptor
                .project
                .as_ref()
                .is_none_or(|project| fingerprint_from_root(project.root()) != fingerprint)
            {
                continue;
            }
            ensure!(
                management != Some(id),
                "session {id} is the management session; select another management session before deleting this project"
            );
            owners.push(SessionOwner::lock(&inspected.dir).with_context(|| {
                format!("session {id} still has an owner; detach it before deleting this project")
            })?);
            // Read again after acquiring ownership: the preceding inspection may overlap a writer's last commit.
            let inspected = RuntimeStore::inspect_session(data_dir, id)?;
            validate_closed_session(&inspected)?;
            sessions.push(inspected.dir);
        }

        // Catalog ownership excludes new opens/creates. Close file locks before rename/removal on every platform.
        drop(owners);
        let trash = runtime.root.join(".deletions").join(fingerprint);
        if !sessions.is_empty() {
            ensure_dir(&trash)?;
            for session in sessions {
                let destination = trash.join(session.file_name().unwrap());
                ensure!(
                    !destination.try_exists()?,
                    "pending session deletion already exists: {}",
                    destination.display()
                );
                fs::rename(&session, &destination)?;
                sync_parent(&session)?;
                sync_parent(&destination)?;
            }
        }
        match fs::remove_dir_all(&trash) {
            Ok(()) => sync_parent(&trash)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("finish pending project session deletion"),
        }
        Ok(guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, SessionDescriptor,
    };
    use uuid::Uuid;

    #[test]
    fn deletion_requires_confirmed_closed_detached_data_and_retires_identity() {
        let data = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let descriptor = SessionDescriptor {
            id: Uuid::now_v7(),
            project: None,
            configuration: Default::default(),
            title: "Delete example".into(),
        };
        let mut session = root.create_session(descriptor.clone()).unwrap();
        let mut flow = session
            .create_flow(FlowDescriptor {
                address: FlowRef {
                    session_id: descriptor.id.into(),
                    instance_id: FlowInstanceId::now(),
                },
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: crate::source_program::RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        let request = SessionRemoval {
            session_id: descriptor.id,
            expected: session.cursor().clone(),
        };
        assert!(
            root.delete_session(&request)
                .unwrap_err()
                .to_string()
                .contains("owner")
        );
        drop(session);
        assert!(root.delete_session(&request).is_err());
        drop(flow);
        assert!(
            root.delete_session(&request)
                .unwrap_err()
                .to_string()
                .contains("not closed")
        );
        let session = root.open_session(descriptor.id).unwrap();
        flow = session
            .open_flow(&session.projection().flows[0].id)
            .unwrap();
        for transition in [FlowTransition::CloseRequested, FlowTransition::Closed] {
            flow.transition(flow.state().revision(), transition)
                .unwrap();
        }
        let path = flow.dir().join("events.jsonl");
        drop((session, flow));
        let original = fs::read(&path).unwrap();
        let mut incomplete = original.clone();
        incomplete.extend_from_slice(b"{\"partial\":");
        fs::write(&path, &incomplete).unwrap();
        assert!(
            root.delete_session(&request)
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        assert_eq!(fs::read(&path).unwrap(), incomplete);
        fs::write(&path, original).unwrap();
        let mut stale = request.clone();
        stale.expected.sequence += 1;
        assert!(
            root.delete_session(&stale)
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        let mut profile = ProfileStore::open(&root).unwrap();
        let manager = profile
            .bootstrap(Default::default())
            .unwrap()
            .management
            .unwrap()
            .session
            .id;
        profile
            .select(
                Uuid::now_v7(),
                profile.snapshot().cursor.clone(),
                descriptor.id,
                vec![],
            )
            .unwrap();
        assert!(
            root.delete_session(&request)
                .unwrap_err()
                .to_string()
                .contains("management session")
        );
        profile
            .select(
                Uuid::now_v7(),
                profile.snapshot().cursor.clone(),
                manager,
                vec![],
            )
            .unwrap();
        root.delete_session(&request).unwrap();
        root.delete_session(&request).unwrap();
        assert!(root.delete_session(&stale).is_err());
        assert!(
            !root
                .root
                .join("sessions")
                .join(descriptor.id.to_string())
                .exists()
        );
        assert!(!root.deletion_path(descriptor.id).join("data").exists());
        assert!(
            root.deletion_path(descriptor.id)
                .join("receipt.json")
                .is_file()
        );
        assert!(root.create_session(descriptor.clone()).is_err());
        assert!(root.open_session(descriptor.id).is_err());
        assert!(RuntimeStore::inspect_session(data.path(), descriptor.id).is_err());
        assert_eq!(
            RuntimeStore::session_ids(data.path()).unwrap(),
            vec![manager]
        );
    }

    #[test]
    fn committed_removal_is_hidden_and_resumes_before_or_after_directory_move() {
        for moved in [false, true] {
            let data = tempfile::tempdir().unwrap();
            let root = RuntimeStore::open(data.path()).unwrap();
            let descriptor = SessionDescriptor {
                id: Uuid::now_v7(),
                project: None,
                configuration: Default::default(),
                title: "Example".into(),
            };
            let session = root.create_session(descriptor.clone()).unwrap();
            let request = SessionRemoval {
                session_id: descriptor.id,
                expected: session.cursor().clone(),
            };
            let source = session.dir().to_owned();
            drop(session);
            let marker = root.deletion_path(descriptor.id);
            ensure_dir(marker.parent().unwrap()).unwrap();
            super::super::initialize_dir(&marker, |dir| {
                super::super::write_json(&dir.join("receipt.json"), &request)
            })
            .unwrap();
            if moved {
                fs::rename(&source, marker.join("data")).unwrap();
            }
            let receipt_bytes = fs::read(marker.join("receipt.json")).unwrap();
            assert_eq!(
                RuntimeStore::pending_session_removals(data.path()).unwrap(),
                vec![descriptor.id]
            );
            assert_eq!(
                RuntimeStore::inspect_session_removal(data.path(), descriptor.id).unwrap(),
                Some(request.clone())
            );
            assert_eq!(
                fs::read(marker.join("receipt.json")).unwrap(),
                receipt_bytes
            );
            assert!(RuntimeStore::session_ids(data.path()).unwrap().is_empty());
            assert!(root.open_session(descriptor.id).is_err());
            assert!(root.create_session(descriptor.clone()).is_err());
            assert!(RuntimeStore::inspect_session(data.path(), descriptor.id).is_err());
            let reopened = RuntimeStore::open(data.path()).unwrap();
            reopened.delete_session(&request).unwrap();
            assert!(!source.exists());
            assert!(!marker.join("data").exists());
            assert!(marker.join("receipt.json").is_file());
            assert!(
                RuntimeStore::pending_session_removals(data.path())
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                RuntimeStore::inspect_session_removal(data.path(), descriptor.id).unwrap(),
                Some(request)
            );
        }
    }
}
