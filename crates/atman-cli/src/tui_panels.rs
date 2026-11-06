//! Host panel services shared independently of conversation scheduling and view selection.

use anyhow::{Context, Result};
use atman_runtime::{
    Executor, Session,
    memory::ConfessionStore,
    provider_lifecycle::{ProviderCatalogRefreshOutcome, ProviderLifecycle},
};
use atman_tui::{ProviderMutationRequest, ProviderMutationSuccess, TuiCommand, TuiControl};
use std::{path::PathBuf, sync::Arc};
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle, JoinSet},
};

mod worker;
pub(crate) use worker::{background, spawn_background, urgent};

use crate::{
    execute_model_mutation, execute_provider_mutation, loaded_rule_views,
    spawn_provider_mutation_task, suggest_confession_organization, switch_smart_model,
    test_model_endpoint, test_provider_endpoint,
};

fn mutate_settings(
    config: &atman_runtime::config_hub::ConfigHub,
    project_root: Option<&std::path::Path>,
    action: &atman_tui::settings::SettingsMutation,
) -> Result<String> {
    use atman_tui::settings::SettingsMutation;
    match action {
        SettingsMutation::ProjectStorage { scope } => {
            let root = project_root.context("Current session has no project")?;
            anyhow::ensure!(
                root.is_dir(),
                "Project path does not exist: {}",
                root.display()
            );
            config.set_project_storage_scope(root, *scope)?;
            Ok(format!(
                "Project storage set to {}; applies to new sessions",
                crate::storage_scope_name(*scope)
            ))
        }
        SettingsMutation::MathRendering { enabled } => {
            config.set_math_rendering_enabled(*enabled)?;
            Ok("Formula display saved; restart Atman to apply".into())
        }
        SettingsMutation::CreateAlias { name, model } => {
            config.add_alias(name, model)?;
            Ok(format!("Alias {name} saved"))
        }
        SettingsMutation::UpdateAlias {
            original,
            name,
            model,
        } => {
            config.update_alias(original.as_deref(), name, model)?;
            Ok(format!("Alias {name} saved"))
        }
        SettingsMutation::RemoveAlias { name } => {
            config.remove_alias(name)?;
            Ok(format!("Alias {name} removed"))
        }
        SettingsMutation::DefaultModel { model } => {
            config.bind_default_model(model)?;
            Ok("Default model saved".into())
        }
        SettingsMutation::SaveMcp {
            original,
            config: server,
        } => {
            if let Some(original) = original {
                config.replace_mcp(original, *server.clone())?;
            } else {
                config.upsert_mcp(*server.clone())?;
            }
            Ok(format!("MCP server {} saved", server.name))
        }
        SettingsMutation::RemoveMcp { name } => {
            config.remove_mcp(name)?;
            Ok(format!("MCP server {name} removed"))
        }
        SettingsMutation::ToggleMcp { name } => {
            let state = if config.toggle_mcp(name)? {
                "disabled"
            } else {
                "enabled"
            };
            Ok(format!("MCP server {name} {state}"))
        }
    }
}

pub(crate) struct PanelServices {
    session: Arc<Session>,
    config: atman_runtime::config_hub::ConfigHub,
    project_root: Option<PathBuf>,
    executor: Executor,
    replies: mpsc::UnboundedSender<TuiCommand>,
    lifecycle: ProviderLifecycle,
    knowledge: Option<Arc<ConfessionStore>>,
    provider_mutations: JoinSet<(
        ProviderMutationRequest,
        Result<ProviderMutationSuccess, String>,
    )>,
    refreshes: JoinSet<(String, Result<ProviderCatalogRefreshOutcome, String>)>,
    organization: Option<JoinHandle<()>>,
    provider_receipts: std::collections::HashMap<u64, atman_tui::control::ControlReplies>,
}

