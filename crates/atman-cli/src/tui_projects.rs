//! Project directory work runs outside the terminal event loop.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atman_runtime::{
    config_hub::ConfigHub,
    flow_runtime::catalog::{CatalogStatus, SessionCatalog},
    flow_store::RuntimeStore,
    project_catalog::ProjectCatalogStore,
    session_meta::{SessionMeta, SessionStats, fingerprint_from_root},
};
use atman_tui::project_hub::{
    ProjectDetails, ProjectDirectory, ProjectRequest, ProjectSession, ProjectSort, SessionTarget,
};

pub(crate) async fn request(
    root: PathBuf,
    current: SessionTarget,
    project: Option<PathBuf>,
    config: Option<ConfigHub>,
    native: Option<&SessionCatalog>,
    request: ProjectRequest,
) -> atman_tui::TuiCommand {
    let mutation_root = root.clone();
    let mutation = request.mutation;
    let result = tokio::task::spawn_blocking(move || -> Result<_> {
        let message = if let Some(mutation) = mutation {
            crate::execute_project_mutation(
                &mutation_root,
                project.as_deref().map(fingerprint_from_root).as_deref(),
                &mutation,
            )?
            .1
        } else {
            String::new()
        };
        Ok(message)
    })
    .await;
    let (success_message, result) = match result {
        Ok(Ok(message)) => (message, directory(root, current, config, native).await),
        Ok(Err(error)) => (String::new(), Err(error)),
        Err(error) => (String::new(), Err(error.into())),
    };
    atman_tui::TuiCommand::ProjectCatalogUpdated {
        request_id: request.id,
        success_message,
        result: result.map_err(|error| format!("{error:#}")),
    }
}

async fn directory(
    root: PathBuf,
    current: SessionTarget,
    config: Option<ConfigHub>,
    native: Option<&SessionCatalog>,
) -> Result<ProjectDirectory> {
    let data = root.clone();
    let mut directory = tokio::task::spawn_blocking(move || -> Result<_> {
        let projects = ProjectCatalogStore::new(&data)
            .reconcile_sessions()?
            .projects;
        let mut details = std::collections::HashMap::new();
        for project in &projects {
            details.insert(
                project.fingerprint.clone(),
                ProjectDetails {
                    path_available: project.path_available(),
                    scope: config
                        .as_ref()
                        .and_then(|config| config.try_storage_config(Some(&project.root)).ok())
                        .map(|cfg| cfg.scope.unwrap_or_default()),
                },
            );
        }
        Ok(ProjectDirectory {
            projects,
            details,
            sessions: Default::default(),
        })
    })
    .await??;
    let (legacy_root, legacy_current) = (root.clone(), current.clone());
    directory.sessions = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut grouped = legacy_sessions(&legacy_root, &legacy_current)?;
        for session in atman_runtime::history_index::discover(&legacy_root)? {
            if let Some(project) = &session.project {
                grouped
                    .entry(fingerprint_from_root(project))
                    .or_default()
                    .push(ProjectSession {
                        target: SessionTarget::Indexed(session.source.clone()),
                        title: session.title(),
                        activity: format!(
                            "{} indexed user messages · Review recovery",
                            session.user_messages
                        ),
                        preview: None,
                        updated_time: chrono::DateTime::parse_from_rfc3339(&session.updated_at)
                            .ok()
                            .map(std::time::SystemTime::from),
                        updated_at: session.updated_at,
                        goal: None,
                        is_current: false,
                    });
            }
        }
        Ok(grouped)
    })
    .await??;
    if let Some(catalog) = native {
        for id in catalog.ids().await? {
            let view = catalog.overview(id).await?;
            let Some(project) = view.summary.project_root else {
                continue;
            };
            let data = root.clone();
            let activity = tokio::task::spawn_blocking(move || {
                RuntimeStore::inspect_session(&data, id).map(|read| read.activity_time())
            })
            .await??;
            let status = match view.summary.status {
                CatalogStatus::Running => "Running",
                CatalogStatus::Idle => "Idle",
                CatalogStatus::Closed => "Closed",
                CatalogStatus::Detached => "Detached",
                CatalogStatus::Unavailable => "Unavailable",
            };
            let preview = if let Some(instance_id) = &view.summary.default_flow {
                catalog
                    .describe_flow(
                        &atman_runtime::flow_store::FlowRef {
                            session_id: id.into(),
                            instance_id: instance_id.clone(),
                        },
                        |_| Ok(()),
                    )
                    .await
                    .ok()
                    .and_then(|flow| flow.preview)
            } else {
                None
            };
            directory
                .sessions
                .entry(fingerprint_from_root(&project))
                .or_default()
                .push(ProjectSession {
                    target: SessionTarget::Persistent(id),
                    title: view
                        .summary
                        .title
                        .unwrap_or_else(|| "Untitled session".into()),
                    activity: format!(
                        "{} flows · {status}",
                        view.summary
                            .flow_count
                            .map_or_else(|| "?".into(), |count| count.to_string())
                    ),
                    updated_at: activity_label(activity),
                    updated_time: activity,
                    preview,
                    goal: view.default_goal,
                    is_current: SessionTarget::Persistent(id) == current,
                });
        }
    }
    for sessions in directory.sessions.values_mut() {
        sessions.sort_by(|a, b| ProjectSort::NewestFirst.compare_sessions(a, b));
    }
    Ok(directory)
}

fn activity_label(time: Option<std::time::SystemTime>) -> String {
    time.map(|time| {
        let local: chrono::DateTime<chrono::Local> = time.into();
        local.format("%Y-%m-%d %H:%M").to_string()
    })
    .unwrap_or_else(|| "Unknown".into())
}

