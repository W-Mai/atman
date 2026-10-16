use std::{num::NonZeroUsize, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use atman_proto::flows::{
    FlowRequest, InvokeFlowRequest, LaunchFlowRequest, ReadFlowOutputRequest,
};
use atman_runtime::config_hub::ConfigHub;
use atman_runtime::event::EventSink;
use atman_runtime::flow_authority::{EffectiveAuthority, contract_allows_shell};
use atman_runtime::flow_runtime::launch::EntryExecution;
use atman_runtime::flow_store::inbox::{
    DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity,
};
use atman_runtime::flow_store::launch::{EntryBinding, LaunchInput};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, SessionConfiguration, SessionDescriptor,
};
use atman_runtime::source_program::{load_program, load_program_from_bundle};
use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, Executor};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{SessionService, address, core_address};
use crate::{DaemonState, prompt_bridge::DaemonPromptResolver};

// Transport authentication identifies this operator; callers cannot supply sender provenance.
const OPERATOR: Uuid = Uuid::from_bytes(*b"atman-operator!!");
const YIELD_INTERVAL: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

impl SessionService {
    async fn executor(
        &self,
        state: &Arc<DaemonState>,
        descriptor: &SessionDescriptor,
    ) -> Result<Executor> {
        let slot = self
            .executors
            .lock()
            .unwrap()
            .entry(descriptor.id)
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
                    project: descriptor.project.clone(),
                    home_dir: state
                        .launcher()
                        .and_then(|launcher| launcher.home_dir.clone()),
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
    ) -> Result<(
        Executor,
        EffectiveAuthority,
        atman_runtime::trust::TrustConfig,
    )> {
        let source = descriptor.entry()?;
        let hub = config(session)?;
        let trust = hub.trust_config()?;
        let program =
            load_program_from_bundle(&source.bundle, &source.path, &session.source_roots())?;
        let entry = program
            .entry_flow(&source.name)
            .and_then(|id| program.flow(&id))
            .context("entry flow is unavailable")?;
        let authority = EffectiveAuthority::root(
            &trust,
            contract_allows_shell(entry.contract.as_ref()),
            session
                .project
                .as_ref()
                .map(|project| project.root().to_path_buf()),
        );
        let executor = self.executor(state, session).await?;
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
        let (executor, authority, trust) = self
            .prepare_entry(state, &snapshot.catalog.descriptor, &descriptor)
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
        Ok(json!({"flow": address(&flow), "scheduler": super::scheduling(&status.borrow())}))
    }

    pub(super) async fn launch(
        &self,
        state: &Arc<DaemonState>,
        request: LaunchFlowRequest,
    ) -> Result<Value> {
        let flow = core_address(request.flow.clone());
        let session = self.session(flow.session_id.0)?;
        let snapshot = session.snapshot().await?;
        let payload = InboxPayload::Invocation {
            user: user_content(request.user.as_ref())?,
            arguments: request.arguments.clone(),
            invocation: request.invocation.clone(),
        };
        let descriptor = if snapshot
            .catalog
            .flows
            .iter()
            .any(|row| row.id == flow.instance_id)
        {
            let worker = self.host.flow(&flow).await?;
            let descriptor = worker.descriptor().await?;
            let entry = descriptor.entry()?;
            ensure!(
                descriptor.creation_request == request.request_id
                    && entry.path == request.path
                    && entry.name == request.entry,
                "launch conflicts with the recorded entry"
            );
            if let Some(record) = worker.launch_record().await? {
                ensure!(
                    record.input.request.sender == SenderIdentity::User(OPERATOR)
                        && record.input.request.payload == payload
                        && record.input.initial_goal == request.initial_goal,
                    "launch conflicts with the recorded input"
                );
                return Ok(json!({"flow": address(&flow), "acceptance": record.acceptance}));
            }
            descriptor
        } else {
            ensure!(
                snapshot.cursor.sequence == request.expected_catalog_sequence,
                "session catalog changed; inspect its flows before launching"
            );
            let roots = snapshot.catalog.descriptor.source_roots();
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
        let (executor, authority, trust) = self
            .prepare_entry(state, &snapshot.catalog.descriptor, &descriptor)
            .await?;
        // The creation request identifies the initial invocation in each typed identity domain.
        let input = LaunchInput {
            initial_goal: request.initial_goal,
            execution: Some(EntryBinding {
                first_turn: atman_rt::TurnId(request.request_id.into()),
                first_run: atman_rt::RunId(request.request_id.into()),
                authority: authority.clone(),
            }),
            request: DeliveryRequest {
                request_id: request.request_id,
                sender: SenderIdentity::User(OPERATOR),
                recipient: flow.clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload,
            },
            inherited_context: vec![],
            result_route: None,
        };
        let launched = self
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
            .await?;
        Ok(json!({"flow": address(&flow), "acceptance": launched.record.acceptance}))
    }

    pub(super) async fn invoke(&self, request: InvokeFlowRequest) -> Result<Value> {
        let flow = core_address(request.flow);
        let worker = self.host.flow(&flow).await?;
        let acceptance = worker
            .accept_input(
                DeliveryRequest {
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
        Ok(json!({"acceptance": acceptance}))
    }

    pub(super) async fn output(&self, request: ReadFlowOutputRequest) -> Result<Value> {
        let worker = self.host.flow(&core_address(request.flow)).await?;
        let records = worker.read_output(|_| Ok(())).await?;
        let start = match request.after {
            Some(id) => {
                records
                    .iter()
                    .position(|record| record.publication.id == id)
                    .context("output cursor is not in this flow")?
                    + 1
            }
            None => 0,
        };
        let end = start.saturating_add(request.limit).min(records.len());
        let page = &records[start..end];
        let next = page
            .last()
            .map(|record| record.publication.id)
            .or(request.after);
        Ok(json!({"messages": page, "next": next, "has_more": end < records.len()}))
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
