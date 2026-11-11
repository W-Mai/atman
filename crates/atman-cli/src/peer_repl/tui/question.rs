//! Background question controls share ordinary host execution and native view delivery.

use super::*;
use atman_runtime::{
    flow_runtime::question::{
        PreparedQuestion, QuestionPreparation, QuestionRefreshRequest, QuestionRequest,
        question_followup,
    },
    flow_store::{EntrySnapshot, position::FlowPosition, retention::RetentionReceipt},
};
use atman_tui::{
    control::ControlTicket,
    quick_ask::{Command, HandoffKind, Operation, QuickAsk, Retry},
};

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(super) struct Services {
    pub scope: Attachment,
    pub question: QuickAsk,
    state: Arc<tokio::sync::Mutex<Service>>,
    discarded: Arc<std::sync::atomic::AtomicBool>,
}

struct Service {
    question: QuickAsk,
    worker: Option<FlowWorker>,
    launch: Option<(Vec<u8>, PreparedQuestion)>,
    keep: Option<(Uuid, FlowPosition)>,
    receipt: Option<RetentionReceipt>,
    handoff: Option<Handoff>,
    installed: Option<(HandoffKind, Uuid, ViewBinding)>,
    resume: Option<atman_runtime::flow_store::inbox::Acceptance>,
    resume_required: bool,
}

#[derive(Clone)]
struct Handoff {
    kind: HandoffKind,
    ticket: ControlTicket,
    replies: ControlReplies,
}

impl Services {
    pub(super) fn discarded(&self) -> bool {
        self.discarded.load(std::sync::atomic::Ordering::Acquire)
    }
}

pub(super) fn handles(command: &TuiControl) -> bool {
    matches!(
        command,
        TuiControl::Question(_)
            | TuiControl::Submit(_)
            | TuiControl::SwitchModel { .. }
            | TuiControl::UpdateTrust(_)
            | TuiControl::GoalControl { .. }
            | TuiControl::InsertQueuedSubmission { .. }
            | TuiControl::InterveneQueuedSubmission { .. }
    )
}

pub(super) async fn open(
    scope: Attachment,
    question: QuickAsk,
    request: Uuid,
    controls: mpsc::UnboundedSender<BoundControl>,
    _forms: Arc<atman_runtime::session::FormRegistry>,
) -> Result<Navigation> {
    ensure!(
        question.source.flow.session_id.0 == scope.session.id(),
        "Question source is outside this Session"
    );
    let source = scope
        .session
        .loaded_flow(&question.source.flow.instance_id)
        .context("Open the source Flow before asking")?;
    ensure!(
        !source.is_transient() && !source.is_stopped(),
        "Question source is unavailable"
    );
    let scope = scope.for_worker(&source).await?;
    let binding = ViewBinding {
        view_id: Uuid::now_v7(),
        revision: 1,
        flow: question.address(),
    };
    let mut handle = TuiHandle::from_question_draft(
        question.clone(),
        binding.clone(),
        controls,
        scope.config.clone().into(),
        scope
            .project
            .as_ref()
            .map(|project| project.root().display().to_string()),
    )?;
    let (commands, rx) = mpsc::unbounded_channel();
    handle.trust = source.context().trust_config();
    handle.cmd_rx = Some(rx);
    let services = Services {
        scope: scope.clone(),
        question: question.clone(),
        state: Arc::new(tokio::sync::Mutex::new(Service {
            question,
            worker: None,
            launch: None,
            keep: None,
            receipt: None,
            handoff: None,
            installed: None,
            resume: None,
            resume_required: false,
        })),
        discarded: Arc::default(),
    };
    let service = ViewServices {
        question: Some(services),
        new_session: None,
        planning: None,
        historical_directory: None,
        project: scope
            .project
            .as_ref()
            .map(|project| project.root().to_owned()),
        binding,
        commands,
        execution: None,
    };
    Ok(Navigation::Question {
        request,
        kind: None,
        handle: Box::new(handle),
        service: Box::new(service),
        delivery: None,
        source: None,
    })
}

