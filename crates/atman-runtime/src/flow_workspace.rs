use std::path::{Path, PathBuf};

use crate::git::GitCli;
use crate::git_workspace::{
    WorkspaceBinding, WorkspaceError, WorkspaceFinalizeOutcome, WorkspaceManager, WorkspacePolicy,
    WorkspaceRecord, WorkspaceState,
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

    pub fn retain(
        &self,
        workspace_id: &str,
        owner_session: &str,
        owner_flow: &str,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        self.manager()?
            .retain(workspace_id, true, Some(owner_session), Some(owner_flow))
    }

    pub fn release(
        &self,
        workspace_id: &str,
        owner_session: &str,
        owner_flow: &str,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        self.manager()?
            .release(workspace_id, Some(owner_session), Some(owner_flow), false)
    }

    fn manager(&self) -> Result<WorkspaceManager, WorkspaceError> {
        WorkspaceManager::at(&self.repository_cwd, self.external_root.as_deref())
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
        assert!(
            service
                .release(&dirty.workspace_id, "session", "dirty")
                .is_err()
        );
        assert!(dirty.path.exists());

        let retained = service
            .allocate(WorkspacePolicy::Auto, "session", "retained", None)
            .unwrap()
            .unwrap();
        let record = service
            .retain(&retained.workspace_id, "session", "retained")
            .unwrap();
        assert_eq!(record.lifecycle_state(), WorkspaceState::Retained);
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
