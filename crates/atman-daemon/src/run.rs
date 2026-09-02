use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId as ProtoRunId, SessionId as ProtoSessionId};

use atman_runtime::event::FlowRunId as RuntimeRunId;

use crate::state::{DaemonState, LiveRun};

fn render_value(v: &atman_runtime::Value) -> String {
    match v {
        atman_runtime::Value::Str(s) => s.clone(),
        atman_runtime::Value::Int(n) => n.to_string(),
        atman_runtime::Value::Float(n) => n.to_string(),
        atman_runtime::Value::Bool(b) => b.to_string(),
        atman_runtime::Value::Unit => String::new(),
        other => format!("{other:?}"),
    }
}

pub struct RunLauncher {
    pub project_root: PathBuf,
    pub config_dir: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
}

pub struct SpawnedRun {
    pub session_id: ProtoSessionId,
    pub run_id: ProtoRunId,
}

#[derive(Clone)]
struct ProviderCatalogRefreshDispatcher {
    runtime: tokio::runtime::Handle,
    lifecycle: atman_runtime::ProviderLifecycle,
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
    pub reasoning: Option<String>,
    pub images: Vec<atman_proto::InlineImage>,
}

fn invocation_env_from_reasoning(
    reasoning: Option<String>,
) -> Result<atman_runtime::InvocationEnv> {
    match reasoning {
        Some(reasoning) => {
            let selection: atman_runtime::provider::ReasoningSelection = reasoning
                .parse()
                .map_err(|error: String| anyhow::anyhow!("invalid reasoning: {error}"))?;
            Ok(atman_runtime::InvocationEnv::single(
                "effort",
                atman_runtime::Value::Str(selection.to_string()),
            ))
        }
        None => Ok(atman_runtime::InvocationEnv::default()),
    }
}

fn queue_run_images(
    session: &atman_runtime::Session,
    images: Vec<atman_proto::InlineImage>,
) -> Result<()> {
    for image in images {
        session.queue_image_base64(&image.data_base64, image.name.as_deref())?;
    }
    Ok(())
}

struct RegistryCleanup {
    state: Arc<DaemonState>,
    session_id: ProtoSessionId,
    run_id: ProtoRunId,
}

fn spawn_with_provider_lifecycle(
    builder: std::thread::Builder,
    provider_lifecycle: atman_runtime::ProviderLifecycle,
    body: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    builder.spawn(move || {
        let provider_lifecycle_owner = provider_lifecycle;
        body();
        drop(provider_lifecycle_owner);
    })
}

impl Drop for RegistryCleanup {
    fn drop(&mut self) {
        self.state.finish_run(&self.session_id, &self.run_id);
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
        let plan = crate::bootstrap::prepare_auth_provider_runtime(&lifecycle).await?;
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
        args: Vec<(String, atman_runtime::Value)>,
    ) -> Result<SpawnedRun> {
        self.spawn_as(state, flow_path, args, "local-daemon").await
    }