pub(super) async fn restore(scope: &Attachment, worker: &FlowWorker) -> Result<Option<Services>> {
    if worker.is_transient() {
        return Ok(None);
    }
    let descriptor = worker.descriptor().await?;
    let Ok(entry) = descriptor.entry() else {
        return Ok(None);
    };
    if entry.name != "quickask" {
        return Ok(None);
    }
    let Some(record) = worker.launch_record().await? else {
        return Ok(None);
    };
    if !record
        .input
        .execution
        .as_ref()
        .is_some_and(|execution| execution.authority.read_only() == execution.authority)
    {
        return Ok(None);
    }
    let origin = {
        let messages = worker.context().messages();
        atman_runtime::flow_runtime::question::reference_origin(&messages).cloned()
    };
    let Some(origin) = origin else {
        return Ok(None);
    };
    let snapshot = worker.snapshot().await?;
    let arguments = snapshot
        .last_invocation
        .as_ref()
        .map_or(&record.input.request.arguments, |invocation| {
            &invocation.arguments
        });
    let model = arguments
        .get("model")
        .and_then(serde_json::Value::as_str)
        .context("Retained question model is missing")?
        .to_owned();
    let invocation = snapshot
        .last_invocation
        .as_ref()
        .map_or(&record.input.request.invocation, |invocation| {
            &invocation.invocation
        });
    let effort = invocation
        .get("effort")
        .and_then(serde_json::Value::as_str)
        .map(str::parse)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let question = QuickAsk::from_kept(worker.address().clone(), origin.source, model, effort)?;
    let catalog = scope.session.snapshot().await?;
    let registration = catalog
        .catalog
        .flows
        .iter()
        .find(|row| row.id == worker.address().instance_id);
    let receipt = registration.and_then(|row| row.retention.clone());
    let recorded_resume = registration.and_then(|row| row.retention_resume.clone());
    let running = scope
        .session
        .scheduling_status(worker.address().instance_id.clone())
        .await?
        .is_some_and(|status| {
            *status.borrow() == atman_runtime::flow_runtime::scheduler::SchedulingStatus::Running
        });
    let resume_required = receipt.is_some() && (!running || recorded_resume.is_none());
    let mut resume = None;
    if resume_required && let Some(receipt) = &receipt {
        resume = recorded_resume;
        let pending: std::collections::HashSet<_> = snapshot
            .pending
            .iter()
            .filter(|entry| entry.request.is_editable_user_input())
            .map(|entry| entry.id)
            .collect();
        let mut before = None;
        let history = atman_runtime::flow_runtime::catalog::SessionCatalog::new(
            Some(scope.host.clone()),
            Some(scope.data_root.clone()),
        );
        while resume.is_none() && !pending.is_empty() {
            let page = history
                .inbox_page(
                    worker.address(),
                    atman_runtime::flow_store::inbox_view::InboxSide::Received,
                    before,
                    50,
                    |_| Ok(()),
                )
                .await?;
            for item in page.items {
                if let atman_runtime::flow_store::inbox_view::InboxHistoryItem::Received(entry) =
                    item
                    && pending.contains(&entry.envelope.id)
                    && entry
                        .acceptance
                        .receipt
                        .require_durable()
                        .is_ok_and(|position| {
                            position.journal_id == receipt.destination.journal_id
                                && position.sequence > receipt.destination.sequence
                        })
                {
                    if let atman_runtime::flow_store::inbox::InboxPayload::Invocation {
                        arguments,
                        ..
                    } = &entry.envelope.request.payload
                        && let Some(model) =
                            arguments.get("model").and_then(serde_json::Value::as_str)
                    {
                        question.set_model(model.to_owned());
                    }
                    resume = Some(entry.acceptance);
                    break;
                }
            }
            let Some(next) = page.next else {
                break;
            };
            before = Some(next);
        }
        if resume.is_some() {
            question.set_retry(Some(Retry::Resume));
        }
    }
    Ok(Some(Services {
        scope: scope.clone(),
        question: question.clone(),
        state: Arc::new(tokio::sync::Mutex::new(Service {
            question,
            worker: Some(worker.clone()),
            launch: None,
            keep: None,
            receipt,
            handoff: None,
            installed: None,
            resume,
            resume_required,
        })),
        discarded: Arc::default(),
    }))
}

