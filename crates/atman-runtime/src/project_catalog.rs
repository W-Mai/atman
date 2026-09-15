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
                return project.clone();
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
            project
        })
    }

    pub fn reconcile_sessions(&self) -> Result<ProjectCatalog> {
        let sessions_dir = self.data_dir.join("sessions");
        self.mutate(|catalog| {
            let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
                return catalog.clone();
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
            catalog.projects.sort_by(|a, b| {
                b.pinned
                    .cmp(&a.pinned)
                    .then_with(|| b.last_opened.cmp(&a.last_opened))
                    .then_with(|| a.display_name.cmp(&b.display_name))
            });
            catalog.clone()
        })
    }

    pub fn set_pinned(&self, fingerprint: &str, pinned: bool) -> Result<bool> {
        self.set_flag(fingerprint, |project| project.pinned = pinned)
    }

    pub fn set_archived(&self, fingerprint: &str, archived: bool) -> Result<bool> {
        self.set_flag(fingerprint, |project| project.archived = archived)
    }

    fn set_flag(&self, fingerprint: &str, update: impl FnOnce(&mut ProjectRecord)) -> Result<bool> {
        self.mutate(|catalog| {
            let Some(project) = catalog
                .projects
                .iter_mut()
                .find(|project| project.fingerprint == fingerprint)
            else {
                return false;
            };
            update(project);
            true
        })
    }

    fn mutate<T>(&self, update: impl FnOnce(&mut ProjectCatalog) -> T) -> Result<T> {
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
        let result = update(&mut catalog);
        save_catalog(&self.catalog_path(), &catalog)?;
        FileExt::unlock(&lock)?;
        Ok(result)
    }

    fn catalog_path(&self) -> PathBuf {
        self.data_dir.join(CATALOG_FILENAME)
    }
}

fn load_catalog(path: &Path) -> Result<ProjectCatalog> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(ProjectCatalog::default());
    };
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn save_catalog(path: &Path, catalog: &ProjectCatalog) -> Result<()> {
    let parent = path.parent().context("project catalog has no parent")?;
    let temp_path = parent.join(format!(".{CATALOG_FILENAME}.{}.tmp", uuid::Uuid::new_v4()));
    let write_result = (|| -> Result<()> {
        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        serde_json::to_writer_pretty(&mut temp, catalog)?;
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
}
