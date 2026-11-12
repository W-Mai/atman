use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use atman_proto::flows::{
    FlowRequest, InvokeFlowRequest, LaunchFlowRequest, ReadFlowOutputDetailRequest,
    ReadFlowOutputRequest,
};
use atman_runtime::config_hub::ConfigHub;
use atman_runtime::event::EventSink;
use atman_runtime::flow_authority::{EffectiveAuthority, contract_allows_shell};
use atman_runtime::flow_runtime::catalog::SessionCatalog;
use atman_runtime::flow_runtime::launch::EntryExecution;
use atman_runtime::flow_store::inbox::{
    DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity,
};
use atman_runtime::flow_store::launch::{EntryBinding, EntryRequest, EntryStart, LaunchInput};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, SessionConfiguration, SessionDescriptor,
};
use atman_runtime::source_program::{load_program, load_program_from_bundle};
use atman_runtime::storage::ProjectContext;
use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, Executor};
use serde_json::Value;

use super::{OPERATOR, SessionService, address, core_address};
use crate::{DaemonState, prompt_bridge::DaemonPromptResolver};

const YIELD_INTERVAL: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

#[cfg(test)]
mod tests;

#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct ExecutorScope {
    session: uuid::Uuid,
    project: Option<ProjectContext>,
}

impl SessionService {
    async fn executor(
        &self,
        state: &Arc<DaemonState>,
        descriptor: &SessionDescriptor,
        project: Option<&ProjectContext>,
        home_dir: Option<PathBuf>,
    ) -> Result<Executor> {
        let session = self.host.session(descriptor.id)?;
        session.configure_executor(|| {
            let state = Arc::downgrade(state);
            let descriptor = descriptor.clone();
            Arc::new(move |project| {
                let state = state.clone();
                let descriptor = descriptor.clone();
                let home_dir = home_dir.clone();
                Box::pin(async move {
                    let state = state.upgrade().ok_or_else(|| {
                        std::io::Error::other("execution service owner is unavailable")
                    })?;
                    super::service(&state)
                        .cached_executor(&state, &descriptor, project.as_ref(), home_dir)
                        .await
                        .map_err(std::io::Error::other)
                })
            })
        })?;
        Ok(session.executor_for(project.cloned()).await?)
    }