fn legacy_sessions(
    root: &Path,
    current: &SessionTarget,
) -> Result<std::collections::HashMap<String, Vec<ProjectSession>>> {
    let mut grouped: std::collections::HashMap<String, Vec<ProjectSession>> = Default::default();
    let entries = match std::fs::read_dir(root.join("sessions")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(grouped),
        Err(error) => return Err(error).context("read legacy session directory"),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(meta) = SessionMeta::load(&path) else {
            continue;
        };
        let Some(fingerprint) = meta.project_fingerprint else {
            continue;
        };
        let stats = SessionStats::inspect(&path)?;
        if stats.user_message_count == 0 {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        let activity = path
            .join("events.jsonl")
            .metadata()
            .and_then(|meta| meta.modified())
            .or_else(|_| entry.metadata().and_then(|meta| meta.modified()))
            .ok();
        grouped
            .entry(fingerprint)
            .or_default()
            .push(ProjectSession {
                target: SessionTarget::Legacy(id.clone()),
                title: meta
                    .title
                    .unwrap_or_else(|| format!("Session {}", &id[..id.len().min(8)])),
                activity: format!("{} messages", stats.message_count),
                preview: None,
                updated_at: activity_label(activity),
                updated_time: activity,
                goal: atman_runtime::memory::goal::GoalStore::at(&path).get().ok(),
                is_current: &SessionTarget::Legacy(id) == current,
            });
    }
    Ok(grouped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mixed_directory_keeps_same_uuid_sources_distinct_without_writing_history() {
        let data = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(data.path().join("config"));
        let project = data.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let context =
            atman_runtime::storage::ProjectContext::resolve(&config, &project, data.path())
                .unwrap();
        let id = uuid::Uuid::now_v7();
        let store = RuntimeStore::open(data.path()).unwrap();
        let _owner = store
            .create_session(atman_runtime::flow_store::SessionDescriptor {
                id,
                title: "Persistent".into(),
                project: Some(context),
                configuration: Default::default(),
            })
            .unwrap();
        let legacy = data.path().join("sessions").join(id.to_string());
        std::fs::create_dir_all(&legacy).unwrap();
        let fingerprint = fingerprint_from_root(&project);
        SessionMeta {
            project_root: Some(project),
            project_fingerprint: Some(fingerprint.clone()),
            title: Some("Original".into()),
            ..Default::default()
        }
        .save(&legacy)
        .unwrap();
        std::fs::write(legacy.join("events.jsonl"), "{\"type\":\"user_msg\"}\n").unwrap();
        let catalog = SessionCatalog::new(None, Some(data.path().to_owned()));
        for current in [
            SessionTarget::Legacy(id.to_string()),
            SessionTarget::Persistent(id),
        ] {
            let directory = directory(
                data.path().to_owned(),
                current.clone(),
                None,
                Some(&catalog),
            )
            .await
            .unwrap();
            let sessions = &directory.sessions[&fingerprint];
            assert_eq!(sessions.len(), 2);
            assert_eq!(sessions.iter().filter(|row| row.is_current).count(), 1);
            assert_eq!(
                sessions.iter().find(|row| row.is_current).unwrap().target,
                current
            );
            assert_eq!(std::fs::read_dir(&legacy).unwrap().count(), 2);
        }
    }

    #[tokio::test]
    async fn project_requests_preserve_legacy_targets_and_report_mutation_failures() {
        let data = tempfile::tempdir().unwrap();
        let project = data.path().join("project");
        std::fs::create_dir_all(project.join(".atman")).unwrap();
        std::fs::write(project.join(".atman/config.toml"), "invalid = [").unwrap();
        let config = ConfigHub::from_config_dir(data.path().join("config"));
        let session = data.path().join("sessions/legacy-saved");
        std::fs::create_dir_all(&session).unwrap();
        let fingerprint = fingerprint_from_root(&project);
        SessionMeta {
            project_root: Some(project.clone()),
            project_fingerprint: Some(fingerprint.clone()),
            created_at: Some(chrono::Utc::now()),
            title: Some("Saved work".into()),
            ..Default::default()
        }
        .save(&session)
        .unwrap();
        std::fs::write(session.join("events.jsonl"), "{\"type\":\"user_msg\"}\n").unwrap();
        let id = uuid::Uuid::new_v4();
        let reply = request(
            data.path().to_owned(),
            SessionTarget::Legacy("legacy-saved".into()),
            Some(project.clone()),
            Some(config.clone()),
            None,
            ProjectRequest { id, mutation: None },
        )
        .await;
        let atman_tui::TuiCommand::ProjectCatalogUpdated {
            request_id,
            result: Ok(directory),
            ..
        } = reply
        else {
            panic!("directory request failed");
        };
        assert_eq!(request_id, id);
        assert_eq!(
            directory.sessions[&fingerprint][0].target,
            SessionTarget::Legacy("legacy-saved".into())
        );
        assert!(directory.sessions[&fingerprint][0].is_current);
        assert!(directory.details[&fingerprint].scope.is_none());
        let failed = request(
            data.path().to_owned(),
            SessionTarget::Legacy("legacy-saved".into()),
            Some(project),
            Some(config),
            None,
            ProjectRequest {
                id,
                mutation: Some(atman_tui::ProjectMutation::Delete { fingerprint }),
            },
        )
        .await;
        assert!(
            matches!(failed, atman_tui::TuiCommand::ProjectCatalogUpdated { request_id, result: Err(_), .. } if request_id == id)
        );
        assert!(session.join("events.jsonl").exists());
    }
}