pub(super) async fn dispatch(
    services: Services,
    ticket: ControlTicket,
    replies: ControlReplies,
    command: TuiControl,
    controls: mpsc::UnboundedSender<BoundControl>,
    forms: Arc<atman_runtime::session::FormRegistry>,
) -> Result<Navigation> {
    // This queue is only acquired by background services, never by input or rendering.
    let mut service = services.state.lock().await;
    let command = match command {
        TuiControl::SwitchModel { request_id, model } => {
            TuiControl::Question(Command::SwitchModel {
                request_id,
                model,
                effort: None,
            })
        }
        command => command,
    };
    match command {
        TuiControl::Submit(submission) => {
            let result = submit(
                &services,
                &mut service,
                &ticket,
                &replies,
                submission,
                controls,
                forms,
            )
            .await;
            Ok(result.unwrap_or_else(|error| {
                reply(TuiCommand::SubmissionResult {
                    result: Err(format!("{error:#}")),
                })
            }))
        }
        TuiControl::Question(Command::SwitchModel {
            request_id,
            model,
            effort,
        }) => {
            let result = async {
                ensure!(
                    !services.discarded() && !service.question.retention_pending(),
                    "Question is not available for model changes"
                );
                ensure!(
                    service.handoff.is_none()
                        && service.launch.is_none()
                        && service.resume.is_none(),
                    "Retry the pending question operation before changing its model"
                );
                let resolved = if let Some(worker) = &service.worker {
                    atman_runtime::flow_runtime::question::validate_question_model_switch(
                        worker,
                        &services.scope.executor,
                        &model,
                        effort.as_ref(),
                    )
                    .await?
                    .model
                } else {
                    let models = services.scope.config.models();
                    let resolved = models.resolve_alias(&model);
                    ensure!(
                        models.model_entry(&resolved).is_some(),
                        "Selected model is unavailable"
                    );
                    resolved
                };
                service.question.set_model(resolved.clone());
                Ok::<_, anyhow::Error>(resolved)
            }
            .await
            .map_err(|error| error.to_string());
            Ok(reply(TuiCommand::ModelSwitchResult {
                request_id,
                model,
                result,
            }))
        }
        TuiControl::UpdateTrust(_) => Ok(reply(TuiCommand::TrustUpdateResult(Err(
            "Quick Ask has a read-only capability ceiling".into(),
        )))),
        TuiControl::Question(Command::Installed { kind, request_id }) => {
            let expected = (kind, request_id, ticket.binding.clone());
            if service.installed.as_ref() != Some(&expected) {
                let handoff = service
                    .handoff
                    .as_ref()
                    .context("Question handoff is not pending")?;
                let worker = service
                    .worker
                    .as_ref()
                    .context("Question owner is unavailable")?;
                ensure!(
                    handoff.kind == kind
                        && handoff.ticket.request_id == request_id
                        && handoff.ticket.binding.view_id == ticket.binding.view_id
                        && handoff.ticket.binding.revision.checked_add(1)
                            == Some(ticket.binding.revision)
                        && ticket.binding.flow.instance() == Some(worker.address()),
                    "Question installation acknowledgement changed"
                );
                service.installed = Some(expected);
                service.handoff = None;
                service.launch = None;
            }
            if kind == HandoffKind::Keep {
                let receipt = service
                    .receipt
                    .clone()
                    .context("Retained question receipt is unavailable")?;
                services
                    .scope
                    .host
                    .acknowledge_retention(receipt, |_| Ok(()))
                    .await?;
            }
            Ok(Navigation::Applied)
        }
        TuiControl::Question(Command::Retry(Retry::Handoff { kind, request_id })) => {
            ensure!(!services.discarded(), "Question was discarded");
            let handoff = service
                .handoff
                .as_ref()
                .context("Question handoff is not pending")?;
            ensure!(
                handoff.kind == kind
                    && handoff.ticket.request_id == request_id
                    && handoff.ticket.binding == ticket.binding,
                "Question retry identity changed"
            );
            prepare_handoff(&services, &service, controls, forms).await
        }
        TuiControl::Question(Command::Retry(Retry::Resume)) => {
            if service.resume.is_none() {
                service.question.set_retry(None);
                return Ok(Navigation::Applied);
            }
            resume(&services, &mut service, &replies).await?;
            Ok(Navigation::Applied)
        }
        command @ (TuiControl::CancelFlow | TuiControl::HardStop) => {
            let worker = service
                .worker
                .as_ref()
                .context("Question has not started")?;
            services
                .scope
                .tui_control(worker, ticket.request_id, command, &replies, &forms)
                .await?;
            Ok(Navigation::Applied)
        }
        TuiControl::Question(Command::Operate(operation, effort)) => {
            let result = operate(
                &services,
                &mut service,
                &ticket,
                &replies,
                operation,
                effort,
                controls,
                forms,
            )
            .await;
            Ok(result.unwrap_or_else(|error| {
                reply(TuiCommand::QuestionOperationResult {
                    operation,
                    result: Err(format!("{error:#}")),
                })
            }))
        }
        _ => Ok(reply(TuiCommand::Toast {
            message: "This action is unavailable in a read-only question".into(),
            level: atman_tui::app::NoteLevel::Warn,
        })),
    }
}

