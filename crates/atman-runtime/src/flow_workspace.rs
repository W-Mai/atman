use std::path::{Path, PathBuf};

use crate::flow_store::{
    FlowRef,
    workspace::{WorkspaceRequest, WorkspaceSource},
};
use crate::git::GitCli;
use crate::git_workspace::{
    WorkspaceBinding, WorkspaceError, WorkspaceFinalizeOutcome, WorkspaceManager, WorkspacePolicy,
    WorkspaceState,
};

#[derive(Debug, Clone)]
pub struct FlowWorkspaceService {
    repository_cwd: PathBuf,
    external_root: Option<PathBuf>,
    daemon_generation: String,
}

impl FlowWorkspaceService {
    pub fn new(
        repository_cwd: impl Into<PathBuf>,
        external_root: Option<PathBuf>,
        daemon_generation: impl Into<String>,
    ) -> Result<Self, WorkspaceError> {
        let daemon_generation = daemon_generation.into();
        if daemon_generation.is_empty() {
            return Err(WorkspaceError::Invalid(
                "daemon generation must be non-empty".into(),
            ));
        }
        Ok(Self {
            repository_cwd: repository_cwd.into(),
            external_root,
            daemon_generation,
        })
    }

    pub fn allocate(
        &self,
        policy: WorkspacePolicy,
        owner_session: &str,
        owner_flow: &str,
        parent_execution_root: Option<&Path>,
    ) -> Result<Option<WorkspaceBinding>, WorkspaceError> {
        if policy == WorkspacePolicy::None {
            return Ok(None);
        }
        let manager = self.manager()?;
        let execution_root = parent_execution_root.unwrap_or(&self.repository_cwd);
        let execution_manager =
            WorkspaceManager::at(execution_root, self.external_root.as_deref())?;
        if execution_manager.repository_root() != manager.repository_root() {
            return Err(WorkspaceError::Invalid(
                "parent execution root belongs to a different repository".into(),
            ));
        }
        let base_oid = GitCli::at(execution_root).head_oid()?;
        let id = workspace_id(owner_flow);
        let record = manager.create_managed(
            &id,
            owner_session,
            owner_flow,
            &self.daemon_generation,
            policy == WorkspacePolicy::Retain,
            &base_oid,
        )?;
        Ok(Some(record.binding()))
    }

    /// Prepare immutable allocation metadata without creating a worktree or registry record.
    pub fn prepare_instance(
        &self,
        policy: WorkspacePolicy,
        owner: &FlowRef,
        parent_execution_root: Option<&Path>,
    ) -> Result<WorkspaceRequest, WorkspaceError> {
        if policy == WorkspacePolicy::None {
            return Err(WorkspaceError::Invalid(
                "allocation requires auto or retain".into(),
            ));
        }
        let manager =
            WorkspaceManager::locate(&self.repository_cwd, self.external_root.as_deref())?;
        let execution_root = parent_execution_root.unwrap_or(&self.repository_cwd);
        let execution_manager =
            WorkspaceManager::locate(execution_root, self.external_root.as_deref())?;
        if manager.repository_root() != execution_manager.repository_root() {
            return Err(WorkspaceError::Invalid(
                "parent execution root belongs to a different repository".into(),
            ));
        }
        let id = workspace_id(&owner.instance_id.to_string());
        Ok(WorkspaceRequest {
            binding: WorkspaceBinding {
                path: manager.managed_root().join(&id),
                workspace_id: id,
                repository_root: manager.repository_root().into(),
                branch: None,
            },
            source: WorkspaceSource::Allocate {
                base: GitCli::at(execution_root).head_oid()?,
                retained: policy == WorkspacePolicy::Retain,
            },
        })
    }

    pub fn finalize(
        &self,
        binding: &WorkspaceBinding,
        owner_session: &str,
        owner_flow: &str,
    ) -> Result<WorkspaceFinalizeOutcome, WorkspaceError> {
        let manager = self.manager()?;
        if manager.repository_root() != binding.repository_root {
            return Err(WorkspaceError::Invalid(
                "workspace binding repository mismatch".into(),
            ));
        }
        manager.finalize_managed(&binding.workspace_id, owner_session, owner_flow)
    }

    pub fn persisted_state(&self, binding: &WorkspaceBinding) -> Option<WorkspaceState> {
        let manager = self.manager().ok()?;
        if manager.repository_root() != binding.repository_root {
            return None;
        }
        manager
            .get(&binding.workspace_id)
            .ok()
            .map(|record| record.lifecycle_state())
    }

    fn manager(&self) -> Result<WorkspaceManager, WorkspaceError> {
        WorkspaceManager::at(&self.repository_cwd, self.external_root.as_deref())
    }
}

