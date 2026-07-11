use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId as ProtoRunId, SessionId as ProtoSessionId};

fn parse_model_config(text: &str) -> Option<atman_runtime::model_registry::ModelConfig> {
    use atman_runtime::model_registry::{AliasEntry, ModelConfig, ModelEntry};
    #[derive(serde::Deserialize, Default)]
    struct RawModel {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        context_budget: Option<u64>,
        #[serde(default)]
        compact_threshold_ratio: Option<f64>,
        #[serde(default)]
        thinking: Option<bool>,
        #[serde(default)]
        max_tokens: Option<u32>,
    }
    #[derive(serde::Deserialize, Default)]
    struct RawAlias {
        model: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct RawFile {
        #[serde(default)]
        models: std::collections::HashMap<String, RawModel>,
        #[serde(default)]
        alias: std::collections::HashMap<String, RawAlias>,
    }
    let raw: RawFile = toml::from_str(text).ok()?;
    let mut cfg = ModelConfig::default();
    for (name, m) in raw.models {
        cfg.models.insert(
            name,
            ModelEntry {
                model: m.model.unwrap_or_default(),
                provider: m.provider,
                api_key: m.api_key,
                base_url: m.base_url,
                context_budget: m.context_budget,
                compact_threshold_ratio: m.compact_threshold_ratio,
                thinking: m.thinking,
                max_tokens: m.max_tokens,
            },
        );
    }
    for (name, a) in raw.alias {
        cfg.aliases.insert(name, AliasEntry { model: a.model });
    }
    if cfg.models.is_empty() && cfg.aliases.is_empty() {
        return None;
    }
    Some(cfg)
}
use atman_runtime::event::FlowRunId as RuntimeRunId;

use crate::state::{DaemonState, LiveSession};

pub struct RunLauncher {
    pub project_root: PathBuf,
    pub config_dir: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
}

pub struct SpawnedRun {
    pub session_id: ProtoSessionId,
    pub run_id: ProtoRunId,
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
        let path = PathBuf::from(flow_path);
        std::fs::metadata(&path).with_context(|| format!("stat flow {}", path.display()))?;

        let redactor = crate::bootstrap::build_redactor(self.config_dir.as_deref());
        let project_index =
            match atman_runtime::storage::resolve_project_scope_for(&self.project_root) {
                Ok(scope) => match atman_runtime::index::AnchorIndex::open_project(&scope) {
                    Ok(idx) => Some(std::sync::Arc::new(idx)),
                    Err(e) => {
                        eprintln!(
                            "[atman-daemon] project index unavailable at {}: {e}",
                            scope.display()
                        );
                        None
                    }
                },
                Err(e) => {
                    eprintln!("[atman-daemon] resolve project scope failed: {e}");
                    None
                }
            };
        let session =
            atman_runtime::Session::open_with_context(state.data_dir(), redactor, project_index)
                .with_context(|| format!("opening session under {}", state.data_dir().display()))?;
        let sid_proto = ProtoSessionId(session.id().0);
        let run_id_runtime = RuntimeRunId::now();
        let run_id_proto = ProtoRunId(run_id_runtime.0);

        let cancel = session.flow_cancel_token();
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

        std::thread::Builder::new()
            .name(format!("atman-run-{}", sid_proto))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build current-thread runtime");
                let state_for_run = state_for_task.clone();
                rt.block_on(async move {
                    if let Err(e) = run_flow_inner(
                        &session,
                        &path,
                        args,
                        run_id_runtime,
                        project_root,
                        config_dir,
                        home_dir,
                        Some(state_for_run),
                    )
                    .await
                    {
                        eprintln!("[atman-daemon] flow run failed: {e:#}");
                    }
                    session.shutdown().await;
                    state_for_task.deregister_live(&sid_for_task);
                });
            })
            .context("spawn run thread")?;

        Ok(SpawnedRun {
            session_id: sid_proto,
            run_id: run_id_proto,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_flow_inner(
    session: &atman_runtime::Session,
    path: &std::path::Path,
    args: Vec<(String, atman_runtime::Value)>,
    run_id: RuntimeRunId,
    project_root: PathBuf,
    config_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    daemon_state: Option<Arc<crate::DaemonState>>,
) -> Result<()> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("reading flow {}", path.display()))?;
    let parsed = atman_dsl::parse::parse_file(&source)
        .with_context(|| format!("parsing {}", path.display()))?;
    if parsed.flows.is_empty() {
        anyhow::bail!("{} contains no flows", path.display());
    }
    let flow_name = parsed.flows[0].name.name.clone();

    let outcome = crate::bootstrap::build_executor(crate::bootstrap::BootstrapOptions {
        events: session.sink().clone(),
        mock: false,
        config_dir: config_dir.clone(),
        project_root: project_root.clone(),
        home_dir,
    })
    .await?;
    let mut executor = outcome.executor;

    if let Some(dir) = &config_dir {
        if let Ok(text) = std::fs::read_to_string(dir.join("config.toml")) {
            if let Some(mc) = parse_model_config(&text) {
                atman_runtime::model_registry::set_model_config(mc);
            }
        }
    }

    let lifecycles = match &config_dir {
        Some(c) => atman_runtime::lifecycle::LifecycleRunner::from_dir(c),
        None => atman_runtime::lifecycle::LifecycleRunner::new(),
    };

    let scope_root = atman_runtime::storage::resolve_project_scope_for(&project_root)
        .unwrap_or_else(|_| project_root.join(".atman"));
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
    let user_msg = atman_runtime::message::Message::user_text(
        turn_id.clone(),
        format!("daemon run {} flow={flow_name}", path.display()),
    );
    session.begin_turn(user_msg);
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::TurnStart)
        .await;
    let _result = executor
        .run_in_turn_with_run_id(
            &parsed,
            &flow_name,
            args,
            Some(turn_id),
            Some(session),
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
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionEnd)
        .await;
    Ok(())
}