    pub async fn spawn_as(
        &self,
        state: Arc<DaemonState>,
        flow_path: &str,
        args: Vec<(String, atman_runtime::Value)>,
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
        args: Vec<(String, atman_runtime::Value)>,
        owner_principal: &str,
        options: RunOptions,
    ) -> Result<SpawnedRun> {
        let path = PathBuf::from(flow_path);
        std::fs::metadata(&path).with_context(|| format!("stat flow {}", path.display()))?;
        let RunOptions { reasoning, images } = options;
        let invocation_env = invocation_env_from_reasoning(reasoning)?;

        reload_model_config(self.config_dir.as_deref());
        let provider_lifecycle = state.provider_lifecycle_for(self.config_dir.as_deref())?;
        let provider_catalog_refresh = ProviderCatalogRefreshDispatcher {
            runtime: tokio::runtime::Handle::current(),
            lifecycle: provider_lifecycle.clone(),
        };

        let redactor = crate::bootstrap::build_redactor(self.config_dir.as_deref());
        let hub = match &self.config_dir {
            Some(dir) => atman_runtime::config_hub::ConfigHub::from_config_dir(dir),
            None => atman_runtime::config_hub::ConfigHub::global()
                .map_err(|error| anyhow::anyhow!("resolve config hub: {error}"))?,
        };
        let scope_root = atman_runtime::storage::resolve_project_scope_with(
            &hub,
            &self.project_root,
            state.data_dir(),
        )?;
        let project_index = match atman_runtime::index::AnchorIndex::open_project(&scope_root) {
            Ok(idx) => Some(std::sync::Arc::new(idx)),
            Err(e) => {
                atman_runtime::notify!(
                    warn,
                    "project index unavailable at {}: {e}",
                    scope_root.display()
                );
                None
            }
        };
        let trust = hub.trust_config().context("load global trust config")?;
        let session = std::sync::Arc::new(
            atman_runtime::Session::open_with_context_and_trust(
                state.data_dir(),
                redactor,
                project_index,
                trust,
            )
            .with_context(|| format!("opening session under {}", state.data_dir().display()))?,
        );
        queue_run_images(&session, images)?;
        let sid_proto = ProtoSessionId(session.id().0);
        let run_id_runtime = RuntimeRunId::now();
        let run_id_proto = ProtoRunId(run_id_runtime.0);

        let cancel = session.flow_cancel_token();
        state.register_session_run(
            sid_proto.clone(),
            session.clone(),
            LiveRun {
                run_id: run_id_proto.clone(),
                flow_name: String::new(),
                cancel,
                started_at: chrono::Utc::now(),
            },
            owner_principal,
        )?;

        let project_root = self.project_root.clone();
        let config_dir = self.config_dir.clone();
        let home_dir = self.home_dir.clone();
        let state_for_task = state.clone();
        let sid_for_task = sid_proto.clone();
        let run_id_for_task = run_id_proto.clone();

        let spawn_result = spawn_with_provider_lifecycle(
            std::thread::Builder::new().name(format!("atman-run-{}", sid_proto)),
            provider_lifecycle,
            move || {
                let _cleanup = RegistryCleanup {
                    state: state_for_task.clone(),
                    session_id: sid_for_task.clone(),
                    run_id: run_id_for_task,
                };
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(error) => {
                        atman_runtime::notify!(error, "build flow runtime failed: {error:#}");
                        return;
                    }
                };
                let state_for_run = state_for_task.clone();
                rt.block_on(async move {
                    if let Err(e) = run_flow_inner(
                        session.clone(),
                        &path,
                        args,
                        run_id_runtime,
                        project_root,
                        scope_root,
                        config_dir,
                        home_dir,
                        Some(state_for_run),
                        provider_catalog_refresh,
                        invocation_env,
                    )
                    .await
                    {
                        atman_runtime::notify!(error, "flow run failed: {e:#}");
                    }
                    session.flush_writer().await;
                });
            },
        );
        if let Err(error) = spawn_result {
            state.remove_session(&sid_proto);
            return Err(error).context("spawn run thread");
        }