fn reply(command: TuiCommand) -> Navigation {
    Navigation::Reply(Box::new(command))
}

async fn preparation(scope: &Attachment) -> Result<QuestionPreparation> {
    let (program, name, _, _) = crate::resolve_slash_command_from(
        "/quickask",
        scope.config.config_dir(),
        scope.project.as_ref().map(|project| project.root()),
    )?;
    let flow = program
        .entry_flow(&name)
        .and_then(|id| program.flow(&id))
        .context("Quick Ask entry is unavailable")?;
    let trust = scope.config.trust_config()?;
    let authority = EffectiveAuthority::root(
        &trust,
        contract_allows_shell(flow.contract.as_ref()),
        scope
            .project
            .as_ref()
            .map(|project| project.root().to_owned()),
    )
    .read_only();
    Ok(QuestionPreparation {
        entry: EntrySnapshot {
            name,
            path: program
                .entry_path()
                .context("Question source path is unavailable")?
                .to_owned(),
            bundle: program.revision_bundle(),
        },
        executor: scope.executor.clone(),
        authority,
        trust,
        yield_interval: YIELD,
    })
}

async fn submit(
    services: &Services,
    service: &mut Service,
    ticket: &ControlTicket,
    replies: &ControlReplies,
    submission: TuiSubmission,
    controls: mpsc::UnboundedSender<BoundControl>,
    forms: Arc<atman_runtime::session::FormRegistry>,
) -> Result<Navigation> {
    ensure!(!services.discarded(), "Question was discarded");
    ensure!(
        service.handoff.is_none(),
        "Retry opening the accepted question before sending another message"
    );
    ensure!(
        service.resume.is_none(),
        "Retry resuming the accepted question before sending another message"
    );
    ensure!(
        !service.question.retention_pending(),
        "Retry pending Keep before sending another question"
    );
    let model = service.question.selected_model();
    if let Some(worker) = &service.worker {
        let request = question_followup(
            worker.address().clone(),
            ticket.request_id,
            USER,
            UserContent {
                text: submission.text,
                images: submission.images,
                presentation: submission.presentation,
            },
            &model,
            submission.reasoning.as_ref(),
            &services.scope.executor,
        )?;
        let accepted = worker.accept_input(request, |_| Ok(())).await?;
        if service.resume_required {
            service.resume = Some(accepted);
            let _ = replies.send(TuiCommand::SubmissionResult { result: Ok(()) });
            resume(services, service, replies).await?;
            return Ok(Navigation::Applied);
        }
        return Ok(reply(TuiCommand::SubmissionResult { result: Ok(()) }));
    }
    let signature = serde_json::to_vec(&(
        &submission.text,
        &submission.images,
        &submission.presentation,
        &submission.reasoning,
        &model,
    ))?;
    if let Some((original, _)) = &service.launch {
        ensure!(
            original == &signature,
            "Retry the original question or open New Ask; its launch may already have been accepted"
        );
    } else {
        let prepared = services
            .scope
            .host
            .prepare_question(
                QuestionRequest {
                    id: service.question.id,
                    request_id: ticket.request_id,
                    source: service.question.captured_source(),
                    user: USER,
                    content: UserContent {
                        text: submission.text,
                        images: submission.images,
                        presentation: submission.presentation,
                    },
                    model,
                    effort: submission.reasoning,
                },
                preparation(&services.scope).await?,
                |_| Ok(()),
            )
            .await?;
        service.launch = Some((signature, prepared));
    }
    let prepared = &service.launch.as_ref().unwrap().1;
    let launched = prepared.launch(&services.scope.host, |_, _| Ok(())).await?;
    service.question.captured(launched.source);
    let worker = launched.flow.worker;
    capture_detail(&service.question, &worker, &launched.budget);
    service.worker = Some(worker);
    service.handoff = Some(Handoff {
        kind: HandoffKind::Launch,
        ticket: ticket.clone(),
        replies: replies.observation(),
    });
    prepare_handoff(services, service, controls, forms).await
}

