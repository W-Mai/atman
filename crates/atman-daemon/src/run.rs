use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use atman_proto::{FlowRunId as ProtoRunId, SessionId as ProtoSessionId};
use atman_rt::Value as CoreValue;

use crate::state::DaemonState;

type Value = CoreValue<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

pub(crate) fn render_value(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Unit => String::new(),
        other => format!("{other:?}"),
    }
}

#[derive(Clone)]
pub struct RunLauncher {
    pub project_root: PathBuf,
    pub config_dir: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
}

pub struct SpawnedRun {
    pub session_id: ProtoSessionId,
    pub instance_id: uuid::Uuid,
    pub run_id: ProtoRunId,
    pub goal_id: Option<String>,
}

#[derive(Clone)]
struct ProviderCatalogRefreshDispatcher {
    runtime: tokio::runtime::Handle,
    lifecycle: atman_runtime::ProviderLifecycle,
}

pub(crate) fn refresh_provider_catalogs(executor: &atman_runtime::Executor, plan: Vec<String>) {
    if let Some(lifecycle) = executor.provider_lifecycle() {
        ProviderCatalogRefreshDispatcher {
            runtime: tokio::runtime::Handle::current(),
            lifecycle,
        }
        .dispatch(plan);
    }
}

impl ProviderCatalogRefreshDispatcher {
    fn dispatch(&self, plan: Vec<String>) {
        for provider_id in plan {
            let lifecycle = self.lifecycle.clone();
            drop(self.runtime.spawn(async move {
                let task_provider_id = provider_id.clone();
                let task = tokio::spawn(async move {
                    lifecycle
                        .refresh_models_if_stale(&task_provider_id)
                        .await
                });
                match task.await {
                    Ok(Ok(
                        atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::NotNeeded
                        | atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::AlreadyInFlight,
                    )) => {}
                    Ok(Ok(
                        atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::CatalogUpdated(
                            delta,
                        ),
                    )) => eprintln!(
                        "[atman-daemon] provider catalog `{provider_id}` refreshed: +{} ~{} -{} ({} total)",
                        delta.added, delta.updated, delta.removed, delta.total
                    ),
                    Ok(Ok(_)) => {}
                    Ok(Err(
                        atman_runtime::ProviderLifecycleError::ProviderNotFound { .. }
                        | atman_runtime::ProviderLifecycleError::ProviderDisabled { .. }
                        | atman_runtime::ProviderLifecycleError::Stale { .. },
                    )) => {}
                    Ok(Err(error)) => eprintln!(
                        "[atman-daemon] provider catalog `{provider_id}` background refresh failed: {error}"
                    ),
                    Err(error) => eprintln!(
                        "[atman-daemon] provider catalog `{provider_id}` background refresh task failed: {error}"
                    ),
                }
            }));
        }
    }
}

#[derive(Default)]
pub struct RunOptions {
    pub goal: bool,
    pub reasoning: Option<String>,
    pub images: Vec<atman_proto::InlineImage>,
}

pub(crate) fn invocation_env_from_reasoning(
    reasoning: Option<String>,
) -> Result<atman_runtime::InvocationEnv> {
    match reasoning {
        Some(reasoning) => {
            let selection: atman_runtime::provider::ReasoningSelection = reasoning
                .parse()
                .map_err(|error: String| anyhow::anyhow!("invalid reasoning: {error}"))?;
            Ok(atman_runtime::InvocationEnv::single(
                "effort",
                Value::Str(selection.to_string()),
            ))
        }
        None => Ok(atman_runtime::InvocationEnv::default()),
    }
}