        Ok(SpawnedRun {
            session_id: sid_proto,
            run_id: run_id_proto,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_flow_inner(
    session: std::sync::Arc<atman_runtime::Session>,
    path: &std::path::Path,
    args: Vec<(String, atman_runtime::Value)>,
    run_id: RuntimeRunId,
    project_root: PathBuf,
    scope_root: PathBuf,
    config_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    daemon_state: Option<Arc<crate::DaemonState>>,
    provider_catalog_refresh: ProviderCatalogRefreshDispatcher,
    invocation_env: atman_runtime::InvocationEnv,
) -> Result<()> {
    if path_is_managed_agent_at(path, config_dir.as_deref()) {
        if let Some(dir) = &config_dir {
            atman_runtime::templates::ensure_managed_agent_at(dir)?;
        }
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("reading flow {}", path.display()))?;
    let parsed = atman_dsl::parse::parse_file(&source)
        .with_context(|| format!("parsing {}", path.display()))?;
    if parsed.flows.is_empty() {
        anyhow::bail!("{} contains no flows", path.display());
    }
    let flow_name = parsed.flows[0].name.name.clone();

    let workspace_generation = daemon_state
        .as_ref()
        .map(|state| state.daemon_generation().to_owned())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let outcome = crate::bootstrap::build_executor(crate::bootstrap::BootstrapOptions {
        events: session.sink().clone(),
        mock: false,
        config_dir: config_dir.clone(),
        project_root: project_root.clone(),
        home_dir,
        workspace_generation,
    })
    .await?;
    provider_catalog_refresh.dispatch(outcome.provider_catalog_refresh_plan);
    let mut executor = outcome.executor;
    executor.source_dir = path.parent().map(|p| p.to_path_buf());

    let lifecycles = match &config_dir {
        Some(c) => atman_runtime::lifecycle::LifecycleRunner::from_dir(c),
        None => atman_runtime::lifecycle::LifecycleRunner::new(),
    };

    let redactor = crate::bootstrap::build_redactor(config_dir.as_deref());
    crate::bootstrap::attach_memory_stores_with_redactor(
        &mut executor,
        session.dir(),
        &scope_root,
        redactor,
        session.project_index(),
        session.goal_watch().clone(),
        session.todos_watch().clone(),
        session.plans_watch().clone(),
    );
    if let Some(state) = daemon_state {
        executor.tool_ctx.prompt_resolver =
            Some(Arc::new(crate::prompt_bridge::DaemonPromptResolver {
                state,
                sink: session.sink().clone(),
            }));
    }
    let (lifecycle_tx, mut lifecycle_rx) =
        tokio::sync::mpsc::unbounded_channel::<atman_dsl::ast::LifecycleEvent>();
    executor.tool_ctx.lifecycle_fire_tx = Some(lifecycle_tx);

    let target_flow = parsed
        .flows
        .iter()
        .find(|f| f.name.name == flow_name)
        .ok_or_else(|| anyhow::anyhow!("flow `{flow_name}` not found in {}", path.display()))?;
    if let Err(errs) = atman_runtime::validate::validate(target_flow, &executor.tools) {
        anyhow::bail!(
            "flow validation failed: {}",
            errs.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionStart)
        .await;

    let turn_id = atman_runtime::event::TurnId::now();
    let user_text = if args.is_empty() {
        flow_name.clone()
    } else {
        args.iter()
            .map(|(k, v)| format!("{k}={}", render_value(v)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut parts: Vec<atman_runtime::message::MessagePart> = session
        .take_pending_images()
        .into_iter()
        .map(|source| atman_runtime::message::MessagePart::Image { source })
        .collect();
    parts.push(atman_runtime::message::MessagePart::Text {
        text: user_text.clone(),
    });
    let user_msg = atman_runtime::message::Message {
        role: atman_runtime::message::MessageRole::User,
        parts,
        turn_id: turn_id.clone(),
        origin: atman_runtime::message::MessageOrigin::User,
    };
    {
        let _compact_guard = session.acquire_compact_lock().await;
        session.begin_turn(user_msg);
    }
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::TurnStart)
        .await;
    let result = executor
        .run_with_invocation(
            &parsed,
            &flow_name,
            args,
            atman_runtime::RootInvocation {
                turn_id: Some(turn_id),
                session: Some(session.clone()),
                first_run_id: Some(run_id),
                env: invocation_env,
            },
        )
        .await;
    while let Ok(ev) = lifecycle_rx.try_recv() {
        lifecycles.fire(&executor, ev).await;
    }
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::TurnEnd)
        .await;
    session.end_turn();
    if result.is_ok() && session.record_successful_flow().is_some() {
        let _ =
            atman_runtime::session_naming::maybe_generate_session_name(&executor, &session).await;
    }
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionEnd)
        .await;
    Ok(())
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

fn path_is_managed_agent_at(path: &Path, config_dir: Option<&Path>) -> bool {
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
    fn registry_cleanup_guard_finishes_run_during_unwind() {
        let state = Arc::new(DaemonState::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let session_id = ProtoSessionId(session.id().0);
        let run_id = ProtoRunId(uuid::Uuid::now_v7());
        state
            .register_session_run(
                session_id.clone(),
                session.clone(),
                LiveRun {
                    run_id: run_id.clone(),
                    flow_name: "panic-test".into(),
                    cancel: tokio_util::sync::CancellationToken::new(),
                    started_at: chrono::Utc::now(),
                },
                "test-principal",
            )
            .unwrap();
        assert!(
            state
                .authorized_live_session(&session_id, "test-principal")
                .is_some()
        );

        let unwind = std::panic::catch_unwind({
            let state = Arc::clone(&state);
            let session_id = session_id.clone();
            let run_id = run_id.clone();
            move || {
                let _cleanup = RegistryCleanup {
                    state,
                    session_id,
                    run_id,
                };
                panic!("simulate flow-thread panic");
            }
        });
        assert!(unwind.is_err());
        assert!(state.live_session(&session_id).is_none());
        assert!(state.can_read_session(&session_id, "test-principal"));
        assert!(Arc::ptr_eq(
            &state
                .authorized_session(&session_id, "test-principal")
                .unwrap(),
            &session
        ));
        assert!(
            state
                .authorized_live_session(&session_id, "test-principal")
                .is_none()
        );
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
    fn spawned_thread_owns_provider_lifecycle_until_body_finishes() {
        const PROVIDER_ID: &str = "spawn-owner-oauth";

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
        let config = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let provider = Arc::new(atman_runtime::providers::mock::MockProvider::new(
            PROVIDER_ID,
        ));
        let weak_provider = Arc::downgrade(&provider);
        let state = Arc::new(DaemonState::new(project.path().join("data")));
        let launcher = RunLauncher::new(
            project.path().to_path_buf(),
            Some(config.path().to_path_buf()),
            None,
        )
        .unwrap();
        let provider_lifecycle = state.provider_lifecycle_for(Some(config.path())).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                provider_lifecycle
                    .install_pre_discovered_provider(
                        atman_runtime::auth_store::StoredProvider {
                            id: PROVIDER_ID.into(),
                            name: "Spawn Owner".into(),
                            kind: atman_runtime::auth_store::ProviderKind::Codex,
                            access_token: "access".into(),
                            refresh_token: None,
                            expires_at: i64::MAX,
                            account: None,
                            enabled: true,
                            model_cache: None,
                        },
                        provider.clone(),
                        vec![atman_runtime::provider::DiscoveredModelDetails {
                            slug: "spawn-owner-model".into(),
                            context_budget: Some(128_000),
                            capability_knowledge:
                                atman_runtime::provider::CapabilityKnowledge::Legacy {
                                    thinking: true,
                                },
                        }],
                    )
                    .await
                    .unwrap();
            });
        drop(provider);

        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let handle = spawn_with_provider_lifecycle(
            std::thread::Builder::new().name("provider-owner-test".into()),
            provider_lifecycle,
            move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            },
        )
        .unwrap();
        drop(launcher);
        drop(state);
        entered_rx.recv().unwrap();

        assert!(weak_provider.upgrade().is_some());
        release_tx.send(()).unwrap();
        handle.join().unwrap();
        assert!(weak_provider.upgrade().is_none());
    }

    #[test]
    fn bootstrap_workspace_service_uses_daemon_generation() {
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
                    crate::bootstrap::build_executor(crate::bootstrap::BootstrapOptions {
                        events: atman_runtime::event::EventSink::new(),
                        mock: true,
                        config_dir: None,
                        project_root: repository.path().to_path_buf(),
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
                let options = || crate::bootstrap::BootstrapOptions {
                    events: atman_runtime::event::EventSink::new(),
                    mock: false,
                    config_dir: Some(config.path().to_path_buf()),
                    project_root: project.path().to_path_buf(),
                    home_dir: Some(home.path().to_path_buf()),
                    workspace_generation: "launcher-root-test".into(),
                };

                let first = crate::bootstrap::build_executor(options()).await.unwrap();
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
                let second = crate::bootstrap::build_executor(options()).await.unwrap();
                assert!(!second.executor.providers.contains(PROVIDER_ID));
                assert!(
                    !state
                        .provider_lifecycle_for(Some(config.path()))
                        .unwrap()
                        .provider_registry()
                        .contains(PROVIDER_ID)
                );
                assert!(
                    atman_runtime::model_registry::model_entry("launcher@OAuth:gpt-launcher")
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

        reload_model_config(Some(dir.path()));
        let configured = atman_runtime::model_registry::model_info("daemon-reload");
        assert_eq!(configured.name, "daemon-reload");
        assert_eq!(configured.context_budget, 4242);

        std::fs::write(&config_path, "[models\n").unwrap();
        reload_model_config(Some(dir.path()));
        let preserved = atman_runtime::model_registry::model_info("daemon-reload");
        assert_eq!(preserved.name, "daemon-reload");
        assert_eq!(preserved.context_budget, 4242);
    }

    #[test]
    fn run_options_isolate_reasoning_and_queue_image_inputs() {
        let session = atman_runtime::Session::open_ephemeral();
        let invocation_env = invocation_env_from_reasoning(Some("high@pro".into())).unwrap();
        queue_run_images(
            &session,
            vec![atman_proto::InlineImage {
                data_base64: "iVBORw0KGgo=".into(),
                name: Some("input.png".into()),
            }],
        )
        .unwrap();

        assert!(matches!(
            invocation_env.get("effort"),
            Some(atman_runtime::Value::Str(value)) if value == "high@pro"
        ));
        assert_eq!(session.pending_image_count(), 1);

        let next_invocation_env = invocation_env_from_reasoning(None).unwrap();
        assert!(next_invocation_env.get("effort").is_none());
    }
}
