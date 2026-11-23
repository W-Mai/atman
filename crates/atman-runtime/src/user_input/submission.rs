//! Human input preparation and admission through the original host and inbox.

use std::{collections::BTreeMap, num::NonZeroUsize, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use uuid::Uuid;

use super::{CommandArguments, UserInputPresentation, resolve_command, resolve_slash_command};
use crate::{
    Executor,
    config_hub::ConfigHub,
    flow_authority::{EffectiveAuthority, contract_allows_shell},
    flow_runtime::{
        FlowWorker,
        host::{RuntimeHost, SessionHandle},
        scheduler::SchedulingStatus,
    },
    flow_store::{
        FlowDescriptor, FlowRegistration, SessionConfiguration, SessionDescriptor,
        SessionProjection,
        branches::HistoryHead,
        inbox::{
            Acceptance, AuthoredInputFingerprint, DeliveryMode, DeliveryRequest, InboxPayload,
            UserContent,
        },
        launch::LaunchRecord,
        local_operator::LocalOperator,
    },
    peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan},
    source_program::{SourceRoots, load_program_from_bundle},
    storage::ProjectContext,
};

/// Authored request identity before routing, image imports or command capture.
#[derive(Clone, Debug, Serialize)]
pub struct HumanInputRequest {
    pub request_id: Uuid,
    pub content: UserContent,
    pub invocation: BTreeMap<String, serde_json::Value>,
    pub delivery: DeliveryMode,
    pub expected_head: Option<HistoryHead>,
}

/// Trusted host bindings for one original conversation owner.
#[derive(Clone)]
pub struct HumanInputService {
    host: RuntimeHost,
    session: SessionHandle,
    worker: FlowWorker,
    executor: Executor,
    config: ConfigHub,
    project: Option<ProjectContext>,
    yield_interval: NonZeroUsize,
}

impl HumanInputService {
    pub fn new(
        host: RuntimeHost,
        session: SessionHandle,
        worker: FlowWorker,
        executor: Executor,
        config: ConfigHub,
        project: Option<ProjectContext>,
        yield_interval: NonZeroUsize,
    ) -> Self {
        Self {
            host,
            session,
            worker,
            executor,
            config,
            project,
            yield_interval,
        }
    }

    /// Reconcile exact authored retries without reading current command or configuration files.
    pub async fn accepted(
        &self,
        operator: LocalOperator,
        request: &HumanInputRequest,
    ) -> Result<Option<Acceptance>> {
        Self::observe_accepted(&self.host, &self.session, &self.worker, operator, request).await
    }

    /// Query original authored receipts without resolving current configuration or executors.
    pub async fn observe_accepted(
        host: &RuntimeHost,
        session: &SessionHandle,
        worker: &FlowWorker,
        operator: LocalOperator,
        request: &HumanInputRequest,
    ) -> Result<Option<Acceptance>> {
        validate_owner(host, session, worker)?;
        let authored = AuthoredInputFingerprint::for_request(&(worker.address(), request))?;
        Ok(worker
            .observe_authored_input(operator, request.request_id, authored)
            .await?)
    }

    /// Keep accepted host coordination alive through preparation and the original inbox receipt.
    pub async fn submit(
        &self,
        operator: LocalOperator,
        request: HumanInputRequest,
    ) -> Result<Acceptance> {
        self.validate_owner()?;
        let operation = self.host.retain_host_operation()?;
        let owner = self.session.retain_operation().await?;
        let service = self.clone();
        tokio::spawn(async move {
            let _operation = operation;
            let _owner = owner;
            let accepted = match service.accepted(operator, &request).await? {
                Some(accepted) => accepted,
                None => match service.submit_new(operator, &request).await {
                    Ok(accepted) => accepted,
                    Err(error) => service.accepted(operator, &request).await?.ok_or(error)?,
                },
            };
            if Self::resume_retained_input(
                &service.host,
                &service.session,
                &service.worker,
                &accepted,
            )
            .await?
            {
                service.bind().await?;
            }
            Ok(accepted)
        })
        .await?
    }

