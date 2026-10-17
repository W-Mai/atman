//! Terminal attachment to persistent instances. Scheduling belongs to RuntimeHost.

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use atman_runtime::{
    AtmanRuntime, AtmanRuntimeOptions, Executor,
    config_hub::ConfigHub,
    event::EventSink,
    event_log::journal::CommittedBatch,
    flow_authority::{EffectiveAuthority, contract_allows_shell},
    flow_runtime::{
        FlowWorker,
        access::PeerControl,
        host::{HostServices, RuntimeHost, SessionHandle},
    },
    flow_store::{
        FlowInstanceId, RuntimeStore, SessionConfiguration, SessionDescriptor,
        goal::{GoalRequest, GoalVersion},
        inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity, UserContent},
        profile::ProfileStore,
        view::FlowUpdate,
    },
    memory::goal::{GoalActor, GoalEdit, GoalPauseReason},
    permission::{DecisionAuthority, PermissionAction, PermissionClients, PermissionRequestId},
    projection::flow::{FlowRecord, FlowTranscript},
    source_program::load_program_from_bundle,
};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

mod prompts;

const PAGE: NonZeroUsize = NonZeroUsize::new(64).unwrap();
const YIELD: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
const USER: Uuid = Uuid::from_bytes(*b"atman-local-user");

struct Attachment {
    host: RuntimeHost,
    session: SessionHandle,
    descriptor: SessionDescriptor,
    executor: Executor,
    config: ConfigHub,
    prompts: Arc<prompts::Prompts>,
    // Keep the endpoint alive until input closes, independently of view focus.
    permissions: Option<Arc<PermissionClients>>,
}

pub(crate) async fn run(root: &Path, id: Uuid) -> Result<()> {
    let store = RuntimeStore::open(root)?;
    let recorded = RuntimeStore::inspect_session(root, id)?;
    let descriptor = recorded.projection.descriptor;
    let SessionConfiguration::Atman(source) = &descriptor.configuration else {
        bail!("session requires an Atman configuration source to execute");
    };
    let config = ConfigHub::from_source(source)?;
    let permissions = Arc::new(PermissionClients::default());
    let redactor = atman_daemon::bootstrap::build_redactor(Some(config.config_dir()));
    let host = RuntimeHost::with_services(HostServices {
        redactor: redactor.clone(),
        permission_clients: Arc::downgrade(&permissions),
    });
    let outcome = async {
        let session = host.attach_session(store.open_session(id)?)?;
        // Profile ownership can belong to another frontend; local interaction remains available.
        match ProfileStore::open(&store).and_then(|profile| host.attach_profile(profile)) {
            Ok(_) => {}
            Err(error) => eprintln!("profile management unavailable: {error}"),
        }
        let mut events = EventSink::new();
        if let Some(redactor) = redactor {
            events = events.with_redactor(redactor);
        }
        let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
            events,
            mock: false,
            config: config.clone(),
            project: descriptor.project.clone(),
            home_dir: std::env::var_os("HOME").map(Into::into),
            workspace_generation: Uuid::now_v7().to_string(),
        })
        .await?;
        if let Some(lifecycle) = runtime.executor.provider_lifecycle() {
            crate::consume_provider_catalog_refresh_plan(
                runtime.provider_catalog_refresh_plan,
                move |id| {
                    let lifecycle = lifecycle.clone();
                    async move { lifecycle.refresh_models_if_stale(&id).await }
                },
            )
            .await;
        }
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let prompts = Arc::new(prompts::Prompts::new(prompt_tx));
        let mut executor = runtime.executor;
        executor.tool_ctx.prompt_resolver = Some(prompts.clone());
        let attachment = Attachment {
            host: host.clone(),
            session,
            descriptor,
            executor,
            config,
            prompts,
            permissions: Some(permissions),
        };
        attachment.interact(prompt_rx).await
    }
    .await;
    // Errors and explicit exit release ownership without closing persistent instances.
    let shutdown = host.shutdown().await;
    outcome?;
    shutdown?;
    Ok(())
}

