//! Operator catalog reads; no execution services or workspace mutations are acquired.

use std::path::PathBuf;

use anyhow::Result;
use atman_runtime::{
    flow_runtime::catalog::{SessionCatalog, WorkspaceCatalogEntry},
    git_workspace::{WorkspaceManager, WorkspaceSnapshot},
    project_catalog::ProjectCatalogStore,
};
use atman_tui::flow_hub::{
    WorkspaceCursor, WorkspaceDirectory, WorkspaceDirectoryEntry, WorkspaceFlowReference,
    WorkspaceLocation,
};

pub(super) async fn directory(
    catalog: &SessionCatalog,
    root: PathBuf,
    after: Option<WorkspaceCursor>,
) -> Result<WorkspaceDirectory> {
    let mut directory = WorkspaceDirectory {
        after: after.clone(),
        next: None,
        locations: Vec::new(),
        errors: Vec::new(),
    };
    if after.is_none() {
        match tokio::task::spawn_blocking(move || ProjectCatalogStore::new(root).load()).await? {
            Ok(projects) => {
                for project in projects.projects {
                    add_project(&mut directory, project.root);
                }
            }
            Err(error) => directory.errors.push(format!("Project catalog: {error:#}")),
        }
    }
    match after {
        None | Some(WorkspaceCursor::Sessions { .. }) => {
            let before = match directory.after {
                Some(WorkspaceCursor::Sessions { before }) => Some(before),
                _ => None,
            };
            let page = catalog.page(before, 8, |_| Ok(true)).await?;
            for session in page.sessions {
                if let Some(error) = session.unavailable {
                    directory
                        .errors
                        .push(format!("Session {}: {error}", session.session_id));
                }
                if let Some(root) = session.project_root {
                    add_project(&mut directory, root);
                }
            }
            directory.next = Some(
                page.next
                    .map_or(WorkspaceCursor::Bindings { after: None }, |before| {
                        WorkspaceCursor::Sessions { before }
                    }),
            );
        }
        Some(WorkspaceCursor::Bindings { after }) => {
            let page = catalog
                .workspace_page(after, 8, |_| Ok(true), |_| Ok(()))
                .await?;
            for entry in page.entries {
                match entry {
                    WorkspaceCatalogEntry::SessionUnavailable { session_id, error } => directory
                        .errors
                        .push(format!("Session {session_id}: {error}")),
                    WorkspaceCatalogEntry::Flow {
                        owner, snapshot, ..
                    } => match snapshot {
                        Err(error) => directory.errors.push(format!(
                            "Flow {} / {}: {error}",
                            owner.session_id.0, owner.instance_id.0.0
                        )),
                        Ok(snapshot) => {
                            if let Some(root) = snapshot.project_root {
                                add_project(&mut directory, root);
                            }
                            if let Some(request) = snapshot.workspace {
                                // WorkspaceRequest has already passed the shared replay's path validation.
                                let storage = request
                                    .binding
                                    .path
                                    .ancestors()
                                    .nth(3)
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("Invalid workspace storage location")
                                    })?
                                    .to_owned();
                                directory.insert(WorkspaceDirectoryEntry {
                                    location: WorkspaceLocation {
                                        repository: request.binding.repository_root,
                                        storage,
                                    },
                                    references: vec![WorkspaceFlowReference {
                                        owner,
                                        title: snapshot.title,
                                        workspace_id: request.binding.workspace_id,
                                    }],
                                });
                            }
                        }
                    },
                }
            }
            directory.next = page
                .next
                .map(|after| WorkspaceCursor::Bindings { after: Some(after) });
        }
    }
    Ok(directory)
}

fn add_project(directory: &mut WorkspaceDirectory, root: PathBuf) {
    directory.insert(WorkspaceDirectoryEntry {
        location: WorkspaceLocation {
            repository: root.clone(),
            storage: root,
        },
        references: Vec::new(),
    });
}

pub(super) async fn inspect(location: WorkspaceLocation) -> Result<WorkspaceSnapshot> {
    Ok(tokio::task::spawn_blocking(move || {
        WorkspaceManager::inspect(&location.repository, &location.storage)
    })
    .await??)
}

pub(super) async fn retain(change: atman_tui::flow_hub::WorkspaceRetention) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        WorkspaceManager::retain_observed(
            &change.location.repository,
            &change.location.storage,
            &change.expected,
            change.retained,
        )
    })
    .await??;
    Ok(())
}