#[allow(clippy::too_many_arguments)]
async fn operate(
    services: &Services,
    service: &mut Service,
    ticket: &ControlTicket,
    replies: &ControlReplies,
    operation: Operation,
    effort: Option<atman_runtime::provider::ReasoningSelection>,
    controls: mpsc::UnboundedSender<BoundControl>,
    forms: Arc<atman_runtime::session::FormRegistry>,
) -> Result<Navigation> {
    ensure!(!services.discarded(), "Question was discarded");
    ensure!(
        service.handoff.is_none() || operation == Operation::Discard,
        "Retry opening the accepted question before changing context or retention"
    );
    match operation {
        Operation::Discard => {
            ensure!(
                service.receipt.is_none() && !service.question.retention_pending(),
                "Retained or pending history cannot be discarded here"
            );
            let owner = service.worker.clone().or_else(|| {
                service.launch.as_ref().and_then(|(_, prepared)| {
                    services
                        .scope
                        .session
                        .loaded_flow(&prepared.address().instance_id)
                })
            });
            if let Some(worker) = owner {
                ensure!(
                    worker.is_transient(),
                    "Retained history cannot be discarded here"
                );
                let expected = worker.snapshot().await?.cursor;
                services
                    .scope
                    .host
                    .discard_transient(worker.address().clone(), expected, |_| Ok(()))
                    .await?;
            }
            services
                .discarded
                .store(true, std::sync::atomic::Ordering::Release);
            service.worker = None;
            service.launch = None;
            service.handoff = None;
            service.resume = None;
            Ok(reply(TuiCommand::QuestionOperationResult {
                operation,
                result: Ok(None),
            }))
        }
        Operation::Refresh => {
            ensure!(
                !service.question.retention_pending(),
                "Retry pending Keep before refreshing"
            );
            let worker = service
                .worker
                .as_ref()
                .context("Send a question before refreshing context")?;
            let refresh = services
                .scope
                .host
                .prepare_question_refresh(
                    QuestionRefreshRequest {
                        id: ticket.request_id,
                        question: worker.address().clone(),
                        expected: worker.snapshot().await?.cursor,
                        source: service.question.captured_source(),
                        model: service.question.selected_model(),
                        effort,
                    },
                    preparation(&services.scope).await?,
                    |_| Ok(()),
                )
                .await?;
            refresh.apply(worker, |_| Ok(())).await?;
            capture_detail(&service.question, worker, &refresh.budget);
            Ok(reply(TuiCommand::QuestionOperationResult {
                operation,
                result: Ok(Some(refresh.source)),
            }))
        }
        Operation::Keep => {
            let worker = service
                .worker
                .as_ref()
                .context("Send a question before keeping it")?;
            if service.keep.is_none() {
                service.keep = Some((ticket.request_id, worker.snapshot().await?.cursor));
            }
            let (request, expected) = service.keep.as_ref().unwrap();
            let kept = match services
                .scope
                .host
                .keep_transient(worker.address().clone(), *request, expected.clone(), |_| {
                    Ok(())
                })
                .await
            {
                Ok(kept) => kept,
                Err(error) => {
                    let pending = error.get_ref().is_some_and(|error| {
                        error.is::<atman_runtime::flow_runtime::host::RetentionPending>()
                    });
                    service.question.set_retention_pending(pending);
                    if !pending {
                        service.keep = None;
                    }
                    return Err(error.into());
                }
            };
            service.question.set_retention(kept.receipt.clone());
            service.question.set_retention_pending(true);
            service.receipt = Some(kept.receipt);
            service.resume_required = true;
            service.worker = Some(kept.worker);
            service.handoff = Some(Handoff {
                kind: HandoffKind::Keep,
                ticket: ticket.clone(),
                replies: replies.observation(),
            });
            prepare_handoff(services, service, controls, forms).await
        }
    }
}