impl PanelServices {
    pub fn for_scope(
        session: Arc<Session>,
        executor: Executor,
        replies: mpsc::UnboundedSender<TuiCommand>,
        refresh: Vec<String>,
        config: atman_runtime::config_hub::ConfigHub,
        project: Option<atman_runtime::storage::ProjectContext>,
    ) -> Result<Self> {
        let lifecycle = executor
            .provider_lifecycle()
            .context("provider lifecycle unavailable")?;
        anyhow::ensure!(
            lifecycle.config_hub().source() == config.source(),
            "panel configuration does not match executor scope"
        );
        let project_root = project.as_ref().map(|project| project.root().to_path_buf());
        let knowledge = project.as_ref().map(|project| {
            let mut store = atman_runtime::memory::ConfessionStore::at(
                project.storage_root().join("confessions"),
            );
            if let Some(index) = &executor.tool_ctx.project_index {
                store = store.with_index(index.clone());
            }
            if let Some(redactor) =
                atman_daemon::bootstrap::build_redactor(Some(config.config_dir()))
            {
                store = store.with_redactor(redactor);
            }
            std::sync::Arc::new(store)
        });
        let mut services = Self {
            session,
            config,
            project_root,
            executor,
            replies,
            lifecycle,
            knowledge,
            provider_mutations: JoinSet::new(),
            refreshes: JoinSet::new(),
            organization: None,
            provider_receipts: Default::default(),
        };
        for provider_id in refresh {
            let lifecycle = services.lifecycle.clone();
            services.refreshes.spawn(background(async move {
                let result = match lifecycle.refresh_models_if_stale(&provider_id).await {
                    Ok(outcome) => Ok(outcome),
                    Err(
                        atman_runtime::ProviderLifecycleError::ProviderNotFound { .. }
                        | atman_runtime::ProviderLifecycleError::ProviderDisabled { .. }
                        | atman_runtime::ProviderLifecycleError::Stale { .. },
                    ) => Ok(ProviderCatalogRefreshOutcome::NotNeeded),
                    Err(error) => Err(error.to_string()),
                };
                (provider_id, result)
            }));
        }
        Ok(services)
    }

    pub fn has_pending_work(&self) -> bool {
        !self.provider_mutations.is_empty() || !self.refreshes.is_empty()
    }

    pub fn has_pending_mutations(&self) -> bool {
        !self.provider_mutations.is_empty()
    }

    pub async fn next_completion(&mut self) -> Result<(), JoinError> {
        tokio::select! {
            result = self.provider_mutations.join_next(), if !self.provider_mutations.is_empty() => {
                if let Some(result) = result {
                    let (request, result) = result?;
                    let id = request.request_id;
                    let _ = self.replies.send(TuiCommand::ProviderMutationResult { request, result });
                    self.provider_receipts.remove(&id);
                }
            }
            result = self.refreshes.join_next(), if !self.refreshes.is_empty() => {
                if let Some(result) = result {
                    let (provider_id, result) = result?;
                    let _ = self.replies.send(TuiCommand::ProviderCatalogRefreshResult { provider_id, result });
                }
            }
            else => std::future::pending::<()>().await,
        }
        Ok(())
    }

