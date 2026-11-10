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
    flow_authority::{EffectiveAuthority, contract_allows_shell},
    flow_runtime::{
        FlowView, FlowViewReader, FlowWorker,
        access::PeerControl,
        host::{HostServices, RuntimeHost, SessionHandle},
    },
    flow_store::{
        FlowInstanceId, RuntimeStore, SessionConfiguration, SessionDescriptor,
        goal::{GoalRequest, GoalVersion},
        inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity, UserContent},
        profile::ProfileStore,
    },
    memory::goal::{GoalActor, GoalEdit, GoalPauseReason},
    permission::{DecisionAuthority, PermissionAction, PermissionClients, PermissionRequestId},
    projection::flow::{ArchiveTranscript, FlowRecord, LegacyFlowView, LegacyHistory},
    source_program::load_program_from_bundle,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

mod continuation;
mod execution;
mod operations;
mod prompts;
mod scopes;
mod tui;

const PAGE: NonZeroUsize = NonZeroUsize::new(64).unwrap();
const YIELD: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
const USER: Uuid = Uuid::from_bytes(*b"atman-local-user");

#[derive(Clone)]
struct Attachment {
    data_root: std::path::PathBuf,
    host: RuntimeHost,
    session: SessionHandle,
    descriptor: SessionDescriptor,
    project: Option<atman_runtime::storage::ProjectContext>,
    executors: execution::ScopeExecutors,
    executor: Executor,
    config: ConfigHub,
    prompts: Arc<prompts::Prompts>,
    // Keep the endpoint alive until input closes, independently of view focus.
    permissions: Option<Arc<PermissionClients>>,
}

pub(crate) async fn create(root: &Path) -> Result<Uuid> {
    let host = RuntimeHost::default();
    let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();
    let scopes = Arc::new(scopes::Scopes::empty(
        root,
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(prompt_tx)),
    ));
    let outcome = async {
        let config = ConfigHub::global()?;
        let project = atman_runtime::storage::ProjectContext::resolve(
            &config,
            &std::env::current_dir().context("reading current project root")?,
            root,
        )?;
        scopes
            .create_session(
                scopes::SessionSeed {
                    config,
                    project: Some(project),
                    directory: None,
                    prompt_resolver: None,
                },
                Uuid::now_v7(),
                scopes::CreationMode::Deferred,
            )
            .await
    }
    .await;
    let shutdown = host.shutdown().await;
    let created = outcome?;
    shutdown?;
    Ok(created.session_id.0)
}

fn session_redactor(
    descriptor: &SessionDescriptor,
) -> std::io::Result<Option<Arc<atman_runtime::redact::Redactor>>> {
    match &descriptor.configuration {
        SessionConfiguration::Atman(source) => {
            let config = ConfigHub::from_source(source).map_err(std::io::Error::other)?;
            Ok(atman_daemon::bootstrap::build_redactor(Some(
                config.config_dir(),
            )))
        }
        _ => Ok(None),
    }
}

