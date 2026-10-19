//! Explicit resource removal records its intent before external effects.

use super::{
    WorkspaceError, WorkspaceManager, WorkspaceRecord, WorkspaceRegistry, WorkspaceRelease,
    WorkspaceState, canonicalize, validate_ownership,
};
use crate::git::GitCli;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReleaseStage {
    IntentSaved,
    CheckoutRemoved,
}

impl WorkspaceManager {
    /// Release an observed resource only while its complete record still matches. Callers authorize both roots. Live references and invocation leases cannot be released by directory controls; pending removals retain their recorded options.
    pub fn release_observed(
        repository_root: &std::path::Path,
        storage_root: &std::path::Path,
        expected: &WorkspaceRecord,
        force: bool,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        Self::mutate_observed(
            repository_root,
            storage_root,
            expected,
            |manager, registry| {
                manager.release_locked(
                    registry,
                    &expected.id,
                    expected.owner_session.as_deref(),
                    expected.owner_flow.as_deref(),
                    force,
                    |_| Ok(()),
                )
            },
        )
    }

    pub fn release(
        &self,
        id: &str,
        owner_session: Option<&str>,
        owner_flow: Option<&str>,
        force: bool,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        self.release_with_hook(id, owner_session, owner_flow, force, |_| Ok(()))
    }

    pub(super) fn release_with_hook(
        &self,
        id: &str,
        owner_session: Option<&str>,
        owner_flow: Option<&str>,
        force: bool,
        hook: impl FnMut(ReleaseStage) -> Result<(), WorkspaceError>,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        self.release_locked(&mut registry, id, owner_session, owner_flow, force, hook)
    }

    fn release_locked(
        &self,
        registry: &mut WorkspaceRegistry,
        id: &str,
        owner_session: Option<&str>,
        owner_flow: Option<&str>,
        force: bool,
        mut hook: impl FnMut(ReleaseStage) -> Result<(), WorkspaceError>,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let index = registry
            .workspaces
            .iter()
            .position(|record| record.id == id)
            .ok_or_else(|| WorkspaceError::Invalid(format!("workspace {id} not found")))?;
        let record = &registry.workspaces[index];
        validate_ownership(record, owner_session, owner_flow)?;
        self.validate_record_scope(record)?;
        if record.is_instance_bound() {
            return Err(WorkspaceError::Invalid(
                "workspace is still referenced by a flow instance".into(),
            ));
        }
        match record.lifecycle_state() {
            WorkspaceState::Released if record.release.is_some() => return Ok(record.clone()),
            WorkspaceState::Releasing => {
                if record
                    .release
                    .as_ref()
                    .is_none_or(|intent| intent.force != force)
                {
                    return Err(WorkspaceError::Invalid(
                        "release options differ from the recorded intent".into(),
                    ));
                }
            }
            WorkspaceState::Active | WorkspaceState::Retained => {
                // Validate before recording intent, so a rejected initial request leaves no mutation.
                self.validate_worktree(record)?;
                if !force && crate::git::has_changes(&record.worktree_path)? {
                    return Err(WorkspaceError::Invalid(
                        "dirty workspace requires force=true".into(),
                    ));
                }
                let request_id = Self::prepare_release_marker(record)?;
                let pending = &mut registry.workspaces[index];
                pending.state = WorkspaceState::Releasing.as_str().into();
                pending.retained = false;
                pending.release = Some(WorkspaceRelease { request_id, force });
                self.save(registry)?;
            }
            state => {
                return Err(WorkspaceError::Invalid(format!(
                    "workspace {id} in state {} cannot be released",
                    state.as_str()
                )));
            }
        }
        hook(ReleaseStage::IntentSaved)?;
        self.remove_recorded_checkout(&registry.workspaces[index], force)?;
        hook(ReleaseStage::CheckoutRemoved)?;
        let released = &mut registry.workspaces[index];
        released.state = WorkspaceState::Released.as_str().into();
        released.lease = None;
        released.reconciled_at = Some(chrono::Utc::now());
        released.reconciliation_reason = Some("released by explicit workspace action".into());
        let result = released.clone();
        self.save(registry)?;
        Ok(result)
    }

    /// Called only after a durable cleanup intent. A surviving path or registration cannot be treated as completed removal.
    pub(super) fn remove_recorded_checkout(
        &self,
        record: &WorkspaceRecord,
        force: bool,
    ) -> Result<(), WorkspaceError> {
        self.validate_record_scope(record)?;
        let (present, registered) = self.checkout_presence(record)?;
        if !present && !registered {
            return Ok(());
        }
        if present != registered {
            return Err(WorkspaceError::Invalid(
                "workspace has a surviving path or Git registration; explicit recovery is required"
                    .into(),
            ));
        }
        self.validate_worktree(record)?;
        if let Some(intent) = &record.release {
            let marker = Self::release_marker_path(record)?;
            if Self::read_release_marker(&marker)? != intent.request_id {
                return Err(WorkspaceError::Invalid(
                    "checkout release identity changed; refusing removal".into(),
                ));
            }
        }
        GitCli::at(&self.repository_root).worktree_remove(&record.worktree_path, force)?;
        if self.checkout_presence(record)? != (false, false) {
            return Err(WorkspaceError::Invalid(
                "workspace removal is incomplete".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn checkout_presence(
        &self,
        record: &WorkspaceRecord,
    ) -> Result<(bool, bool), WorkspaceError> {
        let present = match std::fs::symlink_metadata(&record.worktree_path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let registered = GitCli::at(&self.repository_root)
            .worktree_list()?
            .iter()
            .any(|worktree| canonicalize(&worktree.path) == record.worktree_path);
        Ok((present, registered))
    }

    fn release_marker_path(record: &WorkspaceRecord) -> Result<std::path::PathBuf, WorkspaceError> {
        let repository =
            git2::Repository::open(&record.worktree_path).map_err(crate::git::GitError::from)?;
        Ok(repository.path().join("atman-release"))
    }

    fn read_release_marker(path: &std::path::Path) -> Result<uuid::Uuid, WorkspaceError> {
        if !std::fs::symlink_metadata(path)?.file_type().is_file() {
            return Err(WorkspaceError::Invalid(
                "invalid checkout release marker".into(),
            ));
        }
        let id = uuid::Uuid::parse_str(std::fs::read_to_string(path)?.trim())
            .map_err(|_| WorkspaceError::Invalid("invalid checkout release marker".into()))?;
        if id.is_nil() {
            return Err(WorkspaceError::Invalid(
                "invalid checkout release marker".into(),
            ));
        }
        Ok(id)
    }

    fn prepare_release_marker(record: &WorkspaceRecord) -> Result<uuid::Uuid, WorkspaceError> {
        use std::io::Write;
        let path = Self::release_marker_path(record)?;
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Self::read_release_marker(&path);
            }
            Err(error) => return Err(error.into()),
        };
        let id = uuid::Uuid::new_v4();
        let result = (|| -> Result<(), WorkspaceError> {
            file.write_all(id.to_string().as_bytes())?;
            file.sync_all()?;
            #[cfg(unix)]
            std::fs::File::open(path.parent().expect("Git administrative directory"))?
                .sync_all()?;
            Ok(())
        })();
        drop(file);
        if result.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        result?;
        Ok(id)
    }
}