    /// Apply the original scheduling gate to a newly accepted input, including an exact retry.
    /// Returns true when the caller must bind the recorded entry because no scheduler was retained.
    pub async fn resume_retained_input(
        host: &RuntimeHost,
        session: &SessionHandle,
        worker: &FlowWorker,
        accepted: &Acceptance,
    ) -> Result<bool> {
        validate_owner(host, session, worker)?;
        let snapshot = session.snapshot().await?;
        let Some(registration) = retained_registration(&snapshot.catalog, worker) else {
            return Ok(false);
        };
        let retained = registration
            .retention
            .as_ref()
            .expect("retained registration");
        let receipt = accepted.receipt.require_durable()?;
        if receipt.journal_id != retained.destination.journal_id
            || receipt.sequence <= retained.destination.sequence
            || registration
                .retention_resume
                .as_ref()
                .is_some_and(|original| !original.same_input(accepted))
            || scheduler_running(session, worker).await?
        {
            return Ok(false);
        }
        let target = worker.address().clone();
        let authorized = target.clone();
        let resumed = host
            .resume_retained_scheduling(
                target,
                retained.request,
                accepted.clone(),
                move |candidate| {
                    if candidate != &authorized {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "retained input belongs to another Flow",
                        ));
                    }
                    Ok(())
                },
            )
            .await;
        let running = scheduler_running(session, worker).await?;
        if !running {
            resumed?;
        }
        Ok(!running)
    }

    async fn submit_new(
        &self,
        operator: LocalOperator,
        request: &HumanInputRequest,
    ) -> Result<Acceptance> {
        let authored = AuthoredInputFingerprint::for_request(&(self.worker.address(), request))?;
        let mut invocation = request.invocation.clone();
        // Host defaults belong to the admitted invocation, not to the authored retry identity.
        if !invocation.contains_key("effort")
            && let Some(settings) = self.worker.context().input_preferences()
            && let Some(selection) = settings.reasoning.selection()
            && let Ok(resolved) = self
                .executor
                .providers
                .models()
                .model_call_config_for_input(
                    super::controls::DEFAULT_INPUT_MODEL,
                    settings.model.as_deref(),
                )
                .resolve_reasoning(selection)
        {
            invocation.insert("effort".into(), resolved.to_string().into());
        }
        let payload = self
            .prepare_payload(request.content.clone(), invocation)
            .await?;
        let snapshot = self.session.snapshot().await?;
        let waiting_for_input = retained_registration(&snapshot.catalog, &self.worker)
            .is_some_and(|registration| registration.retention_resume.is_none());
        if !waiting_for_input {
            self.bind().await?;
        }
        Ok(self
            .worker
            .accept_authored_input(
                operator,
                DeliveryRequest {
                    expected_head: request.expected_head.clone(),
                    request_id: request.request_id,
                    sender: operator.sender(),
                    recipient: self.worker.address().clone(),
                    delivery: request.delivery,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload,
                },
                authored,
                |_| Ok(()),
            )
            .await?)
    }

    /// Capture source and attachments while retaining the original writer's effect fence.
    pub async fn prepare_payload(
        &self,
        content: UserContent,
        invocation: BTreeMap<String, serde_json::Value>,
    ) -> Result<InboxPayload> {
        let session = self.validate_scope().await?;
        let descriptor = self.worker.descriptor().await?;
        let record = self.worker.launch_record().await?;
        let preparation = PayloadPreparation {
            roots: session.source_roots_for(self.project.as_ref()),
            config: self.config.clone(),
            project: self.project.clone(),
            executor: self.executor.clone(),
            context: self.worker.context(),
        };
        self.effect(move || preparation.prepare(&descriptor, record.as_ref(), content, invocation))
            .await
    }

    /// Validate plain input against its retained entry without changing execution state.
    pub async fn validate_text_input(&self) -> Result<()> {
        let session = self.validate_scope().await?;
        let descriptor = self.worker.descriptor().await?;
        let roots = session.source_roots_for(self.project.as_ref());
        crate::read_work::capture(move || validate_text_input(&descriptor, &roots))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Preserve a running scheduler or install the retained entry under current host ceilings.
    pub async fn bind(&self) -> Result<()> {
        let session = self.validate_scope().await?;
        if scheduler_running(&self.session, &self.worker).await? {
            return Ok(());
        }
        let descriptor = self.worker.descriptor().await?;
        let directory = if descriptor.created_by.is_none()
            && descriptor.creation_request == self.session.id()
            && descriptor.address.instance_id.0.0 == self.session.id()
        {
            self.worker
                .launch_record()
                .await?
                .and_then(|record| record.input.execution)
                .and_then(|entry| entry.authority.workspace_root)
        } else {
            None
        };
        let roots = session.source_roots_for(self.project.as_ref());
        let config = self.config.clone();
        let project = self.project.clone();
        let tools = self.executor.tools.clone();
        let (authority, trust) = crate::read_work::capture(move || {
            let entry = descriptor.entry()?;
            let program = load_program_from_bundle(&entry.bundle, &entry.path, &roots)?;
            let flow = program
                .entry_flow(&entry.name)
                .and_then(|id| program.flow(&id))
                .context("entry is unavailable")?;
            let trust = config.trust_config()?;
            let authority = EffectiveAuthority::root(
                &trust,
                contract_allows_shell(flow.contract.as_ref()),
                directory.or_else(|| project.map(|project| project.root().to_path_buf())),
            );
            validate_flow(flow, &tools, "entry")?;
            Ok::<_, anyhow::Error>((authority, trust))
        })
        .await
        .map_err(std::io::Error::other)??;
        self.session
            .ensure_entry(
                self.worker.address().instance_id.clone(),
                self.executor.clone(),
                authority,
                trust,
                self.yield_interval,
            )
            .await?;
        Ok(())
    }

    /// Admit an explicit native control payload without reinterpreting it as authored text.
    pub async fn accept_payload(
        &self,
        operator: LocalOperator,
        request_id: Uuid,
        payload: InboxPayload,
        delivery: DeliveryMode,
        expected_head: Option<HistoryHead>,
    ) -> Result<Acceptance> {
        self.bind().await?;
        Ok(self
            .worker
            .accept_operator_input(
                operator,
                DeliveryRequest {
                    expected_head,
                    request_id,
                    sender: operator.sender(),
                    recipient: self.worker.address().clone(),
                    delivery,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload,
                },
                |_| Ok(()),
            )
            .await?)
    }

    fn validate_owner(&self) -> Result<()> {
        validate_owner(&self.host, &self.session, &self.worker)
    }

    async fn validate_scope(&self) -> Result<SessionDescriptor> {
        self.validate_owner()?;
        ensure!(
            self.worker.project().await? == self.project,
            "flow and execution project differ"
        );
        let descriptor = self.session.snapshot().await?.catalog.descriptor;
        ensure!(
            descriptor.configuration == SessionConfiguration::Atman(self.config.source()),
            "human input configuration does not match its original Session"
        );
        Ok(descriptor)
    }

    async fn effect<T: Send + 'static>(
        &self,
        effect: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        match self.host.authority_domain() {
            Some(authority) => {
                // Installed commands can update managed templates; preparation is not a pure read.
                let plan = AuthorityPlan::new([
                    (AuthorityKey::Profile, AuthorityMode::Exclusive),
                    (
                        AuthorityKey::Session(self.session.id()),
                        AuthorityMode::Exclusive,
                    ),
                ])?;
                let guard = authority
                    .acquire(
                        plan,
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + std::time::Duration::from_secs(10),
                    )
                    .await?;
                self.worker
                    .request_with_authority(Arc::new(guard), move |_| Ok(effect()))
                    .await?
            }
            None => self.worker.context().apply_instance_effect(effect).await?,
        }
    }
}

