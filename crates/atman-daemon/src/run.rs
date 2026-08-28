use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId as ProtoRunId, SessionId as ProtoSessionId};

use atman_runtime::event::FlowRunId as RuntimeRunId;

use crate::state::{DaemonState, LiveSession};

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
    // Runs on a dedicated blocking thread + current-thread runtime because
    // atman_dsl::ast::File and Executor are !Send (proc-macro2 spans hold Rc<()>).
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
        let path = PathBuf::from(flow_path);
        std::fs::metadata(&path).with_context(|| format!("stat flow {}", path.display()))?;

        reload_model_config(self.config_dir.as_deref());

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
        let sid_proto = ProtoSessionId(session.id().0);
        let run_id_runtime = RuntimeRunId::now();
        let run_id_proto = ProtoRunId(run_id_runtime.0);

        let cancel = session.flow_cancel_token();
        state.register_broker(sid_proto.clone(), session.clone(), owner_principal);
        state.register_live(
            sid_proto.clone(),
            LiveSession {
                run_id: run_id_proto.clone(),
                flow_name: String::new(),
                cancel,
                started_at: chrono::Utc::now(),
            },
        );

        let project_root = self.project_root.clone();
        let config_dir = self.config_dir.clone();
        let home_dir = self.home_dir.clone();
        let state_for_task = state.clone();
        let sid_for_task = sid_proto.clone();

        let spawn_result = std::thread::Builder::new()
            .name(format!("atman-run-{}", sid_proto))
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(error) => {
                        atman_runtime::notify!(error, "build flow runtime failed: {error:#}");
                        state_for_task.deregister_broker(&sid_for_task);
                        state_for_task.deregister_live(&sid_for_task);
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
                    )
                    .await
                    {
                        atman_runtime::notify!(error, "flow run failed: {e:#}");
                    }
                    state_for_task.deregister_broker(&sid_for_task);
                    match std::sync::Arc::try_unwrap(session) {
                        Ok(s) => s.shutdown().await,
                        Err(_) => atman_runtime::notify!(
                            warn,
                            location = Log,
                            stack = dedupe("session.refs_at_shutdown", 60_000),
                            "session still had refs at shutdown"
                        ),
                    }
                    state_for_task.deregister_live(&sid_for_task);
                });
            });
        if let Err(error) = spawn_result {
            state.deregister_broker(&sid_proto);
            state.deregister_live(&sid_proto);
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
    let user_msg = atman_runtime::message::Message::user_text(turn_id.clone(), user_text.clone());
    {
        let _compact_guard = session.acquire_compact_lock().await;
        session.begin_turn(user_msg);
    }
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::TurnStart)
        .await;
    let result = executor
        .run_in_turn_with_run_id(
            &parsed,
            &flow_name,
            args,
            Some(turn_id),
            Some(session.clone()),
            Some(run_id),
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
    use std::sync::Mutex;

    static MODEL_CONFIG_LOCK: Mutex<()> = Mutex::new(());

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

    #[tokio::test]
    async fn bootstrap_workspace_service_uses_daemon_generation() {
        let repository = repo();
        let state = DaemonState::new_with_generation(
            repository.path().join("data"),
            "daemon-generation".into(),
        );
        let outcome = crate::bootstrap::build_executor(crate::bootstrap::BootstrapOptions {
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
            atman_runtime::git_workspace::WorkspaceManager::at(repository.path(), None).unwrap();
        let record = manager.get(&binding.workspace_id).unwrap();
        assert_eq!(
            record
                .lease
                .as_ref()
                .map(|lease| lease.daemon_generation.as_str()),
            Some("daemon-generation")
        );
    }

    #[test]
    fn reload_model_config_refreshes_registry_and_ignores_invalid_toml() {
        let _lock = MODEL_CONFIG_LOCK.lock().unwrap();
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
}