impl Attachment {
    async fn bind(&self, worker: &FlowWorker) -> Result<()> {
        if self
            .session
            .scheduling_status(worker.address().instance_id.clone())
            .await?
            .is_some_and(|status| {
                *status.borrow()
                    == atman_runtime::flow_runtime::scheduler::SchedulingStatus::Running
            })
        {
            return Ok(());
        }
        let descriptor = worker.descriptor().await?;
        let entry = descriptor.entry()?;
        let program =
            load_program_from_bundle(&entry.bundle, &entry.path, &self.descriptor.source_roots())?;
        let flow = program
            .entry_flow(&entry.name)
            .and_then(|id| program.flow(&id))
            .context("entry is unavailable")?;
        let trust = self.config.trust_config()?;
        let authority = EffectiveAuthority::root(
            &trust,
            contract_allows_shell(flow.contract.as_ref()),
            self.descriptor
                .project
                .as_ref()
                .map(|project| project.root().to_path_buf()),
        );
        if let Err(errors) = atman_runtime::validate::validate(flow, &self.executor.tools) {
            bail!(
                "entry validation failed: {}",
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        self.session
            .bind_entry(
                worker.address().instance_id.clone(),
                self.executor.clone(),
                authority,
                trust,
                YIELD,
            )
            .await?;
        Ok(())
    }

    async fn select(&self, prefix: &str) -> Result<FlowWorker> {
        let catalog = self.session.snapshot().await?;
        let candidates: Vec<_> = catalog
            .catalog
            .flows
            .iter()
            .filter(|flow| flow.id.to_string().starts_with(prefix))
            .collect();
        ensure!(
            candidates.len() == 1,
            "flow prefix must match exactly one instance (matched {})",
            candidates.len()
        );
        Ok(self.session.open_flow(candidates[0].id.clone()).await?)
    }

    async fn interact(mut self, mut prompts: mpsc::UnboundedReceiver<String>) -> Result<()> {
        let catalog = self.session.snapshot().await?;
        let default = catalog
            .catalog
            .default_flow
            .context("session has no default flow")?;
        let mut focus = self.session.open_flow(default).await?;
        // Subscribe before any scheduler can publish. The journal closes catch-up/live races.
        let mut updates = focus.subscribe_updates(None, PAGE).await?;
        for row in catalog.catalog.flows {
            let result = async {
                let worker = self.session.open_flow(row.id.clone()).await?;
                if matches!(
                    worker.state().await?.state(),
                    atman_rt::InstanceState::Open(_)
                ) {
                    self.bind(&worker).await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                eprintln!("flow {} remains detached: {error:#}", row.id);
            }
        }
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (printer_tx, printer_rx) = oneshot::channel();
        crate::spawn_stdin_reader(input_tx, printer_tx);
        let mut printer = printer_rx.await.unwrap_or(None);
        crate::render_note(
            &mut printer,
            format!(
                "Session {} · Flow {} · :help",
                self.session.id(),
                focus.address().instance_id
            ),
        );
        let mut transcripts = HashMap::<FlowInstanceId, FlowTranscript>::new();
        let mut draining = false;
        let mut drain = std::pin::pin!(self.host.wait_quiescent());
        loop {
            tokio::select! {
                page = updates.next() => {
                    let page = page.context("flow update stream closed")??;
                    let transcript = transcripts.entry(focus.address().instance_id.clone())
                        .or_insert_with(|| FlowTranscript::new(focus.address().clone()));
                    for batch in page.batches {
                        render_batch(&mut printer, transcript, batch)?;
                    }
                }
                Some(prompt) = prompts.recv() => crate::render_note(&mut printer, prompt),
                result = &mut drain, if draining => {
                    result?;
                    // Commit observation may still be queued when host execution becomes idle.
                    let transcript = transcripts.entry(focus.address().instance_id.clone())
                        .or_insert_with(|| FlowTranscript::new(focus.address().clone()));
                    loop {
                        let page = focus.read_updates(transcript.cursor().cloned(), PAGE).await?;
                        for batch in page.batches { render_batch(&mut printer, transcript, batch)?; }
                        if !page.has_more { break; }
                    }
                    break;
                }
                input = input_rx.recv(), if !draining => {
                    let Some(input) = input else {
                        self.prompts.disconnect();
                        self.permissions.take();
                        draining = true;
                        continue;
                    };
                    let line = input.text.trim();
                    if line.is_empty() { continue; }
                    if matches!(line, ":quit" | ":q" | ":exit") { break; }
                    if let Some(prefix) = line.strip_prefix(":focus ") {
                        match self.select(prefix.trim()).await {
                            Ok(worker) => {
                                let after = transcripts.get(&worker.address().instance_id).and_then(FlowTranscript::cursor).cloned();
                                let feed = worker.subscribe_updates(after, PAGE).await?;
                                focus = worker;
                                updates = feed;
                                crate::render_note(&mut printer, format!("Flow {}", focus.address().instance_id));
                            }
                            Err(error) => crate::render_note(&mut printer, error.to_string()),
                        }
                        continue;
                    }
                    let result = self.command(&focus, &input.text).await;
                    match result {
                        Ok(text) if !text.is_empty() => crate::render_note(&mut printer, text),
                        Ok(_) => {},
                        Err(error) => crate::render_note(&mut printer, format!("{error:#}")),
                    }
                }
            }
        }
        Ok(())
    }

    async fn command(&self, worker: &FlowWorker, line: &str) -> Result<String> {
        let original = line;
        let line = line.trim();
        let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
        let argument = argument.trim();
        match command {
            ":help" => Ok("Text submits to the focused flow. :focus <flow-prefix> · :flows · :status · :invoke <JSON arguments> · :insert <text> · :pause/:resume (execution) · :recover · :cancel · :close · :goal [set <text>|pause|resume|clear] · :pending · :reply <prompt-id> <JSON> · :approve/:deny <flow-prefix> <request-id> · :quit. EOF waits for runnable work; :quit detaches immediately.".into()),
            ":flows" => {
                let catalog = self.session.snapshot().await?;
                let mut rows = Vec::new();
                for row in catalog.catalog.flows {
                    let flow = self.session.open_flow(row.id.clone()).await?;
                    let state = flow.state().await?;
                    rows.push(format!("{} · {:?} · {} execution pauses", row.id, state.state(), state.pauses().len()));
                }
                Ok(rows.join("\n"))
            }
            ":status" => {
                let status = worker.status().await?;
                Ok(format!("Flow {} · {:?} · {} execution pauses · Goal {}", worker.address().instance_id, status.execution.state(), status.execution.pauses().len(), status.goal.map_or_else(|| "none".into(), |goal| format!("{:?}: {}", goal.status, goal.objective))))
            }
            ":pause" | ":resume" | ":cancel" | ":close" | ":recover" => {
                let control = match command { ":pause" => PeerControl::Pause, ":resume" => PeerControl::Resume, ":cancel" => PeerControl::CancelTurn, ":recover" => PeerControl::Recover, _ => PeerControl::Close };
                let state = worker.state().await?;
                worker.control(state.revision(), Uuid::from_bytes(*b"atman-user-pause").into(), control, |_| Ok(())).await?;
                if matches!(control, PeerControl::Resume | PeerControl::Recover) { self.bind(worker).await?; }
                Ok(format!("{control:?} acknowledged"))
            }
            ":goal" if argument.is_empty() => Ok(serde_json::to_string_pretty(&worker.goal().await?.goal)?),
            ":goal" => {
                let edit = match argument {
                    "pause" => GoalEdit::Pause { actor: GoalActor::User, reason: GoalPauseReason::UserRequested, summary: String::new() },
                    "resume" => GoalEdit::Resume { actor: GoalActor::User },
                    "clear" => GoalEdit::Clear,
                    text if text.starts_with("set ") => GoalEdit::Set { text: text[4..].into(), replace_completed: false },
                    _ => bail!("use :goal set <text>, pause, resume, or clear"),
                };
                let start_turn = matches!(edit, GoalEdit::Resume { .. });
                if start_turn { self.bind(worker).await?; }
                let expected = worker.goal().await?.goal.as_ref().map(GoalVersion::from);
                let result = worker.edit_goal(GoalRequest { id: Uuid::now_v7(), sender: SenderIdentity::User(USER), expected, edit, start_turn }, |_| Ok(())).await?;
                Ok(serde_json::to_string_pretty(&result.goal)?)
            }
            ":pending" => {
                let mut lines = self.prompts.pending();
                for row in self.session.snapshot().await?.catalog.flows {
                    let flow = self.session.open_flow(row.id.clone()).await?;
                    for request in flow.context().permission_broker().user_list(&self.session.id().to_string()).0 {
                        lines.push(format!("Flow {} · permission {} · {:?} · {:?}", row.id, request.request_id, request.state, request.intent));
                    }
                }
                Ok(if lines.is_empty() { "No pending interactions".into() } else { lines.join("\n") })
            }
            ":reply" => {
                let (id, json) = argument.split_once(' ').context("use :reply <prompt-id> <JSON>")?;
                self.prompts.reply(id.parse()?, serde_json::from_str(json)?)?;
                Ok("Prompt answered".into())
            }
            ":approve" | ":deny" => {
                let (flow, id) = argument.split_once(' ').context("use :approve/:deny <flow-prefix> <request-id>")?;
                let flow = self.select(flow).await?;
                let context = flow.context();
                let broker = context.permission_broker();
                let authority = DecisionAuthority::User(broker.user_authority(self.session.id().to_string(), Some(USER.to_string())));
                broker.resolve(&PermissionRequestId(id.trim().parse()?), &authority, if command == ":approve" { PermissionAction::Approve } else { PermissionAction::Deny }, None, None)?;
                Ok("Permission decision recorded".into())
            }
            _ => {
                let payload = match command {
                    ":invoke" => InboxPayload::Invocation { arguments: serde_json::from_str(argument)?, invocation: BTreeMap::new(), user: None },
                    name if (name.starts_with(':') && name != ":insert") || name.starts_with('/') => bail!("unknown persistent flow command; use :help"),
                    _ => {
                        let descriptor = worker.descriptor().await?;
                        let entry = descriptor.entry()?;
                        let program = load_program_from_bundle(&entry.bundle, &entry.path, &self.descriptor.source_roots())?;
                        let flow = program.entry_flow(&entry.name).and_then(|id| program.flow(&id)).context("entry is unavailable")?;
                        ensure!(atman_runtime::validate::invocation_message_parameter(flow)?.is_some(), "entry has no invocation user_message parameter; use :invoke <JSON arguments>");
                        if command == ":insert" {
                            ensure!(!argument.is_empty(), "insertion must not be empty");
                            InboxPayload::Interjection { text: argument.into() }
                        } else {
                            InboxPayload::UserSubmission { content: UserContent { text: original.into(), images: vec![], presentation: None }, invocation: BTreeMap::new() }
                        }
                    }
                };
                self.bind(worker).await?;
                let receipt = worker.accept_input(DeliveryRequest {
                    request_id: Uuid::now_v7(), sender: SenderIdentity::User(USER), recipient: worker.address().clone(),
                    delivery: if command == ":insert" { DeliveryMode::AtBoundary } else { DeliveryMode::NextTurn },
                    cause: None, correlation: None, scope: None, payload,
                }, |_| Ok(())).await?;
                Ok(format!("Input accepted: {}", receipt.message_id))
            }
        }
    }
}

fn render_batch(
    printer: &mut Option<crate::ExternalPrinter>,
    transcript: &mut FlowTranscript,
    batch: CommittedBatch<FlowUpdate>,
) -> Result<()> {
    let appended = transcript.apply(batch)?;
    for entry in &transcript.entries()[appended] {
        render_record(printer, &entry.record);
    }
    Ok(())
}

fn render_record(printer: &mut Option<crate::ExternalPrinter>, record: &FlowRecord) {
    match record {
        FlowRecord::Publication(publication)
            if publication.context_run.is_none()
                && publication.message.is_visible_assistant_reply() =>
        {
            crate::emit(printer, format!("{}\n", publication.message.text_concat()));
        }
        FlowRecord::Completion(completion) => {
            if let atman_runtime::flow_store::completion::InvocationResult::Failed(error) =
                &completion.result
            {
                crate::render_note(printer, format!("Turn failed: {error}"));
            }
        }
        FlowRecord::Execution(event) => {
            if let atman_runtime::event::Event::PermissionRequestCreated { payload } = &event.event
            {
                crate::render_note(
                    printer,
                    format!(
                        "Permission record: {} · {}\nUse :pending to inspect current requests, then :approve/:deny <flow-prefix> <request-id>",
                        payload.tool,
                        payload
                            .request_id
                            .as_ref()
                            .map_or_else(|| "policy decision".into(), |id| id.to_string()),
                    ),
                );
            }
        }
        _ => {}
    }
}