impl WorkspaceRequest {
    fn manager(&self, create: bool) -> Result<WorkspaceManager, WorkspaceError> {
        let root = self
            .binding
            .path
            .parent()
            .and_then(|path| path.parent())
            .and_then(|path| path.parent())
            .ok_or_else(|| WorkspaceError::Invalid("invalid workspace storage root".into()))?;
        let manager = WorkspaceManager::locate(&self.binding.repository_root, Some(root))?;
        if manager.repository_root() != self.binding.repository_root
            || manager.managed_root().join(&self.binding.workspace_id) != self.binding.path
        {
            return Err(WorkspaceError::Invalid(
                "workspace storage binding mismatch".into(),
            ));
        }
        if create {
            WorkspaceManager::at(&self.binding.repository_root, Some(root))
        } else {
            Ok(manager)
        }
    }

    pub(crate) fn constrain_authority(
        &self,
        owner: &FlowRef,
        current: &crate::flow_authority::EffectiveAuthority,
        requested: &crate::flow_authority::EffectiveAuthority,
        shell: bool,
    ) -> std::io::Result<crate::flow_authority::EffectiveAuthority> {
        use crate::flow_authority::ChildWorkspaceAuthority;
        use crate::flow_store::invalid;
        self.validate(owner)?;
        let manager = self.manager(false).map_err(invalid)?;
        let root = current
            .workspace_root
            .as_deref()
            .ok_or_else(|| invalid("workspace allocation requires project authority"))?;
        let external = manager
            .managed_root()
            .parent()
            .and_then(|path| path.parent());
        let authorized = WorkspaceManager::locate(root, external).map_err(invalid)?;
        if authorized.repository_root() != manager.repository_root()
            || requested.workspace_root.as_ref() != Some(&self.binding.path)
        {
            return Err(invalid(
                "workspace request exceeds the bound repository authority",
            ));
        }
        if matches!(self.source, WorkspaceSource::Share { .. })
            && !self
                .binding
                .path
                .starts_with(crate::fs_access::canonicalize_stable(root))
        {
            return Err(invalid(
                "shared workspace exceeds current directory authority",
            ));
        }
        current
            .inherited_child(
                shell,
                ChildWorkspaceAuthority::TrustedDelegation(self.binding.path.clone()),
            )
            .and_then(|ceiling| {
                ceiling.for_child(requested, shell, Some(self.binding.path.clone()))
            })
            .map_err(invalid)
    }

    pub(crate) fn acquire(&self, owner: &FlowRef) -> std::io::Result<()> {
        use crate::flow_store::invalid;
        self.validate(owner)?;
        let manager = self.manager(true).map_err(invalid)?;
        match &self.source {
            WorkspaceSource::Allocate { base, retained } => {
                manager
                    .create_instance(
                        &self.binding.workspace_id,
                        owner,
                        &uuid::Uuid::new_v4().to_string(),
                        *retained,
                        base,
                    )
                    .map_err(invalid)?;
            }
            WorkspaceSource::Share { sponsor } => {
                manager
                    .share_instance(&self.binding, sponsor, owner)
                    .map_err(invalid)?;
            }
        }
        self.validate_live(owner)
    }

    pub(crate) fn validate_live(&self, owner: &FlowRef) -> std::io::Result<()> {
        self.validate(owner)?;
        self.manager(false)
            .and_then(|manager| manager.validate_instance(&self.binding, owner))
            .map_err(crate::flow_store::invalid)?;
        Ok(())
    }

    pub(crate) fn release(&self, owner: &FlowRef) -> std::io::Result<()> {
        use crate::flow_store::invalid;
        self.validate(owner)?;
        let manager = self.manager(false).map_err(invalid)?;
        let Some(record) = manager
            .list()
            .map_err(invalid)?
            .into_iter()
            .find(|record| record.id == self.binding.workspace_id)
        else {
            return Ok(());
        };
        if record.binding() != self.binding {
            return Err(invalid("workspace release binding mismatch"));
        }
        if record
            .instances
            .iter()
            .any(|reference| &reference.owner == owner)
        {
            manager
                .release_instance(&self.binding.workspace_id, owner)
                .map_err(invalid)?;
        }
        Ok(())
    }
}

