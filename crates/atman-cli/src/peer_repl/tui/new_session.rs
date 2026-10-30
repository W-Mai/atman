//! View-bound operator choices, independent of conversation or execution state.

use super::*;
use atman_runtime::storage::ProjectContext;
use atman_tui::new_session::{Location, Locations, Selection};
use std::path::PathBuf;

pub(super) struct Plan {
    pub locations: Locations,
    configurations: Vec<ConfigHub>,
}

impl Plan {
    fn new(
        id: Uuid,
        current_config: ConfigHub,
        project: Option<PathBuf>,
        directory: Option<PathBuf>,
        startup: PathBuf,
        startup_config: ConfigHub,
    ) -> Self {
        let normalize = |path: PathBuf| atman_runtime::fs_access::canonicalize_stable(&path);
        let project = project.map(normalize);
        let directory = directory.map(normalize);
        let startup = normalize(startup);
        let mut projects = vec![Location {
            label: project.as_ref().map_or_else(
                || "Session project · No project".into(),
                |p| label("Session project", p),
            ),
            path: project.clone(),
        }];
        let mut configurations = vec![current_config];
        if project.as_ref() == Some(&startup) {
            projects[0].label = label("Session / startup project", &startup);
        } else {
            projects.push(Location {
                label: label("Startup project", &startup),
                path: Some(startup.clone()),
            });
            configurations.push(startup_config);
        }
        Self {
            locations: Locations {
                id,
                projects,
                current_directory: directory,
                startup_directory: startup,
            },
            configurations,
        }
    }

    pub fn direct(&self) -> Option<Selection> {
        let locations = &self.locations;
        (locations.projects.len() == 1
            && locations.projects[0].path.as_ref() == Some(&locations.startup_directory)
            && locations.current_directory.as_ref() == Some(&locations.startup_directory)
            && locations.startup_directory.is_dir())
        .then(|| Selection {
            project: 0,
            directory: locations.startup_directory.clone(),
        })
    }

    pub fn seed(&self, selection: Selection, root: &Path) -> Result<scopes::SessionSeed> {
        let project = self
            .locations
            .projects
            .get(selection.project)
            .context("Project choice is unavailable")?;
        ensure!(
            self.locations
                .projects
                .iter()
                .enumerate()
                .any(|(project, _)| self
                    .locations
                    .directories(project)
                    .iter()
                    .any(|location| location.path.as_ref() == Some(&selection.directory))),
            "Working directory is not part of this request"
        );
        validate_directory(&selection.directory)?;
        let config = self.configurations[selection.project].clone();
        let project = project
            .path
            .as_ref()
            .map(|path| {
                validate_directory(path)?;
                ProjectContext::resolve(&config, path, root)
            })
            .transpose()?;
        Ok(scopes::SessionSeed {
            config,
            project,
            directory: Some(selection.directory),
            prompt_resolver: None,
        })
    }
}

fn label(source: &str, path: &Path) -> String {
    format!(
        "{source} · {}{}",
        path.display(),
        if path.is_dir() { "" } else { " (unavailable)" }
    )
}

fn validate_directory(path: &Path) -> Result<()> {
    ensure!(
        path.is_dir(),
        "Directory is unavailable: {}",
        path.display()
    );
    ensure!(
        path.canonicalize()? == path,
        "Directory location has changed: {}",
        path.display()
    );
    Ok(())
}

/// Resolve the catalog identity at the host; the UI never supplies an arbitrary path.
pub(super) fn project_seed(
    root: &Path,
    fingerprint: &str,
    config: ConfigHub,
) -> Result<scopes::SessionSeed> {
    let project = atman_runtime::project_catalog::ProjectCatalogStore::new(root)
        .load()?
        .projects
        .into_iter()
        .find(|project| project.fingerprint == fingerprint)
        .context("Project is no longer in the directory")?;
    ensure!(
        !project.archived,
        "Unarchive this project before creating a session"
    );
    validate_directory(&project.root)?;
    ensure!(
        atman_runtime::session_meta::fingerprint_from_root(&project.root) == fingerprint,
        "Project directory identity has changed"
    );
    let context = ProjectContext::resolve(&config, &project.root, root)?;
    Ok(scopes::SessionSeed {
        config,
        project: Some(context),
        directory: Some(project.root),
        prompt_resolver: None,
    })
}