    /// Unhandled controls retain their payload and return to the bound execution host.
    pub async fn dispatch(
        &mut self,
        command: TuiControl,
        receipt: Option<atman_tui::control::ControlReplies>,
    ) -> Option<TuiControl> {
        match command {
            TuiControl::MutateSettings(request) => {
                let result =
                    mutate_settings(&self.config, self.project_root.as_deref(), &request.action)
                        .map_err(|error| error.to_string());
                let reload = result.is_ok()
                    && matches!(
                        request.action,
                        atman_tui::settings::SettingsMutation::SaveMcp { .. }
                            | atman_tui::settings::SettingsMutation::RemoveMcp { .. }
                            | atman_tui::settings::SettingsMutation::ToggleMcp { .. }
                    );
                let _ = self
                    .replies
                    .send(TuiCommand::SettingsMutationResult { request, result });
                if reload
                    && let Some(service) = self.executor.tool_ctx.mcp_service()
                    && let Err(error) = service.reload()
                {
                    let _ = self.replies.send(TuiCommand::Toast {
                        message: format!("Settings saved; MCP reload failed: {error}"),
                        level: atman_tui::app::NoteLevel::Warn,
                    });
                }
            }
            atman_tui::TuiControl::MutateProvider(request) => {
                if let Some(receipt) = &receipt {
                    self.provider_receipts
                        .insert(request.request_id, receipt.clone());
                }
                let lifecycle = self.lifecycle.clone();
                let action = request.action.clone();
                spawn_provider_mutation_task(&mut self.provider_mutations, request, async move {
                    let _receipt = receipt;
                    execute_provider_mutation(&lifecycle, action).await
                });
            }
            atman_tui::TuiControl::MutateModel(request) => {
                let result = execute_model_mutation(&self.config, &request.action)
                    .map_err(|error| error.to_string());
                let _ = self
                    .replies
                    .send(atman_tui::TuiCommand::ModelMutationResult { request, result });
            }
            atman_tui::TuiControl::OpenAliasManager { .. } => {
                // handled internally in the TUI — no-op here
            }
            atman_tui::TuiControl::OnboardingInit => {
                let _ = crate::init::init_config_dir_with_mode(self.config.config_dir(), None);
            }
            atman_tui::TuiControl::SwitchModel { request_id, model } => {
                let result = switch_smart_model(
                    &self.config,
                    &self.executor.providers,
                    &self.session,
                    &model,
                );
                let _ = self.replies.send(atman_tui::TuiCommand::ModelSwitchResult {
                    request_id,
                    model,
                    result,
                });
            }
            atman_tui::TuiControl::TestProviderConnection(request) => {
                let tx = self.replies.clone();
                spawn_background(async move {
                    let (message, ok) = test_provider_endpoint(&request.name, &request.entry).await;
                    let result = if ok { Ok(message) } else { Err(message) };
                    let _ = tx.send(atman_tui::TuiCommand::ProviderTestResult {
                        request_id: request.request_id,
                        result,
                    });
                });
            }
            atman_tui::TuiControl::TestModel(request) => {
                let tx = self.replies.clone();
                let providers = self.executor.providers.clone();
                spawn_background(async move {
                    let result = test_model_endpoint(&providers, &request).await;
                    let _ = tx.send(atman_tui::TuiCommand::ModelTestResult {
                        request_id: request.request_id,
                        result,
                    });
                });
            }
            atman_tui::TuiControl::McpTest { name } => {
                let tx = self.replies.clone();
                let name = name.clone();
                let configs = self.config.load_mcp();
                spawn_background(async move {
                    let Some(cfg) = configs.into_iter().find(|c| c.name == name) else {
                        let _ = tx.send(atman_tui::TuiCommand::McpTestResult {
                            name,
                            message: "not found in config".into(),
                            ok: false,
                        });
                        return;
                    };
                    let probe = atman_runtime::ToolRegistry::new();
                    let results = atman_runtime::mcp::register_from_configs(&probe, &[cfg]).await;
                    let (msg, ok) = match &results[0] {
                        Ok(s) => (format!("{} tools discovered", s.tool_count), true),
                        Err(e) => (e.error.to_string(), false),
                    };
                    let _ = tx.send(atman_tui::TuiCommand::McpTestResult {
                        name,
                        message: msg,
                        ok,
                    });
                });
            }
            atman_tui::TuiControl::McpListResources { name } => {
                let tx = self.replies.clone();
                let name = name.clone();
                let configs = self.config.load_mcp();
                spawn_background(async move {
                    let Some(cfg) = configs.into_iter().find(|c| c.name == name) else {
                        let _ = tx.send(atman_tui::TuiCommand::McpResourcesResult {
                            name,
                            resources: Vec::new(),
                        });
                        return;
                    };
                    let client = atman_runtime::mcp::McpClient::connect_config(&cfg).await;
                    let resources = match client {
                        Ok(c) => c.list_resources().await.unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };
                    let _ = tx.send(atman_tui::TuiCommand::McpResourcesResult { name, resources });
                });
            }
            atman_tui::TuiControl::McpListPrompts { name } => {
                let tx = self.replies.clone();
                let name = name.clone();
                let configs = self.config.load_mcp();
                spawn_background(async move {
                    let Some(cfg) = configs.into_iter().find(|c| c.name == name) else {
                        let _ = tx.send(atman_tui::TuiCommand::McpPromptsResult {
                            name,
                            prompts: Vec::new(),
                        });
                        return;
                    };
                    let client = atman_runtime::mcp::McpClient::connect_config(&cfg).await;
                    let prompts = match client {
                        Ok(c) => c.list_prompts().await.unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };
                    let _ = tx.send(atman_tui::TuiCommand::McpPromptsResult { name, prompts });
                });
            }
            atman_tui::TuiControl::McpReload => {
                if let Some(service) = self.executor.tool_ctx.mcp_service() {
                    match service.reload() {
                        Ok(()) => {
                            let _ = self.replies.send(atman_tui::TuiCommand::McpReloaded);
                        }
                        Err(error) => {
                            atman_runtime::notify!(error, "MCP reload failed: {error}")
                        }
                    }
                }
            }
            atman_tui::TuiControl::ListKnowledge { request_id } => {
                let result = match &self.knowledge {
                    Some(store) => {
                        let confessions =
                            store.list_with_meta(false).await.map_err(|e| e.to_string());
                        let rules = loaded_rule_views(&self.executor)
                            .await
                            .map_err(|e| e.to_string());
                        confessions.and_then(|rows| rules.map(|rules| (rows, rules)))
                    }
                    None => Err("Project scope is unavailable".into()),
                };
                let _ = self.replies.send(atman_tui::TuiCommand::KnowledgeResult {
                    request_id: Some(request_id),
                    result,
                });
            }
            atman_tui::TuiControl::ReloadRules { request_id } => {
                let result = match (self.project_root.clone(), std::env::var_os("HOME")) {
                    (Some(project_root), Some(home)) => {
                        let rules = if std::env::var_os("ATMAN_DISABLE_MIGRATION").is_some() {
                            Vec::new()
                        } else {
                            atman_runtime::migration::scan_migrated_rules(
                                &project_root,
                                &PathBuf::from(home),
                            )
                        };
                        let fetch = atman_runtime::tools::memory_stubs::RuleFetch::new();
                        fetch.set_migrated(rules).await;
                        self.executor.tools.register(Arc::new(fetch));
                        let confessions = match &self.knowledge {
                            Some(store) => {
                                store.list_with_meta(false).await.map_err(|e| e.to_string())
                            }
                            None => Err("Project scope is unavailable".into()),
                        };
                        let rules = loaded_rule_views(&self.executor)
                            .await
                            .map_err(|e| e.to_string());
                        confessions.and_then(|rows| rules.map(|rules| (rows, rules)))
                    }
                    _ => Err("Project root or home directory is unavailable".into()),
                };
                let _ = self.replies.send(atman_tui::TuiCommand::KnowledgeResult {
                    request_id: Some(request_id),
                    result,
                });
            }
            atman_tui::TuiControl::GetConfessionHistory { request_id, id } => {
                let result = match &self.knowledge {
                    Some(store) => store.history(&id).await.map_err(|e| e.to_string()),
                    None => Err("Project scope is unavailable".into()),
                };
                let _ = self
                    .replies
                    .send(atman_tui::TuiCommand::ConfessionHistoryResult {
                        request_id,
                        id,
                        result,
                    });
            }
            atman_tui::TuiControl::ReviseConfession {
                id,
                base_revision,
                fields,
            } => {
                let saved_id = id.clone();
                let result = match &self.knowledge {
                    Some(store) => store
                        .revise(id, base_revision, fields)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                    None => Err("Project scope is unavailable".into()),
                };
                match result {
                    Ok(()) => {
                        if let Some(store) = &self.knowledge {
                            let rows = store.list_with_meta(false).await.map_err(|e| e.to_string());
                            let rules = loaded_rule_views(&self.executor)
                                .await
                                .map_err(|e| e.to_string());
                            let _ = self.replies.send(atman_tui::TuiCommand::KnowledgeResult {
                                request_id: None,
                                result: rows.and_then(|rows| rules.map(|rules| (rows, rules))),
                            });
                            let _ = self
                                .replies
                                .send(atman_tui::TuiCommand::ConfessionSaved(saved_id));
                        }
                    }
                    Err(error) => {
                        let _ = self
                            .replies
                            .send(atman_tui::TuiCommand::ConfessionSaveFailed {
                                id: saved_id,
                                error,
                            });
                    }
                }
            }
            atman_tui::TuiControl::ArchiveConfession { id, base_revision } => {
                let result = match &self.knowledge {
                    Some(store) => store
                        .archive(id, base_revision, "Archived by user".into())
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    None => Err("Project scope is unavailable".into()),
                };
                match result {
                    Ok(()) => {
                        if let Some(store) = &self.knowledge {
                            let rows = store
                                .list_with_meta(false)
                                .await
                                .map_err(|error| error.to_string());
                            let rules = loaded_rule_views(&self.executor)
                                .await
                                .map_err(|error| error.to_string());
                            let _ = self.replies.send(atman_tui::TuiCommand::KnowledgeResult {
                                request_id: None,
                                result: rows.and_then(|rows| rules.map(|rules| (rows, rules))),
                            });
                        }
                        let _ = self.replies.send(atman_tui::TuiCommand::ConfessionArchived);
                    }
                    Err(error) => {
                        let _ = self
                            .replies
                            .send(atman_tui::TuiCommand::ConfessionArchiveFailed(error));
                    }
                }
            }
            atman_tui::TuiControl::SuggestOrganization { request_id } => {
                if let Some(task) = self.organization.take() {
                    task.abort();
                }
                let store = self.knowledge.clone();
                let executor = self.executor.clone();
                let session = self.session.clone();
                let tx = self.replies.clone();
                self.organization = Some(spawn_background(async move {
                    let result = match store {
                        Some(store) => {
                            let progress_tx = tx.clone();
                            suggest_confession_organization(
                                &executor,
                                &session,
                                &store,
                                move |completed, total| {
                                    let _ = progress_tx.send(
                                        atman_tui::TuiCommand::OrganizationProgress {
                                            request_id,
                                            completed,
                                            total,
                                        },
                                    );
                                },
                            )
                            .await
                            .map_err(|e| e.to_string())
                        }
                        None => Err("Project scope is unavailable".into()),
                    };
                    let _ =
                        tx.send(atman_tui::TuiCommand::OrganizationResult { request_id, result });
                }));
            }
            atman_tui::TuiControl::CancelOrganization => {
                if let Some(task) = self.organization.take() {
                    task.abort();
                }
            }
            atman_tui::TuiControl::OrganizeConfessions(changes) => {
                let result = match &self.knowledge {
                    Some(store) => store
                        .organize(changes)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                    None => Err("Project scope is unavailable".into()),
                };
                match result {
                    Ok(()) => {
                        if let Some(store) = &self.knowledge {
                            let rows = store.list_with_meta(false).await.map_err(|e| e.to_string());
                            let rules = loaded_rule_views(&self.executor)
                                .await
                                .map_err(|e| e.to_string());
                            let _ = self.replies.send(atman_tui::TuiCommand::KnowledgeResult {
                                request_id: None,
                                result: rows.and_then(|rows| rules.map(|rules| (rows, rules))),
                            });
                            let _ = self
                                .replies
                                .send(atman_tui::TuiCommand::OrganizationApplied);
                        }
                    }
                    Err(error) => {
                        let _ = self
                            .replies
                            .send(atman_tui::TuiCommand::OrganizationApplyFailed(error));
                    }
                }
            }
            other => return Some(other),
        }
        None
    }
}

