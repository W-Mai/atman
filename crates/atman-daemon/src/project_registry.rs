use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use atman_proto::ProjectId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub root: PathBuf,
}

impl ProjectRecord {
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }
}

#[derive(Default)]
struct RegistryInner {
    by_id: HashMap<ProjectId, ProjectRecord>,
    by_root: HashMap<PathBuf, ProjectId>,
}

#[derive(Default)]
pub struct ProjectRegistry {
    inner: Mutex<RegistryInner>,
}

impl ProjectRegistry {
    pub fn resolve(&self, root: &Path) -> Result<ProjectRecord> {
        anyhow::ensure!(root.is_absolute(), "project root must be an absolute path");
        let root = std::fs::canonicalize(root)
            .with_context(|| format!("resolve project root {}", root.display()))?;
        anyhow::ensure!(
            root.is_dir(),
            "project root is not a directory: {}",
            root.display()
        );
        let id = ProjectId(atman_runtime::session_meta::fingerprint_from_root(&root));
        self.insert(ProjectRecord { id, root })
    }

    pub fn observe_persisted(
        &self,
        root: &Path,
        persisted_fingerprint: Option<&str>,
    ) -> Result<ProjectRecord> {
        anyhow::ensure!(root.is_absolute(), "project root must be an absolute path");
        let (root, expected_fingerprint) = if root.exists() {
            let canonical = std::fs::canonicalize(root)
                .with_context(|| format!("resolve persisted project root {}", root.display()))?;
            anyhow::ensure!(
                canonical.is_dir(),
                "persisted project root is not a directory: {}",
                canonical.display()
            );
            let fingerprint = atman_runtime::session_meta::fingerprint_from_root(&canonical);
            (canonical, fingerprint)
        } else {
            (
                root.to_path_buf(),
                atman_runtime::session_meta::fingerprint_from_root(root),
            )
        };
        let fingerprint = persisted_fingerprint.unwrap_or(&expected_fingerprint);
        anyhow::ensure!(
            !fingerprint.is_empty(),
            "project fingerprint must be non-empty"
        );
        if root.exists() {
            anyhow::ensure!(
                fingerprint == expected_fingerprint,
                "persisted project fingerprint does not match root {}",
                root.display()
            );
        }
        self.insert(ProjectRecord {
            id: ProjectId(fingerprint.to_owned()),
            root,
        })
    }

    pub fn get(&self, id: &ProjectId) -> Option<ProjectRecord> {
        self.inner.lock().unwrap().by_id.get(id).cloned()
    }

    pub fn list(&self) -> Vec<ProjectRecord> {
        let mut projects = self
            .inner
            .lock()
            .unwrap()
            .by_id
            .values()
            .cloned()
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.root.cmp(&right.root));
        projects
    }

    fn insert(&self, project: ProjectRecord) -> Result<ProjectRecord> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(existing_id) = inner.by_root.get(&project.root) {
            anyhow::ensure!(
                existing_id == &project.id,
                "project root {} is already registered as {existing_id}",
                project.root.display()
            );
        }
        if let Some(existing) = inner.by_id.get(&project.id) {
            anyhow::ensure!(
                existing.root == project.root,
                "project id {} is already registered for {}",
                project.id,
                existing.root.display()
            );
            return Ok(existing.clone());
        }
        inner
            .by_root
            .insert(project.root.clone(), project.id.clone());
        inner.by_id.insert(project.id.clone(), project.clone());
        Ok(project)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_is_canonical_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir(&project_dir).unwrap();
        let registry = ProjectRegistry::default();

        let first = registry.resolve(&project_dir).unwrap();
        let second = registry.resolve(&project_dir).unwrap();

        assert_eq!(first, second);
        assert_eq!(registry.get(&first.id), Some(first.clone()));
        assert_eq!(registry.list(), vec![first]);
    }

    #[test]
    fn missing_historical_root_keeps_persisted_identity() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("removed-project");
        let registry = ProjectRegistry::default();

        let project = registry
            .observe_persisted(&missing, Some("persisted-id"))
            .unwrap();

        assert_eq!(project.id, ProjectId("persisted-id".into()));
        assert_eq!(project.root, missing);
    }

    #[test]
    fn observed_identity_conflicts_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let registry = ProjectRegistry::default();

        registry
            .observe_persisted(&first, Some("shared-id"))
            .unwrap();
        let error = registry
            .observe_persisted(&second, Some("shared-id"))
            .unwrap_err();

        assert!(error.to_string().contains("already registered"));
    }

    #[test]
    fn existing_root_rejects_stale_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        let registry = ProjectRegistry::default();
        let error = registry
            .observe_persisted(temp.path(), Some("stale-id"))
            .unwrap_err();

        assert!(error.to_string().contains("does not match root"));
    }
}