fn retained_registration<'a>(
    catalog: &'a SessionProjection,
    worker: &FlowWorker,
) -> Option<&'a FlowRegistration> {
    catalog
        .flows
        .iter()
        .find(|flow| flow.id == worker.address().instance_id && flow.retention.is_some())
}

async fn scheduler_running(session: &SessionHandle, worker: &FlowWorker) -> std::io::Result<bool> {
    Ok(session
        .scheduling_status(worker.address().instance_id.clone())
        .await?
        .is_some_and(|status| *status.borrow() == SchedulingStatus::Running))
}

fn validate_owner(host: &RuntimeHost, session: &SessionHandle, worker: &FlowWorker) -> Result<()> {
    ensure!(
        worker.address().session_id.0 == session.id()
            && host.session(session.id())?.same_owner(session)
            && session
                .loaded_flow(&worker.address().instance_id)
                .is_some_and(|loaded| Arc::ptr_eq(&loaded.context(), &worker.context())),
        "human input requires its original Session and Flow owner"
    );
    Ok(())
}

struct PayloadPreparation {
    roots: SourceRoots,
    config: ConfigHub,
    project: Option<ProjectContext>,
    executor: Executor,
    context: Arc<crate::Session>,
}

impl PayloadPreparation {
    fn prepare(
        &self,
        descriptor: &FlowDescriptor,
        record: Option<&LaunchRecord>,
        mut content: UserContent,
        invocation: BTreeMap<String, serde_json::Value>,
    ) -> Result<InboxPayload> {
        let (text, paths) = extract_presented_at_paths(&content.text, &mut content.presentation);
        content.text = text;
        for path in paths {
            content.images.push(import_user_image(
                &self.context,
                record,
                self.project.as_ref(),
                &path,
            )?);
        }
        let prompt = content
            .presentation
            .as_ref()
            .map_or(content.text.as_str(), |value| &value.prompt);
        let command = if prompt.trim_start().starts_with('/') {
            Some(resolve_slash_command(
                prompt,
                self.config.config_dir(),
                self.project.as_ref().map(|project| project.root()),
            )?)
        } else if record.is_some_and(|record| {
            record
                .input
                .execution
                .as_ref()
                .is_some_and(|binding| binding.user_routes)
        }) {
            let route = crate::routing::RouteProgram::load(&self.config)?
                .resolve(prompt.trim())
                .context("no route matched; configure routes.at or use /name args")?;
            Some(resolve_command(
                &route.command,
                CommandArguments::Text(&route.args),
                self.config.config_dir(),
                self.project.as_ref().map(|project| project.root()),
            )?)
        } else {
            None
        };
        if let Some((program, name, arguments, _)) = command {
            let flow = program
                .entry_flow(&name)
                .and_then(|id| program.flow(&id))
                .context("command entry is unavailable")?;
            validate_command(flow, &arguments, &self.executor.tools)?;
            return Ok(InboxPayload::Command {
                entry: Box::new(crate::flow_store::EntrySnapshot {
                    name,
                    path: program
                        .entry_path()
                        .context("command source path is unavailable")?
                        .to_path_buf(),
                    bundle: program.revision_bundle(),
                }),
                arguments: arguments
                    .into_iter()
                    .map(|(name, value)| Ok((name, crate::value::ValueJson::to_json(&value)?)))
                    .collect::<Result<_, crate::RuntimeError>>()?,
                invocation,
                user: Some(content),
            });
        }
        validate_text_input(descriptor, &self.roots)?;
        Ok(InboxPayload::UserSubmission {
            content,
            invocation,
        })
    }
}

