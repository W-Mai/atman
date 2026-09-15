use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::session_meta::{SessionMeta, fingerprint_from_root};

const CATALOG_FILENAME: &str = "project-catalog.json";
const LOCK_FILENAME: &str = ".project-catalog.lock";
const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectRecord {
    pub fingerprint: String,
    pub root: PathBuf,
    pub display_name: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    pub first_seen: DateTime<Utc>,
    pub last_opened: DateTime<Utc>,
}

impl ProjectRecord {
    pub fn path_available(&self) -> bool {
        self.root.is_dir()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectCatalog {
    #[serde(default = "schema_version")]
    pub schema_version: u8,
    #[serde(default)]
    pub projects: Vec<ProjectRecord>,
}

impl Default for ProjectCatalog {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            projects: Vec::new(),
        }
    }
}

pub struct ProjectCatalogStore {
    data_dir: PathBuf,
}

impl ProjectCatalogStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn load(&self) -> Result<ProjectCatalog> {
        load_catalog(&self.catalog_path())
    }

    pub fn register(&self, root: &Path, opened_at: DateTime<Utc>) -> Result<ProjectRecord> {
        let root = canonical_root(root);
        let fingerprint = fingerprint_from_root(&root);
        self.mutate(|catalog| {
            if let Some(project) = catalog
                .projects
                .iter_mut()
                .find(|project| project.fingerprint == fingerprint)
            {
                project.last_opened = project.last_opened.max(opened_at);
                return Ok(project.clone());
            }
            let project = ProjectRecord {
                fingerprint,
                display_name: display_name(&root),
                root,
                pinned: false,
                archived: false,
                first_seen: opened_at,
                last_opened: opened_at,
            };
            catalog.projects.push(project.clone());
            sort_projects(&mut catalog.projects);
            Ok(project)
        })
    }

    pub fn reconcile_sessions(&self) -> Result<ProjectCatalog> {
        let sessions_dir = self.data_dir.join("sessions");
        self.mutate(|catalog| {
            let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
                return Ok(catalog.clone());
            };
            for entry in entries.flatten() {
                let session_dir = entry.path();
                if !session_dir.is_dir() {
                    continue;
                }
                let Some(meta) = SessionMeta::load(&session_dir) else {
                    continue;
                };
                let (Some(root), Some(fingerprint), Some(opened_at)) =
                    (meta.project_root, meta.project_fingerprint, meta.created_at)
                else {
                    continue;
                };
                if let Some(project) = catalog
                    .projects
                    .iter_mut()
                    .find(|project| project.fingerprint == fingerprint)
                {
                    project.first_seen = project.first_seen.min(opened_at);
                    project.last_opened = project.last_opened.max(opened_at);
                    continue;
                }
                let root = canonical_root(&root);
                catalog.projects.push(ProjectRecord {
                    fingerprint,
                    display_name: display_name(&root),
                    root,
                    pinned: false,
                    archived: false,
                    first_seen: opened_at,
                    last_opened: opened_at,
                });
            }
            sort_projects(&mut catalog.projects);
            Ok(catalog.clone())
        })
    }

    pub fn set_pinned(&self, fingerprint: &str, pinned: bool) -> Result<bool> {
        self.set_flag(fingerprint, |project| project.pinned = pinned)
    }

    pub fn set_archived(&self, fingerprint: &str, archived: bool) -> Result<bool> {
        self.set_flag(fingerprint, |project| project.archived = archived)
    }

    pub fn delete_archived(
        &self,
        fingerprint: &str,
        active_project_fingerprint: Option<&str>,
    ) -> Result<ProjectCatalog> {
        self.mutate(|catalog| {
            let index = catalog
                .projects
                .iter()
                .position(|project| project.fingerprint == fingerprint)
                .with_context(|| format!("project {fingerprint} is not registered"))?;
            let project = catalog.projects[index].clone();
            anyhow::ensure!(project.archived, "archive this project before deleting it");
            anyhow::ensure!(
                fingerprint_from_root(&canonical_root(&project.root)) == project.fingerprint,
                "project identity does not match its catalog root"
            );
            anyhow::ensure!(
                active_project_fingerprint != Some(fingerprint),
                "cannot delete active session project"
            );
            purge_project_data(&self.data_dir, &project)?;
            catalog.projects.remove(index);
            Ok(catalog.clone())
        })
    }

    fn set_flag(&self, fingerprint: &str, update: impl FnOnce(&mut ProjectRecord)) -> Result<bool> {
        self.mutate(|catalog| {
            let Some(project) = catalog
                .projects
                .iter_mut()
                .find(|project| project.fingerprint == fingerprint)
            else {
                return Ok(false);
            };
            update(project);
            sort_projects(&mut catalog.projects);
            Ok(true)
        })
    }

    fn mutate<T>(&self, update: impl FnOnce(&mut ProjectCatalog) -> Result<T>) -> Result<T> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("create {}", self.data_dir.display()))?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.data_dir.join(LOCK_FILENAME))?;
        lock.lock_exclusive()?;
        let mut catalog = load_catalog(&self.catalog_path())?;
        let result = update(&mut catalog)?;
        catalog.schema_version = SCHEMA_VERSION;
        save_catalog(&self.catalog_path(), &catalog)?;
        FileExt::unlock(&lock)?;
        Ok(result)
    }

    fn catalog_path(&self) -> PathBuf {
        self.data_dir.join(CATALOG_FILENAME)
    }
}

