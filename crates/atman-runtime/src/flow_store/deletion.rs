//! Project removal requires exclusive catalog access and closed, detached instances.

use std::{fs, fs::File, io, path::Path};

use anyhow::{Context, Result, ensure};
use atman_rt::InstanceState;

use super::{FlowContent, RuntimeStore, SessionOwner, ensure_dir, profile::ProfileStore};
use crate::{event_log::journal::sync_parent, session_meta::fingerprint_from_root};

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
            ensure!(
                inspected.journal.trailing_bytes == 0,
                "session {id} has an incomplete journal"
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
                    "flow {} is not closed; close it before deleting this project",
                    registered.id
                );
            }
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