fn validate_command(
    flow: &atman_rt::ast::FlowDecl,
    arguments: &[(String, crate::Value)],
    tools: &crate::ToolRegistry,
) -> Result<()> {
    validate_flow(flow, tools, "command")?;
    for (name, _) in arguments {
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
    Ok(())
}

fn validate_flow(
    flow: &atman_rt::ast::FlowDecl,
    tools: &crate::ToolRegistry,
    kind: &str,
) -> Result<()> {
    if let Err(errors) = crate::validate::validate(flow, tools) {
        anyhow::bail!(
            "{kind} validation failed: {}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(())
}

fn validate_text_input(descriptor: &FlowDescriptor, roots: &SourceRoots) -> Result<()> {
    let entry = descriptor.entry()?;
    let program = load_program_from_bundle(&entry.bundle, &entry.path, roots)?;
    let flow = program
        .entry_flow(&entry.name)
        .and_then(|id| program.flow(&id))
        .context("entry is unavailable")?;
    ensure!(
        crate::validate::invocation_message_parameter(flow)?.is_some(),
        "entry has no invocation user_message parameter; use :invoke <JSON arguments>"
    );
    Ok(())
}

/// Import an explicitly authored host path using the original conversation's attachment store.
pub fn import_user_image(
    context: &crate::Session,
    record: Option<&LaunchRecord>,
    project: Option<&ProjectContext>,
    path: &Path,
) -> Result<crate::message::ImageSource> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        record
            .and_then(|record| record.input.workspace.as_ref())
            .map(|workspace| workspace.binding.path.as_path())
            .or_else(|| project.map(|project| project.root()))
            .context("relative image paths require a bound project or workspace")?
            .join(path)
    };
    Ok(context.import_image_path(&path)?)
}

fn extract_presented_at_paths(
    text: &str,
    presentation: &mut Option<UserInputPresentation>,
) -> (String, Vec<std::path::PathBuf>) {
    let prompt = presentation.as_ref().map_or(text, |value| &value.prompt);
    let (prompt, paths) = extract_at_paths(prompt);
    let text = match presentation.as_mut() {
        Some(value) => {
            value.prompt = prompt;
            value.model_text()
        }
        None => prompt,
    };
    (text, paths)
}

fn extract_at_paths(line: &str) -> (String, Vec<std::path::PathBuf>) {
    let mut text = String::with_capacity(line.len());
    let mut attachments = Vec::new();
    for chunk in line.split_inclusive(char::is_whitespace) {
        let token = chunk.trim_end();
        if token.starts_with("@./") || token.starts_with("@../") || token.starts_with("@/") {
            attachments.push(std::path::PathBuf::from(&token[1..]));
            text.push_str(&chunk[token.len()..]);
        } else {
            text.push_str(chunk);
        }
    }
    if !attachments.is_empty() {
        text.truncate(text.trim_end().len());
    }
    (text, attachments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_image_paths_preserve_unrelated_prompt_formatting() {
        for text in [
            "first\n  second\n",
            "中文  spaces\tand @symbol",
            "  /echo text",
        ] {
            assert_eq!(extract_at_paths(text), (text.to_owned(), vec![]));
        }
        assert_eq!(
            extract_at_paths("first\n  second @./image.png\n  third @../other.png"),
            (
                "first\n  second \n  third".into(),
                vec!["./image.png".into(), "../other.png".into()]
            )
        );
    }
}
