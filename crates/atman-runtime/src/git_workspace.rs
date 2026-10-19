use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::git::{GitCli, GitError};

mod inspection;
pub use inspection::{WorkspaceCheckout, WorkspaceInspection, WorkspaceSnapshot};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspacePolicy {
    #[default]
    None,
    Auto,
    Retain,
}

impl std::str::FromStr for WorkspacePolicy {
    type Err = WorkspaceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "auto" => Ok(Self::Auto),
            "retain" => Ok(Self::Retain),
            other => Err(WorkspaceError::Invalid(format!(
                "unknown workspace policy {other:?}; expected none, auto, or retain"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceState {
    Allocating,
    Active,
    Bound,
    TerminalPending,
    Retained,
    Dirty,
    Orphaned,
    Released,
    Unknown(String),
}

impl WorkspaceState {
    pub fn parse(value: &str) -> Self {
        match value {
            "allocating" => Self::Allocating,
            "active" => Self::Active,
            "bound" => Self::Bound,
            "terminal_pending" => Self::TerminalPending,
            "retained" => Self::Retained,
            "dirty" => Self::Dirty,
            "orphaned" => Self::Orphaned,
            "released" => Self::Released,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Allocating => "allocating",
            Self::Active => "active",
            Self::Bound => "bound",
            Self::TerminalPending => "terminal_pending",
            Self::Retained => "retained",
            Self::Dirty => "dirty",
            Self::Orphaned => "orphaned",
            Self::Released => "released",
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceBinding {
    pub workspace_id: String,
    pub path: PathBuf,
    pub repository_root: PathBuf,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceLease {
    pub daemon_generation: String,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
}

/// A durable instance reference, independent of a host process or invocation lease.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInstanceBinding {
    pub owner: crate::flow_store::FlowRef,
    pub released: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceFinalizeOutcome {
    Released(WorkspaceRecord),
    Referenced(WorkspaceRecord),
    Dirty(WorkspaceRecord),
    Retained(WorkspaceRecord),
    AlreadyReleased(WorkspaceRecord),
}

const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("git: {0}")]
    Git(#[from] GitError),
    #[error("workspace I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace registry: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid workspace: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRecord {
    pub id: String,
    pub repository_root: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
    pub owner_session: Option<String>,
    pub owner_flow: Option<String>,
    pub state: String,
    pub retained: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<WorkspaceLease>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<WorkspaceInstanceBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciled_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciliation_reason: Option<String>,
}

impl WorkspaceRecord {
    pub fn is_instance_bound(&self) -> bool {
        self.instances.iter().any(|binding| !binding.released)
    }

    pub fn lifecycle_state(&self) -> WorkspaceState {
        WorkspaceState::parse(&self.state)
    }

    pub fn binding(&self) -> WorkspaceBinding {
        WorkspaceBinding {
            workspace_id: self.id.clone(),
            path: self.worktree_path.clone(),
            repository_root: self.repository_root.clone(),
            branch: self.branch.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceRegistry {
    version: u32,
    workspaces: Vec<WorkspaceRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllocationStage {
    BeforeWorktreeAdd,
    AfterWorktreeAdd,
    BeforeFinalSave,
}

pub struct WorkspaceManager {
    repository_root: PathBuf,
    managed_root: PathBuf,
    registry_path: PathBuf,
    registry_lock_path: PathBuf,
}

impl WorkspaceManager {
    pub(crate) fn locate(cwd: &Path, external_root: Option<&Path>) -> Result<Self, WorkspaceError> {
        Self::resolve(cwd, external_root).map(|(manager, _, _)| manager)
    }

    pub fn at(cwd: &Path, external_root: Option<&Path>) -> Result<Self, WorkspaceError> {
        let (manager, common, is_bare) = Self::resolve(cwd, external_root)?;
        fs::create_dir_all(&manager.managed_root)?;
        let ignore = manager.managed_root.join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, "*\n")?;
        }
        if !is_bare {
            let exclude = common.join("info").join("exclude");
            fs::create_dir_all(exclude.parent().expect("exclude parent"))?;
            add_exclude(&exclude, "/.atman/worktrees/")?;
        }
        Ok(manager)
    }

    pub fn open_existing(
        cwd: &Path,
        external_root: Option<&Path>,
    ) -> Result<Option<Self>, WorkspaceError> {
        let (manager, _, _) = Self::resolve(cwd, external_root)?;
        Ok(manager.registry_path.exists().then_some(manager))
    }

    fn resolve(
        cwd: &Path,
        external_root: Option<&Path>,
    ) -> Result<(Self, PathBuf, bool), WorkspaceError> {
        let git_common = git_output(cwd, &["rev-parse", "--git-common-dir"])?;
        let common = canonicalize_from(cwd, Path::new(git_common.trim()));
        let is_bare = git_output(cwd, &["rev-parse", "--is-bare-repository"])?.trim() == "true";
        let repository_root = if is_bare {
            common.clone()
        } else {
            common
                .parent()
                .ok_or_else(|| {
                    WorkspaceError::Invalid("git common directory has no parent".into())
                })?
                .to_path_buf()
        };
        let storage_root = if is_bare {
            external_root
                .ok_or_else(|| {
                    WorkspaceError::Invalid("bare repositories require external_root".into())
                })?
                .canonicalize()?
        } else {
            repository_root.clone()
        };
        Ok((
            Self {
                repository_root,
                managed_root: storage_root.join(".atman").join("worktrees"),
                registry_path: storage_root.join(".atman").join("workspaces.json"),
                registry_lock_path: storage_root.join(".atman").join("workspaces.lock"),
            },
            common,
            is_bare,
        ))
    }

    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }
    pub fn managed_root(&self) -> &Path {
        &self.managed_root
    }

    pub fn create(
        &self,
        id: &str,
        branch: Option<&str>,
        base: Option<&str>,
        create_branch: bool,
        owner_session: Option<&str>,
        owner_flow: Option<&str>,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        validate_id(id)?;
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        if let Some(existing) = registry.workspaces.iter().find(|w| w.id == id) {
            return Ok(existing.clone());
        }
        let path = self.managed_root.join(id);
        ensure_inside(&path, &self.managed_root)?;
        let record = GitCli::at(&self.repository_root).worktree_add(
            &path,
            branch,
            base,
            create_branch,
            false,
        )?;
        let item = WorkspaceRecord {
            id: id.into(),
            repository_root: self.repository_root.clone(),
            worktree_path: canonicalize(&record.path),
            branch: record.branch,
            owner_session: owner_session.map(str::to_owned),
            owner_flow: owner_flow.map(str::to_owned),
            state: WorkspaceState::Active.as_str().into(),
            retained: false,
            allocation_base: base.map(str::to_owned),
            allocation_policy: None,
            lease: None,
            instances: Vec::new(),
            reconciled_at: None,
            reconciliation_reason: None,
        };
        registry.workspaces.push(item.clone());
        self.save(&registry)?;
        Ok(item)
    }

    pub fn create_managed(
        &self,
        id: &str,
        owner_session: &str,
        owner_flow: &str,
        daemon_generation: &str,
        retained: bool,
        base_oid: &str,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        self.create_managed_with_hook(
            id,
            owner_session,
            owner_flow,
            daemon_generation,
            retained,
            base_oid,
            None,
            |_| Ok(()),
        )
    }

    /// Allocate a workspace reference before creating Git state. The instance owner must authorize creation and retain the returned binding in its durable journal.
    pub fn create_instance(
        &self,
        id: &str,
        owner: &crate::flow_store::FlowRef,
        daemon_generation: &str,
        retained: bool,
        base_oid: &str,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        if owner.session_id.0.is_nil() || owner.instance_id.0.0.is_nil() {
            return Err(WorkspaceError::Invalid(
                "instance identity must not be nil".into(),
            ));
        }
        self.create_managed_with_hook(
            id,
            &owner.session_id.to_string(),
            &owner.instance_id.to_string(),
            daemon_generation,
            retained,
            base_oid,
            Some(owner.clone()),
            |_| Ok(()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_managed_with_hook(
        &self,
        id: &str,
        owner_session: &str,
        owner_flow: &str,
        daemon_generation: &str,
        retained: bool,
        base_oid: &str,
        instance: Option<crate::flow_store::FlowRef>,
        mut hook: impl FnMut(AllocationStage) -> Result<(), WorkspaceError>,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        if owner_session.is_empty() || owner_flow.is_empty() || daemon_generation.is_empty() {
            return Err(WorkspaceError::Invalid(
                "managed workspace ownership and daemon generation must be non-empty".into(),
            ));
        }
        validate_id(id)?;
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        if let Some(item) = registry.workspaces.iter().find(|item| item.id == id) {
            validate_ownership(item, Some(owner_session), Some(owner_flow))?;
            let same_generation = item
                .lease
                .as_ref()
                .is_some_and(|lease| lease.daemon_generation == daemon_generation);
            let same_instance =
                item.instances.first().map(|binding| &binding.owner) == instance.as_ref();
            if same_instance
                && ((item.lifecycle_state() == WorkspaceState::Bound
                    && item
                        .instances
                        .first()
                        .is_some_and(|binding| !binding.released))
                    || (item.lifecycle_state() == WorkspaceState::Active && same_generation))
                && item.retained == retained
                && item.allocation_base.as_deref() == Some(base_oid)
            {
                if item.is_instance_bound() {
                    self.validate_worktree(item)?;
                }
                return Ok(item.clone());
            }
            return Err(WorkspaceError::Invalid(format!(
                "workspace {id} already exists with a different lifecycle lease"
            )));
        }

        let path = self.managed_root.join(id);
        ensure_inside(&path, &self.managed_root)?;
        let item = WorkspaceRecord {
            id: id.into(),
            repository_root: self.repository_root.clone(),
            worktree_path: path.clone(),
            branch: None,
            owner_session: Some(owner_session.to_owned()),
            owner_flow: Some(owner_flow.to_owned()),
            state: WorkspaceState::Allocating.as_str().into(),
            retained,
            allocation_base: Some(base_oid.to_owned()),
            allocation_policy: Some(if retained { "retain" } else { "auto" }.into()),
            lease: Some(WorkspaceLease {
                daemon_generation: daemon_generation.to_owned(),
                acquired_at: chrono::Utc::now(),
            }),
            instances: instance
                .into_iter()
                .map(|owner| WorkspaceInstanceBinding {
                    owner,
                    released: false,
                })
                .collect(),
            reconciled_at: None,
            reconciliation_reason: None,
        };
        registry.workspaces.push(item);
        self.save(&registry)?;
        hook(AllocationStage::BeforeWorktreeAdd)?;

        let worktree = match GitCli::at(&self.repository_root).worktree_add(
            &path,
            None,
            Some(base_oid),
            false,
            false,
        ) {
            Ok(worktree) => worktree,
            Err(error) => {
                let index = registry.workspaces.len() - 1;
                let registered = GitCli::at(&self.repository_root)
                    .worktree_list()
                    .map(|worktrees| {
                        worktrees
                            .iter()
                            .any(|worktree| canonicalize(&worktree.path) == canonicalize(&path))
                    })
                    .unwrap_or(true);
                let record = &mut registry.workspaces[index];
                record.reconciled_at = Some(chrono::Utc::now());
                if !path.exists() && !registered {
                    record.state = WorkspaceState::Released.as_str().into();
                    record.lease = None;
                    release_instance_reference(record);
                    record.reconciliation_reason = Some(format!(
                        "allocation failed before creating worktree: {error}"
                    ));
                } else {
                    record.state = WorkspaceState::Orphaned.as_str().into();
                    record.reconciliation_reason = Some(format!(
                        "allocation failed with residual path or Git registration: {error}"
                    ));
                }
                self.save(&registry)?;
                return Err(error.into());
            }
        };
        hook(AllocationStage::AfterWorktreeAdd)?;

        let index = registry.workspaces.len() - 1;
        registry.workspaces[index].worktree_path = canonicalize(&worktree.path);
        registry.workspaces[index].branch = worktree.branch;
        registry.workspaces[index].state = WorkspaceState::Active.as_str().into();
        if registry.workspaces[index].is_instance_bound() {
            registry.workspaces[index].state = WorkspaceState::Bound.as_str().into();
            registry.workspaces[index].lease = None;
        }
        hook(AllocationStage::BeforeFinalSave)?;
        self.save(&registry)?;
        Ok(registry.workspaces[index].clone())
    }

    pub fn finalize_managed(
        &self,
        id: &str,
        owner_session: &str,
        owner_flow: &str,
    ) -> Result<WorkspaceFinalizeOutcome, WorkspaceError> {
        self.finalize_owned(id, owner_session, owner_flow, None)
    }

    /// Validate a recorded instance binding without recreating a missing worktree.
    pub fn validate_instance(
        &self,
        binding: &WorkspaceBinding,
        owner: &crate::flow_store::FlowRef,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let _lock = self.lock_registry()?;
        let record = self.get(&binding.workspace_id)?;
        if !record
            .instances
            .iter()
            .any(|instance| &instance.owner == owner && !instance.released)
            || record.lifecycle_state() != WorkspaceState::Bound
            || record.binding() != *binding
        {
            return Err(WorkspaceError::Invalid(
                "workspace instance binding mismatch".into(),
            ));
        }
        self.validate_worktree(&record)?;
        Ok(record)
    }

    /// Share a live workspace reference before activating another instance. The caller authenticates and authorizes both instances.
    pub fn share_instance(
        &self,
        binding: &WorkspaceBinding,
        sponsor: &crate::flow_store::FlowRef,
        owner: &crate::flow_store::FlowRef,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        let record = registry
            .workspaces
            .iter_mut()
            .find(|record| record.id == binding.workspace_id)
            .ok_or_else(|| WorkspaceError::Invalid("workspace not found".into()))?;
        if record.binding() != *binding {
            return Err(WorkspaceError::Invalid("workspace binding mismatch".into()));
        }
        // A retry is a lookup, including after the sponsor or recipient has released it.
        if record
            .instances
            .iter()
            .any(|reference| &reference.owner == owner)
        {
            return Ok(record.clone());
        }
        if owner.session_id.0.is_nil()
            || owner.instance_id.0.0.is_nil()
            || record.lifecycle_state() != WorkspaceState::Bound
            || !record
                .instances
                .iter()
                .any(|reference| &reference.owner == sponsor && !reference.released)
        {
            return Err(WorkspaceError::Invalid(
                "sharing requires a live sponsor and a valid instance".into(),
            ));
        }
        self.validate_worktree(record)?;
        record.instances.push(WorkspaceInstanceBinding {
            owner: owner.clone(),
            released: false,
        });
        let result = record.clone();
        self.save(&registry)?;
        Ok(result)
    }

    fn validate_worktree(&self, record: &WorkspaceRecord) -> Result<(), WorkspaceError> {
        let path = &record.worktree_path;
        if record.repository_root != self.repository_root
            || path.canonicalize()? != *path
            || !GitCli::at(&self.repository_root)
                .worktree_list()?
                .iter()
                .any(|worktree| canonicalize(&worktree.path) == *path)
            || canonicalize_from(
                path,
                Path::new(git_output(path, &["rev-parse", "--git-common-dir"])?.trim()),
            ) != canonicalize_from(
                &self.repository_root,
                Path::new(
                    git_output(&self.repository_root, &["rev-parse", "--git-common-dir"])?.trim(),
                ),
            )
            || canonicalize(Path::new(
                git_output(path, &["rev-parse", "--show-toplevel"])?.trim(),
            )) != *path
        {
            return Err(WorkspaceError::Invalid(
                "workspace Git registration mismatch".into(),
            ));
        }
        Ok(())
    }

    /// Release the exact instance reference after its execution and process resources have stopped. Failed cleanup remains retryable; dirty and explicitly retained worktrees survive.
    pub fn release_instance(
        &self,
        id: &str,
        owner: &crate::flow_store::FlowRef,
    ) -> Result<WorkspaceFinalizeOutcome, WorkspaceError> {
        self.finalize_owned(
            id,
            &owner.session_id.to_string(),
            &owner.instance_id.to_string(),
            Some(owner),
        )
    }

    fn finalize_owned(
        &self,
        id: &str,
        owner_session: &str,
        owner_flow: &str,
        instance: Option<&crate::flow_store::FlowRef>,
    ) -> Result<WorkspaceFinalizeOutcome, WorkspaceError> {
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        let index = registry
            .workspaces
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| WorkspaceError::Invalid(format!("workspace {id} not found")))?;
        let item = &mut registry.workspaces[index];
        match instance {
            Some(owner) => {
                let reference = item
                    .instances
                    .iter()
                    .position(|binding| &binding.owner == owner)
                    .ok_or_else(|| {
                        WorkspaceError::Invalid("workspace instance ownership mismatch".into())
                    })?;
                if item.instances[reference].released {
                    let record = item.clone();
                    return match record.lifecycle_state() {
                        WorkspaceState::Released => {
                            Ok(WorkspaceFinalizeOutcome::AlreadyReleased(record))
                        }
                        WorkspaceState::Dirty => Ok(WorkspaceFinalizeOutcome::Dirty(record)),
                        WorkspaceState::Retained => Ok(WorkspaceFinalizeOutcome::Retained(record)),
                        _ if record.is_instance_bound() => {
                            Ok(WorkspaceFinalizeOutcome::Referenced(record))
                        }
                        _ => Err(WorkspaceError::Invalid(
                            "released instance has incomplete workspace cleanup".into(),
                        )),
                    };
                }
                if item
                    .instances
                    .iter()
                    .any(|binding| &binding.owner != owner && !binding.released)
                {
                    item.instances[reference].released = true;
                    let record = item.clone();
                    self.save(&registry)?;
                    return Ok(WorkspaceFinalizeOutcome::Referenced(record));
                }
            }
            None => {
                validate_ownership(item, Some(owner_session), Some(owner_flow))?;
                if !item.instances.is_empty() {
                    return Err(WorkspaceError::Invalid(
                        "workspace lifetime does not match the releasing owner".into(),
                    ));
                }
            }
        }

        let state = registry.workspaces[index].lifecycle_state();
        if state == WorkspaceState::Released {
            return Ok(WorkspaceFinalizeOutcome::AlreadyReleased(
                registry.workspaces[index].clone(),
            ));
        }
        if instance.is_some() {
            if matches!(state, WorkspaceState::Unknown(_)) {
                return Err(WorkspaceError::Invalid(
                    "unknown instance workspace state".into(),
                ));
            }
            let record = &registry.workspaces[index];
            let registered = GitCli::at(&self.repository_root)
                .worktree_list()?
                .iter()
                .any(|worktree| {
                    canonicalize(&worktree.path) == canonicalize(&record.worktree_path)
                });
            if !record.worktree_path.exists() && !registered {
                let record = &mut registry.workspaces[index];
                record.state = WorkspaceState::Released.as_str().into();
                record.lease = None;
                release_instance_reference(record);
                let result = record.clone();
                self.save(&registry)?;
                return Ok(WorkspaceFinalizeOutcome::Released(result));
            }
            // Recovery may discard only the registered worktree owned by this instance.
            self.validate_worktree(record)?;
        }
        if registry.workspaces[index].retained || state == WorkspaceState::Retained {
            registry.workspaces[index].retained = true;
            registry.workspaces[index].state = WorkspaceState::Retained.as_str().into();
            registry.workspaces[index].lease = None;
            release_instance_reference(&mut registry.workspaces[index]);
            let result = registry.workspaces[index].clone();
            self.save(&registry)?;
            return Ok(WorkspaceFinalizeOutcome::Retained(result));
        }
        if instance.is_none()
            && matches!(
                state,
                WorkspaceState::Allocating | WorkspaceState::Orphaned | WorkspaceState::Unknown(_)
            )
        {
            return Err(WorkspaceError::Invalid(format!(
                "workspace {id} in state {} requires explicit recovery",
                state.as_str()
            )));
        }

        if state == WorkspaceState::TerminalPending {
            let path = &registry.workspaces[index].worktree_path;
            let registered = GitCli::at(&self.repository_root)
                .worktree_list()?
                .iter()
                .any(|worktree| canonicalize(&worktree.path) == canonicalize(path));
            if !path.exists() && !registered {
                registry.workspaces[index].state = WorkspaceState::Released.as_str().into();
                registry.workspaces[index].lease = None;
                release_instance_reference(&mut registry.workspaces[index]);
                let result = registry.workspaces[index].clone();
                self.save(&registry)?;
                return Ok(WorkspaceFinalizeOutcome::Released(result));
            }
        }

        registry.workspaces[index].state = WorkspaceState::TerminalPending.as_str().into();
        self.save(&registry)?;

        if crate::git::has_changes(&registry.workspaces[index].worktree_path)? {
            registry.workspaces[index].state = WorkspaceState::Dirty.as_str().into();
            registry.workspaces[index].lease = None;
            release_instance_reference(&mut registry.workspaces[index]);
            let result = registry.workspaces[index].clone();
            self.save(&registry)?;
            return Ok(WorkspaceFinalizeOutcome::Dirty(result));
        }

        let record = &mut registry.workspaces[index];
        let head = GitCli::at(&record.worktree_path).head_oid()?;
        if record.allocation_base.as_deref() != Some(head.as_str()) {
            // A clean index does not prove that a detached checkout has no work to preserve.
            record.retained = true;
            record.state = WorkspaceState::Retained.as_str().into();
            record.lease = None;
            release_instance_reference(record);
            let result = record.clone();
            self.save(&registry)?;
            return Ok(WorkspaceFinalizeOutcome::Retained(result));
        }

        GitCli::at(&self.repository_root)
            .worktree_remove(&registry.workspaces[index].worktree_path, false)?;
        registry.workspaces[index].state = WorkspaceState::Released.as_str().into();
        registry.workspaces[index].lease = None;
        release_instance_reference(&mut registry.workspaces[index]);
        let result = registry.workspaces[index].clone();
        self.save(&registry)?;
        Ok(WorkspaceFinalizeOutcome::Released(result))
    }

    pub fn reconcile_generation(
        &self,
        daemon_generation: &str,
    ) -> Result<Vec<WorkspaceRecord>, WorkspaceError> {
        if daemon_generation.is_empty() {
            return Err(WorkspaceError::Invalid(
                "daemon generation must be non-empty".into(),
            ));
        }
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        let reconciled_at = chrono::Utc::now();
        let worktrees = GitCli::at(&self.repository_root).worktree_list()?;
        let mut changed = Vec::new();
        for item in &mut registry.workspaces {
            let Some(lease) = &item.lease else {
                continue;
            };
            let previous_generation = lease.daemon_generation.clone();
            if previous_generation == daemon_generation {
                continue;
            }
            match item.lifecycle_state() {
                WorkspaceState::Allocating => {
                    let path_exists = item.worktree_path.exists();
                    let registered = worktrees.iter().any(|worktree| {
                        canonicalize(&worktree.path) == canonicalize(&item.worktree_path)
                    });
                    item.reconciled_at = Some(reconciled_at);
                    if !path_exists && !registered {
                        item.state = WorkspaceState::Released.as_str().into();
                        item.lease = None;
                        release_instance_reference(item);
                        item.reconciliation_reason = Some(format!(
                            "allocation from older daemon generation {} left no path or Git registration",
                            previous_generation
                        ));
                    } else {
                        item.state = WorkspaceState::Orphaned.as_str().into();
                        item.reconciliation_reason = Some(format!(
                            "allocation from older daemon generation {} has residual state (path_exists={path_exists}, git_registered={registered})",
                            previous_generation
                        ));
                    }
                    changed.push(item.clone());
                }
                WorkspaceState::Active => {
                    item.state = WorkspaceState::Orphaned.as_str().into();
                    item.reconciled_at = Some(reconciled_at);
                    item.reconciliation_reason = Some(format!(
                        "active lease belongs to older daemon generation {}",
                        previous_generation
                    ));
                    changed.push(item.clone());
                }
                _ => {}
            }
        }
        if !changed.is_empty() {
            self.save(&registry)?;
        }
        Ok(changed)
    }

    pub fn list(&self) -> Result<Vec<WorkspaceRecord>, WorkspaceError> {
        Ok(self.load()?.workspaces)
    }
    pub fn get(&self, id: &str) -> Result<WorkspaceRecord, WorkspaceError> {
        self.load()?
            .workspaces
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::Invalid(format!("workspace {id} not found")))
    }

    pub fn retain(
        &self,
        id: &str,
        retained: bool,
        owner_session: Option<&str>,
        owner_flow: Option<&str>,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        let item = registry
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::Invalid(format!("workspace {id} not found")))?;
        validate_ownership(item, owner_session, owner_flow)?;
        if item.is_instance_bound() && item.lifecycle_state() == WorkspaceState::Bound {
            item.retained = retained;
            let result = item.clone();
            self.save(&registry)?;
            return Ok(result);
        }
        match (item.lifecycle_state(), retained) {
            (WorkspaceState::Active, true) => {
                item.retained = true;
                item.state = WorkspaceState::Retained.as_str().into();
                item.lease = None;
            }
            (WorkspaceState::Active, false) | (WorkspaceState::Retained, true) => {}
            (WorkspaceState::Retained, false) => {
                return Err(WorkspaceError::Invalid(format!(
                    "workspace {id} cannot become active without explicit adoption"
                )));
            }
            (state, _) => {
                return Err(WorkspaceError::Invalid(format!(
                    "workspace {id} in state {} cannot change retention",
                    state.as_str()
                )));
            }
        }
        let result = item.clone();
        self.save(&registry)?;
        Ok(result)
    }

    pub fn release(
        &self,
        id: &str,
        owner_session: Option<&str>,
        owner_flow: Option<&str>,
        force: bool,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        let index = registry
            .workspaces
            .iter()
            .position(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::Invalid(format!("workspace {id} not found")))?;
        let item = &registry.workspaces[index];
        validate_ownership(item, owner_session, owner_flow)?;
        if item.is_instance_bound() {
            return Err(WorkspaceError::Invalid(
                "workspace is still referenced by a flow instance".into(),
            ));
        }
        let state = item.lifecycle_state();
        if !matches!(state, WorkspaceState::Active | WorkspaceState::Retained) {
            return Err(WorkspaceError::Invalid(format!(
                "workspace {id} in state {} cannot be released",
                state.as_str()
            )));
        }
        if !force && crate::git::has_changes(&item.worktree_path)? {
            return Err(WorkspaceError::Invalid(
                "dirty workspace requires force=true".into(),
            ));
        }
        GitCli::at(&self.repository_root).worktree_remove(&item.worktree_path, force)?;
        let released = &mut registry.workspaces[index];
        released.state = WorkspaceState::Released.as_str().into();
        released.retained = false;
        released.lease = None;
        released.reconciled_at = Some(chrono::Utc::now());
        released.reconciliation_reason = Some("released by explicit workspace action".into());
        let result = released.clone();
        self.save(&registry)?;
        Ok(result)
    }

    pub fn prune(&self, dry_run: bool) -> Result<Vec<WorkspaceRecord>, WorkspaceError> {
        let _lock = self.lock_registry()?;
        let mut registry = self.load()?;
        let worktrees = GitCli::at(&self.repository_root).worktree_list()?;
        let mut candidates = Vec::new();
        for item in &registry.workspaces {
            if !item.retained
                && !item.is_instance_bound()
                && item.lifecycle_state() == WorkspaceState::Orphaned
            {
                candidates.push(item.clone());
            }
        }
        if dry_run {
            return Ok(candidates);
        }

        for item in &candidates {
            let path_exists = item.worktree_path.exists();
            let registered = worktrees
                .iter()
                .any(|worktree| canonicalize(&worktree.path) == canonicalize(&item.worktree_path));
            if path_exists != registered {
                let surviving_side = if path_exists {
                    "filesystem path"
                } else {
                    "Git registration"
                };
                return Err(WorkspaceError::Invalid(format!(
                    "workspace {} has a surviving {surviving_side}; refusing to prune one-sided state",
                    item.id
                )));
            }
        }

        for item in &candidates {
            if item.worktree_path.exists() {
                GitCli::at(&self.repository_root).worktree_remove(&item.worktree_path, false)?;
            }
            let still_registered = GitCli::at(&self.repository_root)
                .worktree_list()?
                .iter()
                .any(|worktree| canonicalize(&worktree.path) == canonicalize(&item.worktree_path));
            if item.worktree_path.exists() || still_registered {
                return Err(WorkspaceError::Invalid(format!(
                    "workspace {} was not fully removed; preserving its lifecycle state",
                    item.id
                )));
            }
            if let Some(found) = registry.workspaces.iter_mut().find(|w| w.id == item.id) {
                found.state = WorkspaceState::Released.as_str().into();
                found.retained = false;
                found.lease = None;
                found.reconciled_at = Some(chrono::Utc::now());
                found.reconciliation_reason = Some("released by explicit orphan prune".into());
            }
        }
        self.save(&registry)?;
        Ok(candidates)
    }

    fn lock_registry(&self) -> Result<fs::File, WorkspaceError> {
        fs::create_dir_all(
            self.registry_lock_path
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        )?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.registry_lock_path)?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    fn load(&self) -> Result<WorkspaceRegistry, WorkspaceError> {
        Ok(self.load_existing()?.unwrap_or_else(|| WorkspaceRegistry {
            version: REGISTRY_VERSION,
            workspaces: Vec::new(),
        }))
    }

    fn load_existing(&self) -> Result<Option<WorkspaceRegistry>, WorkspaceError> {
        let bytes = match fs::read(&self.registry_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut registry: WorkspaceRegistry = serde_json::from_slice(&bytes)?;
        if registry.version != REGISTRY_VERSION {
            return Err(WorkspaceError::Invalid(format!(
                "unsupported registry version {}",
                registry.version
            )));
        }
        for item in &mut registry.workspaces {
            if let Some(creator) = item.instances.first() {
                validate_ownership(
                    item,
                    Some(&creator.owner.session_id.to_string()),
                    Some(&creator.owner.instance_id.to_string()),
                )?;
                let mut owners = std::collections::HashSet::new();
                let invalid_owners = item.instances.iter().any(|binding| {
                    binding.owner.session_id.0.is_nil()
                        || binding.owner.instance_id.0.0.is_nil()
                        || !owners.insert(&binding.owner)
                });
                let terminal = matches!(
                    item.lifecycle_state(),
                    WorkspaceState::Released | WorkspaceState::Dirty | WorkspaceState::Retained
                );
                if invalid_owners
                    || (item.is_instance_bound() == terminal)
                    || item.lifecycle_state() == WorkspaceState::Active
                    || !matches!(item.allocation_policy.as_deref(), Some("auto" | "retain"))
                    || item.allocation_base.is_none()
                    || (item.lifecycle_state() == WorkspaceState::Bound && item.lease.is_some())
                {
                    return Err(WorkspaceError::Invalid(
                        "inconsistent instance workspace ownership".into(),
                    ));
                }
            } else if item.lifecycle_state() == WorkspaceState::Bound {
                return Err(WorkspaceError::Invalid(
                    "bound workspace has no instance owner".into(),
                ));
            }
            if item.retained && item.allocation_policy.is_none() {
                item.state = WorkspaceState::Retained.as_str().into();
            } else if item.lifecycle_state() == WorkspaceState::Retained {
                item.retained = true;
            }
        }
        Ok(Some(registry))
    }

    fn save(&self, registry: &WorkspaceRegistry) -> Result<(), WorkspaceError> {
        let parent = self
            .registry_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".workspaces.json.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let result = (|| {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(registry)?)?;
            file.sync_all()?;
            fs::rename(&tmp, &self.registry_path)?;
            #[cfg(unix)]
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

fn release_instance_reference(record: &mut WorkspaceRecord) {
    for instance in &mut record.instances {
        instance.released = true;
    }
}

fn validate_ownership(
    item: &WorkspaceRecord,
    owner_session: Option<&str>,
    owner_flow: Option<&str>,
) -> Result<(), WorkspaceError> {
    if item.owner_session.as_deref() != owner_session || item.owner_flow.as_deref() != owner_flow {
        return Err(WorkspaceError::Invalid(
            "workspace ownership mismatch".into(),
        ));
    }
    Ok(())
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        return Err(WorkspaceError::Git(GitError::ExitNonZero {
            args: args.join(" "),
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
fn canonicalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
fn canonicalize_from(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        canonicalize(path)
    } else {
        canonicalize(&base.join(path))
    }
}
fn ensure_inside(path: &Path, root: &Path) -> Result<(), WorkspaceError> {
    if canonicalize(path).starts_with(canonicalize(root)) {
        Ok(())
    } else {
        Err(WorkspaceError::Invalid(
            "workspace path escapes managed root".into(),
        ))
    }
}
fn validate_id(id: &str) -> Result<(), WorkspaceError> {
    if id.is_empty() || id == "." || id == ".." || id.contains('/') || id.contains('\\') {
        Err(WorkspaceError::Invalid(
            "workspace id must be a single safe path component".into(),
        ))
    } else {
        Ok(())
    }
}
fn add_exclude(path: &Path, entry: &str) -> Result<(), WorkspaceError> {
    let old = fs::read_to_string(path).unwrap_or_default();
    if !old.lines().any(|line| line.trim() == entry) {
        let mut next = old;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(entry);
        next.push('\n');
        fs::write(path, next)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn instance_owner() -> crate::flow_store::FlowRef {
        crate::flow_store::FlowRef {
            session_id: uuid::Uuid::new_v4().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        }
    }

    #[test]
    fn inspection_preserves_shared_ownership_and_missing_checkout_history() {
        fn files(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
            fn walk(
                root: &Path,
                path: &Path,
                out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
            ) {
                for entry in fs::read_dir(path).unwrap() {
                    let entry = entry.unwrap();
                    let path = entry.path();
                    if entry.file_type().unwrap().is_dir() {
                        walk(root, &path, out);
                    } else if entry.file_type().unwrap().is_file() {
                        out.insert(
                            path.strip_prefix(root).unwrap().to_owned(),
                            fs::read(&path).unwrap(),
                        );
                    }
                }
            }
            let mut result = Default::default();
            walk(root, root, &mut result);
            result
        }
        let tmp = repo();
        let before = files(tmp.path());
        let empty = WorkspaceManager::inspect(tmp.path(), tmp.path()).unwrap();
        assert!(!empty.registry_present);
        assert!(empty.workspaces.is_empty());
        assert_eq!(files(tmp.path()), before);
        assert!(!tmp.path().join(".atman").exists());
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let creator = instance_owner();
        let peer = instance_owner();
        let base = GitCli::at(tmp.path()).head_oid().unwrap();
        let record = manager
            .create_instance("shared", &creator, "host", false, &base)
            .unwrap();
        let shared = manager
            .share_instance(&record.binding(), &creator, &peer)
            .unwrap();
        let before = files(tmp.path());
        let snapshot = WorkspaceManager::inspect(tmp.path(), tmp.path()).unwrap();
        assert!(snapshot.registry_present);
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].record, shared);
        assert_eq!(
            snapshot.workspaces[0].checkout,
            WorkspaceCheckout::Available {
                head: base.clone(),
                branch: None,
                dirty: false
            }
        );
        assert_eq!(
            files(tmp.path()),
            before,
            "inspection must not write registry, index, refs, or lock files"
        );
        git(&record.worktree_path, &["checkout", "-q", "-b", "topic"]);
        fs::write(record.worktree_path.join("README.md"), "changed\n").unwrap();
        let before = files(tmp.path());
        let snapshot = WorkspaceManager::inspect(tmp.path(), tmp.path()).unwrap();
        assert_eq!(
            snapshot.workspaces[0].record.branch, None,
            "recorded allocation remains distinct from current branch"
        );
        assert_eq!(
            snapshot.workspaces[0].checkout,
            WorkspaceCheckout::Available {
                head: base,
                branch: Some("topic".into()),
                dirty: true
            }
        );
        assert_eq!(files(tmp.path()), before);
        fs::remove_dir_all(&record.worktree_path).unwrap();
        let before = files(tmp.path());
        let snapshot = WorkspaceManager::inspect(tmp.path(), tmp.path()).unwrap();
        assert_eq!(snapshot.workspaces[0].checkout, WorkspaceCheckout::Missing);
        assert_eq!(snapshot.workspaces[0].record, shared);
        assert_eq!(files(tmp.path()), before);
        fs::create_dir(&record.worktree_path).unwrap();
        let before = files(tmp.path());
        let snapshot = WorkspaceManager::inspect(tmp.path(), tmp.path()).unwrap();
        assert!(matches!(
            snapshot.workspaces[0].checkout,
            WorkspaceCheckout::Unavailable { .. }
        ));
        assert_eq!(snapshot.workspaces[0].record, shared);
        assert_eq!(files(tmp.path()), before);
        fs::rename(tmp.path().join(".git"), tmp.path().join("hidden-git")).unwrap();
        let before = files(tmp.path());
        let snapshot = WorkspaceManager::inspect(tmp.path(), tmp.path()).unwrap();
        assert!(matches!(
            snapshot.workspaces[0].checkout,
            WorkspaceCheckout::Unavailable { .. }
        ));
        assert_eq!(snapshot.workspaces[0].record, shared);
        assert_eq!(files(tmp.path()), before);
    }

    #[test]
    fn inspection_validates_scope_and_keeps_external_registry_without_repository() {
        let tmp = repo();
        let external = tempfile::tempdir().unwrap();
        let bare = external.path().canonicalize().unwrap().join("repo.git");
        git(
            tmp.path(),
            &[
                "clone",
                "--bare",
                "-q",
                tmp.path().to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let manager = WorkspaceManager::at(&bare, Some(external.path())).unwrap();
        let record = manager
            .create_instance(
                "external",
                &instance_owner(),
                "host",
                true,
                &GitCli::at(&bare).head_oid().unwrap(),
            )
            .unwrap();
        let snapshot = WorkspaceManager::inspect(&bare, external.path()).unwrap();
        assert!(matches!(
            snapshot.workspaces[0].checkout,
            WorkspaceCheckout::Available { dirty: false, .. }
        ));
        fs::rename(&bare, external.path().join("moved.git")).unwrap();
        let snapshot = WorkspaceManager::inspect(&bare, external.path()).unwrap();
        assert_eq!(snapshot.workspaces[0].record, record);
        assert!(matches!(
            snapshot.workspaces[0].checkout,
            WorkspaceCheckout::Unavailable { .. }
        ));
        assert!(WorkspaceManager::inspect(Path::new("relative"), external.path()).is_err());
        assert!(WorkspaceManager::inspect(&bare.join("../other"), external.path()).is_err());
        assert!(WorkspaceManager::inspect(tmp.path(), external.path()).is_err());
        let original = fs::read(&manager.registry_path).unwrap();
        let mut registry: WorkspaceRegistry = serde_json::from_slice(&original).unwrap();
        registry.workspaces.push(record.clone());
        fs::write(
            &manager.registry_path,
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        assert!(WorkspaceManager::inspect(&bare, external.path()).is_err());
        registry.workspaces.pop();
        registry.workspaces[0].worktree_path = tmp.path().into();
        fs::write(
            &manager.registry_path,
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        assert!(WorkspaceManager::inspect(&bare, external.path()).is_err());
        fs::write(&manager.registry_path, b"corrupt").unwrap();
        assert!(WorkspaceManager::inspect(&bare, external.path()).is_err());
        assert_eq!(fs::read(&manager.registry_path).unwrap(), b"corrupt");
        fs::remove_file(&manager.registry_path).unwrap();
        fs::create_dir(&manager.registry_path).unwrap();
        assert!(
            WorkspaceManager::inspect(&bare, external.path()).is_err(),
            "unreadable registry is not an empty snapshot"
        );
    }

    #[test]
    fn instance_reference_survives_hosts_and_blocks_generic_cleanup() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let owner = instance_owner();
        let base = GitCli::at(tmp.path()).head_oid().unwrap();
        let record = manager
            .create_instance("peer", &owner, "old", false, &base)
            .unwrap();
        assert_eq!(record.lifecycle_state(), WorkspaceState::Bound);
        assert!(record.lease.is_none());
        assert!(manager.reconcile_generation("new").unwrap().is_empty());
        assert_eq!(
            manager
                .create_instance("peer", &owner, "new", false, &base)
                .unwrap(),
            record
        );
        assert_eq!(
            manager
                .validate_instance(&record.binding(), &owner)
                .unwrap(),
            record
        );
        let (session, flow) = (owner.session_id.to_string(), owner.instance_id.to_string());
        assert!(manager.finalize_managed("peer", &session, &flow).is_err());
        assert!(
            manager
                .release("peer", Some(&session), Some(&flow), true)
                .is_err()
        );
        assert!(manager.prune(false).unwrap().is_empty());
        assert!(manager.release_instance("peer", &instance_owner()).is_err());
        let mut forged = record.binding();
        forged.path = tmp.path().into();
        assert!(manager.validate_instance(&forged, &owner).is_err());
        assert_eq!(manager.get("peer").unwrap(), record);

        let reopened = WorkspaceManager::open_existing(tmp.path(), None)
            .unwrap()
            .unwrap();
        assert!(matches!(
            reopened.release_instance("peer", &owner).unwrap(),
            WorkspaceFinalizeOutcome::Released(_)
        ));
        assert!(!record.worktree_path.exists());
        assert!(!reopened.get("peer").unwrap().is_instance_bound());
        assert!(matches!(
            reopened.release_instance("peer", &owner).unwrap(),
            WorkspaceFinalizeOutcome::AlreadyReleased(_)
        ));
        assert!(
            reopened
                .create_instance("peer", &owner, "new", false, &base)
                .is_err()
        );
    }

    #[test]
    fn shared_workspace_outlives_creator_and_releases_only_the_last_reference() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let creator = instance_owner();
        let child = instance_owner();
        let peer = instance_owner();
        let base = GitCli::at(tmp.path()).head_oid().unwrap();
        let original = manager
            .create_instance("shared", &creator, "host", false, &base)
            .unwrap();
        let binding = original.binding();
        assert!(manager.share_instance(&binding, &child, &peer).is_err());
        let shared = manager.share_instance(&binding, &creator, &child).unwrap();
        assert_eq!(
            manager.share_instance(&binding, &creator, &child).unwrap(),
            shared
        );
        assert!(matches!(
            manager.release_instance("shared", &creator).unwrap(),
            WorkspaceFinalizeOutcome::Referenced(_)
        ));
        assert!(binding.path.exists());
        assert!(manager.validate_instance(&binding, &creator).is_err());
        assert!(manager.validate_instance(&binding, &child).is_ok());
        assert!(manager.share_instance(&binding, &creator, &peer).is_err());
        assert!(
            manager
                .create_instance("shared", &creator, "new", false, &base)
                .is_err()
        );
        assert!(manager.reconcile_generation("new").unwrap().is_empty());
        assert!(manager.prune(false).unwrap().is_empty());
        let rebound = WorkspaceManager::open_existing(tmp.path(), None)
            .unwrap()
            .unwrap();
        rebound.share_instance(&binding, &child, &peer).unwrap();
        assert!(matches!(
            rebound.release_instance("shared", &child).unwrap(),
            WorkspaceFinalizeOutcome::Referenced(_)
        ));
        let repeated = rebound.share_instance(&binding, &creator, &child).unwrap();
        assert!(
            repeated
                .instances
                .iter()
                .find(|binding| binding.owner == child)
                .unwrap()
                .released
        );
        assert!(matches!(
            rebound.release_instance("shared", &peer).unwrap(),
            WorkspaceFinalizeOutcome::Released(_)
        ));
        assert!(!binding.path.exists());
        assert!(matches!(
            rebound.release_instance("shared", &creator).unwrap(),
            WorkspaceFinalizeOutcome::AlreadyReleased(_)
        ));
        assert!(
            rebound
                .get("shared")
                .unwrap()
                .instances
                .iter()
                .all(|reference| reference.released)
        );
    }

    #[test]
    fn sharing_and_last_release_are_serialized() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let creator = instance_owner();
        let child = instance_owner();
        let original = manager
            .create_instance(
                "race",
                &creator,
                "host",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        let root = tmp.path().to_path_buf();
        let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
        let sharing = {
            let (ready, root, binding, creator, child) = (
                ready.clone(),
                root.clone(),
                original.binding(),
                creator.clone(),
                child.clone(),
            );
            std::thread::spawn(move || {
                let manager = WorkspaceManager::open_existing(&root, None)
                    .unwrap()
                    .unwrap();
                ready.wait();
                manager.share_instance(&binding, &creator, &child)
            })
        };
        ready.wait();
        manager.release_instance("race", &creator).unwrap();
        let shared = sharing.join().unwrap();
        if shared.is_ok() {
            manager
                .validate_instance(&original.binding(), &child)
                .unwrap();
            assert!(original.worktree_path.exists());
            manager.release_instance("race", &child).unwrap();
        } else {
            assert!(!manager.get("race").unwrap().is_instance_bound());
        }
        assert!(!original.worktree_path.exists());
    }

    #[test]
    fn instance_release_preserves_dirty_and_retained_worktrees() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let base = GitCli::at(tmp.path()).head_oid().unwrap();
        for retained in [false, true] {
            let owner = instance_owner();
            let id = if retained { "retain" } else { "dirty" };
            let record = manager
                .create_instance(id, &owner, "host", false, &base)
                .unwrap();
            let content = record.worktree_path.join("uncommitted.txt");
            fs::write(&content, "preserve this").unwrap();
            let toggled = manager
                .retain(
                    id,
                    retained,
                    Some(&owner.session_id.to_string()),
                    Some(&owner.instance_id.to_string()),
                )
                .unwrap();
            assert!(toggled.is_instance_bound());
            assert_eq!(toggled.lifecycle_state(), WorkspaceState::Bound);
            let released = manager.release_instance(id, &owner).unwrap();
            assert_eq!(manager.release_instance(id, &owner).unwrap(), released);
            assert_eq!(fs::read_to_string(content).unwrap(), "preserve this");
            let stored = manager.get(id).unwrap();
            assert!(!stored.is_instance_bound());
            assert_eq!(
                stored.lifecycle_state(),
                if retained {
                    WorkspaceState::Retained
                } else {
                    WorkspaceState::Dirty
                }
            );
            assert!(
                manager
                    .validate_instance(&record.binding(), &owner)
                    .is_err()
            );
        }
    }

    #[test]
    fn automatic_cleanup_preserves_commits_after_the_allocation_base() {
        for persistent in [false, true] {
            let tmp = repo();
            let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
            let owner = instance_owner();
            let base = GitCli::at(tmp.path()).head_oid().unwrap();
            let session = owner.session_id.to_string();
            let flow = owner.instance_id.to_string();
            let record = if persistent {
                manager.create_instance("committed", &owner, "host", false, &base)
            } else {
                manager.create_managed("committed", &session, &flow, "host", false, &base)
            }
            .unwrap();
            fs::write(record.worktree_path.join("result.txt"), "preserve result").unwrap();
            git(&record.worktree_path, &["add", "result.txt"]);
            git(&record.worktree_path, &["commit", "-q", "-m", "result"]);
            let result_head = GitCli::at(&record.worktree_path).head_oid().unwrap();
            assert_ne!(result_head, base);
            assert!(!crate::git::has_changes(&record.worktree_path).unwrap());
            let release = || {
                if persistent {
                    manager.release_instance("committed", &owner)
                } else {
                    manager.finalize_managed("committed", &session, &flow)
                }
                .unwrap()
            };
            let released = release();
            assert!(matches!(released, WorkspaceFinalizeOutcome::Retained(_)));
            assert_eq!(release(), released);
            let stored = manager.get("committed").unwrap();
            assert!(stored.retained);
            assert!(!stored.is_instance_bound());
            assert_eq!(
                GitCli::at(&record.worktree_path).head_oid().unwrap(),
                result_head
            );
            assert_eq!(
                fs::read_to_string(record.worktree_path.join("result.txt")).unwrap(),
                "preserve result"
            );
            assert!(manager.prune(false).unwrap().is_empty());
        }
    }

    #[test]
    fn instance_allocation_and_release_crashes_preserve_reference_ownership() {
        for (stage, residual) in [
            (AllocationStage::BeforeWorktreeAdd, false),
            (AllocationStage::AfterWorktreeAdd, true),
            (AllocationStage::BeforeFinalSave, true),
        ] {
            let tmp = repo();
            let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
            let owner = instance_owner();
            assert!(
                manager
                    .create_managed_with_hook(
                        "crash",
                        &owner.session_id.to_string(),
                        &owner.instance_id.to_string(),
                        "old",
                        false,
                        &GitCli::at(tmp.path()).head_oid().unwrap(),
                        Some(owner.clone()),
                        |current| {
                            if current == stage {
                                Err(WorkspaceError::Invalid("injected crash".into()))
                            } else {
                                Ok(())
                            }
                        }
                    )
                    .is_err()
            );
            let allocating = manager.get("crash").unwrap();
            assert!(allocating.is_instance_bound());
            manager.reconcile_generation("new").unwrap();
            let recovered = manager.get("crash").unwrap();
            assert_eq!(recovered.is_instance_bound(), residual);
            assert!(manager.prune(false).unwrap().is_empty());
            if residual {
                assert!(recovered.worktree_path.exists());
                assert!(
                    manager
                        .retain(
                            "crash",
                            true,
                            Some(&owner.session_id.to_string()),
                            Some(&owner.instance_id.to_string())
                        )
                        .is_err()
                );
            }
            manager.release_instance("crash", &owner).unwrap();
            assert!(!manager.get("crash").unwrap().is_instance_bound());
            assert!(!recovered.worktree_path.exists());
        }

        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let owner = instance_owner();
        let record = manager
            .create_instance(
                "closing",
                &owner,
                "host",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        let mut registry = manager.load().unwrap();
        registry.workspaces[0].state = WorkspaceState::TerminalPending.as_str().into();
        manager.save(&registry).unwrap();
        GitCli::at(tmp.path())
            .worktree_remove(&record.worktree_path, false)
            .unwrap();
        assert!(matches!(
            manager.release_instance("closing", &owner).unwrap(),
            WorkspaceFinalizeOutcome::Released(_)
        ));
        assert!(!manager.get("closing").unwrap().is_instance_bound());
    }

    #[test]
    fn missing_or_replaced_instance_workspace_remains_readable_without_recreation() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let owner = instance_owner();
        let base = GitCli::at(tmp.path()).head_oid().unwrap();
        let record = manager
            .create_instance("missing", &owner, "host", false, &base)
            .unwrap();
        fs::remove_dir_all(&record.worktree_path).unwrap();
        assert!(
            manager
                .validate_instance(&record.binding(), &owner)
                .is_err()
        );
        assert!(
            manager
                .create_instance("missing", &owner, "next", false, &base)
                .is_err()
        );
        assert_eq!(manager.list().unwrap(), vec![record.clone()]);
        assert!(!record.worktree_path.exists());
        assert!(manager.release_instance("missing", &owner).is_err());
        fs::create_dir_all(&record.worktree_path).unwrap();
        git(&record.worktree_path, &["init", "-q"]);
        assert!(
            manager
                .validate_instance(&record.binding(), &owner)
                .is_err()
        );
        assert!(manager.release_instance("missing", &owner).is_err());
        assert_eq!(manager.get("missing").unwrap(), record);
        assert!(record.worktree_path.join(".git").exists());
    }

    #[test]
    fn parses_workspace_policies_and_rejects_unknown_values() {
        assert_eq!(
            "none".parse::<WorkspacePolicy>().unwrap(),
            WorkspacePolicy::None
        );
        assert_eq!(
            "auto".parse::<WorkspacePolicy>().unwrap(),
            WorkspacePolicy::Auto
        );
        assert_eq!(
            "retain".parse::<WorkspacePolicy>().unwrap(),
            WorkspacePolicy::Retain
        );
        assert!("always".parse::<WorkspacePolicy>().is_err());
    }

    #[test]
    fn reads_v1_records_without_lease_and_normalizes_retention() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let old = serde_json::json!({
            "version": 1,
            "workspaces": [{
                "id": "old",
                "repository_root": tmp.path(),
                "worktree_path": tmp.path().join("old"),
                "branch": null,
                "owner_session": "session",
                "owner_flow": "flow",
                "state": "active",
                "retained": true
            }]
        });
        fs::write(
            &manager.registry_path,
            serde_json::to_vec_pretty(&old).unwrap(),
        )
        .unwrap();

        let record = manager.get("old").unwrap();
        assert_eq!(record.lifecycle_state(), WorkspaceState::Retained);
        assert!(record.retained);
        assert_eq!(record.lease, None);
    }

    #[test]
    fn restart_reconciliation_only_orphans_active_older_generation_leases() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let stale = manager
            .create_managed(
                "stale",
                "session",
                "stale",
                "old-generation",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        manager
            .create_managed(
                "current",
                "session",
                "current",
                "new-generation",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        manager
            .create_managed(
                "retained",
                "session",
                "retained",
                "old-generation",
                true,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        assert!(matches!(
            manager
                .finalize_managed("retained", "session", "retained")
                .unwrap(),
            WorkspaceFinalizeOutcome::Retained(_)
        ));
        manager
            .create_managed(
                "dirty",
                "session",
                "dirty",
                "old-generation",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        let mut registry = manager.load().unwrap();
        registry
            .workspaces
            .iter_mut()
            .find(|item| item.id == "dirty")
            .unwrap()
            .state = WorkspaceState::Dirty.as_str().into();
        manager.save(&registry).unwrap();

        let changed = manager.reconcile_generation("new-generation").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].id, "stale");
        assert!(stale.worktree_path.exists());
        let orphaned = manager.get("stale").unwrap();
        assert_eq!(orphaned.lifecycle_state(), WorkspaceState::Orphaned);
        assert_eq!(
            orphaned
                .lease
                .as_ref()
                .map(|lease| lease.daemon_generation.as_str()),
            Some("old-generation")
        );
        assert!(orphaned.reconciled_at.is_some());
        assert!(orphaned.reconciliation_reason.is_some());
        assert_eq!(
            manager.get("current").unwrap().lifecycle_state(),
            WorkspaceState::Active
        );
        assert_eq!(
            manager.get("retained").unwrap().lifecycle_state(),
            WorkspaceState::Retained
        );
        assert_eq!(
            manager.get("dirty").unwrap().lifecycle_state(),
            WorkspaceState::Dirty
        );

        assert!(
            manager
                .reconcile_generation("new-generation")
                .unwrap()
                .is_empty()
        );
        assert_eq!(manager.get("stale").unwrap(), orphaned);
    }

    #[test]
    fn managed_create_is_idempotent_and_retain_starts_active() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let first = manager
            .create_managed(
                "managed",
                "session",
                "flow",
                "generation",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        let count = GitCli::at(tmp.path()).worktree_list().unwrap().len();
        let second = manager
            .create_managed(
                "managed",
                "session",
                "flow",
                "generation",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        assert_eq!(second, first);
        assert_eq!(GitCli::at(tmp.path()).worktree_list().unwrap().len(), count);
        assert!(
            manager
                .create_managed(
                    "managed",
                    "session",
                    "flow",
                    "other-generation",
                    false,
                    &GitCli::at(tmp.path()).head_oid().unwrap()
                )
                .is_err()
        );
        assert_eq!(GitCli::at(tmp.path()).worktree_list().unwrap().len(), count);

        let retained = manager
            .create_managed(
                "retained-active",
                "session",
                "retained",
                "generation",
                true,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();
        assert!(retained.retained);
        assert_eq!(retained.lifecycle_state(), WorkspaceState::Active);
        assert_eq!(retained.allocation_policy.as_deref(), Some("retain"));
    }

    #[test]
    fn managed_allocation_failure_preserves_residual_directory() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let path = manager.managed_root().join("failed");
        fs::create_dir_all(&path).unwrap();
        let sentinel = path.join("sentinel.txt");
        fs::write(&sentinel, "must survive\n").unwrap();

        assert!(
            manager
                .create_managed(
                    "failed",
                    "session",
                    "flow",
                    "generation",
                    false,
                    &GitCli::at(tmp.path()).head_oid().unwrap()
                )
                .is_err()
        );
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "must survive\n");
        let record = manager.get("failed").unwrap();
        assert_eq!(record.lifecycle_state(), WorkspaceState::Orphaned);
        assert!(record.reconciliation_reason.is_some());
    }

    #[test]
    fn managed_allocation_crash_windows_reconcile_without_deleting_residuals() {
        for (stage, expect_residual) in [
            (AllocationStage::BeforeWorktreeAdd, false),
            (AllocationStage::AfterWorktreeAdd, true),
            (AllocationStage::BeforeFinalSave, true),
        ] {
            let tmp = repo();
            let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
            let result = manager.create_managed_with_hook(
                "crash",
                "session",
                "flow",
                "old-generation",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
                None,
                |current| {
                    if current == stage {
                        Err(WorkspaceError::Invalid("injected crash".into()))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(result.is_err());
            let allocating = manager.get("crash").unwrap();
            assert_eq!(allocating.lifecycle_state(), WorkspaceState::Allocating);
            assert_eq!(allocating.worktree_path.exists(), expect_residual);

            let changed = manager.reconcile_generation("new-generation").unwrap();
            assert_eq!(changed.len(), 1);
            let recovered = manager.get("crash").unwrap();
            if expect_residual {
                assert_eq!(recovered.lifecycle_state(), WorkspaceState::Orphaned);
                assert!(recovered.worktree_path.exists());
                assert!(
                    GitCli::at(tmp.path())
                        .worktree_list()
                        .unwrap()
                        .iter()
                        .any(|worktree| canonicalize(&worktree.path)
                            == canonicalize(&recovered.worktree_path))
                );
            } else {
                assert_eq!(recovered.lifecycle_state(), WorkspaceState::Released);
                assert!(!recovered.worktree_path.exists());
            }
        }
    }

    #[test]
    fn concurrent_registry_mutations_do_not_lose_records() {
        let tmp = repo();
        let root = tmp.path().to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let mut threads = Vec::new();
        for index in 0..8 {
            let root = root.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let manager = WorkspaceManager::at(&root, None).unwrap();
                barrier.wait();
                manager
                    .create(
                        &format!("concurrent-{index}"),
                        None,
                        None,
                        false,
                        None,
                        None,
                    )
                    .unwrap();
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let records = manager.list().unwrap();
        assert_eq!(records.len(), 8);
        for index in 0..8 {
            assert!(
                records
                    .iter()
                    .any(|item| item.id == format!("concurrent-{index}"))
            );
        }
    }

    #[test]
    fn managed_finalize_recovers_after_worktree_removal_before_released_save() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let record = manager
            .create_managed(
                "recover",
                "session",
                "flow",
                "generation",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();

        let mut registry = manager.load().unwrap();
        let persisted = registry
            .workspaces
            .iter_mut()
            .find(|item| item.id == record.id)
            .unwrap();
        persisted.state = WorkspaceState::TerminalPending.as_str().into();
        manager.save(&registry).unwrap();
        GitCli::at(tmp.path())
            .worktree_remove(&record.worktree_path, false)
            .unwrap();

        let outcome = manager
            .finalize_managed("recover", "session", "flow")
            .unwrap();
        let WorkspaceFinalizeOutcome::Released(released) = outcome else {
            panic!("expected recovered release, got {outcome:?}");
        };
        assert_eq!(released.lifecycle_state(), WorkspaceState::Released);
        assert_eq!(manager.get("recover").unwrap(), released);
        assert!(matches!(
            manager
                .finalize_managed("recover", "session", "flow")
                .unwrap(),
            WorkspaceFinalizeOutcome::AlreadyReleased(_)
        ));
    }

    #[test]
    fn managed_finalize_preserves_unregistered_workspace_directory() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let record = manager
            .create_managed(
                "residual",
                "session",
                "flow",
                "generation",
                false,
                &GitCli::at(tmp.path()).head_oid().unwrap(),
            )
            .unwrap();

        let mut registry = manager.load().unwrap();
        let persisted = registry
            .workspaces
            .iter_mut()
            .find(|item| item.id == record.id)
            .unwrap();
        persisted.state = WorkspaceState::TerminalPending.as_str().into();
        manager.save(&registry).unwrap();
        GitCli::at(tmp.path())
            .worktree_remove(&record.worktree_path, false)
            .unwrap();
        fs::create_dir_all(&record.worktree_path).unwrap();
        let residual = record.worktree_path.join("residual.txt");
        fs::write(&residual, "must survive\n").unwrap();

        assert!(
            manager
                .finalize_managed("residual", "session", "flow")
                .is_err()
        );
        assert_eq!(fs::read_to_string(residual).unwrap(), "must survive\n");
        assert_eq!(
            manager.get("residual").unwrap().lifecycle_state(),
            WorkspaceState::TerminalPending
        );
    }

    #[test]
    fn creates_lists_gets_and_reloads_committed_workspace_with_ignore_policy() {
        let tmp = repo();
        let head = git(tmp.path(), &["rev-parse", "HEAD"]);
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let item = manager
            .create("one", None, None, false, Some("session"), Some("flow"))
            .unwrap();

        assert_eq!(
            manager.managed_root(),
            tmp.path().join(".atman/worktrees").canonicalize().unwrap()
        );
        assert_eq!(
            fs::read_to_string(item.worktree_path.join("README.md")).unwrap(),
            "committed\n"
        );
        assert_eq!(git(&item.worktree_path, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read_to_string(manager.managed_root().join(".gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(manager.list().unwrap(), vec![item.clone()]);
        assert_eq!(manager.get("one").unwrap(), item);

        let reloaded = WorkspaceManager::at(&item.worktree_path, None).unwrap();
        assert_eq!(reloaded.repository_root(), manager.repository_root());
        assert_eq!(reloaded.managed_root(), manager.managed_root());
        assert_eq!(reloaded.get("one").unwrap(), item);

        WorkspaceManager::at(tmp.path(), None).unwrap();
        let exclude = fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert_eq!(
            exclude
                .lines()
                .filter(|line| line.trim() == "/.atman/worktrees/")
                .count(),
            1
        );
    }

    #[test]
    fn normalizes_full_and_short_branch_names() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let short = manager
            .create("short", Some("probe"), None, true, None, None)
            .unwrap();
        assert_eq!(short.branch.as_deref(), Some("refs/heads/probe"));
        manager.release("short", None, None, false).unwrap();

        let full = manager
            .create("full", Some("refs/heads/probe"), None, false, None, None)
            .unwrap();
        assert_eq!(full.branch.as_deref(), Some("refs/heads/probe"));
    }

    #[test]
    fn retain_and_release_validate_session_and_flow_owners() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        manager
            .create("owned", None, None, false, Some("session"), Some("flow"))
            .unwrap();

        assert!(
            manager
                .retain("owned", true, Some("other"), Some("flow"))
                .is_err()
        );
        assert!(
            manager
                .retain("owned", true, Some("session"), Some("other"))
                .is_err()
        );
        manager
            .retain("owned", true, Some("session"), Some("flow"))
            .unwrap();
        assert!(
            manager
                .release("owned", Some("session"), Some("other"), false)
                .is_err()
        );
        manager
            .release("owned", Some("session"), Some("flow"), false)
            .unwrap();
    }

    #[test]
    fn conservative_states_reject_retain_and_release_without_mutation() {
        for (index, state) in [
            "unknown-legacy",
            "allocating",
            "orphaned",
            "dirty",
            "terminal_pending",
            "released",
        ]
        .into_iter()
        .enumerate()
        {
            let tmp = repo();
            let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
            let id = format!("state-{index}");
            manager
                .create(&id, None, None, false, Some("session"), Some("flow"))
                .unwrap();
            let mut registry = manager.load().unwrap();
            let item = registry
                .workspaces
                .iter_mut()
                .find(|item| item.id == id)
                .unwrap();
            item.state = state.into();
            item.retained = false;
            item.lease = Some(WorkspaceLease {
                daemon_generation: "old-generation".into(),
                acquired_at: chrono::Utc::now(),
            });
            let expected = item.clone();
            manager.save(&registry).unwrap();

            assert!(
                manager
                    .retain(&id, false, Some("session"), Some("flow"))
                    .is_err(),
                "retain(false) accepted {state}"
            );
            assert_eq!(manager.get(&id).unwrap(), expected);
            assert!(
                manager
                    .release(&id, Some("session"), Some("flow"), true)
                    .is_err(),
                "release accepted {state}"
            );
            assert_eq!(manager.get(&id).unwrap(), expected);
            assert!(expected.worktree_path.exists());
        }
    }

    #[test]
    fn retain_transitions_never_reactivate_retained_workspaces() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let active = manager
            .create("active-retain", None, None, false, None, None)
            .unwrap();
        let mut registry = manager.load().unwrap();
        registry
            .workspaces
            .iter_mut()
            .find(|item| item.id == active.id)
            .unwrap()
            .lease = Some(WorkspaceLease {
            daemon_generation: "generation".into(),
            acquired_at: chrono::Utc::now(),
        });
        manager.save(&registry).unwrap();

        let retained = manager.retain(&active.id, true, None, None).unwrap();
        assert_eq!(retained.lifecycle_state(), WorkspaceState::Retained);
        assert!(retained.retained);
        assert!(retained.lease.is_none());
        assert!(manager.retain(&active.id, false, None, None).is_err());

        let mut registry = manager.load().unwrap();
        registry
            .workspaces
            .iter_mut()
            .find(|item| item.id == active.id)
            .unwrap()
            .lease = Some(WorkspaceLease {
            daemon_generation: "generation".into(),
            acquired_at: chrono::Utc::now(),
        });
        manager.save(&registry).unwrap();
        let expected = manager.get(&active.id).unwrap();
        assert!(manager.retain(&active.id, false, None, None).is_err());
        assert_eq!(manager.get(&active.id).unwrap(), expected);
        assert!(active.worktree_path.exists());

        let released = manager.release(&active.id, None, None, false).unwrap();
        assert_eq!(released.lifecycle_state(), WorkspaceState::Released);
        assert!(released.lease.is_none());
        assert!(released.reconciliation_reason.is_some());

        let retained = manager
            .create("retained-release", None, None, false, None, None)
            .unwrap();
        manager.retain(&retained.id, true, None, None).unwrap();
        let released = manager.release(&retained.id, None, None, false).unwrap();
        assert_eq!(released.lifecycle_state(), WorkspaceState::Released);
        assert!(!released.retained);
    }

    #[test]
    fn dirty_workspace_requires_force_to_release() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let item = manager
            .create("dirty", None, None, false, None, None)
            .unwrap();
        fs::write(item.worktree_path.join("dirty.txt"), "dirty\n").unwrap();

        assert!(manager.release("dirty", None, None, false).is_err());
        assert!(item.worktree_path.exists());
        let released = manager.release("dirty", None, None, true).unwrap();
        assert_eq!(released.state, "released");
        assert!(!item.worktree_path.exists());
    }

    #[test]
    fn prune_only_releases_explicit_orphans_dry_run_then_real() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let active = manager
            .create("active", None, None, false, Some(""), Some(""))
            .unwrap();
        let missing = manager
            .create("missing", None, None, false, None, None)
            .unwrap();
        let mut registry = manager.load().unwrap();
        registry
            .workspaces
            .iter_mut()
            .find(|item| item.id == missing.id)
            .unwrap()
            .state = WorkspaceState::Orphaned.as_str().into();
        manager.save(&registry).unwrap();
        GitCli::at(tmp.path())
            .worktree_remove(&missing.worktree_path, false)
            .unwrap();

        let dry_run = manager.prune(true).unwrap();
        assert_eq!(
            dry_run
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["missing"]
        );
        assert_eq!(manager.get("missing").unwrap().state, "orphaned");
        assert!(active.worktree_path.exists());

        let pruned = manager.prune(false).unwrap();
        assert_eq!(
            pruned
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["missing"]
        );
        assert_eq!(manager.get("missing").unwrap().state, "released");
        assert_eq!(manager.get("active").unwrap().state, "active");
    }

    #[test]
    fn prune_preserves_unknown_state_when_both_physical_sides_are_missing() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let item = manager
            .create("unknown-missing", None, None, false, None, None)
            .unwrap();
        let mut registry = manager.load().unwrap();
        let record = registry
            .workspaces
            .iter_mut()
            .find(|record| record.id == item.id)
            .unwrap();
        record.state = "unknown-legacy".into();
        record.lease = Some(WorkspaceLease {
            daemon_generation: "old-generation".into(),
            acquired_at: chrono::Utc::now(),
        });
        let expected = record.clone();
        manager.save(&registry).unwrap();
        GitCli::at(tmp.path())
            .worktree_remove(&item.worktree_path, false)
            .unwrap();

        assert!(manager.prune(true).unwrap().is_empty());
        assert!(manager.prune(false).unwrap().is_empty());
        assert_eq!(manager.get(&item.id).unwrap(), expected);
        assert!(!item.worktree_path.exists());
        assert!(
            !GitCli::at(tmp.path())
                .worktree_list()
                .unwrap()
                .iter()
                .any(|worktree| canonicalize(&worktree.path) == canonicalize(&item.worktree_path))
        );
    }

    #[test]
    fn prune_preserves_one_sided_workspace_states() {
        for git_only in [false, true] {
            let tmp = repo();
            let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
            let id = if git_only { "git-only" } else { "path-only" };
            let item = manager.create(id, None, None, false, None, None).unwrap();
            let mut registry = manager.load().unwrap();
            let record = registry
                .workspaces
                .iter_mut()
                .find(|record| record.id == id)
                .unwrap();
            record.state = WorkspaceState::Orphaned.as_str().into();
            record.lease = Some(WorkspaceLease {
                daemon_generation: "old-generation".into(),
                acquired_at: chrono::Utc::now(),
            });
            let expected = record.clone();
            manager.save(&registry).unwrap();

            if git_only {
                fs::remove_dir_all(&item.worktree_path).unwrap();
            } else {
                GitCli::at(tmp.path())
                    .worktree_remove(&item.worktree_path, false)
                    .unwrap();
                fs::create_dir_all(&item.worktree_path).unwrap();
                fs::write(item.worktree_path.join("survivor.txt"), "keep\n").unwrap();
            }

            assert_eq!(manager.prune(true).unwrap(), vec![expected.clone()]);
            assert!(manager.prune(false).is_err());
            assert_eq!(manager.get(id).unwrap(), expected);
            if git_only {
                assert!(
                    GitCli::at(tmp.path())
                        .worktree_list()
                        .unwrap()
                        .iter()
                        .any(|worktree| canonicalize(&worktree.path)
                            == canonicalize(&item.worktree_path))
                );
            } else {
                assert_eq!(
                    fs::read_to_string(item.worktree_path.join("survivor.txt")).unwrap(),
                    "keep\n"
                );
            }
        }
    }

    #[test]
    fn prune_releases_intact_orphan_and_clears_lease() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let item = manager
            .create("orphan", None, None, false, None, None)
            .unwrap();
        let mut registry = manager.load().unwrap();
        let record = registry
            .workspaces
            .iter_mut()
            .find(|record| record.id == item.id)
            .unwrap();
        record.state = WorkspaceState::Orphaned.as_str().into();
        record.lease = Some(WorkspaceLease {
            daemon_generation: "old-generation".into(),
            acquired_at: chrono::Utc::now(),
        });
        manager.save(&registry).unwrap();

        let pruned = manager.prune(false).unwrap();
        assert_eq!(pruned.len(), 1);
        let released = manager.get(&item.id).unwrap();
        assert_eq!(released.lifecycle_state(), WorkspaceState::Released);
        assert!(released.lease.is_none());
        assert!(released.reconciled_at.is_some());
        assert_eq!(
            released.reconciliation_reason.as_deref(),
            Some("released by explicit orphan prune")
        );
        assert!(!item.worktree_path.exists());
        assert!(
            !GitCli::at(tmp.path())
                .worktree_list()
                .unwrap()
                .iter()
                .any(|worktree| canonicalize(&worktree.path) == canonicalize(&item.worktree_path))
        );
    }

    #[test]
    fn bare_repository_requires_and_uses_external_root() {
        let source = repo();
        let bare_parent = tempfile::tempdir().unwrap();
        let bare = bare_parent.path().join("repo.git");
        git(
            source.path(),
            &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
        );
        let external = tempfile::tempdir().unwrap();

        assert!(WorkspaceManager::at(&bare, None).is_err());
        let manager = WorkspaceManager::at(&bare, Some(external.path())).unwrap();
        let item = manager
            .create("bare-workspace", None, None, false, None, None)
            .unwrap();

        assert!(
            item.worktree_path
                .starts_with(external.path().canonicalize().unwrap())
        );
        assert_eq!(
            fs::read_to_string(item.worktree_path.join("README.md")).unwrap(),
            "committed\n"
        );
        let exclude = bare.join("info/exclude");
        if exclude.exists() {
            assert!(
                !fs::read_to_string(exclude)
                    .unwrap()
                    .lines()
                    .any(|line| line.trim() == "/.atman/worktrees/")
            );
        }
    }
}