pub(crate) async fn run(root: &Path, requested_id: Option<Uuid>) -> Result<()> {
    let tui = crate::tui_mode_requested();
    let terminal_guard = if tui {
        Some(atman_tui::terminal_guard::TerminalGuard::install()?)
    } else {
        None
    };
    let permissions = Arc::new(PermissionClients::default());
    let host = RuntimeHost::with_services(HostServices {
        redactor: None,
        session_redactor: Some(Arc::new(session_redactor)),
        permission_clients: Arc::downgrade(&permissions),
    });
    let (collector, notifications) = atman_runtime::notify::ToastCollector::new();
    let mut notification_sink =
        tui.then(|| atman_runtime::notify::ScopedSink::replace_with(Arc::new(collector)));
    let mut unused_session = None;
    let outcome = async {
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let prompts = Arc::new(prompts::Prompts::new(prompt_tx));
        let scopes = Arc::new(scopes::Scopes::empty(
            root,
            host.clone(),
            Some(permissions),
            prompts.clone(),
        ));
        let id = match requested_id {
            Some(id) => id,
            None => {
                let config = ConfigHub::global()?;
                let project_root =
                    std::env::current_dir().context("reading current project root")?;
                let project =
                    atman_runtime::storage::ProjectContext::resolve(&config, &project_root, root)?;
                let flow = scopes
                    .create_session(
                        scopes::SessionSeed {
                            config,
                            project: Some(project),
                            directory: None,
                            prompt_resolver: None,
                        },
                        Uuid::now_v7(),
                        scopes::CreationMode::Deferred,
                    )
                    .await?;
                let id = flow.session_id.0;
                unused_session = Some(atman_runtime::flow_store::SessionRemoval {
                    session_id: id,
                    expected: host.session(id)?.snapshot().await?.cursor,
                });
                id
            }
        };
        if tui {
            use atman_tui::boot_animation::{BootStepId, run_boot_animation};
            let (progress, updates) = mpsc::unbounded_channel();
            let (prepared, animation) = tokio::join!(
                prepare_attachment(&scopes, id, progress),
                run_boot_animation(
                    updates,
                    atman_tui::app::StartupCard {
                        version: env!("CARGO_PKG_VERSION").into(),
                        recent: vec![],
                        directory: atman_tui::app::StartupDirectory::Sessions,
                    },
                    &[BootStepId::OpenSession, BootStepId::Ready],
                    notifications,
                ),
            );
            drop(notification_sink.take());
            notification_sink = Some(atman_runtime::notify::ScopedSink::tui());
            let attachment = prepared?;
            let (terminal, notifications) = animation?;
            match attachment {
                Prepared::Live(attachment) => {
                    attachment
                        .interact_tui(scopes, terminal, notifications)
                        .await
                }
                Prepared::Archive(archive) => {
                    tui::interact_archive(*archive, scopes, terminal, notifications).await
                }
                Prepared::Legacy(view, meta) => {
                    tui::interact_legacy(*view, meta, scopes, terminal, notifications).await
                }
            }
        } else {
            match prepare(&scopes, id).await? {
                Prepared::Live(attachment) => {
                    attachment.interact(prompt_rx).await.map(|()| Some(id))
                }
                Prepared::Archive(archive) => {
                    print_archive(&archive.view.transcript().archived_transcript()?)?;
                    continuation::interact_history(
                        scopes,
                        archive.view.transcript().flow().clone().into(),
                        prompt_rx,
                    )
                    .await
                    .map(Some)
                }
                Prepared::Legacy(view, _) => {
                    print_archive(view.transcript())?;
                    let address = atman_tui::control::ViewAddress::Legacy {
                        session: view.session_id(),
                        owner: view.owner().clone(),
                    };
                    continuation::interact_history(scopes, address, prompt_rx)
                        .await
                        .map(Some)
                }
            }
        }
    }
    .await;
    // Errors and explicit exit release ownership without closing persistent instances.
    let shutdown = host.shutdown().await;
    let mut discarded = None;
    if shutdown.is_ok() {
        if let Some(request) = unused_session {
            let session = request.session_id;
            let root = root.to_owned();
            let cleanup = tokio::task::spawn_blocking(move || {
                RuntimeStore::open(&root)?.discard_unused_session(&request)
            })
            .await;
            match cleanup
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            {
                Ok(true) => discarded = Some(session),
                Ok(false) => {}
                Err(error) => {
                    atman_runtime::notify!(warn, "unused session cleanup failed: {error:#}");
                }
            }
        }
    }
    drop(notification_sink);
    drop(terminal_guard);
    let session = outcome?;
    shutdown?;
    if let Some(session) = session.filter(|session| Some(*session) != discarded) {
        println!("\n  atman --continue {session}\n");
    }
    Ok(())
}

enum Prepared {
    Live(Box<Attachment>),
    Archive(Box<ArchivedSelection>),
    Legacy(
        Box<LegacyFlowView>,
        Option<atman_runtime::session_meta::SessionMeta>,
    ),
}

struct ArchivedSelection {
    view: FlowView,
    title: Option<String>,
    project: Option<atman_runtime::storage::ProjectContext>,
}

async fn legacy_selection(
    root: &Path,
    id: Uuid,
    owner: Option<atman_runtime::event_log::legacy::LegacyFlowKey>,
) -> Result<(
    LegacyFlowView,
    Option<atman_runtime::session_meta::SessionMeta>,
)> {
    let directory = root.join("sessions").join(id.to_string());
    tokio::task::spawn_blocking(move || {
        let history = LegacyHistory::inspect(&directory.join("events.jsonl"), id)?;
        // Prefer the default partition when it has records; an empty default
        // must not conceal a source containing only unassigned records.
        let owner = owner.unwrap_or_else(|| legacy_default_owner(&history));
        Ok((
            history.read_flow(&owner)?,
            atman_runtime::session_meta::SessionMeta::load(&directory),
        ))
    })
    .await?
}