fn purge_project_data(data_dir: &Path, project: &ProjectRecord) -> Result<()> {
    purge_project_sessions(data_dir, &project.fingerprint)?;
    remove_preview_registration(data_dir, &project.root)?;
    remove_dir_if_exists(&data_dir.join("projects").join(&project.fingerprint))?;
    purge_local_project_data(&project.root.join(".atman"))?;
    Ok(())
}

fn purge_project_sessions(data_dir: &Path, fingerprint: &str) -> Result<()> {
    let sessions_dir = data_dir.join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context(format!("read {}", sessions_dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read {} entry", sessions_dir.display()))?;
        let path = entry.path();
        if path.is_dir()
            && SessionMeta::load(&path)
                .and_then(|meta| meta.project_fingerprint)
                .as_deref()
                == Some(fingerprint)
        {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("delete project session {}", path.display()))?;
        }
    }
    Ok(())
}

fn purge_local_project_data(atman_dir: &Path) -> Result<()> {
    for name in ["confessions", "specs", "preview"] {
        remove_dir_if_exists(&atman_dir.join(name))?;
    }
    for name in [
        "index.db",
        "index.db-wal",
        "index.db-shm",
        "index.db.recovered",
        "restore-index.sh",
    ] {
        remove_file_if_exists(&atman_dir.join(name))?;
    }
    let entries = match std::fs::read_dir(atman_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context(format!("read {}", atman_dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read {} entry", atman_dir.display()))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".corrupt-backup-"))
        {
            remove_dir_if_exists(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_preview_registration(data_dir: &Path, project_root: &Path) -> Result<()> {
    let path = data_dir.join("preview/projects.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context(format!("read {}", path.display())),
    };
    let mut projects: Vec<serde_json::Value> =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    let before = projects.len();
    projects.retain(|preview| {
        preview
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
            .map(canonical_root)
            .is_none_or(|path| path != project_root)
    });
    if projects.len() != before {
        save_json(&path, &projects)?;
    }
    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context(format!("delete {}", path.display())),
    }
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context(format!("delete {}", path.display())),
    }
}

fn load_catalog(path: &Path) -> Result<ProjectCatalog> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(ProjectCatalog::default());
    };
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn save_catalog(path: &Path, catalog: &ProjectCatalog) -> Result<()> {
    save_json(path, catalog)
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("JSON file has no parent")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(CATALOG_FILENAME);
    let temp_path = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let write_result = (|| -> Result<()> {
        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        serde_json::to_writer_pretty(&mut temp, value)?;
        temp.write_all(b"\n")?;
        temp.sync_all()?;
        std::fs::rename(&temp_path, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result?;
    sync_parent(parent)?;
    Ok(())
}

fn sync_parent(parent: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| {
        if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(root))
                .unwrap_or_else(|_| root.to_path_buf())
        }
    })
}

fn display_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

fn sort_projects(projects: &mut [ProjectRecord]) {
    projects.sort_by(|a, b| {
        a.archived
            .cmp(&b.archived)
            .then_with(|| b.pinned.cmp(&a.pinned))
            .then_with(|| b.last_opened.cmp(&a.last_opened))
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
}

fn schema_version() -> u8 {
    SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).unwrap()
    }

    #[test]
    fn register_persists_zero_session_project_and_flags() {
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(data.path());

        let record = store.register(project.path(), at(10)).unwrap();
        assert!(store.set_pinned(&record.fingerprint, true).unwrap());
        assert!(store.set_archived(&record.fingerprint, true).unwrap());

        let catalog = store.load().unwrap();
        assert_eq!(catalog.projects.len(), 1);
        assert!(catalog.projects[0].pinned);
        assert!(catalog.projects[0].archived);
        assert!(catalog.projects[0].path_available());
    }

    #[test]
    fn reconciliation_imports_sessions_and_preserves_user_flags() {
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let record = store.register(project.path(), at(20)).unwrap();
        store.set_pinned(&record.fingerprint, true).unwrap();

        let session_dir = data.path().join("sessions/session-a");
        std::fs::create_dir_all(&session_dir).unwrap();
        SessionMeta {
            project_root: Some(project.path().to_path_buf()),
            project_fingerprint: Some(record.fingerprint.clone()),
            created_at: Some(at(30)),
            ..Default::default()
        }
        .save(&session_dir)
        .unwrap();

        let catalog = store.reconcile_sessions().unwrap();
        assert_eq!(catalog.projects.len(), 1);
        assert!(catalog.projects[0].pinned);
        assert_eq!(catalog.projects[0].first_seen, at(20));
        assert_eq!(catalog.projects[0].last_opened, at(30));
    }

    #[test]
    fn deleting_project_requires_archive_and_rejects_active_project() {
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let record = store.register(project.path(), at(10)).unwrap();

        let error = store
            .delete_archived(&record.fingerprint, None)
            .unwrap_err();
        assert!(error.to_string().contains("archive this project"));

        store.set_archived(&record.fingerprint, true).unwrap();
        let error = store
            .delete_archived(&record.fingerprint, Some(&record.fingerprint))
            .unwrap_err();
        assert!(error.to_string().contains("active session"));
        assert_eq!(store.load().unwrap().projects.len(), 1);
    }

    #[test]
    fn deleting_project_rejects_a_catalog_identity_mismatch() {
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let record = store.register(project.path(), at(10)).unwrap();
        store.set_archived(&record.fingerprint, true).unwrap();
        let mut catalog = store.load().unwrap();
        catalog.projects[0].fingerprint = "../outside".into();
        save_catalog(&store.catalog_path(), &catalog).unwrap();

        let error = store.delete_archived("../outside", None).unwrap_err();

        assert!(error.to_string().contains("identity"));
        assert!(project.path().exists());
    }

    #[test]
    fn deleting_archived_project_purges_atman_data_and_preserves_source_config() {
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let other_project = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let record = store.register(project.path(), at(10)).unwrap();
        let other = store.register(other_project.path(), at(20)).unwrap();
        store.set_archived(&record.fingerprint, true).unwrap();

        for (session_id, fingerprint, root) in [
            ("delete-me", &record.fingerprint, project.path()),
            ("keep-me", &other.fingerprint, other_project.path()),
        ] {
            let session_dir = data.path().join("sessions").join(session_id);
            std::fs::create_dir_all(&session_dir).unwrap();
            SessionMeta {
                project_root: Some(root.to_path_buf()),
                project_fingerprint: Some(fingerprint.clone()),
                created_at: Some(at(30)),
                ..Default::default()
            }
            .save(&session_dir)
            .unwrap();
        }

        let global_scope = data.path().join("projects").join(&record.fingerprint);
        std::fs::create_dir_all(global_scope.join("specs")).unwrap();
        std::fs::write(global_scope.join("index.db"), "index").unwrap();

        let local_scope = project.path().join(".atman");
        for name in ["confessions", "specs", "preview", ".corrupt-backup-old"] {
            std::fs::create_dir_all(local_scope.join(name)).unwrap();
        }
        std::fs::create_dir_all(local_scope.join("commands")).unwrap();
        std::fs::write(local_scope.join("index.db"), "index").unwrap();
        std::fs::write(
            local_scope.join("config.toml"),
            "[storage]\nscope='local'\n",
        )
        .unwrap();
        std::fs::write(local_scope.join("commands/agent.at"), "flow agent() {}\n").unwrap();

        let preview_dir = data.path().join("preview");
        std::fs::create_dir_all(&preview_dir).unwrap();
        std::fs::write(
            preview_dir.join("projects.json"),
            serde_json::to_vec(&serde_json::json!([
                {"id":"delete","name":"Delete","path":record.root,"scope":global_scope},
                {"id":"keep","name":"Keep","path":other.root,"scope":"/tmp/keep"}
            ]))
            .unwrap(),
        )
        .unwrap();

        let catalog = store.delete_archived(&record.fingerprint, None).unwrap();

        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].fingerprint, other.fingerprint);
        assert!(!data.path().join("sessions/delete-me").exists());
        assert!(data.path().join("sessions/keep-me").exists());
        assert!(
            !data
                .path()
                .join("projects")
                .join(&record.fingerprint)
                .exists()
        );
        assert!(!local_scope.join("confessions").exists());
        assert!(!local_scope.join("specs").exists());
        assert!(!local_scope.join("preview").exists());
        assert!(!local_scope.join(".corrupt-backup-old").exists());
        assert!(!local_scope.join("index.db").exists());
        assert!(local_scope.join("config.toml").exists());
        assert!(local_scope.join("commands/agent.at").exists());
        let previews: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(preview_dir.join("projects.json")).unwrap())
                .unwrap();
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0]["id"], "keep");
    }
}