pub fn reconcile_workspaces(
    project_root: &Path,
    daemon_generation: &str,
) -> Result<Vec<atman_runtime::git_workspace::WorkspaceRecord>> {
    let repository = match atman_runtime::git::discover_repository(project_root) {
        Ok(repository) => repository,
        Err(atman_runtime::git::GitError::NotARepo(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if repository.bare {
        return Ok(Vec::new());
    }
    let Some(manager) =
        atman_runtime::git_workspace::WorkspaceManager::open_existing(project_root, None)?
    else {
        return Ok(Vec::new());
    };
    Ok(manager.reconcile_generation(daemon_generation)?)
}

impl RunLauncher {
    pub fn new(
        project_root: PathBuf,
        config_dir: Option<PathBuf>,
        home_dir: Option<PathBuf>,
    ) -> Result<Self> {
        crate::bootstrap::resolve_config_hub(config_dir.as_deref())?;
        Ok(Self {
            project_root,
            config_dir,
            home_dir,
        })
    }

    pub async fn start_provider_catalog_refreshes(&self, state: &DaemonState) -> Result<()> {
        reload_model_config(self.config_dir.as_deref());
        let lifecycle = state.provider_lifecycle_for(self.config_dir.as_deref())?;
        let plan = lifecycle.prepare_auth_runtime().await?;
        ProviderCatalogRefreshDispatcher {
            runtime: tokio::runtime::Handle::current(),
            lifecycle,
        }
        .dispatch(plan);
        Ok(())
    }

    pub async fn spawn(
        &self,
        state: Arc<DaemonState>,
        flow_path: &str,
        args: Vec<(String, Value)>,
    ) -> Result<SpawnedRun> {
        self.spawn_as(state, flow_path, args, crate::state::LOCAL_OPERATOR)
            .await
    }

    pub async fn spawn_as(
        &self,
        state: Arc<DaemonState>,
        flow_path: &str,
        args: Vec<(String, Value)>,
        owner_principal: &str,
    ) -> Result<SpawnedRun> {
        self.spawn_as_with_options(
            state,
            flow_path,
            args,
            owner_principal,
            RunOptions::default(),
        )
        .await
    }

    pub async fn spawn_as_with_options(
        &self,
        state: Arc<DaemonState>,
        flow_path: &str,
        args: Vec<(String, Value)>,
        owner_principal: &str,
        options: RunOptions,
    ) -> Result<SpawnedRun> {
        crate::flow_service::authorize_operator(owner_principal)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let launcher = self.clone();
        let path = PathBuf::from(flow_path);
        // The daemon retains accepted creation work if its RPC waiter disconnects.
        tokio::spawn(crate::flow_service::launch_run(
            state, launcher, path, args, options,
        ))
        .await?
    }
}

fn reload_model_config(config_dir: Option<&Path>) {
    let Some(dir) = config_dir else {
        return;
    };
    if let Err(error) =
        atman_runtime::config_hub::ConfigHub::from_config_dir(dir).migrate_and_reload_models()
    {
        atman_runtime::notify!(
            error,
            "config.toml migration/reload failed; disk migration may already be committed: {error}"
        );
    }
}

pub(crate) fn path_is_managed_agent_at(path: &Path, config_dir: Option<&Path>) -> bool {
    let Some(dir) = config_dir else {
        return false;
    };
    let managed = dir.join("commands").join("agent.at");
    if same_path(path, &managed) {
        return true;
    }
    let Some(file_name) = path.file_name() else {
        return false;
    };
    if file_name != "agent.at" {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    same_path(parent, &dir.join("commands"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
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
    }

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["config", "user.name", "Atman Test"]);
        git(
            tmp.path(),
            &["config", "user.email", "atman@example.invalid"],
        );
        git(tmp.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(tmp.path().join("README.md"), "committed\n").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "initial"]);
        tmp
    }

    #[test]
    fn reconciliation_is_noop_without_git_or_registry_and_reports_bad_registry() {
        let plain = tempfile::tempdir().unwrap();
        assert!(
            reconcile_workspaces(plain.path(), "generation")
                .unwrap()
                .is_empty()
        );
        assert!(!plain.path().join(".atman").exists());

        let repository = repo();
        assert!(
            reconcile_workspaces(repository.path(), "generation")
                .unwrap()
                .is_empty()
        );
        assert!(!repository.path().join(".atman").exists());

        let manager =
            atman_runtime::git_workspace::WorkspaceManager::at(repository.path(), None).unwrap();
        std::fs::write(
            repository.path().join(".atman/workspaces.json"),
            b"not-json",
        )
        .unwrap();
        assert!(reconcile_workspaces(repository.path(), "generation").is_err());
        assert!(manager.managed_root().exists());
    }

    #[test]
    fn runtime_workspace_service_uses_daemon_generation() {
        let _registry_lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let repository = repo();
                let state = DaemonState::new_with_generation(
                    repository.path().join("data"),
                    "daemon-generation".into(),
                );
                let outcome =
                    atman_runtime::AtmanRuntime::build(atman_runtime::AtmanRuntimeOptions {
                        events: atman_runtime::event::EventSink::new(),
                        mock: true,
                        config: atman_runtime::config_hub::ConfigHub::global().unwrap(),
                        project: Some(
                            atman_runtime::storage::ProjectContext::resolve(
                                &atman_runtime::config_hub::ConfigHub::global().unwrap(),
                                repository.path(),
                                state.data_dir(),
                            )
                            .unwrap(),
                        ),
                        home_dir: None,
                        workspace_generation: state.daemon_generation().to_owned(),
                    })
                    .await
                    .unwrap();

                let service = outcome
                    .executor
                    .tool_ctx
                    .flow_workspace_service
                    .as_ref()
                    .unwrap();
                let binding = service
                    .allocate(
                        atman_runtime::git_workspace::WorkspacePolicy::Auto,
                        "session",
                        "flow",
                        None,
                    )
                    .unwrap()
                    .unwrap();
                let manager =
                    atman_runtime::git_workspace::WorkspaceManager::at(repository.path(), None)
                        .unwrap();
                let record = manager.get(&binding.workspace_id).unwrap();
                assert_eq!(
                    record
                        .lease
                        .as_ref()
                        .map(|lease| lease.daemon_generation.as_str()),
                    Some("daemon-generation")
                );
            });
    }

    #[test]
    fn daemon_state_retains_provider_lifecycle_between_executors() {
        const PROVIDER_ID: &str = "launcher-root-oauth";

        struct CatalogCleanup;

        impl Drop for CatalogCleanup {
            fn drop(&mut self) {
                atman_runtime::model_registry::remove_provider_catalog(PROVIDER_ID);
            }
        }

        let _registry_lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        atman_runtime::model_registry::remove_provider_catalog(PROVIDER_ID);
        let _catalog_cleanup = CatalogCleanup;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let config = tempfile::tempdir().unwrap();
                let project = tempfile::tempdir().unwrap();
                let home = tempfile::tempdir().unwrap();
                let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(config.path());
                hub.add_auth_provider(atman_runtime::auth_store::StoredProvider {
                    id: PROVIDER_ID.into(),
                    name: "Launcher OAuth".into(),
                    kind: atman_runtime::auth_store::ProviderKind::Codex,
                    access_token: "expired-access".into(),
                    refresh_token: None,
                    expires_at: chrono::Utc::now().timestamp() - 3_600,
                    account: Some("account-id".into()),
                    enabled: true,
                    model_cache: Some(atman_runtime::auth_store::ModelCache {
                        fetched_at: 1,
                        models: vec![atman_runtime::auth_store::CachedModel {
                            slug: "gpt-launcher".into(),
                            context_budget: Some(128_000),
                            thinking: true,
                        }],
                    }),
                })
                .unwrap();
                hub.ensure_auth_model_namespace(PROVIDER_ID, "launcher@OAuth")
                    .unwrap();
                let state = DaemonState::new(project.path().join("data"));
                drop(state.provider_lifecycle_for(Some(config.path())).unwrap());
                let options = || atman_runtime::AtmanRuntimeOptions {
                    events: atman_runtime::event::EventSink::new(),
                    mock: false,
                    config: atman_runtime::config_hub::ConfigHub::from_config_dir(config.path()),
                    project: Some(
                        atman_runtime::storage::ProjectContext::resolve(
                            &atman_runtime::config_hub::ConfigHub::from_config_dir(config.path()),
                            project.path(),
                            &config.path().join("data"),
                        )
                        .unwrap(),
                    ),
                    home_dir: Some(home.path().to_path_buf()),
                    workspace_generation: "launcher-root-test".into(),
                };

                let first = atman_runtime::AtmanRuntime::build(options()).await.unwrap();
                assert!(first.executor.providers.contains(PROVIDER_ID));
                drop(first);
                assert!(
                    state
                        .provider_lifecycle_for(Some(config.path()))
                        .unwrap()
                        .provider_registry()
                        .contains(PROVIDER_ID)
                );

                hub.set_auth_provider_enabled(PROVIDER_ID, false).unwrap();
                let second = atman_runtime::AtmanRuntime::build(options()).await.unwrap();
                assert!(!second.executor.providers.contains(PROVIDER_ID));
                assert!(
                    !state
                        .provider_lifecycle_for(Some(config.path()))
                        .unwrap()
                        .provider_registry()
                        .contains(PROVIDER_ID)
                );
                assert!(
                    hub.models()
                        .model_entry("launcher@OAuth:gpt-launcher")
                        .is_none()
                );
                state
                    .provider_lifecycle_for(Some(config.path()))
                    .unwrap()
                    .remove_provider(PROVIDER_ID)
                    .unwrap();
            });
    }

    #[test]
    fn reload_model_config_refreshes_registry_and_ignores_invalid_toml() {
        let _lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[models.daemon-reload]\nmodel = \"provider/reloaded\"\ncontext_budget = 4242\n",
        )
        .unwrap();

        let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path());
        reload_model_config(Some(dir.path()));
        let configured = hub.models().model_info("daemon-reload");
        assert_eq!(configured.name, "daemon-reload");
        assert_eq!(configured.context_budget, 4242);

        std::fs::write(&config_path, "[models\n").unwrap();
        reload_model_config(Some(dir.path()));
        let preserved = hub.models().model_info("daemon-reload");
        assert_eq!(preserved.name, "daemon-reload");
        assert_eq!(preserved.context_budget, 4242);
    }

    #[test]
    fn run_options_isolate_reasoning() {
        let invocation_env = invocation_env_from_reasoning(Some("high@pro".into())).unwrap();
        assert!(matches!(
            invocation_env.get("effort"),
            Some(Value::Str(value)) if value == "high@pro"
        ));

        let next_invocation_env = invocation_env_from_reasoning(None).unwrap();
        assert!(next_invocation_env.get("effort").is_none());
    }
}