impl scopes::Scopes {
    pub(super) async fn new_session_plan(
        &self,
        view: &ViewServices,
        request: Uuid,
    ) -> Result<Plan> {
        let startup = self
            .startup_directory
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))?
            .clone();
        let startup_config = ConfigHub::global()?;
        let (config, project) = match &view.binding.flow {
            ViewAddress::Instance(flow) => {
                let root = self.root.clone();
                let session = flow.session_id.0;
                let catalog = tokio::task::spawn_blocking(move || {
                    RuntimeStore::inspect_session(&root, session)
                })
                .await??
                .projection;
                let config = match &catalog.descriptor.configuration {
                    SessionConfiguration::Atman(source) => ConfigHub::from_source(source)?,
                    _ => startup_config.clone(),
                };
                (config, catalog.project.map(|p| p.root().to_owned()))
            }
            ViewAddress::Legacy { .. } => (startup_config.clone(), view.project.clone()),
        };
        let directory = if let Some(execution) = &view.execution {
            execution
                .worker
                .launch_record()
                .await?
                .and_then(|record| record.input.execution)
                .and_then(|entry| entry.authority.workspace_root)
                .or_else(|| view.project.clone())
        } else {
            view.historical_directory.clone()
        };
        Ok(Plan::new(
            request,
            config,
            project,
            directory,
            startup,
            startup_config,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_creation_resolves_catalog_identity_and_storage_without_startup_inference() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path().canonicalize().unwrap();
        let config = ConfigHub::from_config_dir(root.join("config"));
        let project = root.join("target-project");
        std::fs::create_dir_all(project.join(".atman")).unwrap();
        std::fs::write(
            project.join(".atman/config.toml"),
            "[storage]\nscope='local'\n",
        )
        .unwrap();
        let store = atman_runtime::project_catalog::ProjectCatalogStore::new(&root);
        let record = store.register(&project, chrono::Utc::now()).unwrap();
        let seed = project_seed(&root, &record.fingerprint, config.clone()).unwrap();
        assert_eq!(seed.directory.as_ref(), Some(&project));
        assert_eq!(seed.project.as_ref().unwrap().root(), project);
        assert_eq!(
            seed.project.as_ref().unwrap().scope(),
            atman_runtime::storage::StorageScope::Local
        );
        assert!(project_seed(&root, "unknown-project", config.clone()).is_err());
        store.set_archived(&record.fingerprint, true).unwrap();
        assert!(project_seed(&root, &record.fingerprint, config.clone()).is_err());
        store.set_archived(&record.fingerprint, false).unwrap();
        std::fs::rename(&project, root.join("moved-project")).unwrap();
        assert!(project_seed(&root, &record.fingerprint, config).is_err());
    }

    #[tokio::test]
    async fn new_session_location_is_independent_and_survives_reopening() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path().canonicalize().unwrap();
        let config = ConfigHub::from_config_dir(root.join("config"));
        let project = root.join("project");
        let directory = root.join("startup/subdir");
        for path in [
            config.config_dir().join("commands"),
            project.join(".atman/commands"),
            directory.clone(),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(
            config.config_dir().join("config.toml"),
            "[trust]\nmode = 'reckless'\n",
        )
        .unwrap();
        std::fs::write(
            project.join("AGENTS.md"),
            "# location-rule\nPROJECT-RESOURCE",
        )
        .unwrap();
        std::fs::write(project.join("marker.txt"), "WRONG-DIRECTORY").unwrap();
        std::fs::write(directory.join("marker.txt"), "SELECTED-DIRECTORY").unwrap();
        std::fs::write(project.join(".atman/commands/agent.at"), r#"
            flow agent(input: string) -> string {
                contract { invocation { user_message: input } }
                return final.answer(message: rule.fetch(name: "location-rule") + "|" + fs.read("marker.txt") + "|" + input)
            }
        "#).unwrap();
        let request = Uuid::now_v7();
        let plan = Plan::new(
            request,
            config.clone(),
            Some(project.clone()),
            Some(project.clone()),
            directory.clone(),
            config.clone(),
        );
        assert!(plan.direct().is_none());
        let select = || Selection {
            project: 0,
            directory: directory.clone(),
        };
        let host = RuntimeHost::default();
        let (notices, _received) = mpsc::unbounded_channel();
        let prompts = Arc::new(prompts::Prompts::new(notices));
        let scopes = Arc::new(scopes::Scopes::empty(
            &root,
            host.clone(),
            None,
            prompts.clone(),
        ));
        let invalid = Plan::new(
            Uuid::now_v7(),
            config.clone(),
            Some(project.clone()),
            None,
            root.join("missing"),
            config.clone(),
        );
        assert!(
            invalid
                .seed(
                    Selection {
                        project: 0,
                        directory: root.join("missing")
                    },
                    &root
                )
                .is_err()
        );
        assert!(RuntimeStore::session_ids(&root).unwrap().is_empty());
        let (controls, _incoming) = mpsc::unbounded_channel();
        let Navigation::Created {
            flow: ViewAddress::Instance(flow),
            service,
            ..
        } = scopes
            .create_tui_session(
                plan.seed(select(), &root).unwrap(),
                request,
                controls,
                Arc::new(atman_runtime::session::FormRegistry::default()),
            )
            .await
            .unwrap()
        else {
            panic!("missing new session view")
        };
        assert!(
            service
                .execution
                .as_ref()
                .unwrap()
                .scope
                .executor
                .tool_ctx
                .prompt_resolver
                .is_some()
        );
        drop(service);
        assert_eq!(
            scopes
                .create_session(
                    plan.seed(select(), &root).unwrap(),
                    request,
                    scopes::CreationMode::Interactive
                )
                .await
                .unwrap(),
            flow
        );
        let changed = plan
            .seed(
                Selection {
                    project: 0,
                    directory: project.clone(),
                },
                &root,
            )
            .unwrap();
        assert!(
            scopes
                .create_session(changed, request, scopes::CreationMode::Interactive)
                .await
                .is_err()
        );
        let worker = host.flow(&flow).await.unwrap();
        let snapshot = worker.snapshot().await.unwrap();
        assert!(snapshot.pending.is_empty());
        assert!(worker.context().messages_full().is_empty());
        assert_eq!(worker.project().await.unwrap().unwrap().root(), project);
        let scope = scopes.open_flow_scope(&flow).await.unwrap();
        scope
            .command(&worker, ":invoke {\"input\":\"before-reopen\"}")
            .await
            .unwrap();
        host.wait_quiescent().await.unwrap();
        let text = worker
            .context()
            .messages_full()
            .last()
            .unwrap()
            .text_concat();
        assert!(
            text.contains("PROJECT-RESOURCE")
                && text.contains("SELECTED-DIRECTORY")
                && text.contains("before-reopen"),
            "{text}"
        );
        assert!(!text.contains("WRONG-DIRECTORY"));
        host.shutdown().await.unwrap();
        drop((scope, scopes, worker, host));
        let host = RuntimeHost::default();
        let scopes = Arc::new(scopes::Scopes::empty(&root, host.clone(), None, prompts));
        let scope = scopes.open_flow_scope(&flow).await.unwrap();
        let worker = host.flow(&flow).await.unwrap();
        scope
            .command(&worker, ":invoke {\"input\":\"after-reopen\"}")
            .await
            .unwrap();
        host.wait_quiescent().await.unwrap();
        let text = worker
            .context()
            .messages_full()
            .last()
            .unwrap()
            .text_concat();
        assert!(
            text.contains("PROJECT-RESOURCE")
                && text.contains("SELECTED-DIRECTORY")
                && text.contains("after-reopen"),
            "{text}"
        );
        assert_eq!(
            scope
                .session
                .snapshot()
                .await
                .unwrap()
                .catalog
                .project
                .unwrap()
                .root(),
            project
        );
        assert!(
            worker
                .launch_record()
                .await
                .unwrap()
                .unwrap()
                .input
                .workspace
                .is_none()
        );
        host.shutdown().await.unwrap();
    }

    #[test]
    fn new_session_location_equivalence_uses_project_and_actual_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = ConfigHub::from_config_dir(&root);
        let plan = |project, directory, startup| {
            Plan::new(
                Uuid::now_v7(),
                config.clone(),
                project,
                directory,
                startup,
                config.clone(),
            )
        };
        assert!(
            plan(Some(root.clone()), Some(root.join(".")), root.clone())
                .direct()
                .is_some()
        );
        assert!(
            plan(Some(root.clone()), None, root.clone())
                .direct()
                .is_none()
        );
        assert!(
            plan(None, Some(root.clone()), root.clone())
                .direct()
                .is_none()
        );
        let subdir = root.join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        assert!(
            plan(Some(root.clone()), Some(subdir), root.clone())
                .direct()
                .is_none()
        );
        #[cfg(unix)]
        {
            let alias = root.join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            let same = plan(Some(alias.clone()), Some(alias.clone()), root.clone());
            assert!(same.direct().is_some());
            assert_eq!(same.locations.directories(0).len(), 1);
        }
    }
}
