use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::git::{GitCli, GitError};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceRegistry {
    version: u32,
    workspaces: Vec<WorkspaceRecord>,
}

pub struct WorkspaceManager {
    repository_root: PathBuf,
    managed_root: PathBuf,
    registry_path: PathBuf,
}

impl WorkspaceManager {
    pub fn at(cwd: &Path, external_root: Option<&Path>) -> Result<Self, WorkspaceError> {
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
        let managed_root = storage_root.join(".atman").join("worktrees");
        fs::create_dir_all(&managed_root)?;
        let ignore = managed_root.join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, "*\n")?;
        }
        if !is_bare {
            let exclude = common.join("info").join("exclude");
            fs::create_dir_all(exclude.parent().expect("exclude parent"))?;
            add_exclude(&exclude, "/.atman/worktrees/")?;
        }
        let registry_path = storage_root.join(".atman").join("workspaces.json");
        Ok(Self {
            repository_root,
            managed_root,
            registry_path,
        })
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
            state: "active".into(),
            retained: false,
        };
        registry.workspaces.push(item.clone());
        self.save(&registry)?;
        Ok(item)
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
        let mut registry = self.load()?;
        let item = registry
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::Invalid(format!("workspace {id} not found")))?;
        validate_ownership(item, owner_session, owner_flow)?;
        item.retained = retained;
        item.state = if retained { "retained" } else { "active" }.into();
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
        let mut registry = self.load()?;
        let index = registry
            .workspaces
            .iter()
            .position(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::Invalid(format!("workspace {id} not found")))?;
        let item = &registry.workspaces[index];
        validate_ownership(item, owner_session, owner_flow)?;
        if item.state == "released" {
            return Ok(item.clone());
        }
        if !force && crate::git::has_changes(&item.worktree_path)? {
            return Err(WorkspaceError::Invalid(
                "dirty workspace requires force=true".into(),
            ));
        }
        GitCli::at(&self.repository_root).worktree_remove(&item.worktree_path, force)?;
        registry.workspaces[index].state = "released".into();
        let result = registry.workspaces[index].clone();
        self.save(&registry)?;
        Ok(result)
    }

    pub fn prune(&self, dry_run: bool) -> Result<Vec<WorkspaceRecord>, WorkspaceError> {
        let mut registry = self.load()?;
        let worktrees = GitCli::at(&self.repository_root).worktree_list()?;
        let mut candidates = Vec::new();
        for item in &registry.workspaces {
            if item.retained || item.state == "released" {
                continue;
            }
            let registered = worktrees
                .iter()
                .any(|worktree| canonicalize(&worktree.path) == canonicalize(&item.worktree_path));
            if !item.worktree_path.exists() || !registered || item.state == "orphaned" {
                candidates.push(item.clone());
            }
        }
        if !dry_run {
            for item in &candidates {
                if item.worktree_path.exists() {
                    GitCli::at(&self.repository_root)
                        .worktree_remove(&item.worktree_path, false)?;
                }
                if let Some(found) = registry.workspaces.iter_mut().find(|w| w.id == item.id) {
                    found.state = "released".into();
                }
            }
            self.save(&registry)?;
        }
        Ok(candidates)
    }

    fn load(&self) -> Result<WorkspaceRegistry, WorkspaceError> {
        if !self.registry_path.exists() {
            return Ok(WorkspaceRegistry {
                version: REGISTRY_VERSION,
                workspaces: Vec::new(),
            });
        }
        let registry: WorkspaceRegistry = serde_json::from_slice(&fs::read(&self.registry_path)?)?;
        if registry.version != REGISTRY_VERSION {
            return Err(WorkspaceError::Invalid(format!(
                "unsupported registry version {}",
                registry.version
            )));
        }
        Ok(registry)
    }
    fn save(&self, registry: &WorkspaceRegistry) -> Result<(), WorkspaceError> {
        let tmp = self.registry_path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(registry)?)?;
        fs::rename(tmp, &self.registry_path)?;
        Ok(())
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
        fs::write(tmp.path().join("README.md"), "committed\n").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "initial"]);
        tmp
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
    fn prune_ignores_empty_owners_and_prunes_missing_records_dry_run_then_real() {
        let tmp = repo();
        let manager = WorkspaceManager::at(tmp.path(), None).unwrap();
        let active = manager
            .create("active", None, None, false, Some(""), Some(""))
            .unwrap();
        let missing = manager
            .create("missing", None, None, false, None, None)
            .unwrap();
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
        assert_eq!(manager.get("missing").unwrap().state, "active");
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