fn legacy_default_owner(
    history: &LegacyHistory,
) -> atman_runtime::event_log::legacy::LegacyFlowKey {
    history
        .inspection()
        .records
        .iter()
        .map(|record| record.owner.key())
        .min()
        .unwrap_or(atman_runtime::event_log::legacy::LegacyFlowKey::Default)
}

async fn archived_selection(
    root: &Path,
    id: Uuid,
    instance: Option<FlowInstanceId>,
) -> Result<Option<ArchivedSelection>> {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        let read = RuntimeStore::inspect_session(&root, id)?;
        let instance = instance
            .or_else(|| read.projection.default_flow.clone())
            .context("session has no default flow")?;
        let project = execution::selected_project(&read.projection, Some(&instance))?;
        Ok(read
            .archived_view(&instance)?
            .map(|view| ArchivedSelection {
                view,
                title: read.projection.title,
                project,
            }))
    })
    .await?
}

async fn prepare(scopes: &Arc<scopes::Scopes>, id: Uuid) -> Result<Prepared> {
    let root = scopes.root.clone();
    let native = tokio::task::spawn_blocking(move || {
        RuntimeStore::session_ids(&root).map(|ids| ids.contains(&id))
    })
    .await??;
    if !native {
        let (view, meta) = legacy_selection(&scopes.root, id, None).await?;
        return Ok(Prepared::Legacy(Box::new(view), meta));
    }
    if let Some(archive) = archived_selection(&scopes.root, id, None).await? {
        return Ok(Prepared::Archive(Box::new(archive)));
    }
    Ok(Prepared::Live(Box::new(scopes.open(id).await?)))
}

fn print_archive(archive: &ArchiveTranscript) -> Result<()> {
    use atman_runtime::{
        TranscriptEntry,
        message::{MessageOrigin, MessageRole},
    };
    let mut printer = None;
    crate::render_note(&mut printer, "Historical flow · original preserved".into());
    for entry in &archive.entries {
        if let TranscriptEntry::Message { message, .. } = entry {
            if message.is_visible_assistant_reply()
                || (message.role == MessageRole::User && message.origin == MessageOrigin::User)
            {
                // Historical text must not execute terminal control sequences.
                crate::emit(
                    &mut printer,
                    format!(
                        "{:?}: {}\n",
                        message.role,
                        message
                            .text_concat()
                            .chars()
                            .flat_map(|c| {
                                if c.is_control() && c != '\n' && c != '\t' {
                                    c.escape_default().collect::<Vec<_>>()
                                } else {
                                    vec![c]
                                }
                            })
                            .collect::<String>()
                    ),
                );
            }
        }
    }
    for record in &archive.unparsed {
        crate::render_note(
            &mut printer,
            format!(
                "Retained source record · line {} · {:?}\n{}",
                record.source.line,
                record.source.issues,
                String::from_utf8_lossy(&record.bytes()?).escape_debug()
            ),
        );
    }
    Ok(())
}

async fn prepare_attachment(
    scopes: &Arc<scopes::Scopes>,
    id: Uuid,
    progress: mpsc::UnboundedSender<atman_tui::boot_animation::BootProgress>,
) -> Result<Prepared> {
    use atman_tui::boot_animation::{BootProgress, BootStepId};
    let emit = |event| {
        let _ = progress.send(event);
    };
    emit(BootProgress::Start(BootStepId::OpenSession));
    let attachment = prepare(scopes, id).await?;
    emit(BootProgress::Finish(
        BootStepId::OpenSession,
        atman_runtime::workflow::NodeStatus::Ok,
    ));
    emit(BootProgress::Finish(
        BootStepId::Ready,
        atman_runtime::workflow::NodeStatus::Ok,
    ));
    Ok(attachment)
}

