//! Read local directories and prepare the original native session creation request.

use super::*;
use atman_runtime::{project_catalog::ProjectCatalogStore, storage::ProjectContext};
use atman_tui::project_picker::DirectoryListing;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

pub(super) struct Location {
    worker: Option<FlowWorker>,
    fallback: Option<PathBuf>,
}

impl From<&ViewServices> for Location {
    fn from(view: &ViewServices) -> Self {
        Self {
            worker: view.execution.as_ref().map(|view| view.worker.clone()),
            fallback: view
                .historical_directory
                .clone()
                .or_else(|| view.project.clone()),
        }
    }
}

pub(super) async fn browse(
    location: Location,
    startup: Result<PathBuf, String>,
    path: Option<PathBuf>,
    cancellation: CancellationToken,
) -> Result<DirectoryListing> {
    if let Some(path) = path {
        return crate::tui_panels::read_only(move || {
            list(existing_directory(&path)?, &cancellation)
        })
        .await?;
    }
    let current_directory = match location.worker {
        Some(worker) => worker
            .launch_record()
            .await?
            .and_then(|record| record.input.execution)
            .and_then(|entry| entry.authority.workspace_root),
        None => None,
    };
    crate::tui_panels::read_only(move || {
        let directory = [current_directory, location.fallback, startup.ok()]
            .into_iter()
            .flatten()
            .find_map(|candidate| existing_directory(&candidate).ok())
            .context("No initial directory is available; enter a path")?;
        list(directory, &cancellation)
    })
    .await?
}

fn list(directory: PathBuf, cancellation: &CancellationToken) -> Result<DirectoryListing> {
    let mut folders = Vec::new();
    for entry in std::fs::read_dir(&directory)
        .with_context(|| format!("Read directory {}", directory.display()))?
    {
        if cancellation.is_cancelled() {
            bail!("Directory read cancelled");
        }
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() || (kind.is_symlink() && path.is_dir()) {
            folders.push(path);
        }
    }
    folders.sort_unstable();
    Ok(DirectoryListing {
        directory,
        folders,
        roots: roots(),
    })
}

#[cfg(not(windows))]
fn roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}

#[cfg(windows)]
fn roots() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    // The API returns a bit mask of mounted drive letters without probing their contents.
    let drives = unsafe { GetLogicalDrives() };
    (b'A'..=b'Z')
        .filter(|&drive| drives & (1 << (drive - b'A')) != 0)
        .map(|drive| PathBuf::from(format!("{}:\\", char::from(drive))))
        .collect()
}

fn existing_directory(path: &Path) -> Result<PathBuf> {
    let path = if let Ok(suffix) = path.strip_prefix("~") {
        atman_runtime::platform::user_home_dir()
            .context("Home directory is unavailable")?
            .join(suffix)
    } else {
        path.to_owned()
    };
    ensure!(path.is_absolute(), "Enter an absolute directory path");
    let directory = path
        .canonicalize()
        .with_context(|| format!("Open directory {}", path.display()))?;
    ensure!(
        directory.is_dir(),
        "Not a directory: {}",
        directory.display()
    );
    Ok(directory)
}

pub(super) fn prepare(data: &Path, path: &Path, config: ConfigHub) -> Result<scopes::SessionSeed> {
    let directory = existing_directory(path)?;
    let project = ProjectContext::resolve(&config, &directory, data)?;
    Ok(scopes::SessionSeed {
        config,
        project: Some(project),
        directory: Some(directory),
        prompt_resolver: None,
    })
}

pub(super) fn pin(data: &Path, expected: &Path) -> Result<()> {
    let observed = existing_directory(expected)?;
    ensure!(observed == expected, "Directory changed before opening");
    ProjectCatalogStore::new(data).register_pinned(expected, chrono::Utc::now())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_project_survives_its_unused_startup_session() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let data = root.join("data");
        let config = ConfigHub::from_config_dir(root.join("config"));
        let repo = root.join("repository");
        let workspace = repo.join("selected");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(workspace.join(".atman/commands")).unwrap();
        std::fs::write(workspace.join("not-a-folder"), "ignore").unwrap();
        std::fs::write(
            workspace.join(".atman/config.toml"),
            "[storage]\nscope='local'\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join(".atman/commands/agent.at"),
            "flow agent(input: string) -> string { contract { invocation { user_message: input } } return input }\n",
        )
        .unwrap();
        let listing = list(repo.clone(), &CancellationToken::new()).unwrap();
        assert!(listing.folders.contains(&workspace));
        assert!(!data.exists());
        assert!(prepare(&data, &workspace.join("not-a-folder"), config.clone()).is_err());
        let seed = prepare(&data, &workspace, config).unwrap();
        let project = seed.project.as_ref().unwrap();
        assert_eq!(project.root(), workspace);
        assert_eq!(project.scope(), atman_runtime::storage::StorageScope::Local);
        pin(&data, &workspace).unwrap();
        pin(&data, &workspace).unwrap();

        let host = RuntimeHost::default();
        let (notices, _received) = mpsc::unbounded_channel();
        let scopes = Arc::new(scopes::Scopes::empty(
            &data,
            host.clone(),
            None,
            Arc::new(prompts::Prompts::new(notices)),
        ));
        let request = Uuid::now_v7();
        let flow = scopes
            .create_session(seed, request, scopes::CreationMode::Deferred)
            .await
            .unwrap();
        let launch = host
            .flow(&flow)
            .await
            .unwrap()
            .launch_record()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            launch
                .input
                .execution
                .unwrap()
                .authority
                .workspace_root
                .as_deref(),
            Some(workspace.as_path())
        );
        scopes.track_startup_session(request).await.unwrap();
        assert_eq!(scopes.discard_startup_sessions(true).await, vec![request]);
        assert!(RuntimeStore::session_ids(&data).unwrap().is_empty());
        let records = ProjectCatalogStore::new(&data).load().unwrap().projects;
        assert_eq!(records.len(), 1);
        assert!(records[0].pinned);
        assert_eq!(records[0].root, workspace);
        host.shutdown().await.unwrap();
    }
}