impl Drop for PanelServices {
    fn drop(&mut self) {
        if let Some(task) = self.organization.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_mutations_and_replies_retain_the_host_scope() {
        use atman_tui::settings::{SettingsMutation as Mutation, SettingsMutationRequest};
        let temp = tempfile::tempdir().unwrap();
        let first =
            atman_runtime::config_hub::ConfigHub::from_config_dir(temp.path().join("first"));
        let second =
            atman_runtime::config_hub::ConfigHub::from_config_dir(temp.path().join("second"));
        for (hub, model) in [(&first, "first-model"), (&second, "second-model")] {
            std::fs::create_dir_all(hub.config_dir()).unwrap();
            std::fs::write(hub.config_dir().join("config.toml"), format!(
                "[providers.example]\ntype = 'openai'\napi_key = 'test-key'\n[models.model]\nprovider = 'example'\nmodel = '{model}'\n"
            )).unwrap();
            hub.reload().unwrap();
        }
        let mut executor = Executor::new();
        executor.providers =
            atman_runtime::provider::ProviderRegistry::with_models(first.models().clone());
        executor.attach_provider_lifecycle(first.clone()).unwrap();
        let session = Arc::new(Session::open_ephemeral());
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(
            PanelServices::for_scope(
                session.clone(),
                executor.clone(),
                tx.clone(),
                vec![],
                second.clone(),
                None
            )
            .is_err()
        );
        let mut services =
            PanelServices::for_scope(session.clone(), executor, tx, vec![], first.clone(), None)
                .unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        services.project_root = Some(project.clone());
        let mut mcp = atman_runtime::mcp::McpServerConfig::stdio(
            "example",
            "example-mcp",
            vec![],
            atman_runtime::tool::Tier::Zero,
            1_000,
        );
        mcp.disabled = true;
        let actions = [
            Mutation::MathRendering { enabled: false },
            Mutation::ProjectStorage {
                scope: atman_runtime::storage::StorageScope::Local,
            },
            Mutation::CreateAlias {
                name: "work".into(),
                model: "model".into(),
            },
            Mutation::UpdateAlias {
                original: Some("work".into()),
                name: "fast".into(),
                model: "model".into(),
            },
            Mutation::DefaultModel {
                model: "model".into(),
            },
            Mutation::SaveMcp {
                original: None,
                config: Box::new(mcp),
            },
            Mutation::ToggleMcp {
                name: "example".into(),
            },
        ];
        for action in actions {
            let request = SettingsMutationRequest {
                id: uuid::Uuid::now_v7(),
                action,
            };
            services
                .dispatch(TuiControl::MutateSettings(request.clone()), None)
                .await;
            let TuiCommand::SettingsMutationResult {
                request: returned,
                result,
            } = rx.try_recv().unwrap()
            else {
                panic!()
            };
            assert_eq!(returned.id, request.id);
            assert!(result.is_ok(), "{result:?}");
        }
        assert_eq!(first.models().resolve_alias("smart"), "model");
        assert!(!first.math_rendering_enabled().unwrap());
        assert!(second.math_rendering_enabled().unwrap());
        assert_eq!(
            first.try_storage_config(Some(&project)).unwrap().scope,
            Some(atman_runtime::storage::StorageScope::Local)
        );
        let stored = std::fs::read(project.join(".atman/config.toml")).unwrap();
        for invalid_root in [None, Some(temp.path().join("missing").as_path())] {
            assert!(
                mutate_settings(
                    &first,
                    invalid_root,
                    &Mutation::ProjectStorage {
                        scope: atman_runtime::storage::StorageScope::Global,
                    }
                )
                .is_err()
            );
        }
        assert_eq!(
            std::fs::read(project.join(".atman/config.toml")).unwrap(),
            stored
        );
        assert_eq!(first.models().resolve_alias("fast"), "model");
        assert_eq!(first.load_local_mcp().len(), 1);
        assert!(!first.load_local_mcp()[0].disabled);
        assert!(second.models().all_aliases().is_empty());
        assert!(second.load_local_mcp().is_empty());
        let original = std::fs::read(first.config_dir().join("config.toml")).unwrap();
        assert!(
            mutate_settings(
                &first,
                None,
                &Mutation::CreateAlias {
                    name: "fast".into(),
                    model: "model".into()
                }
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(first.config_dir().join("config.toml")).unwrap(),
            original
        );
        mutate_settings(
            &first,
            None,
            &Mutation::RemoveAlias {
                name: "fast".into(),
            },
        )
        .unwrap();
        mutate_settings(
            &first,
            None,
            &Mutation::RemoveMcp {
                name: "example".into(),
            },
        )
        .unwrap();
        assert!(first.load_local_mcp().is_empty());
        assert!(session.messages_full().is_empty());
    }

    #[tokio::test]
    async fn panel_dispatch_preserves_host_controls_and_correlates_async_results() {
        let config = tempfile::tempdir().unwrap();
        let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(config.path());
        let mut executor = Executor::new();
        executor.providers =
            atman_runtime::provider::ProviderRegistry::with_models(hub.models().clone());
        executor.attach_provider_lifecycle(hub.clone()).unwrap();
        let session = Arc::new(Session::open_ephemeral());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut services =
            PanelServices::for_scope(session.clone(), executor, tx, vec![], hub, None).unwrap();
        let input = TuiControl::Submit(atman_tui::TuiSubmission {
            text: "keep the target and payload".into(),
            images: vec![],
            reasoning: Some(atman_runtime::provider::ReasoningSelection::Effort {
                effort: atman_runtime::provider::ReasoningEffort::High,
                execution_mode: None,
            }),
            presentation: None,
        });
        let Some(TuiControl::Submit(input)) = services.dispatch(input, None).await else {
            panic!("panel services consumed an execution command");
        };
        assert_eq!(input.text, "keep the target and payload");
        assert!(input.reasoning.is_some());
        assert!(session.messages_full().is_empty());
        assert!(rx.try_recv().is_err());
        assert!(matches!(
            services.dispatch(TuiControl::CancelFlow, None).await,
            Some(TuiControl::CancelFlow)
        ));

        let request = ProviderMutationRequest::new(
            42,
            atman_tui::ProviderMutation::Refresh {
                provider_id: "missing-provider".into(),
            },
        );
        assert!(
            services
                .dispatch(TuiControl::MutateProvider(request.clone()), None)
                .await
                .is_none()
        );
        assert!(services.has_pending_work());
        services.next_completion().await.unwrap();
        let TuiCommand::ProviderMutationResult {
            request: returned,
            result,
        } = rx.recv().await.unwrap()
        else {
            panic!("provider response lost its request identity");
        };
        assert_eq!(returned, request);
        assert!(result.is_err());
        assert!(!services.has_pending_work());
        drop(services);
    }
}