async fn prepare_handoff(
    services: &Services,
    service: &Service,
    controls: mpsc::UnboundedSender<BoundControl>,
    forms: Arc<atman_runtime::session::FormRegistry>,
) -> Result<Navigation> {
    let pending = service
        .handoff
        .as_ref()
        .context("Question handoff is unavailable")?;
    let worker = service
        .worker
        .as_ref()
        .context("Question execution owner is unavailable")?;
    match attachment(
        services,
        worker,
        &pending.ticket,
        controls,
        forms,
        pending.kind,
    )
    .await
    {
        Ok(mut navigation) => {
            if let Navigation::Question { delivery, .. } = &mut navigation {
                *delivery = Some(pending.replies.clone());
            }
            Ok(navigation)
        }
        Err(error) => {
            service.question.set_retry(Some(Retry::Handoff {
                kind: pending.kind,
                request_id: pending.ticket.request_id,
            }));
            let _ = pending.replies.send(TuiCommand::QuestionHandoffFailed {
                kind: pending.kind,
                request_id: pending.ticket.request_id,
                error: format!("{error:#}"),
            });
            Ok(Navigation::Applied)
        }
    }
}

async fn resume(
    services: &Services,
    service: &mut Service,
    replies: &ControlReplies,
) -> Result<()> {
    ensure!(!services.discarded(), "Question was discarded");
    let result = async {
        let accepted = service
            .resume
            .clone()
            .context("Question has no accepted input awaiting resume")?;
        let receipt = service
            .receipt
            .as_ref()
            .context("Question retention receipt is unavailable")?;
        let worker = service
            .worker
            .as_ref()
            .context("Question execution owner is unavailable")?;
        services
            .scope
            .host
            .resume_retained_scheduling(worker.address().clone(), receipt.request, accepted, |_| {
                Ok(())
            })
            .await?;
        let running = services
            .scope
            .session
            .scheduling_status(worker.address().instance_id.clone())
            .await?
            .is_some_and(|status| {
                *status.borrow()
                    == atman_runtime::flow_runtime::scheduler::SchedulingStatus::Running
            });
        if !running {
            let scope = services.scope.for_worker(worker).await?;
            let authority = worker
                .launch_record()
                .await?
                .and_then(|record| record.input.execution)
                .context("Retained question execution ceiling is unavailable")?
                .authority
                .read_only();
            scope
                .session
                .bind_entry(
                    worker.address().instance_id.clone(),
                    scope.executor.clone(),
                    authority,
                    scope.config.trust_config()?,
                    YIELD,
                )
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    match result {
        Ok(()) => {
            service.resume = None;
            service.resume_required = false;
            service.question.set_retry(None);
        }
        Err(error) => {
            service.question.set_retry(Some(Retry::Resume));
            let _ = replies.send(TuiCommand::Toast {
                message: format!("Question input was accepted; retry resuming it: {error:#}"),
                level: atman_tui::app::NoteLevel::Error,
            });
        }
    }
    Ok(())
}

fn capture_detail(
    question: &QuickAsk,
    worker: &FlowWorker,
    budget: &atman_runtime::flow_runtime::question::QuestionModelBudget,
) {
    let messages = worker.context().messages();
    let Some(origin) = atman_runtime::flow_runtime::question::reference_origin(&messages) else {
        return;
    };
    let omissions = &origin.omissions;
    question.set_source_detail(format!(
        "{} · commit {} · reference budget ≈{} tokens{}{}{}{}",
        origin.source.flow.instance_id,
        origin.source.cursor.sequence,
        budget.reference_tokens,
        if omissions.earlier_context {
            " · earlier context omitted"
        } else {
            ""
        },
        if omissions.incomplete_tool_parts > 0 {
            " · incomplete tool transaction omitted"
        } else {
            ""
        },
        if omissions.unavailable_images > 0 {
            " · unavailable images omitted"
        } else {
            ""
        },
        if omissions.unavailable_outputs > 0 {
            " · unavailable outputs omitted"
        } else {
            ""
        },
    ));
}

async fn attachment(
    services: &Services,
    worker: &FlowWorker,
    ticket: &ControlTicket,
    controls: mpsc::UnboundedSender<BoundControl>,
    forms: Arc<atman_runtime::session::FormRegistry>,
    kind: HandoffKind,
) -> Result<Navigation> {
    let scope = services.scope.for_worker(worker).await?;
    worker
        .context()
        .bind_model_registry(scope.config.models().clone())?;
    let binding = ViewBinding {
        revision: ticket
            .binding
            .revision
            .checked_add(1)
            .context("Question view revision exhausted")?,
        flow: worker.address().clone().into(),
        ..ticket.binding.clone()
    };
    let mut handle =
        TuiHandle::from_flow(worker, binding.clone(), controls, scope.config.clone()).await?;
    handle.project_root = scope
        .project
        .as_ref()
        .map(|project| project.root().display().to_string());
    handle.session_name = Some("Quick Ask".into());
    handle.form_rx = Some(forms.subscribe_for(worker.context().deferred_form_inbox()));
    handle.trust = worker.context().trust_config();
    let (commands, rx) = mpsc::unbounded_channel();
    handle.cmd_rx = Some(rx);
    let panels = crate::tui_panels::PanelServices::for_scope(
        worker.context(),
        scope.executor.clone(),
        commands.clone(),
        vec![],
        scope.config.clone(),
        scope.project.clone(),
    )?;
    let service = ViewServices {
        question: Some(services.clone()),
        new_session: None,
        planning: None,
        historical_directory: None,
        project: scope
            .project
            .as_ref()
            .map(|project| project.root().to_owned()),
        binding,
        commands: commands.clone(),
        execution: Some(ExecutionServices {
            scope: scope.clone(),
            worker: worker.clone(),
            controls: controls::Controls::start(scope, worker.clone(), panels, forms, commands),
        }),
    };
    // The original shared question is installed by the source-bound handoff reducer.
    Ok(Navigation::Question {
        request: ticket.request_id,
        kind: Some(kind),
        handle: Box::new(handle),
        service: Box::new(service),
        delivery: None,
        source: Some(ticket.binding.clone()),
    })
}