fn workspace_id(owner_flow: &str) -> String {
    format!("flow-{owner_flow}")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::git_workspace::WorkspaceState;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["config", "user.name", "Atman Test"]);
        git(
            tmp.path(),
            &["config", "user.email", "atman@example.invalid"],
        );
        git(tmp.path(), &["config", "commit.gpgsign", "false"]);
        fs::write(tmp.path().join("README.md"), "committed\n").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "initial"]);
        tmp
    }

    #[test]
    fn none_policy_does_not_require_a_repository() {
        let service = FlowWorkspaceService::new("missing", None, "generation").unwrap();
        assert_eq!(
            service
                .allocate(WorkspacePolicy::None, "session", "flow", None)
                .unwrap(),
            None
        );
    }

    #[test]
    fn auto_workspace_persists_trusted_lease_and_releases_cleanly() {
        let repo = repo();
        let service = FlowWorkspaceService::new(repo.path(), None, "generation-one").unwrap();
        let binding = service
            .allocate(WorkspacePolicy::Auto, "session", "0195-flow", None)
            .unwrap()
            .unwrap();
        let manager = WorkspaceManager::at(repo.path(), None).unwrap();
        let record = manager.get(&binding.workspace_id).unwrap();
        assert_eq!(record.owner_session.as_deref(), Some("session"));
        assert_eq!(record.owner_flow.as_deref(), Some("0195-flow"));
        assert_eq!(record.lifecycle_state(), WorkspaceState::Active);
        assert_eq!(
            record
                .lease
                .as_ref()
                .map(|lease| lease.daemon_generation.as_str()),
            Some("generation-one")
        );

        let outcome = service.finalize(&binding, "session", "0195-flow").unwrap();
        assert!(matches!(outcome, WorkspaceFinalizeOutcome::Released(_)));
        assert!(!binding.path.exists());
        assert!(matches!(
            service.finalize(&binding, "session", "0195-flow").unwrap(),
            WorkspaceFinalizeOutcome::AlreadyReleased(_)
        ));
    }

    #[test]
    fn nested_workspace_starts_from_parent_execution_head() {
        let repo = repo();
        let original_cwd = std::env::current_dir().unwrap();
        let main_head = git(repo.path(), &["rev-parse", "HEAD"]);
        let service = FlowWorkspaceService::new(repo.path(), None, "generation").unwrap();
        let parent = service
            .allocate(WorkspacePolicy::Retain, "session", "parent", None)
            .unwrap()
            .unwrap();

        fs::write(parent.path.join("parent-marker.txt"), "from parent\n").unwrap();
        git(&parent.path, &["add", "parent-marker.txt"]);
        git(&parent.path, &["commit", "-q", "-m", "parent marker"]);
        let parent_head = git(&parent.path, &["rev-parse", "HEAD"]);

        let child = service
            .allocate(
                WorkspacePolicy::Retain,
                "session",
                "child",
                Some(&parent.path),
            )
            .unwrap()
            .unwrap();
        let child_head = git(&child.path, &["rev-parse", "HEAD"]);
        let manager = WorkspaceManager::at(repo.path(), None).unwrap();
        let child_record = manager.get(&child.workspace_id).unwrap();

        assert_eq!(child_head, parent_head);
        assert_eq!(
            child_record.allocation_base.as_deref(),
            Some(parent_head.as_str())
        );
        assert_eq!(child.repository_root, parent.repository_root);
        assert_eq!(child.repository_root, manager.repository_root());
        assert_eq!(
            fs::read_to_string(child.path.join("parent-marker.txt")).unwrap(),
            "from parent\n"
        );
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), main_head);
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
    }

    #[test]
    fn dirty_and_retained_workspaces_are_preserved() {
        let repo = repo();
        let service = FlowWorkspaceService::new(repo.path(), None, "generation").unwrap();
        let dirty = service
            .allocate(WorkspacePolicy::Auto, "session", "dirty", None)
            .unwrap()
            .unwrap();
        fs::write(dirty.path.join("dirty.txt"), "inspect me\n").unwrap();
        assert!(matches!(
            service.finalize(&dirty, "session", "dirty").unwrap(),
            WorkspaceFinalizeOutcome::Dirty(_)
        ));
        assert!(dirty.path.exists());

        let retained = service
            .allocate(WorkspacePolicy::Retain, "session", "retained", None)
            .unwrap()
            .unwrap();
        assert!(matches!(
            service.finalize(&retained, "session", "retained").unwrap(),
            WorkspaceFinalizeOutcome::Retained(_)
        ));
        assert!(retained.path.exists());
    }

    #[test]
    fn owner_mismatch_and_unconfigured_bare_repository_are_rejected() {
        let repo = repo();
        let service = FlowWorkspaceService::new(repo.path(), None, "generation").unwrap();
        let binding = service
            .allocate(WorkspacePolicy::Auto, "session", "owned", None)
            .unwrap()
            .unwrap();
        assert!(service.finalize(&binding, "other", "owned").is_err());
        assert!(binding.path.exists());

        let bare_parent = tempfile::tempdir().unwrap();
        let bare = bare_parent.path().join("repo.git");
        git(
            repo.path(),
            &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
        );
        let bare_service = FlowWorkspaceService::new(&bare, None, "generation").unwrap();
        assert!(
            bare_service
                .allocate(WorkspacePolicy::Auto, "session", "bare", None)
                .is_err()
        );
    }
}
