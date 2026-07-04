use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_proto::{FlowRunId as ProtoRunId, SessionId as ProtoSessionId};
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

        let session = atman_runtime::Session::open(state.data_dir())
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

    let lifecycles = match &config_dir {
        Some(c) => atman_runtime::lifecycle::LifecycleRunner::from_dir(c),
        None => atman_runtime::lifecycle::LifecycleRunner::new(),
    };

    let confession_root = project_root.join(".atman");
    let _ = std::fs::create_dir_all(&confession_root);
    let spec_root = session
        .dir()
        .parent()
        .unwrap_or(session.dir())
        .join("specs");
    let _ = std::fs::create_dir_all(&spec_root);
    crate::bootstrap::attach_memory_stores(
        &mut executor,
        session.dir(),
        &confession_root,
        &spec_root,
    );
    if let Some(state) = daemon_state {
        executor.tool_ctx.prompt_resolver =
            Some(Arc::new(crate::prompt_bridge::DaemonPromptResolver {
                state,
                sink: session.sink().clone(),
            }));
    }

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
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::TurnEnd)
        .await;
    session.end_turn();
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionEnd)
        .await;
    Ok(())
}