pub(super) async fn binding(
    catalog: &SessionCatalog,
    owner: atman_runtime::flow_store::FlowRef,
) -> Result<atman_tui::flow_hub::FlowWorkspacePage> {
    let inspected = catalog.inspect_flow(&owner, |_| Ok(())).await?;
    Ok(atman_tui::flow_hub::FlowWorkspacePage {
        owner,
        request: inspected.workspace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::{
        config_hub::ConfigHub,
        flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore,
            SessionDescriptor,
            workspace::{WorkspaceRequest, WorkspaceSource},
        },
        git_workspace::WorkspaceBinding,
        source_program::RevisionBundle,
        storage::ProjectContext,
    };
    use uuid::Uuid;

    #[tokio::test]
    async fn worktree_sources_include_empty_sessions_and_external_bindings_without_loading_owners()
    {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let config = ConfigHub::from_config_dir(dir.path().join("config"));
        let missing = dir.path().join("missing-project");
        let project = ProjectContext::resolve(&config, &missing, dir.path()).unwrap();
        let external = dir.path().join("external");
        let instance = FlowInstanceId::now();
        let mut sessions = Vec::new();
        let mut flows = Vec::new();
        for index in 0..10 {
            let id = Uuid::new_v4();
            let mut session = root
                .create_session(SessionDescriptor {
                    id,
                    project: Some(project.clone()),
                    configuration: Default::default(),
                    title: format!("Session {index}"),
                })
                .unwrap();
            if index < 2 {
                let mut flow = session
                    .create_flow(FlowDescriptor {
                        address: FlowRef {
                            session_id: id.into(),
                            instance_id: instance.clone(),
                        },
                        creation_request: Uuid::new_v4(),
                        created_by: None,
                        content: EntrySnapshot {
                            name: "main".into(),
                            path: missing.join("main.at"),
                            bundle: RevisionBundle {
                                entry: "entry:main.at".into(),
                                sources: [(
                                    "entry:main.at".into(),
                                    "flow main() -> int { return 1 }".into(),
                                )]
                                .into(),
                                edges: Default::default(),
                            },
                        }
                        .into(),
                    })
                    .unwrap();
                flow.request_workspace(WorkspaceRequest {
                    binding: WorkspaceBinding {
                        workspace_id: "shared".into(),
                        path: external.join(".atman/worktrees/shared"),
                        repository_root: project.root().to_owned(),
                        branch: None,
                    },
                    source: WorkspaceSource::Allocate {
                        base: "a".repeat(40),
                        retained: false,
                    },
                })
                .unwrap();
                flows.push(flow);
            }
            sessions.push(session);
        }
        let before: Vec<_> = flows
            .iter()
            .map(|flow| (flow.dir().join("events.jsonl"), flow.state().clone()))
            .collect();
        let bytes: Vec<_> = before
            .iter()
            .map(|(path, _)| std::fs::read(path).unwrap())
            .collect();
        let catalog = SessionCatalog::new(None, Some(dir.path().to_owned()));
        let mut after = None;
        let mut refs = Vec::new();
        let mut steps = 0;
        loop {
            let page = directory(&catalog, dir.path().to_owned(), after.clone())
                .await
                .unwrap();
            if steps == 0 {
                assert!(
                    page.locations
                        .iter()
                        .any(|entry| entry.location.repository == project.root())
                );
                assert!(matches!(page.next, Some(WorkspaceCursor::Sessions { .. })));
            }
            assert!(page.next.is_none() || page.next != after);
            for entry in page.locations {
                if entry.location.storage == external {
                    refs.extend(
                        entry
                            .references
                            .into_iter()
                            .map(|reference| reference.owner),
                    );
                }
            }
            steps += 1;
            assert!(steps < 10);
            let Some(next) = page.next else {
                break;
            };
            after = Some(next);
        }
        assert_eq!(refs.len(), 2);
        assert_ne!(refs[0], refs[1]);
        assert_eq!(refs[0].instance_id, refs[1].instance_id);
        for owner in &refs {
            let page = binding(&catalog, owner.clone()).await.unwrap();
            assert_eq!(&page.owner, owner);
            assert_eq!(
                page.request.unwrap().binding.path,
                external.join(".atman/worktrees/shared")
            );
        }
        let snapshot = inspect(WorkspaceLocation {
            repository: project.root().to_owned(),
            storage: external.clone(),
        })
        .await
        .unwrap();
        assert!(!snapshot.registry_present);
        assert!(!external.exists() && !missing.exists());
        for ((path, state), bytes) in before.iter().zip(bytes) {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert_eq!(
                &flows
                    .iter()
                    .find(|flow| flow.dir().join("events.jsonl") == *path)
                    .unwrap()
                    .state()
                    .clone(),
                state
            );
        }
        assert!(!dir.path().join("project-catalog.json").exists());
        std::fs::write(dir.path().join("project-catalog.json"), "broken").unwrap();
        let page = directory(&catalog, dir.path().to_owned(), None)
            .await
            .unwrap();
        assert!(!page.errors.is_empty() && !page.locations.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("project-catalog.json")).unwrap(),
            "broken"
        );
    }
}