impl Attachment {
    fn import_image(
        &self,
        worker: &FlowWorker,
        record: Option<&atman_runtime::flow_store::launch::LaunchRecord>,
        path: &Path,
    ) -> Result<atman_runtime::message::ImageSource> {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            record
                .and_then(|record| record.input.workspace.as_ref())
                .map(|workspace| workspace.binding.path.as_path())
                .or_else(|| self.project.as_ref().map(|project| project.root()))
                .context("relative image paths require a bound project or workspace")?
                .join(path)
        };
        Ok(worker.context().import_image_path(&path)?)
    }

    async fn submit_user(
        &self,
        worker: &FlowWorker,
        request: Uuid,
        content: UserContent,
        invocation: BTreeMap<String, serde_json::Value>,
    ) -> Result<Uuid> {
        let payload = self.user_payload(worker, content, invocation).await?;
        self.accept_payload(worker, request, payload, DeliveryMode::NextTurn)
            .await
    }

    async fn user_payload(
        &self,
        worker: &FlowWorker,
        mut content: UserContent,
        invocation: BTreeMap<String, serde_json::Value>,
    ) -> Result<InboxPayload> {
        let record = worker.launch_record().await?;
        // Only authored prompt text selects files or commands; quoted context remains data.
        let (text, paths) =
            crate::extract_presented_at_paths(&content.text, &mut content.presentation);
        content.text = text;
        for path in paths {
            content
                .images
                .push(self.import_image(worker, record.as_ref(), &path)?);
        }
        let prompt = content
            .presentation
            .as_ref()
            .map_or(content.text.as_str(), |value| &value.prompt);
        let command = if prompt.trim_start().starts_with('/') {
            Some(prompt.to_owned())
        } else if record.as_ref().is_some_and(|record| {
            record
                .input
                .execution
                .as_ref()
                .is_some_and(|binding| binding.user_routes)
        }) {
            Some(
                atman_runtime::routing::RouteProgram::load(&self.config)?
                    .resolve(prompt.trim())
                    .context("no route matched; configure routes.at or use /name args")?
                    .slash_call(),
            )
        } else {
            None
        };
        if let Some(command) = command {
            let (program, name, arguments, _) = crate::resolve_slash_command_from(
                &command,
                self.config.config_dir(),
                self.project.as_ref().map(|project| project.root()),
            )?;
            let flow = program
                .entry_flow(&name)
                .and_then(|id| program.flow(&id))
                .context("command entry is unavailable")?;
            if let Err(errors) = atman_runtime::validate::validate(flow, &self.executor.tools) {
                bail!(
                    "command validation failed: {}",
                    errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            for (name, _) in &arguments {
                ensure!(
                    flow.params
                        .iter()
                        .any(|parameter| parameter.name.name == *name),
                    "unknown command argument: {name}"
                );
                ensure!(
                    arguments.iter().filter(|(key, _)| key == name).count() == 1,
                    "duplicate command argument: {name}"
                );
            }
            for parameter in &flow.params {
                ensure!(
                    parameter.default.is_some()
                        || arguments
                            .iter()
                            .any(|(name, _)| name == &parameter.name.name),
                    "missing command argument: {}",
                    parameter.name.name
                );
            }
            return Ok(InboxPayload::Command {
                entry: Box::new(atman_runtime::flow_store::EntrySnapshot {
                    name,
                    path: program
                        .entry_path()
                        .context("command source path is unavailable")?
                        .to_path_buf(),
                    bundle: program.revision_bundle(),
                }),
                arguments: arguments
                    .into_iter()
                    .map(|(name, value)| {
                        Ok((name, atman_runtime::value::ValueJson::to_json(&value)?))
                    })
                    .collect::<Result<_, atman_runtime::RuntimeError>>()?,
                invocation,
                user: Some(content),
            });
        }
        self.validate_text_input(worker).await?;
        Ok(InboxPayload::UserSubmission {
            content,
            invocation,
        })
    }

    async fn open_selected(
        root: &Path,
        id: Uuid,
        host: RuntimeHost,
        permissions: Option<Arc<PermissionClients>>,
        prompts: Arc<prompts::Prompts>,
        instance: Option<FlowInstanceId>,
    ) -> Result<Self> {
        let session = host.open_session(root, id).await?;
        let catalog = session.snapshot().await?.catalog;
        let project = execution::selected_project(&catalog, instance.as_ref())?;
        let descriptor = catalog.descriptor;
        let SessionConfiguration::Atman(source) = &descriptor.configuration else {
            bail!("session requires an Atman configuration source to execute");
        };
        let config = ConfigHub::from_source(source)?;
        let executors = Default::default();
        execution::configure(&session, &executors, &config, &prompts)?;
        let executor = session.executor_for(project.clone()).await?;
        Ok(Self {
            data_root: root.to_owned(),
            host,
            session,
            descriptor,
            project,
            executors,
            executor,
            config,
            prompts,
            permissions,
        })
    }

    async fn validate_text_input(&self, worker: &FlowWorker) -> Result<()> {
        let descriptor = worker.descriptor().await?;
        let entry = descriptor.entry()?;
        let program = load_program_from_bundle(&entry.bundle, &entry.path, &self.source_roots())?;
        let flow = program
            .entry_flow(&entry.name)
            .and_then(|id| program.flow(&id))
            .context("entry is unavailable")?;
        ensure!(
            atman_runtime::validate::invocation_message_parameter(flow)?.is_some(),
            "entry has no invocation user_message parameter; use :invoke <JSON arguments>"
        );
        Ok(())
    }

    async fn accept_payload(
        &self,
        worker: &FlowWorker,
        request_id: Uuid,
        payload: InboxPayload,
        delivery: DeliveryMode,
    ) -> Result<Uuid> {
        self.bind(worker).await?;
        let receipt = worker
            .accept_input(
                DeliveryRequest {
                    request_id,
                    sender: SenderIdentity::User(USER),
                    recipient: worker.address().clone(),
                    delivery,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload,
                },
                |_| Ok(()),
            )
            .await?;
        Ok(receipt.message_id)
    }
    async fn bind(&self, worker: &FlowWorker) -> Result<()> {
        ensure!(
            worker.project().await? == self.project,
            "flow and execution project differ"
        );
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
        let program = load_program_from_bundle(&entry.bundle, &entry.path, &self.source_roots())?;
        let flow = program
            .entry_flow(&entry.name)
            .and_then(|id| program.flow(&id))
            .context("entry is unavailable")?;
        let trust = self.config.trust_config()?;
        // Only the CLI's host-owned initial entry records an operator-selected directory.
        // Delegated instances continue to be constrained by their registered project.
        let directory = if descriptor.created_by.is_none()
            && descriptor.creation_request == self.session.id()
            && descriptor.address.instance_id.0.0 == self.session.id()
        {
            worker
                .launch_record()
                .await?
                .and_then(|record| record.input.execution)
                .and_then(|entry| entry.authority.workspace_root)
        } else {
            None
        };
        let authority = EffectiveAuthority::root(
            &trust,
            contract_allows_shell(flow.contract.as_ref()),
            directory.or_else(|| {
                self.project
                    .as_ref()
                    .map(|project| project.root().to_path_buf())
            }),
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

    /// Restore the Session once when attaching for conversation, preserving live controls.
    async fn restore_scheduling(&self) -> Result<Vec<String>> {
        let mut errors = Vec::new();
        self.executors
            .restored
            .get_or_try_init(|| async {
                for row in self.session.snapshot().await?.catalog.flows {
                    let result = async {
                        // Retain existing controllers and their trust. Repeated view attachment
                        // must not restart stopped or failed scheduling.
                        if self
                            .session
                            .scheduling_status(row.id.clone())
                            .await?
                            .is_some()
                        {
                            return Ok(());
                        }
                        let worker = self.session.open_flow(row.id.clone()).await?;
                        if matches!(
                            worker.state().await?.state(),
                            atman_rt::InstanceState::Open(_)
                        ) {
                            self.for_worker(&worker).await?.bind(&worker).await?;
                        }
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        errors.push(format!("Flow {} remains detached: {error:#}", row.id));
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await?;
        Ok(errors)
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

    async fn interact(self, prompts: mpsc::UnboundedReceiver<String>) -> Result<()> {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (printer_tx, printer_rx) = oneshot::channel();
        crate::spawn_stdin_reader(input_tx, printer_tx);
        self.interact_using(prompts, input_rx, printer_rx.await.unwrap_or(None))
            .await
    }

    async fn interact_using(
        mut self,
        mut prompts: mpsc::UnboundedReceiver<String>,
        mut input_rx: mpsc::UnboundedReceiver<String>,
        mut printer: Option<crate::ExternalPrinter>,
    ) -> Result<()> {
        let catalog = self.session.snapshot().await?;
        let default = catalog
            .catalog
            .default_flow
            .context("session has no default flow")?;
        let mut focus = self.session.open_flow(default).await?;
        // Subscribe before any scheduler can publish. The journal closes catch-up/live races.
        let mut focused_view = TerminalView::new(&focus).await?;
        for error in self.restore_scheduling().await? {
            eprintln!("{error}");
        }
        crate::render_note(
            &mut printer,
            format!(
                "Session {} · Flow {} · :help",
                self.session.id(),
                focus.address().instance_id
            ),
        );
        let mut focused_scope = self.for_worker(&focus).await?;
        let mut views = HashMap::<FlowInstanceId, TerminalView>::new();
        let mut draining = false;
        let mut operations = operations::Operations::new();
        let mut drain = std::pin::pin!(self.host.wait_quiescent());
        loop {
            tokio::select! {
                result = operations.next() => {
                    let (address, result) = result;
                    let message = result.unwrap_or_else(|error| format!("Operation failed: {error:#}"));
                    crate::render_note(&mut printer, format!("Flow {address} · {message}"));
                }
                view = focused_view.reader.next() => {
                    render_view(&mut printer, &mut focused_view.rendered, view?);
                }
                Some(prompt) = prompts.recv() => crate::render_note(&mut printer, prompt),
                result = &mut drain, if draining => {
                    result?;
                    // Commit observation may still be queued when host execution becomes idle.
                    let view = focused_view.reader.refresh().await?;
                    render_view(&mut printer, &mut focused_view.rendered, view);
                    break;
                }
                input = input_rx.recv(), if !draining => {
                    let Some(input) = input else {
                        self.prompts.disconnect();
                        self.permissions.take();
                        draining = true;
                        continue;
                    };
                    let line = input.trim();
                    if line.is_empty() { continue; }
                    if matches!(line, ":quit" | ":q" | ":exit") { break; }
                    if let Some(operation) = operations::Operation::parse(line) {
                        match operations.start(&focused_scope, &focus, operation, Uuid::now_v7(), focus.address().instance_id.clone(), None) {
                            Ok(()) => crate::render_note(&mut printer, operation.notice().into()),
                            Err(error) => crate::render_note(&mut printer, format!("{error:#}")),
                        }
                        continue;
                    }
                    if let Some(prefix) = line.strip_prefix(":focus ") {
                        let selected = async {
                            let worker = self.select(prefix.trim()).await?;
                            let scope = self.for_worker(&worker).await?;
                            Ok::<_, anyhow::Error>((worker, scope))
                        }.await;
                        match selected {
                            Ok((worker, scope)) => {
                                if worker.address() != focus.address() {
                                    let selected = match views.remove(&worker.address().instance_id) {
                                        Some(view) => view,
                                        None => TerminalView::new(&worker).await?,
                                    };
                                    views.insert(focus.address().instance_id.clone(), std::mem::replace(&mut focused_view, selected));
                                    focus = worker;
                                }
                                focused_scope = scope;
                                crate::render_note(&mut printer, format!("Flow {}", focus.address().instance_id));
                            }
                            Err(error) => crate::render_note(&mut printer, error.to_string()),
                        }
                        continue;
                    }
                    let result = focused_view.submit(&focused_scope, &focus, &input).await;
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
        self.command_with_request(worker, line, Uuid::now_v7())
            .await
    }

    async fn command_with_request(
        &self,
        worker: &FlowWorker,
        line: &str,
        request_id: Uuid,
    ) -> Result<String> {
        let original = line;
        let line = line.trim();
        let (command, argument) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let argument = argument.trim();
        match command {
            ":session" => Ok(format!("Session {}", self.session.id())),
            ":model" => Ok(format!("Current model: {}", worker.context().last_model())),
            ":storage" => {
                let root = self
                    .project
                    .as_ref()
                    .context("current session has no project")?
                    .root();
                match argument {
                    "" => Ok(format!(
                        "Project storage: {}",
                        crate::storage_scope_name(
                            self.config
                                .try_storage_config(Some(root))?
                                .scope
                                .unwrap_or_default()
                        )
                    )),
                    "global" | "local" => {
                        let scope = if argument == "local" {
                            atman_runtime::storage::StorageScope::Local
                        } else {
                            atman_runtime::storage::StorageScope::Global
                        };
                        anyhow::ensure!(root.is_dir(), "project path does not exist: {}", root.display());
                        self.config.set_project_storage_scope(root, scope)?;
                        Ok(format!(
                            "Project storage set to {}; applies to new sessions",
                            crate::storage_scope_name(scope)
                        ))
                    }
                    _ => bail!("use :storage [global|local]"),
                }
            }
            ":sessions" => {
                let mut rows = Vec::new();
                for id in RuntimeStore::session_ids(&self.data_root)? {
                    let read = RuntimeStore::inspect_session(&self.data_root, id)?;
                    let marker = if id == self.session.id() { " *" } else { "" };
                    rows.push(format!(
                        "{}{} · {} · {} flows",
                        id,
                        marker,
                        read.projection.title.as_deref().unwrap_or("Untitled session"),
                        read.projection.flows.len()
                    ));
                }
                Ok(if rows.is_empty() { "No persistent sessions".into() } else { rows.join("\n") })
            }
            ":cost" => {
                let snapshot = worker.context().subscribe_context().borrow().clone();
                Ok(format!(
                    "{} · {} calls · input {} · output {} · cache read {} · cache write {}",
                    snapshot.model,
                    snapshot.primary_usage().map_or(0, |usage| usage.calls),
                    snapshot.tokens_in,
                    snapshot.tokens_out,
                    snapshot.cache_read,
                    snapshot.cache_write
                ))
            }
            ":copy" => {
                let Some(payload) = crate::copy_payload(argument, &worker.context().messages_full())? else {
                    return Ok("Nothing to copy".into());
                };
                crate::write_osc52(&payload);
                Ok(format!("Copied {} chars to terminal clipboard (OSC 52)", payload.chars().count()))
            }
            ":sidebar" => Ok("Sidebar controls are available in TUI mode".into()),
            ":help" => Ok("Text submits to the focused flow; /command [arguments] queues a captured command in the same flow. :attach <path>|list|clear · :name-session · :rename [<text>|clear] · :focus <flow-prefix> · :flows · :status · :session · :sessions · :model · :cost · :storage [global|local] · :suggest · :compact · :copy [last-message|last-tool] · :sidebar on|off · :invoke <JSON arguments> · :insert <text> · :pause/:resume (execution) · :recover · :cancel · :close · :goal [set <text>|pause|resume|clear] · :todo [list|done <id>|cancel <id>|clear] · :pending · :reply <prompt-id> <JSON> · :approve/:deny <flow-prefix> <request-id> · :quit. EOF waits for runnable work; :quit detaches immediately.".into()),
            ":rename" => {
                let snapshot = self.session.snapshot().await?;
                if argument.is_empty() {
                    return Ok(format!("Session title: {} · :rename <text> to set, :rename clear to remove", snapshot.catalog.title.as_deref().unwrap_or("(none)")));
                }
                let title = (argument != "clear").then(|| argument.to_owned());
                self.session.set_title(snapshot.cursor, title.clone()).await?;
                Ok(title.map_or_else(|| "Session title cleared".into(), |name| format!("Session renamed: {name}")))
            }
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
            ":todo" => {
                use atman_runtime::flow_store::planning::{PlanningEdit, PlanningRequest};
                use atman_runtime::memory::todo::TodoStatus;
                let planning = worker.planning().await?;
                if matches!(argument, "" | "list") {
                    return Ok(crate::todo_rows(&planning.todos).join("\n"));
                }
                let edit = if argument == "clear" {
                    PlanningEdit::ClearTodos
                } else {
                    let (action, id) = argument.split_once(char::is_whitespace).context("use :todo list, done <id>, cancel <id>, or clear")?;
                    let status = match action {
                        "done" => TodoStatus::Done,
                        "cancel" => TodoStatus::Cancelled,
                        _ => bail!("use :todo list, done <id>, cancel <id>, or clear"),
                    };
                    let id = crate::resolve_todo_id(id.trim(), &planning.todos).map_err(anyhow::Error::msg)?;
                    PlanningEdit::SetTodoStatus { id, status }
                };
                worker.edit_planning(PlanningRequest {
                    id: request_id, sender: SenderIdentity::User(USER), expected_revision: Some(planning.revision), edit,
                }, |_| Ok(())).await?;
                Ok("Todos updated".into())
            }
            ":pause" | ":resume" | ":cancel" | ":close" | ":recover" => {
                let control = match command { ":pause" => PeerControl::Pause, ":resume" => PeerControl::Resume, ":cancel" => PeerControl::CancelTurn, ":recover" => PeerControl::Recover, _ => PeerControl::Close };
                if control == PeerControl::Recover { self.bind(worker).await?; }
                let state = worker.state().await?;
                worker.control(atman_runtime::flow_store::control::ControlRequest {
                    id: request_id, sender: SenderIdentity::User(USER), target: worker.address().clone(),
                    expected_revision: state.revision(), pause_key: Uuid::from_bytes(*b"atman-user-pause").into(), action: control,
                }, |_| Ok(())).await?;
                if control == PeerControl::Resume { self.bind(worker).await?; }
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
                let result = worker.edit_goal(GoalRequest { id: request_id, sender: SenderIdentity::User(USER), expected, edit, start_turn }, |_| Ok(())).await?;
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
                    name if name.starts_with(':') && name != ":insert" => bail!("unknown persistent flow command; use :help"),
                    _ => {
                        if command == ":insert" {
                            self.validate_text_input(worker).await?;
                            ensure!(!argument.is_empty(), "insertion must not be empty");
                            InboxPayload::Interjection { text: argument.into() }
                        } else {
                            self.user_payload(worker, UserContent { text: original.into(), images: vec![], presentation: None }, BTreeMap::new()).await?
                        }
                    }
                };
                let id = self.accept_payload(worker, request_id, payload, if command == ":insert" { DeliveryMode::AtBoundary } else { DeliveryMode::NextTurn }).await?;
                Ok(format!("Input accepted: {id}"))
            }
        }
    }
}

struct TerminalView {
    reader: FlowViewReader,
    rendered: usize,
    images: Vec<atman_runtime::message::ImageSource>,
}

impl TerminalView {
    async fn new(worker: &FlowWorker) -> Result<Self> {
        Ok(Self {
            reader: worker.subscribe_view(PAGE).await?,
            rendered: 0,
            images: vec![],
        })
    }

    async fn submit(
        &mut self,
        host: &Attachment,
        worker: &FlowWorker,
        text: &str,
    ) -> Result<String> {
        use atman_runtime::{attachment_store::display_name, meta_commands::AttachmentCommand};
        if let Some(command) = AttachmentCommand::from_line(text) {
            return match command.map_err(anyhow::Error::msg)? {
                AttachmentCommand::List => Ok(if self.images.is_empty() {
                    "No pending attachments".into()
                } else {
                    self.images
                        .iter()
                        .enumerate()
                        .map(|(index, image)| format!("{}: {}", index + 1, display_name(image)))
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
                AttachmentCommand::Clear => {
                    self.images.clear();
                    Ok("Pending attachments cleared".into())
                }
                AttachmentCommand::Add(path) => {
                    let record = worker.launch_record().await?;
                    self.images.push(host.import_image(
                        worker,
                        record.as_ref(),
                        Path::new(path),
                    )?);
                    Ok(format!("Attached image ({} pending)", self.images.len()))
                }
            };
        }
        if text.trim_start().starts_with(':') {
            return host.command(worker, text).await;
        }
        let id = host
            .submit_user(
                worker,
                Uuid::now_v7(),
                UserContent {
                    text: text.into(),
                    images: self.images.clone(),
                    presentation: None,
                },
                BTreeMap::new(),
            )
            .await?;
        self.images.clear();
        Ok(format!("Input accepted: {id}"))
    }
}

fn render_view(
    printer: &mut Option<crate::ExternalPrinter>,
    rendered: &mut usize,
    view: &FlowView,
) {
    let entries = view.transcript().entries();
    for entry in &entries[*rendered..] {
        render_record(printer, &entry.record);
    }
    *rendered = entries.len();
}

fn render_record(printer: &mut Option<crate::ExternalPrinter>, record: &FlowRecord) {
    match record {
        FlowRecord::Continuation { source, messages } => {
            crate::render_note(
                printer,
                format!(
                    "Continued from session {} · inherited context",
                    source.session_id
                ),
            );
            for message in messages {
                if message.role == atman_runtime::message::MessageRole::User
                    || message.is_visible_assistant_reply()
                {
                    crate::emit(printer, format!("{}\n", message.text_concat()));
                }
            }
        }
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
