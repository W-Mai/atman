//! Recorded ownership and independently observed checkout availability.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;

use super::{WorkspaceError, WorkspaceManager, WorkspaceRecord, canonicalize, validate_id};

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceSnapshot {
    pub repository_root: PathBuf,
    pub storage_root: PathBuf,
    pub registry_present: bool,
    pub workspaces: Vec<WorkspaceInspection>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceInspection {
    pub record: WorkspaceRecord,
    pub checkout: WorkspaceCheckout,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkspaceCheckout {
    Available {
        head: String,
        branch: Option<String>,
        dirty: bool,
    },
    Missing,
    Unavailable {
        reason: String,
    },
}

impl WorkspaceManager {
    /// Inspect a known repository and storage location without creating metadata, acquiring execution ownership, reconciling leases, or releasing references. Callers authorize both paths before reading. Registry records form one snapshot; checkout observations may change independently.
    pub fn inspect(
        repository_root: &Path,
        storage_root: &Path,
    ) -> Result<WorkspaceSnapshot, WorkspaceError> {
        for path in [repository_root, storage_root] {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
            {
                return Err(WorkspaceError::Invalid(
                    "workspace inspection requires absolute root paths without parent traversal"
                        .into(),
                ));
            }
        }
        let repository_root = canonicalize(repository_root);
        let storage_root = canonicalize(storage_root);
        let manager = Self {
            repository_root: repository_root.clone(),
            managed_root: storage_root.join(".atman/worktrees"),
            registry_path: storage_root.join(".atman/workspaces.json"),
            registry_lock_path: storage_root.join(".atman/workspaces.lock"),
        };
        let registry = manager.load_existing()?;
        let registry_present = registry.is_some();
        let mut identities = std::collections::HashSet::new();
        let mut workspaces = Vec::new();
        for record in registry
            .into_iter()
            .flat_map(|registry| registry.workspaces)
        {
            validate_id(&record.id)?;
            if !identities.insert(record.id.clone())
                || record.repository_root != repository_root
                || record.worktree_path != manager.managed_root.join(&record.id)
            {
                return Err(WorkspaceError::Invalid(
                    "workspace registry identity or path does not match inspection scope".into(),
                ));
            }
            let checkout = match fs::metadata(&record.worktree_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    WorkspaceCheckout::Missing
                }
                Err(error) => WorkspaceCheckout::Unavailable {
                    reason: error.to_string(),
                },
                Ok(_) => match manager.inspect_checkout(&record) {
                    Ok(checkout) => checkout,
                    Err(error) => WorkspaceCheckout::Unavailable {
                        reason: error.to_string(),
                    },
                },
            };
            workspaces.push(WorkspaceInspection { record, checkout });
        }
        Ok(WorkspaceSnapshot {
            repository_root,
            storage_root,
            registry_present,
            workspaces,
        })
    }

    fn inspect_checkout(
        &self,
        record: &WorkspaceRecord,
    ) -> Result<WorkspaceCheckout, WorkspaceError> {
        self.validate_worktree(record)?;
        let repo =
            git2::Repository::open(&record.worktree_path).map_err(crate::git::GitError::from)?;
        let head = repo.head().map_err(crate::git::GitError::from)?;
        let oid = head
            .target()
            .ok_or_else(|| WorkspaceError::Invalid("workspace HEAD has no commit".into()))?;
        let branch = head
            .is_branch()
            .then(|| head.shorthand().map(str::to_owned))
            .flatten();
        let dirty = crate::git::has_changes(&record.worktree_path)?;
        Ok(WorkspaceCheckout::Available {
            head: oid.to_string(),
            branch,
            dirty,
        })
    }
}