    async fn cached_executor(
        &self,
        state: &Arc<DaemonState>,
        descriptor: &SessionDescriptor,
        project: Option<&ProjectContext>,
        home_dir: Option<PathBuf>,
    ) -> Result<Executor> {
        let slot = self
            .executors
            .lock()
            .unwrap()
            .entry(ExecutorScope {
                session: descriptor.id,
                project: project.cloned(),
            })
            .or_default()
            .clone();
        let executor = slot
            .get_or_try_init(|| async {
                let config = config(descriptor)?;
                let mut events = EventSink::new();
                if let Some(redactor) = crate::bootstrap::build_redactor(Some(config.config_dir()))
                {
                    events = events.with_redactor(redactor);
                }
                let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
                    events,
                    mock: false,
                    config,
                    project: project.cloned(),
                    home_dir,
                    workspace_generation: state.daemon_generation().to_owned(),
                })
                .await?;
                let mut executor = runtime.executor;
                executor.tool_ctx.prompt_resolver =
                    Some(Arc::new(DaemonPromptResolver::new(state)));
                crate::run::refresh_provider_catalogs(
                    &executor,
                    runtime.provider_catalog_refresh_plan,
                );
                Ok::<_, anyhow::Error>(executor)
            })
            .await?;
        Ok(executor.clone())
    }

    async fn prepare_entry(
        &self,
        state: &Arc<DaemonState>,
        session: &SessionDescriptor,
        descriptor: &FlowDescriptor,
        project: Option<&ProjectContext>,
        working_directory: Option<&Path>,
        home_dir: Option<PathBuf>,
    ) -> Result<(
        Executor,
        EffectiveAuthority,
        atman_runtime::trust::TrustConfig,
    )> {
        let source = descriptor.entry()?;
        let hub = config(session)?;
        let trust = hub.trust_config()?;
        let program = load_program_from_bundle(
            &source.bundle,
            &source.path,
            &session.source_roots_for(project),
        )?;
        let entry = program
            .entry_flow(&source.name)
            .and_then(|id| program.flow(&id))
            .context("entry flow is unavailable")?;
        let authority = EffectiveAuthority::root(
            &trust,
            contract_allows_shell(entry.contract.as_ref()),
            working_directory
                .map(Path::to_path_buf)
                .or_else(|| project.map(|project| project.root().to_path_buf())),
        );
        let executor = self.executor(state, session, project, home_dir).await?;
        if let Err(errors) = atman_runtime::validate::validate(entry, &executor.tools) {
            anyhow::bail!(
                "entry validation failed: {}",
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        Ok((executor, authority, trust))
    }

    pub(super) async fn bind(
        &self,
        state: &Arc<DaemonState>,
        request: FlowRequest,
    ) -> Result<Value> {
        let flow = core_address(request.flow);
        let session = self.session(flow.session_id.0)?;
        let snapshot = session.snapshot().await?;
        let worker = self.host.flow(&flow).await?;
        let descriptor = worker.descriptor().await?;
        let project = worker.project().await?;
        let (executor, authority, trust) = self
            .prepare_entry(
                state,
                &snapshot.catalog.descriptor,
                &descriptor,
                project.as_ref(),
                None,
                launcher_home(state),
            )
            .await?;
        let status = session
            .bind_entry(
                flow.instance_id.clone(),
                executor,
                authority,
                trust,
                YIELD_INTERVAL,
            )
            .await?;
        Ok(serde_json::to_value(
            atman_proto::responses::BindFlowResponse {
                flow: address(&flow),
                scheduler: super::scheduling(&status.borrow()),
            },
        )?)
    }

    pub(super) async fn launch(
        &self,
        state: &Arc<DaemonState>,
        request: LaunchFlowRequest,
    ) -> Result<Value> {
        let flow = core_address(request.flow.clone());
        let session = self.session(flow.session_id.0)?;
        let snapshot = session.snapshot().await?;
        let start = match request.start {
            atman_proto::flows::FlowStart::Invoke => EntryStart::Invoke {
                user: user_content(request.user.as_ref())?,
            },
            atman_proto::flows::FlowStart::Waiting => {
                ensure!(
                    request.user.is_none(),
                    "waiting activation cannot accept user input"
                );
                EntryStart::Waiting
            }
        };
        let entry_request = EntryRequest {
            request_id: request.request_id,
            recipient: flow.clone(),
            sender: SenderIdentity::User(OPERATOR),
            arguments: request.arguments.clone(),
            invocation: request.invocation.clone(),
            start,
        };
        let descriptor = if snapshot
            .catalog
            .flows
            .iter()
            .any(|row| row.id == flow.instance_id)
        {
            let worker = self.host.flow(&flow).await?;
            let descriptor = worker.descriptor().await?;
            let worker_project = worker.project().await?;
            let entry = descriptor.entry()?;
            ensure!(
                descriptor.creation_request == request.request_id
                    && entry.path == request.path
                    && entry.name == request.entry,
                "launch conflicts with the recorded entry"
            );
            if let Some(record) = worker.launch_record().await? {
                let expected_workspace = request
                    .working_directory
                    .as_deref()
                    .map(PathBuf::from)
                    .or_else(|| {
                        worker_project
                            .as_ref()
                            .map(|project| project.root().to_path_buf())
                    });
                ensure!(
                    record.input.request == entry_request
                        && record.input.initial_goal == request.initial_goal
                        && record
                            .input
                            .execution
                            .as_ref()
                            .and_then(|binding| binding.authority.workspace_root.clone())
                            == expected_workspace,
                    "launch conflicts with the recorded input"
                );
                return Ok(serde_json::to_value(super::wire::launch(
                    address(&flow),
                    &record,
                )?)?);
            }
            descriptor
        } else {
            ensure!(
                snapshot.cursor.sequence == request.expected_catalog_sequence,
                "session catalog changed; inspect its flows before launching"
            );
            let roots = snapshot
                .catalog
                .descriptor
                .source_roots_for(snapshot.catalog.project.as_ref());
            let path = request.path.clone();
            let entry = request.entry.clone();
            let loaded = tokio::task::spawn_blocking(move || {
                let program = load_program(Path::new(&path), &roots)?;
                ensure!(
                    program.entry_flow(&entry).is_some(),
                    "entry flow is unavailable"
                );
                Ok::<_, anyhow::Error>(EntrySnapshot {
                    path: path.into(),
                    name: entry,
                    bundle: program.revision_bundle(),
                })
            })
            .await??;
            FlowDescriptor {
                address: flow.clone(),
                creation_request: request.request_id,
                created_by: None,
                content: loaded.into(),
            }
        };
        let launched = self
            .activate_entry(
                state,
                snapshot,
                PreparedEntry {
                    descriptor,
                    request: entry_request,
                    initial_goal: request.initial_goal,
                    session_start: false,
                    working_directory: request.working_directory.map(PathBuf::from),
                },
                launcher_home(state),
            )
            .await?;
        Ok(serde_json::to_value(super::wire::launch(
            address(&flow),
            &launched.record,
        )?)?)
    }

    async fn activate_entry(
        &self,
        state: &Arc<DaemonState>,
        snapshot: atman_runtime::flow_runtime::host::SessionSnapshot,
        entry: PreparedEntry,
        home_dir: Option<PathBuf>,
    ) -> Result<atman_runtime::flow_runtime::launch::LaunchedFlow> {
        let PreparedEntry {
            descriptor,
            request,
            initial_goal,
            session_start,
            working_directory,
        } = entry;
        let project = snapshot
            .catalog
            .flows
            .iter()
            .find(|flow| flow.id == descriptor.address.instance_id)
            .map_or(snapshot.catalog.project.as_ref(), |flow| {
                flow.project.as_ref()
            });
        let (executor, authority, trust) = self
            .prepare_entry(
                state,
                &snapshot.catalog.descriptor,
                &descriptor,
                project,
                working_directory.as_deref(),
                home_dir,
            )
            .await?;
        let request_id = descriptor.creation_request;
        let input = LaunchInput {
            name: None,
            display_label: None,
            scope: None,
            workspace: None,
            initial_goal,
            execution: Some(EntryBinding {
                user_routes: false,
                session_start,
                first_turn: atman_rt::TurnId(request_id.into()),
                first_run: atman_rt::RunId(request_id.into()),
                authority: authority.clone(),
            }),
            request,
            inherited_outputs: vec![],
            continued_from: None,
            inherited_context: vec![],
            result_route: None,
        };
        Ok(self
            .host
            .launch_entry(
                descriptor,
                input,
                EntryExecution {
                    executor,
                    authority,
                    trust,
                    yield_interval: YIELD_INTERVAL,
                    catalog: snapshot.cursor,
                },
                |_, _| Ok(()),
            )
            .await?)
    }

    pub(super) async fn invoke(&self, request: InvokeFlowRequest) -> Result<Value> {
        let flow = core_address(request.flow);
        let worker = self.host.flow(&flow).await?;
        let acceptance = worker
            .accept_input(
                DeliveryRequest {
                    expected_head: request.expected_head.map(super::wire::core_head),
                    request_id: request.request_id,
                    sender: SenderIdentity::User(OPERATOR),
                    recipient: flow,
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Invocation {
                        user: user_content(request.user.as_ref())?,
                        arguments: request.arguments,
                        invocation: request.invocation,
                    },
                },
                |_| Ok(()),
            )
            .await?;
        Ok(serde_json::to_value(
            atman_proto::responses::InvokeFlowResponse {
                acceptance: super::wire::acceptance(&acceptance)?,
            },
        )?)
    }

    pub(super) async fn output(&self, request: ReadFlowOutputRequest) -> Result<Value> {
        let flow = core_address(request.flow);
        let page = SessionCatalog::new(Some(self.host.clone()), Some(self.data_dir.clone()))
            .output_page(
                &flow,
                request.after,
                NonZeroUsize::new(request.limit).expect("validated output page limit"),
                |_| Ok(()),
            )
            .await?;
        let response = atman_proto::responses::FlowOutputPage {
            messages: page
                .messages
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?,
            next: page.next,
            has_more: page.has_more,
        };
        Ok(serde_json::to_value(response)?)
    }

    pub(super) async fn output_detail(
        &self,
        request: ReadFlowOutputDetailRequest,
    ) -> Result<Value> {
        let flow = core_address(request.flow);
        let detail = SessionCatalog::new(Some(self.host.clone()), Some(self.data_dir.clone()))
            .output_detail(
                &flow,
                request.publication_id,
                request.expected_receipt.map(super::events::receipt),
                |_| Ok(()),
            )
            .await?;
        let response = atman_proto::responses::FlowOutputDetail {
            publication_id: detail.publication_id,
            receipt: super::events::cursor(detail.receipt.require_durable()?),
            has_artifact: !detail.artifacts.is_empty(),
            artifacts: detail
                .artifacts
                .into_iter()
                .map(|artifact| atman_proto::responses::FlowOutputArtifact {
                    id: artifact.id,
                    bytes: artifact.bytes,
                    digest: artifact.digest,
                })
                .collect(),
        };
        Ok(serde_json::to_value(response)?)
    }
}

fn user_content(
    input: Option<&atman_proto::flows::FlowUserContent>,
) -> Result<Option<atman_runtime::flow_store::inbox::UserContent>> {
    input
        .map(|input| {
            // Inline validated bytes live in the committed invocation, independent of client files.
            let attachments = atman_runtime::attachment_store::AttachmentStore::at(Path::new(""));
            let images = input
                .images
                .iter()
                .map(|image| attachments.import_base64(&image.data_base64, image.name.as_deref()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(atman_runtime::flow_store::inbox::UserContent {
                text: input.text.clone(),
                images,
                presentation: None,
            })
        })
        .transpose()
}

fn config(descriptor: &SessionDescriptor) -> Result<ConfigHub> {
    let SessionConfiguration::Atman(source) = &descriptor.configuration else {
        anyhow::bail!("session requires an explicit Atman configuration source to execute");
    };
    Ok(ConfigHub::from_source(source)?)
}

struct PreparedEntry {
    descriptor: FlowDescriptor,
    request: EntryRequest,
    initial_goal: Option<String>,
    session_start: bool,
    working_directory: Option<PathBuf>,
}

fn launcher_home(state: &DaemonState) -> Option<PathBuf> {
    state
        .launcher()
        .and_then(|launcher| launcher.home_dir.clone())
}

pub(crate) async fn launch_run(
    state: Arc<DaemonState>,
    launcher: crate::run::RunLauncher,
    path: PathBuf,
    args: Vec<(String, atman_runtime::Value)>,
    options: crate::run::RunOptions,
) -> Result<crate::run::SpawnedRun> {
    use atman_runtime::{
        ValueJson,
        flow_store::{FlowInstanceId, FlowRef},
    };
    use uuid::Uuid;
    ensure!(!state.is_stopping(), "daemon is stopping");
    let hub = crate::bootstrap::resolve_config_hub(launcher.config_dir.as_deref())?;
    let project = atman_runtime::storage::ProjectContext::resolve(
        &hub,
        &launcher.project_root,
        state.data_dir(),
    )?;
    let path = if path.is_absolute() {
        path
    } else {
        project.root().join(path)
    };
    if crate::run::path_is_managed_agent_at(&path, Some(hub.config_dir())) {
        atman_runtime::templates::ensure_managed_agent_at(hub.config_dir())?;
    }
    let mut session = SessionDescriptor {
        id: Uuid::now_v7(),
        title: String::new(),
        project: Some(project),
        configuration: SessionConfiguration::Atman(hub.source()),
    };
    let roots = session.source_roots();
    let source_path = path.clone();
    let program = tokio::task::spawn_blocking(move || load_program(&source_path, &roots)).await??;
    let flow = program
        .entry_file()
        .flows
        .first()
        .context("source contains no flows")?;
    let initial_goal = if options.goal {
        ensure!(
            atman_runtime::goal::is_managed(flow),
            "goal.run requires a managed agent invocation contract"
        );
        let parameter = atman_runtime::validate::invocation_message_parameter(flow)?
            .context("goal.run requires an invocation user_message parameter")?;
        Some(
            args.iter()
                .find_map(|(name, value)| match value {
                    atman_runtime::Value::Str(text)
                        if name == parameter && !text.trim().is_empty() =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .with_context(|| format!("goal.run requires a non-empty {parameter} argument"))?,
        )
    } else {
        None
    };
    session.title = flow.name.name.clone();
    let text = if args.is_empty() {
        session.title.clone()
    } else {
        args.iter()
            .map(|(name, value)| format!("{name}={}", crate::run::render_value(value)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut arguments = std::collections::BTreeMap::new();
    for (name, value) in args {
        ensure!(
            arguments.insert(name.clone(), value.to_json()?).is_none(),
            "duplicate argument: {name}"
        );
    }
    let env = crate::run::invocation_env_from_reasoning(options.reasoning)?;
    let invocation = env
        .values()
        .map(|(name, value)| Ok((name.to_owned(), value.to_json()?)))
        .collect::<Result<_, atman_runtime::RuntimeError>>()?;
    let user = user_content(Some(&atman_proto::flows::FlowUserContent {
        text,
        images: options.images,
    }))?;
    let descriptor = FlowDescriptor {
        address: FlowRef {
            session_id: session.id.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::now_v7(),
        created_by: None,
        content: EntrySnapshot {
            name: session.title.clone(),
            path,
            bundle: program.revision_bundle(),
        }
        .into(),
    };
    let session_id = atman_proto::SessionId(session.id);
    let instance_id = descriptor.address.instance_id.0.0;
    let run_id = atman_proto::FlowRunId(descriptor.creation_request);
    let service = super::service_with_config(&state, Some(hub.config_dir()));
    let handle = service.attach(session.id, Some(session)).await?;
    let launched = service
        .activate_entry(
            &state,
            handle.snapshot().await?,
            PreparedEntry {
                request: EntryRequest {
                    request_id: descriptor.creation_request,
                    sender: SenderIdentity::User(OPERATOR),
                    recipient: descriptor.address.clone(),
                    arguments,
                    invocation,
                    start: EntryStart::Invoke { user },
                },
                descriptor,
                initial_goal,
                session_start: true,
                working_directory: None,
            },
            launcher.home_dir,
        )
        .await?;
    let goal_id = options
        .goal
        .then(|| {
            launched
                .record
                .acceptance
                .as_ref()
                .context("initial invocation has no acceptance")
                .map(|accepted| accepted.message_id.to_string())
        })
        .transpose()?;
    Ok(crate::run::SpawnedRun {
        session_id,
        instance_id,
        run_id,
        goal_id,
    })
}
