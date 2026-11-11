use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::flow_store::{ProjectDeletion, RuntimeStore};
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
            let entries = match std::fs::read_dir(&sessions_dir) {
                Ok(entries) => Some(entries),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("read legacy sessions"),
            };
            for entry in entries.into_iter().flatten() {
                let entry = entry?;
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
                reconcile_project(catalog, &root, fingerprint, opened_at, opened_at);
            }
            for id in RuntimeStore::session_ids(&self.data_dir)? {
                let session = RuntimeStore::inspect_session(&self.data_dir, id)?;
                let Some(project) = &session.projection.project else {
                    continue;
                };
                let meta = session.dir.join("events.jsonl").metadata()?;
                let first_seen = meta.created().or_else(|_| meta.modified())?;
                let activity = session.activity_time().unwrap_or(first_seen);
                reconcile_project(
                    catalog,
                    project.root(),
                    fingerprint_from_root(project.root()),
                    first_seen.into(),
                    activity.into(),
                );
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
        let (catalog, _deletion) = self.mutate(|catalog| {
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
            let runtime = RuntimeStore::open(&self.data_dir)?;
            let deletion = ProjectDeletion::purge_closed_sessions(&runtime, &project.root)?;
            purge_project_data(&self.data_dir, &project)?;
            catalog.projects.remove(index);
            Ok((catalog.clone(), deletion))
        })?;
        Ok(catalog)
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

fn reconcile_project(
    catalog: &mut ProjectCatalog,
    root: &Path,
    fingerprint: String,
    first_seen: DateTime<Utc>,
    last_opened: DateTime<Utc>,
) {
    if let Some(project) = catalog
        .projects
        .iter_mut()
        .find(|p| p.fingerprint == fingerprint)
    {
        project.first_seen = project.first_seen.min(first_seen);
        project.last_opened = project.last_opened.max(last_opened);
    } else {
        let root = canonical_root(root);
        catalog.projects.push(ProjectRecord {
            fingerprint,
            display_name: display_name(&root),
            root,
            pinned: false,
            archived: false,
            first_seen,
            last_opened,
        });
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
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectCatalog::default());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
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
    use crate::flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowStore, FlowTransition,
        SessionDescriptor, SessionStore,
    };
    use chrono::TimeZone;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[test]
    fn catalog_reads_distinguish_missing_data_from_failures_without_creating_files() {
        let dir = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(dir.path());
        assert!(store.load().unwrap().projects.is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let path = store.catalog_path();
        std::fs::create_dir(&path).unwrap();
        assert!(store.load().unwrap_err().to_string().contains("read"));
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "invalid").unwrap();
        assert!(store.load().unwrap_err().to_string().contains("parse"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "invalid");
        assert!(!dir.path().join(LOCK_FILENAME).exists());
    }

    fn persistent_session(data: &Path, project: &Path) -> (SessionStore, FlowStore) {
        let root = RuntimeStore::open(data).unwrap();
        let binding = serde_json::from_value(serde_json::json!({
            "root": project,
            "scope": "global",
            "storage_root": data.join("projects").join(fingerprint_from_root(project))
        }))
        .unwrap();
        let id = Uuid::new_v4();
        let mut session = root
            .create_session(SessionDescriptor {
                id,
                project: Some(binding),
                configuration: Default::default(),
                title: "Example".into(),
            })
            .unwrap();
        let flow = session
            .create_flow(FlowDescriptor {
                address: FlowRef {
                    session_id: id.into(),
                    instance_id: FlowInstanceId::now(),
                },
                creation_request: Uuid::new_v4(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: crate::source_program::RevisionBundle {
                        entry: "entry:main.at".into(),
                        sources: [("entry:main.at".into(), "flow main() { return 1 }".into())]
                            .into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        (session, flow)
    }

    fn close(flow: &mut FlowStore) {
        for change in [FlowTransition::CloseRequested, FlowTransition::Closed] {
            flow.transition(flow.state().revision(), change).unwrap();
        }
    }

    #[test]
    fn reconciliation_discovers_persistent_sessions_without_legacy_or_project_directories() {
        let data = TempDir::new().unwrap();
        let project = data.path().join("missing-project");
        let (session, flow) = persistent_session(data.path(), &project);
        let events = session.dir().join("events.jsonl");
        let before = std::fs::read(&events).unwrap();
        let cache = session.dir().join("session.json");
        std::fs::write(&cache, "invalid cache").unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let catalog = store.reconcile_sessions().unwrap();
        assert_eq!(catalog.projects.len(), 1);
        assert!(!catalog.projects[0].path_available());
        assert!(!data.path().join("sessions").exists());
        assert_eq!(catalog.projects[0].root, project);
        store
            .set_archived(&catalog.projects[0].fingerprint, true)
            .unwrap();
        store
            .set_pinned(&catalog.projects[0].fingerprint, true)
            .unwrap();
        let again = store.reconcile_sessions().unwrap();
        assert!(again.projects[0].archived && again.projects[0].pinned);
        assert_eq!(again.projects[0].first_seen, catalog.projects[0].first_seen);
        assert_eq!(std::fs::read(events).unwrap(), before);
        assert_eq!(std::fs::read(cache).unwrap(), b"invalid cache");
        assert_eq!(flow.state().revision(), 0);
    }

    #[test]
    fn moved_sessions_retain_original_project_data_until_their_histories_are_removed() {
        let data = TempDir::new().unwrap();
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let catalog = ProjectCatalogStore::new(data.path());
        let original = catalog.register(first.path(), at(10)).unwrap();
        catalog.set_archived(&original.fingerprint, true).unwrap();
        let shared = data.path().join("projects").join(&original.fingerprint);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("index.db"), b"retained").unwrap();
        let (mut session, mut flow) = persistent_session(data.path(), first.path());
        close(&mut flow);
        let hub = crate::config_hub::ConfigHub::from_config_dir(data.path().join("config"));
        let destination =
            crate::storage::ProjectContext::resolve(&hub, second.path(), data.path()).unwrap();
        let receipt = session.cursor().clone();
        session.set_project(&receipt, Some(destination)).unwrap();
        let path = session.dir().to_owned();
        drop((session, flow));
        assert!(
            catalog
                .delete_archived(&original.fingerprint, None)
                .unwrap_err()
                .to_string()
                .contains("retains flows bound")
        );
        assert!(path.exists());
        assert_eq!(std::fs::read(shared.join("index.db")).unwrap(), b"retained");
        let projects = catalog.reconcile_sessions().unwrap();
        assert!(
            projects
                .projects
                .iter()
                .any(|project| project.root == canonical_root(second.path()))
        );
    }

    #[test]
    fn project_deletion_preflights_every_persistent_owner_and_flow_before_purging() {
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let record = store.register(project.path(), at(10)).unwrap();
        store.set_archived(&record.fingerprint, true).unwrap();
        let shared = data.path().join("projects").join(&record.fingerprint);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("index.db"), "retained").unwrap();
        let (closed_session, mut closed_flow) = persistent_session(data.path(), project.path());
        let closed_path = closed_session.dir().to_owned();
        close(&mut closed_flow);
        drop((closed_session, closed_flow));
        let (session, flow) = persistent_session(data.path(), project.path());
        let id = session.projection().descriptor.id;
        let path = session.dir().to_owned();
        let before = std::fs::read(path.join("events.jsonl")).unwrap();
        drop(session);
        // A surviving flow retains the session owner even after the catalog handle is dropped.
        assert!(
            store
                .delete_archived(&record.fingerprint, None)
                .unwrap_err()
                .to_string()
                .contains("owner")
        );
        drop(flow);
        assert!(
            store
                .delete_archived(&record.fingerprint, None)
                .unwrap_err()
                .to_string()
                .contains("not closed")
        );
        assert_eq!(std::fs::read(shared.join("index.db")).unwrap(), b"retained");
        assert_eq!(std::fs::read(path.join("events.jsonl")).unwrap(), before);
        assert!(closed_path.exists());
        let root = RuntimeStore::open(data.path()).unwrap();
        let session = root.open_session(id).unwrap();
        let mut flow = session
            .open_flow(&session.projection().flows[0].id)
            .unwrap();
        close(&mut flow);
        drop((session, flow));
        let (other, other_flow) = persistent_session(data.path(), &project.path().join("other"));
        let other_path = other.dir().to_owned();
        let catalog = store.delete_archived(&record.fingerprint, None).unwrap();
        assert!(catalog.projects.is_empty());
        assert!(!path.exists() && !closed_path.exists() && !shared.exists());
        assert!(other_path.exists());
        assert_eq!(other_flow.state().revision(), 0);
    }

    #[test]
    fn project_deletion_preserves_management_binding_until_replaced() {
        use crate::flow_store::profile::ProfileStore;
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let record = store.register(project.path(), at(10)).unwrap();
        store.set_archived(&record.fingerprint, true).unwrap();
        let (session, mut flow) = persistent_session(data.path(), project.path());
        close(&mut flow);
        let id = session.projection().descriptor.id;
        let path = session.dir().to_owned();
        drop((session, flow));
        let mut profile = ProfileStore::open(&root).unwrap();
        let original = profile
            .bootstrap(Default::default())
            .unwrap()
            .management
            .unwrap()
            .session
            .id;
        profile
            .select(
                Uuid::new_v4(),
                profile.snapshot().cursor.clone(),
                id,
                vec![],
            )
            .unwrap();
        let error = store
            .delete_archived(&record.fingerprint, None)
            .unwrap_err();
        assert!(error.to_string().contains("management session"));
        assert!(path.exists());
        assert_eq!(
            profile.snapshot().management.as_ref().unwrap().session.id,
            id
        );
        profile
            .select(
                Uuid::new_v4(),
                profile.snapshot().cursor.clone(),
                original,
                vec![],
            )
            .unwrap();
        store.delete_archived(&record.fingerprint, None).unwrap();
        assert!(!path.exists());
        assert!(RuntimeStore::inspect_session(data.path(), original).is_ok());
    }

    #[test]
    fn project_deletion_never_repairs_incomplete_journals() {
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let record = store.register(project.path(), at(10)).unwrap();
        store.set_archived(&record.fingerprint, true).unwrap();
        let (session, mut flow) = persistent_session(data.path(), project.path());
        close(&mut flow);
        let path = flow.dir().unwrap().join("events.jsonl");
        drop((session, flow));
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"incomplete\":")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            store
                .delete_archived(&record.fingerprint, None)
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(store.load().unwrap().projects.len(), 1);
    }

    #[test]
    fn project_deletion_serializes_open_create_project_changes_and_management_selection() {
        use crate::flow_store::profile::ProfileStore;
        use std::{sync::mpsc, time::Duration};
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let (session, mut flow) = persistent_session(data.path(), project.path());
        close(&mut flow);
        let descriptor = session.projection().descriptor.clone();
        let id = descriptor.id;
        drop((session, flow));
        let mut moving_descriptor = descriptor.clone();
        moving_descriptor.id = Uuid::new_v4();
        moving_descriptor.project = None;
        let mut moving = root.create_session(moving_descriptor).unwrap();
        let destination = descriptor.project.clone();
        let mut profile = ProfileStore::open(&root).unwrap();
        profile.bootstrap(Default::default()).unwrap();
        let guard = ProjectDeletion::purge_closed_sessions(&root, project.path()).unwrap();
        let (started, ready) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        std::thread::scope(|scope| {
            for operation in ["open", "create"] {
                let started = started.clone();
                let finished = finished.clone();
                let root = &root;
                let descriptor = descriptor.clone();
                scope.spawn(move || {
                    started.send(()).unwrap();
                    match operation {
                        "open" => assert_eq!(
                            root.open_session(id).err().unwrap().kind(),
                            std::io::ErrorKind::NotFound
                        ),
                        "create" => {
                            let mut descriptor = descriptor;
                            descriptor.id = Uuid::new_v4();
                            root.create_session(descriptor).unwrap();
                        }
                        _ => unreachable!(),
                    }
                    finished.send(()).unwrap();
                });
            }
            let moving_started = started.clone();
            let moving_finished = finished.clone();
            scope.spawn(move || {
                moving_started.send(()).unwrap();
                let expected = moving.cursor().clone();
                moving.set_project(&expected, destination).unwrap();
                moving_finished.send(()).unwrap();
            });
            let started = started.clone();
            let finished = finished.clone();
            scope.spawn(move || {
                started.send(()).unwrap();
                let result = profile.replace(crate::flow_store::profile::ReplaceManagement {
                    request_id: Uuid::new_v4(),
                    expected: profile.snapshot().cursor.clone(),
                    binding: crate::flow_store::profile::ManagementBinding {
                        session: descriptor,
                        grants: vec![],
                    },
                });
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
                finished.send(()).unwrap();
            });
            for _ in 0..4 {
                ready.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            assert!(matches!(
                done.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(guard);
            for _ in 0..4 {
                done.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        });
    }

    #[test]
    fn project_deletion_retries_hidden_partial_removal_without_discovering_it() {
        let data = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let store = ProjectCatalogStore::new(data.path());
        let record = store.register(project.path(), at(10)).unwrap();
        store.set_archived(&record.fingerprint, true).unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let (session, mut flow) = persistent_session(data.path(), project.path());
        close(&mut flow);
        let descriptor = session.projection().descriptor.clone();
        drop((session, flow));
        drop(ProjectDeletion::purge_closed_sessions(&root, project.path()).unwrap());
        let marker = root
            .root()
            .join(".deletions/sessions")
            .join(descriptor.id.to_string());
        let partial = marker.join("data");
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("events.jsonl"), "partial deletion").unwrap();
        assert!(RuntimeStore::session_ids(data.path()).unwrap().is_empty());
        store.delete_archived(&record.fingerprint, None).unwrap();
        assert!(!partial.exists());
        assert!(marker.join("receipt.json").exists());
        assert!(marker.join("deliveries.json").exists());
        assert!(root.create_session(descriptor).is_err());
        assert!(store.reconcile_sessions().unwrap().projects.is_empty());
    }

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
